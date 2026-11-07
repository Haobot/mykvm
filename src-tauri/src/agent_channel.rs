//! 跨机 Agent 互联通道（RFC：Doc/Rfc/跨机Agent互联与受控协作通道.md §6.1/§6.3）。
//!
//! P0 范围：协议包结构 + 鉴权 + pending 请求表 + 两段式 echo 闭环。
//! 两段式：request 与 response 是两条独立的 QUIC 流消息，靠 request_id 关联；
//! 应答由接收方反向新建流发回（ConnectionMap 对称，反向拨号命中既有信任面），
//! 不改动 quic_transport.rs 的 StreamHandler 签名。

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc, Arc, Mutex, OnceLock,
    },
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 协议字符串：旧版对端在调度链各分支 decode 失败而落入默认拒绝路径
/// （`b"reject"` ack），对旧版无破坏。
pub(crate) const AGENT_PROTOCOL: &str = "mykvm.agent.v1";

/// kind 取值（P0 仅实现 request/response/cancel/notify）。
pub(crate) const AGENT_KIND_REQUEST: &str = "request";
pub(crate) const AGENT_KIND_RESPONSE: &str = "response";
pub(crate) const AGENT_KIND_NOTIFY: &str = "notify";
pub(crate) const AGENT_KIND_CANCEL: &str = "cancel";

/// P0 仅有的能力：回显 payload，用于验证两段式 RPC 闭环。
pub(crate) const AGENT_CAPABILITY_ECHO: &str = "echo";

/// Agent 通道的用户意图配置（RFC §6.3）：开关 + 能力级别 + 白名单。
/// 前端所有（Settings 编辑），随 layout.json 持久化；载入（normalize_saved_layout）
/// 与保存（save_layout）路径各自钳制，消费点（level gate）再次兜底——
/// 照 focus_halo「存储钳制 + 消费点 sanitized」双层先例。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub(crate) struct AgentChannelConfig {
    /// 总开关（RFC §5.3 对策三）：独立于 input_mode，一键禁用不影响键鼠/剪贴板。
    pub enabled: bool,
    /// 能力级别："readonly"(L0) | "files"(L1) | "commands"(L2) | "full"(L3)。
    pub level: String,
    /// 文件白名单根目录（P1b 消费）；空 = 运行期默认 [日志目录, Downloads]。
    pub file_roots: Vec<String>,
    /// L2 命令白名单（P3 消费），本阶段仅定义与清洗。
    pub cmd_whitelist: Vec<String>,
    /// L3 审批窗口秒数（P3 消费）。
    pub approval_ttl_secs: u64,
}

impl Default for AgentChannelConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            level: "readonly".into(),
            file_roots: Vec::new(),
            cmd_whitelist: Vec::new(),
            approval_ttl_secs: 300,
        }
    }
}

impl AgentChannelConfig {
    /// 钳制非法值：level 越界回 "readonly"（fail closed）、白名单去空白项并
    /// 去重排序（确定性存储）。TTL/开关不钳（布尔无越界；TTL 语义由 P3 消费时定）。
    pub(crate) fn sanitized(&self) -> Self {
        Self {
            enabled: self.enabled,
            level: normalize_agent_level(&self.level),
            file_roots: clean_string_list(&self.file_roots),
            cmd_whitelist: clean_string_list(&self.cmd_whitelist),
            approval_ttl_secs: self.approval_ttl_secs,
        }
    }

    /// capability 是否被当前 level 放行（映射表见 RFC §6.2 + 任务书 P1a 范围）：
    /// echo/status.get/msg.send/log.read/file.stat/file.read ∈ L0+；
    /// file.write ∈ L1+；cmd.run ∈ L2+；未知 capability 一律拒绝。
    pub(crate) fn capability_allowed(&self, capability: &str) -> bool {
        let level = agent_level_number(&self.level);
        match capability {
            "echo" | "status.get" | "msg.send" | "log.read" | "file.stat" | "file.read" => {
                level >= 0
            }
            "file.write" => level >= 1,
            "cmd.run" => level >= 2,
            _ => false,
        }
    }
}

/// level 字符串 → 序数（L0-L3）。非法值（含大小写/空白变体）一律按 L0 处理：
/// gate 的消费点会先 sanitized，这里是第二道 fail closed 兜底。
fn agent_level_number(level: &str) -> i8 {
    match level.trim() {
        "files" => 1,
        "commands" => 2,
        "full" => 3,
        _ => 0,
    }
}

fn normalize_agent_level(level: &str) -> String {
    match level.trim() {
        "files" => "files".into(),
        "commands" => "commands".into(),
        "full" => "full".into(),
        _ => "readonly".into(),
    }
}

/// 白名单清洗：trim、去空白项、排序去重（排序保证落盘内容确定、可 diff）。
fn clean_string_list(values: &[String]) -> Vec<String> {
    let mut cleaned: Vec<String> = values
        .iter()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect();
    cleaned.sort();
    cleaned.dedup();
    cleaned
}

/// LayoutState serde 默认值入口（与 default_input_mode 等惯例一致）。
pub(crate) fn default_agent_channel() -> AgentChannelConfig {
    AgentChannelConfig::default()
}

/// Agent 通道协议包（RFC §6.1）。字段命名与既有线上包一致（camelCase），
/// 新增字段一律带 `#[serde(default)]` 以保证向前兼容（旧解码器忽略未知字段）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AgentChannelPacket {
    pub protocol: String,
    /// "request" | "response" | "notify" | "cancel"
    pub kind: String,
    /// 多路分复用键：单调计数 + 随机 nonce。
    #[serde(default)]
    pub request_id: String,
    /// 发送方 device id。
    pub origin_id: String,
    /// 发送方的 QUIC transport 公钥（base64 证书 DER）。鉴权按它优先匹配——
    /// 与 ClipboardPacket 同因：device id 会随 LAN IP 漂移，证书钉扎的
    /// transport key 才是稳定身份。
    #[serde(default)]
    pub origin_transport_public_key: String,
    /// 目标 device id；空 = 广播语义（复用 packet_targets_local 路由）。
    #[serde(default)]
    pub target_id: String,
    /// 鉴权三件套，语义照抄 clipboard_packet_authorized。
    #[serde(default)]
    pub cluster_id: String,
    #[serde(default)]
    pub pair_secret: String,
    /// 能力名；P0 仅 "echo"。
    #[serde(default)]
    pub capability: String,
    /// 参数/结果；大载荷（P2+）只放 transfer_id 引用。
    #[serde(default)]
    pub payload: Value,
    /// response 专用："ok" | "err" | "denied" | "accepted"。
    #[serde(default)]
    pub status: Option<String>,
}

/// send_agent_request 的成功返回：response 的 status 与 payload。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AgentResponse {
    pub status: String,
    pub payload: Value,
}

/// 一条已注册请求的等待句柄：响应到达（或取消/超时）时经此通道完成等待者。
/// 完成原语用 std mpsc（与仓库既有网络等待风格一致，如
/// quic_transport 的 send_stream_expect_ack recv_timeout；tokio oneshot 无先例）。
pub(crate) type PendingReplyRx = mpsc::Receiver<Result<AgentResponse, String>>;

/// request_id ↔ 等待者 的挂起表。生产路径用 [`global_pending_table`]，
/// 单元测试构造独立实例（避免进程级全局状态串线）。
pub(crate) struct AgentPendingTable {
    entries: Mutex<HashMap<String, PendingEntry>>,
}

struct PendingEntry {
    created_at: Instant,
    timeout: Duration,
    reply: mpsc::SyncSender<Result<AgentResponse, String>>,
}

impl Default for AgentPendingTable {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentPendingTable {
    pub(crate) fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// 注册等待者并返回接收端。调用方随后发出 request；响应到达后由
    /// [`AgentPendingTable::resolve`] 完成此 receiver。
    pub(crate) fn register(&self, request_id: String, timeout: Duration) -> PendingReplyRx {
        self.reap_expired();
        let (tx, rx) = mpsc::sync_channel(1);
        if let Ok(mut entries) = self.entries.lock() {
            // 同一 request_id 重复注册（理论不应发生，防御性处理）：覆盖旧条目，
            // 旧等待者的 sender 被 drop，其 recv 端断连报错——不会收到串线结果。
            entries.insert(
                request_id,
                PendingEntry {
                    created_at: Instant::now(),
                    timeout,
                    reply: tx,
                },
            );
        }
        rx
    }

    /// 按 request_id 完成等待者并移除条目。返回 false = 表中查无此 id
    /// （超时已回收/重复响应/重放），调用方应拒绝该包。
    pub(crate) fn resolve(&self, request_id: &str, result: Result<AgentResponse, String>) -> bool {
        self.reap_expired();
        let entry = self
            .entries
            .lock()
            .ok()
            .and_then(|mut entries| entries.remove(request_id));
        match entry {
            Some(entry) => entry.reply.send(result).is_ok(),
            None => false,
        }
    }

    /// 主动移除挂起请求（kind=cancel / 发送失败清理）。
    pub(crate) fn cancel(&self, request_id: &str) -> bool {
        match self.entries.lock() {
            Ok(mut entries) => entries.remove(request_id).is_some(),
            Err(_) => false,
        }
    }

    /// 回收过期条目：迟到的响应不再完成等待者（防重放，RFC §8 对策四）。
    /// 无后台线程，在 register/resolve 入口顺带执行。
    fn reap_expired(&self) {
        if let Ok(mut entries) = self.entries.lock() {
            entries.retain(|_, entry| entry.created_at.elapsed() < entry.timeout);
        }
    }
}

/// 生产路径的全局挂起表。进程级单例；测试不要用它（构造独立实例）。
pub(crate) fn global_pending_table() -> &'static AgentPendingTable {
    static TABLE: OnceLock<AgentPendingTable> = OnceLock::new();
    TABLE.get_or_init(AgentPendingTable::new)
}

/// request_id 的进程内单调计数部分。
static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(1);

/// 生成 request_id：进程内单调计数 + 随机 nonce（防重放/防碰撞）。
pub(crate) fn new_request_id() -> String {
    format!(
        "{}-{}",
        REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed),
        crate::random_hex(8)
    )
}

/// 鉴权（照抄 lib.rs clipboard_packet_authorized 的模式）：
/// cluster 等值 + pair_secret 恒时比较 + 已配对控制器白名单
/// （transport_public_key 优先、device_id 兜底、legacy local-device 放宽）。
pub(crate) fn agent_packet_authorized(
    layout: &crate::LayoutState,
    packet: &AgentChannelPacket,
) -> bool {
    if layout.cluster_id.trim().is_empty()
        || layout.pair_secret.trim().is_empty()
        || packet.cluster_id != layout.cluster_id
        || !crate::consttime_eq(packet.pair_secret.as_bytes(), layout.pair_secret.as_bytes())
    {
        return false;
    }

    if layout.machine_role == "client" && !layout.paired_controllers.is_empty() {
        // 与 input/clipboard 一致：按证书钉扎的稳定 transport key 优先匹配，
        // device_id 兜底（id 会随 LAN IP 漂移），最后放行 legacy "local-device"。
        let key = packet.origin_transport_public_key.trim();
        return layout.paired_controllers.iter().any(|controller| {
            (!key.is_empty() && controller.transport_public_key == key)
                || controller.id == packet.origin_id
        }) || (layout.paired_controllers.len() == 1
            && packet.origin_id == "local-device"
            && !key.is_empty());
    }

    true
}

/// 路由消歧（语义复用 input.rs packet_targets_local）：空 target = 广播；
/// 精确匹配本机 id；放行本机主机名铸成的历史 id（device id 内嵌 LAN 地址，
/// 换 IP 后 id 漂移不应静默丢弃——见 AGENTS.md 已知坑 6）；最后匹配
/// layout.devices 里 role="local" 的设备。此检查在鉴权之后，只是路由而非安全边界。
pub(crate) fn agent_packet_targets_local(
    layout: &crate::LayoutState,
    packet: &AgentChannelPacket,
    local_peer_id: &str,
) -> bool {
    crate::input::packet_targets_local(layout, &packet.target_id, local_peer_id)
}

/// 向对端发出 agent request 并等待对应 response（两段式，阻塞至超时）。
/// 流程：先注册 pending 再发包（防响应先到无处安放）→ 流发送（自带 ack 确认
/// 到达对端 handler）→ 等待 response 完成等待者。任何失败路径都清理 pending。
/// P0 仅供测试与后续 Tauri 命令接线使用。
pub(crate) fn send_agent_request(
    transport: &crate::quic_transport::TransportHandle,
    peer: &crate::quic_transport::PeerEndpoint,
    layout: &crate::LayoutState,
    local_peer_id: &str,
    local_transport_public_key: &str,
    target_device_id: &str,
    capability: &str,
    payload: Value,
    timeout: Duration,
) -> Result<AgentResponse, String> {
    if layout.cluster_id.trim().is_empty() || layout.pair_secret.trim().is_empty() {
        return Err("当前设备尚未完成配对，无法使用 Agent 通道。".into());
    }

    let request_id = new_request_id();
    let rx = global_pending_table().register(request_id.clone(), timeout);

    let packet = AgentChannelPacket {
        protocol: AGENT_PROTOCOL.into(),
        kind: AGENT_KIND_REQUEST.into(),
        request_id: request_id.clone(),
        origin_id: local_peer_id.into(),
        origin_transport_public_key: local_transport_public_key.into(),
        target_id: target_device_id.into(),
        cluster_id: layout.cluster_id.clone(),
        pair_secret: layout.pair_secret.clone(),
        capability: capability.into(),
        payload,
        status: None,
    };
    let encoded = crate::encode_wire_packet(&packet)?;

    // 流发送的 ack 只说明对端 handler 收下了包（≤6s）；结果本身等 response。
    if let Err(error) = transport.send_stream_expect_ack(peer.clone(), encoded) {
        global_pending_table().cancel(&request_id);
        return Err(format!("agent 请求发送失败: {error}"));
    }

    match rx.recv_timeout(timeout + Duration::from_secs(1)) {
        Ok(result) => result,
        Err(_) => {
            global_pending_table().cancel(&request_id);
            Err(format!("agent 请求超时（{timeout:?}）: 未收到响应"))
        }
    }
}

/// capability 分发函数：按 packet.capability 执行并返回 response payload。
/// 以注入方式持有（而非写死调用），测试可替换为带延迟/计数的处理器来锁定
/// 执行模型的时序性质，生产恒为 [`dispatch_agent_capability`]。
pub(crate) type AgentCapabilityHandler =
    Arc<dyn Fn(&AgentChannelPacket) -> Result<Value, String> + Send + Sync>;

/// on_stream agent 分支的运行期依赖（RFC §6.3）。生产由 start_quic_transport
/// 构造一次、被 on_stream 闭包捕获（[`AgentChannelRuntime::production`]）；
/// 测试按用例构造独立实例，避免进程级全局状态串线。
#[derive(Clone)]
pub(crate) struct AgentChannelRuntime {
    /// response 回发用的 transport 槽位：on_stream 闭包先于 transport 创建，
    /// 只能捕获槽位，transport 就绪后由 start_quic_transport 填充。
    pub transport_slot: Arc<Mutex<Option<crate::quic_transport::TransportHandle>>>,
    pub dispatch: AgentCapabilityHandler,
}

impl AgentChannelRuntime {
    /// 生产构造：capability 分发固定为 [`dispatch_agent_capability`]。
    pub(crate) fn production(
        transport_slot: Arc<Mutex<Option<crate::quic_transport::TransportHandle>>>,
    ) -> Self {
        Self {
            transport_slot,
            dispatch: Arc::new(dispatch_agent_capability),
        }
    }
}

/// on_stream 的 agent 分支入口（lib.rs 调度链在 pairing 之后、drag 之前调用）。
/// 返回 true = 该包属于 agent 协议且已处理；false = 交回调度链继续尝试
/// （decode 失败/协议不符/鉴权失败——与 clipboard 分支相同的静默拒绝策略）。
pub(crate) fn handle_agent_stream_packet(
    payload: &[u8],
    layout: &crate::LayoutState,
    local_peer_id: &str,
    runtime: &AgentChannelRuntime,
    source: SocketAddr,
) -> bool {
    let Some(packet) = crate::decode_wire_packet::<AgentChannelPacket>(payload) else {
        return false;
    };
    if packet.protocol != AGENT_PROTOCOL {
        return false;
    }

    match packet.kind.as_str() {
        AGENT_KIND_REQUEST => handle_agent_request(&packet, layout, local_peer_id, runtime, source),
        AGENT_KIND_RESPONSE => handle_agent_response(&packet, layout),
        AGENT_KIND_CANCEL => {
            if !agent_packet_authorized(layout, &packet) {
                return false;
            }
            if global_pending_table().cancel(&packet.request_id) {
                log::info!(
                    "agent: 请求 {} 已被对端 {} 取消",
                    packet.request_id,
                    packet.origin_id
                );
            }
            true
        }
        AGENT_KIND_NOTIFY => {
            if !agent_packet_authorized(layout, &packet) {
                return false;
            }
            log::info!(
                "agent: 收到来自 {} 的通知: {}",
                packet.origin_id,
                packet.payload
            );
            true
        }
        // 未知 kind：拒绝（reject ack），为未来版本保留向前兼容语义——
        // 旧版对任何它不认识的 kind 也是同样反应。
        _ => false,
    }
}

/// 处理 request：鉴权 → 路由 → 立即接受（返回 true，流 ack 即 accepted），
/// capability 分发 + response 回发在独立线程执行。
///
/// 执行模型（RFC §6.3，P0 验收结论）：on_stream 回调内同步执行会占住 QUIC
/// stream slot（并发上限 8）最长 6s，与文件传输/剪贴板互饿。本分支照 lib.rs
/// handle_peer_request_packet 的 Log/Clipboard 先例：先在回调内完成廉价的
/// 鉴权/路由检查，之后 `std::thread::spawn` 做真正的工作。
/// 鉴权/路由失败照抄 clipboard 分支策略：静默 return false（对端收到 QUIC 层
/// reject ack，不回 agent 层错误包，避免给未授权来源任何确认信息）。
fn handle_agent_request(
    packet: &AgentChannelPacket,
    layout: &crate::LayoutState,
    local_peer_id: &str,
    runtime: &AgentChannelRuntime,
    source: SocketAddr,
) -> bool {
    if !agent_packet_authorized(layout, packet) {
        return false;
    }
    if !agent_packet_targets_local(layout, packet, local_peer_id) {
        return false;
    }

    // spawn 前把回调依赖克隆进线程（Arc clone 是廉价引用计数）；
    // layout 快照也随线程带走，回调线程不再持有任何本分支借用。
    let runtime = runtime.clone();
    let mut packet = packet.clone();
    let layout = layout.clone();
    let local_peer_id = local_peer_id.to_string();
    std::thread::spawn(move || {
        let (status, payload) = match (runtime.dispatch)(&packet) {
            Ok(result) => ("ok", result),
            Err(error) => ("err", serde_json::json!({ "error": error })),
        };
        send_agent_response(
            &runtime,
            &mut packet,
            status,
            payload,
            &layout,
            &local_peer_id,
            source,
        );
    });
    true
}

/// 异步线程内回发 response（两段式反向新建流）。失败只记 warn：请求已被
/// 消费，由发起方超时兜底重试。
fn send_agent_response(
    runtime: &AgentChannelRuntime,
    packet: &mut AgentChannelPacket,
    status: &str,
    payload: Value,
    layout: &crate::LayoutState,
    local_peer_id: &str,
    source: SocketAddr,
) {
    let transport = runtime
        .transport_slot
        .lock()
        .ok()
        .and_then(|slot| slot.clone());
    let Some(transport) = transport else {
        // transport 槽位尚空（启动竞态窗口）：请求已消费但响应无法回发，
        // 由发起方超时兜底重试。
        log::warn!(
            "agent: 收到 {} 请求但 QUIC transport 尚未就绪，无法回发响应",
            packet.capability
        );
        return;
    };

    let response = AgentChannelPacket {
        protocol: AGENT_PROTOCOL.into(),
        kind: AGENT_KIND_RESPONSE.into(),
        request_id: packet.request_id.clone(),
        origin_id: local_peer_id.into(),
        origin_transport_public_key: transport.public_key().to_string(),
        target_id: packet.origin_id.clone(),
        cluster_id: layout.cluster_id.clone(),
        pair_secret: layout.pair_secret.clone(),
        capability: packet.capability.clone(),
        payload,
        status: Some(status.into()),
    };
    let encoded = match crate::encode_wire_packet(&response) {
        Ok(bytes) => bytes,
        Err(error) => {
            log::warn!("agent: 响应 {} 编码失败: {error}", packet.request_id);
            return;
        }
    };
    // 反向新建流（两段式）：目标地址就是 request 的来源；证书钉扎用请求方
    // 自报的 transport 公钥。响应包带齐鉴权三件套（RFC §8 对策四：双向都不裸信任）。
    let peer = transport.peer(
        source.to_string(),
        packet.origin_transport_public_key.clone(),
        crate::quic_transport::PROTOCOL_VERSION,
    );
    if let Err(error) = transport.send_stream_expect_ack(peer, encoded) {
        log::warn!("agent: 响应 {} 回发失败: {error}", packet.request_id);
    }
}

/// 处理 response：鉴权 → 按 request_id 完成 pending 等待者。
/// 查无此 id（超时回收/重放/重复响应）→ 拒绝（RFC §8 对策四）。
fn handle_agent_response(packet: &AgentChannelPacket, layout: &crate::LayoutState) -> bool {
    if !agent_packet_authorized(layout, packet) {
        return false;
    }
    let status = packet.status.clone().unwrap_or_else(|| "err".into());
    let result = if status == "ok" {
        Ok(AgentResponse {
            status,
            payload: packet.payload.clone(),
        })
    } else {
        Err(agent_error_message(&packet.payload, &status))
    };
    global_pending_table().resolve(&packet.request_id, result)
}

/// 生产 capability 分发（P0：仅 echo，原样回 payload）。
fn dispatch_agent_capability(packet: &AgentChannelPacket) -> Result<Value, String> {
    match packet.capability.as_str() {
        AGENT_CAPABILITY_ECHO => Ok(packet.payload.clone()),
        other => Err(format!("未支持的 agent capability: {other}")),
    }
}

/// 从 err/denied 响应的 payload 提取人读错误消息。
fn agent_error_message(payload: &Value, status: &str) -> String {
    payload
        .get("error")
        .and_then(|value| value.as_str())
        .map(Into::into)
        .unwrap_or_else(|| format!("对端返回 {status}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::{quic_transport, Device, LayoutState, PairedController};

    const TEST_CLUSTER: &str = "cluster-test";
    const TEST_SECRET: &str = "secret-test";

    /// 构造 status=ok 的响应结果（pending 表测试用）。
    fn ok_response(payload: Value) -> Result<AgentResponse, String> {
        Ok(AgentResponse {
            status: "ok".into(),
            payload,
        })
    }

    /// 精简 layout 构造：只填鉴权/路由涉及的语义字段，其余用安全默认值。
    fn test_agent_layout(machine_role: &str, controllers: Vec<PairedController>) -> LayoutState {
        LayoutState {
            devices: Vec::new(),
            active_device_id: "local-device".into(),
            selected_screen_id: String::new(),
            input_mode: "control".into(),
            machine_role: machine_role.into(),
            cluster_id: TEST_CLUSTER.into(),
            pair_secret: TEST_SECRET.into(),
            paired_controllers: controllers,
            clipboard_sync: false,
            clipboard_on_demand: false,
            clipboard_paste_hotkey: crate::default_clipboard_paste_hotkey(),
            file_transfer_enabled: true,
            lock_sync: false,
            input_protection: Default::default(),
            language: "cn".into(),
            theme_mode: "system".into(),
            performance_monitor: false,
            transport_port_mode: "auto".into(),
            transport_port: 49152,
            quic_port: 49153,
            modifier_remap: false,
            modifier_map: crate::default_modifier_map(),
            edge_switch_hotkey: crate::default_edge_switch_hotkey(),
            screen_switch_hotkeys: Default::default(),
            focus_halo: crate::halo::FocusHaloSettings::default(),
            agent_channel: AgentChannelConfig::default(),
        }
    }

    fn test_controller(id: &str, transport_public_key: &str) -> PairedController {
        PairedController {
            id: id.into(),
            name: "Peer".into(),
            host: "peer-host".into(),
            ip: "10.0.0.9".into(),
            transport_public_key: transport_public_key.into(),
            protocol_version: quic_transport::PROTOCOL_VERSION,
            cluster_id: TEST_CLUSTER.into(),
            paired_at_ms: crate::now_ms(),
        }
    }

    fn test_local_device(id: &str) -> Device {
        Device {
            pointer_speed: 1.0,
            scroll_speed: 1.0,
            id: id.into(),
            name: "Local".into(),
            platform: "windows".into(),
            host: "local-host".into(),
            transport_port: 47833,
            quic_port: 47834,
            transport_public_key: "local-public-key".into(),
            protocol_version: quic_transport::PROTOCOL_VERSION,
            color: "#2f7af8".into(),
            online: true,
            input_ready: false,
            upgrading: false,
            upgrading_until_ms: 0,
            role: "local".into(),
            source: "detected".into(),
            screens: Vec::new(),
        }
    }

    /// 构造一个凭据齐全的请求包，供各用例按需改坏某个字段。
    fn authorized_packet(origin_id: &str, origin_key: &str) -> AgentChannelPacket {
        AgentChannelPacket {
            protocol: AGENT_PROTOCOL.into(),
            kind: AGENT_KIND_REQUEST.into(),
            request_id: "1-abcdef".into(),
            origin_id: origin_id.into(),
            origin_transport_public_key: origin_key.into(),
            target_id: String::new(),
            cluster_id: TEST_CLUSTER.into(),
            pair_secret: TEST_SECRET.into(),
            capability: AGENT_CAPABILITY_ECHO.into(),
            payload: serde_json::json!({ "hello": "world" }),
            status: None,
        }
    }

    // ---- a. 鉴权肯定/否定路径 ----

    #[test]
    fn authorized_request_from_paired_controller_passes_on_client() {
        let layout = test_agent_layout(
            "client",
            vec![test_controller("controller-1", "peer-key-1")],
        );
        let packet = authorized_packet("controller-1", "peer-key-1");
        assert!(
            agent_packet_authorized(&layout, &packet),
            "已配对控制器（id 与 key 双匹配）的正确凭据必须通过"
        );
    }

    #[test]
    fn authorized_by_transport_key_even_when_id_drifted() {
        // device id 随 LAN IP 漂移后，凭证书钉扎的 transport key 仍须通过
        // （AGENTS.md 已知坑 6 的同源语义）。
        let layout = test_agent_layout(
            "client",
            vec![test_controller("controller-old-id", "peer-key-1")],
        );
        let packet = authorized_packet("controller-new-id", "peer-key-1");
        assert!(
            agent_packet_authorized(&layout, &packet),
            "transport_public_key 匹配时即使 origin_id 漂移也必须通过"
        );
    }

    #[test]
    fn rejected_when_cluster_id_differs() {
        let layout = test_agent_layout(
            "client",
            vec![test_controller("controller-1", "peer-key-1")],
        );
        let mut packet = authorized_packet("controller-1", "peer-key-1");
        packet.cluster_id = "cluster-other".into();
        assert!(
            !agent_packet_authorized(&layout, &packet),
            "错 cluster_id 必须拒绝"
        );
    }

    #[test]
    fn rejected_when_pair_secret_differs_by_one_char() {
        let layout = test_agent_layout(
            "client",
            vec![test_controller("controller-1", "peer-key-1")],
        );
        let mut packet = authorized_packet("controller-1", "peer-key-1");
        let mut secret = TEST_SECRET.to_string();
        secret.replace_range(0..1, if secret.starts_with('s') { "S" } else { "s" });
        packet.pair_secret = secret;
        assert!(
            !agent_packet_authorized(&layout, &packet),
            "pair_secret 仅一位不同必须拒绝"
        );
    }

    #[test]
    fn rejected_when_origin_not_in_paired_controllers() {
        let layout = test_agent_layout(
            "client",
            vec![test_controller("controller-1", "peer-key-1")],
        );
        let packet = authorized_packet("attacker", "attacker-key");
        assert!(
            !agent_packet_authorized(&layout, &packet),
            "白名单外的 origin 必须拒绝"
        );
    }

    #[test]
    fn rejected_when_local_credentials_empty() {
        let mut layout = test_agent_layout("client", Vec::new());
        layout.pair_secret = "  ".into();
        let packet = authorized_packet("controller-1", "peer-key-1");
        assert!(
            !agent_packet_authorized(&layout, &packet),
            "本机 pair_secret 为空（未完成配对）时必须拒绝"
        );
    }

    // ---- b. 协议编解码 + 向前兼容 ----

    #[test]
    fn agent_packet_roundtrips_through_msgpack() {
        let packet = authorized_packet("controller-1", "peer-key-1");
        let bytes = crate::encode_wire_packet(&packet).expect("编码成功");
        let decoded: AgentChannelPacket = crate::decode_wire_packet(&bytes).expect("解码成功");
        assert_eq!(decoded.protocol, AGENT_PROTOCOL);
        assert_eq!(decoded.kind, AGENT_KIND_REQUEST);
        assert_eq!(decoded.request_id, "1-abcdef");
        assert_eq!(decoded.origin_id, "controller-1");
        assert_eq!(decoded.origin_transport_public_key, "peer-key-1");
        assert_eq!(decoded.cluster_id, TEST_CLUSTER);
        assert_eq!(decoded.pair_secret, TEST_SECRET);
        assert_eq!(decoded.capability, AGENT_CAPABILITY_ECHO);
        assert_eq!(decoded.payload, serde_json::json!({ "hello": "world" }));
        assert_eq!(decoded.status, None);
    }

    #[test]
    fn decoding_tolerates_unknown_future_field() {
        // 跨版本兼容：未来版本给 AgentChannelPacket 新增字段后，旧版（本结构）
        // 仍必须能解码。构造方式照 lib.rs tests 的 append_msgpack_named_field：
        // 在 named msgpack 字节层追加未知键并把顶层 map 计数 +1。
        let packet = authorized_packet("controller-1", "peer-key-1");
        let bytes = crate::encode_wire_packet(&packet).expect("编码成功");
        let future_value = rmp_serde::to_vec_named(&"future-field-value").expect("编码未来字段值");
        let forged = append_msgpack_named_field(&bytes, "futureField", &future_value)
            .expect("AgentChannelPacket 编码为 named map，可追加字段");

        let decoded: AgentChannelPacket =
            crate::decode_wire_packet(&forged).expect("旧版解码含未知字段的包必须成功");
        assert_eq!(decoded.request_id, "1-abcdef");
        assert_eq!(decoded.payload, serde_json::json!({ "hello": "world" }));
    }

    /// 在 named-map msgpack 载荷的字节层追加一个字符串键值对，并把 map
    /// 计数加一（与 lib.rs tests 中的同名辅助语义一致；该辅助是
    /// #[cfg(test)] 私有的，此处按测试局部复制的惯例保留一份）。
    fn append_msgpack_named_field(
        payload: &[u8],
        key: &str,
        value_bytes: &[u8],
    ) -> Option<Vec<u8>> {
        let mut forged = payload.to_vec();
        let header = *forged.first()?;
        match header {
            0x80..=0x8f => {
                let count = header & 0x0f;
                if count == 0x0f {
                    return None;
                }
                forged[0] = 0x80 | (count + 1);
            }
            0xde => {
                let count = u16::from_be_bytes([forged[1], forged[2]]);
                forged[1..3].copy_from_slice(&count.checked_add(1)?.to_be_bytes());
            }
            0xdf => {
                let count = u32::from_be_bytes([forged[1], forged[2], forged[3], forged[4]]);
                forged[1..5].copy_from_slice(&count.checked_add(1)?.to_be_bytes());
            }
            _ => return None,
        }
        forged.extend_from_slice(&rmp_serde::to_vec_named(key).ok()?);
        forged.extend_from_slice(value_bytes);
        Some(forged)
    }

    #[test]
    fn request_targets_local_via_local_device_entry() {
        let mut layout = test_agent_layout("client", Vec::new());
        layout.devices = vec![test_local_device("local-new-id")];
        let mut packet = authorized_packet("controller-1", "peer-key-1");

        // devices 里 role="local" 的条目命中 → 落本机
        packet.target_id = "local-new-id".into();
        assert!(
            agent_packet_targets_local(&layout, &packet, "local-current-id"),
            "devices 本机条目必须命中路由"
        );

        // 不属于本机历史 id、不在 devices → 不落本机（多客户端路由消歧）
        packet.target_id = "peer-zzz-not-this-host-999".into();
        assert!(
            !agent_packet_targets_local(&layout, &packet, "local-current-id"),
            "无关目标 id 不得被本机接收"
        );
    }

    #[test]
    fn agent_routing_matches_input_routing_on_every_branch() {
        // 路由统一（P1a 技术债消除）：agent_packet_targets_local 委托
        // input::packet_targets_local，四个路由分支的判定必须逐分支一致——
        // 广播 / 精确 id / 本机主机名铸成的历史 id（AGENTS.md 已知坑 6）/
        // devices 本机条目 / 无关 id。
        let host = crate::hostname().unwrap_or_else(|| "test-host".into());
        let mut layout = test_agent_layout("client", Vec::new());
        layout.devices = vec![test_local_device("local-device")];
        let targets = [
            String::new(),
            crate::local_peer_id(&host, "192.168.3.65"),
            crate::local_peer_id(&host, "192.168.3.42"),
            "local-device".into(),
            crate::local_peer_id("some-other-host", "192.168.3.42"),
        ];
        for target in &targets {
            let mut packet = authorized_packet("controller-1", "peer-key-1");
            packet.target_id = target.clone();
            let agent = agent_packet_targets_local(&layout, &packet, "peer-local-current");
            let input = crate::input::packet_targets_local(&layout, target, "peer-local-current");
            assert_eq!(
                agent, input,
                "target={target:?} 的路由判定必须与 input.rs 完全一致"
            );
        }
    }

    // ---- c. Pending 表 ----

    #[test]
    fn response_resolves_waiting_request() {
        let table = AgentPendingTable::new();
        let rx = table.register("req-1".into(), Duration::from_secs(1));
        assert!(
            table.resolve("req-1", ok_response(serde_json::json!({ "echo": true })),),
            "已注册的 request_id 必须能被响应完成"
        );
        let result = rx
            .recv_timeout(Duration::from_secs(1))
            .expect("等待者必须被唤醒")
            .expect("结果本身必须是 Ok");
        assert_eq!(result.status, "ok");
        assert_eq!(result.payload, serde_json::json!({ "echo": true }));
    }

    #[test]
    fn resolve_unknown_request_id_is_rejected() {
        let table = AgentPendingTable::new();
        assert!(
            !table.resolve("ghost", ok_response(Value::Null)),
            "查无此 id（超时回收/重放）的响应必须被拒绝"
        );
    }

    #[test]
    fn expired_entry_is_reaped_and_late_response_rejected() {
        let table = AgentPendingTable::new();
        let _rx = table.register("req-2".into(), Duration::from_millis(30));
        std::thread::sleep(Duration::from_millis(80));
        assert!(
            !table.resolve("req-2", ok_response(Value::Null)),
            "过期条目回收后，迟到响应必须被拒绝"
        );
    }

    #[test]
    fn duplicate_request_id_does_not_cross_wires() {
        let table = AgentPendingTable::new();
        let first = table.register("req-3".into(), Duration::from_secs(2));
        let second = table.register("req-3".into(), Duration::from_secs(2));
        assert!(
            table.resolve("req-3", ok_response(serde_json::json!("second"))),
            "重复注册后响应完成的是最新等待者"
        );
        let result = second
            .recv_timeout(Duration::from_secs(1))
            .unwrap()
            .unwrap();
        assert_eq!(
            result.payload,
            serde_json::json!("second"),
            "第二个等待者收到结果"
        );
        assert!(
            first.recv_timeout(Duration::from_millis(50)).is_err(),
            "第一个等待者被覆盖，不得收到第二个请求的结果"
        );
    }

    #[test]
    fn cancel_removes_pending_request() {
        let table = AgentPendingTable::new();
        let rx = table.register("req-4".into(), Duration::from_secs(2));
        assert!(table.cancel("req-4"), "取消已注册的请求必须成功");
        assert!(!table.cancel("req-4"), "重复取消必须返回 false");
        assert!(rx.recv_timeout(Duration::from_millis(50)).is_err());
    }

    #[test]
    fn request_id_contains_counter_and_random_nonce() {
        let a = new_request_id();
        let b = new_request_id();
        assert_ne!(
            a, b,
            "request_id 必须唯一（单调计数保证顺序、nonce 防碰撞）"
        );
        assert!(a.contains('-'), "格式应为 计数-nonce");
    }

    // ---- d/e. QUIC 真实闭环（复用 quic_transport.rs tests 的双端 loopback 模式：
    // 127.0.0.1 随机端口 + 独立临时身份目录 + 互相持证书钉扎）----

    fn free_quic_port() -> u16 {
        std::net::UdpSocket::bind(("127.0.0.1", 0))
            .expect("预留随机端口")
            .local_addr()
            .expect("读取随机端口")
            .port()
    }

    fn temp_identity_dir(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "mykvm-agent-{tag}-{}-{}",
            std::process::id(),
            free_quic_port()
        ))
    }

    /// 测试期把 log crate 的 warn/info 打到 stderr：QUIC/agent 通道的失败
    /// 路径只写 log::warn，不打开 logger 就无法定位闭环断点。
    fn init_test_logger() {
        static INIT: OnceLock<()> = OnceLock::new();
        INIT.get_or_init(|| {
            struct StderrLogger;
            impl log::Log for StderrLogger {
                fn enabled(&self, metadata: &log::Metadata) -> bool {
                    metadata.level() <= log::Level::Info
                }
                fn log(&self, record: &log::Record) {
                    eprintln!("[{}] {}", record.level(), record.args());
                }
                fn flush(&self) {}
            }
            let _ = log::set_boxed_logger(Box::new(StderrLogger));
            log::set_max_level(log::LevelFilter::Info);
        });
    }

    #[test]
    fn agent_echo_roundtrip_over_real_quic_loopback() {
        init_test_logger();
        let dir = temp_identity_dir("echo");
        let _ = std::fs::remove_dir_all(&dir);

        // 发起端 A：先起，拿到稳定 transport 公钥供 B 钉扎/白名单。
        // A 的 on_stream 同样走生产接线（response 分支按 request_id 完成
        // pending 等待者）——发起方也是完整接线的端点，不是哑管道。
        let layout_a = test_agent_layout("server", Vec::new());
        let slot_a: Arc<Mutex<Option<quic_transport::TransportHandle>>> =
            Arc::new(Mutex::new(None));
        let runtime_a = AgentChannelRuntime::production(Arc::clone(&slot_a));
        let layout_for_a_stream = layout_a.clone();
        let sender = quic_transport::start(
            free_quic_port(),
            dir.join("sender"),
            Arc::new(|_, _| {}),
            Arc::new(move |payload: Vec<u8>, source: SocketAddr| {
                handle_agent_stream_packet(
                    &payload,
                    &layout_for_a_stream,
                    "peer-a",
                    &runtime_a,
                    source,
                )
            }),
        )
        .expect("发起端 transport 启动");
        *slot_a.lock().expect("发起端槽位锁") = Some(sender.clone());

        // 受控端 B：client 角色 + 白名单只认 A（走最严格的鉴权路径）。
        let layout_b = test_agent_layout(
            "client",
            vec![test_controller("peer-a", sender.public_key())],
        );
        let slot_b: Arc<Mutex<Option<quic_transport::TransportHandle>>> =
            Arc::new(Mutex::new(None));
        let runtime_b = AgentChannelRuntime::production(Arc::clone(&slot_b));
        let receiver = quic_transport::start(
            free_quic_port(),
            dir.join("receiver"),
            Arc::new(|_, _| {}),
            // 生产等价分支：on_stream 的 agent 处理入口（layout 快照 + 本机 id +
            // 运行期依赖 + 来源地址，与 lib.rs 接线形态一致）。
            Arc::new(move |payload: Vec<u8>, source: SocketAddr| {
                handle_agent_stream_packet(&payload, &layout_b, "peer-b", &runtime_b, source)
            }),
        )
        .expect("受控端 transport 启动");
        // 生产 start_quic_transport 同款时序：transport 就绪后把 handle 填进槽位。
        *slot_b.lock().expect("槽位锁") = Some(receiver.clone());

        let peer = sender.peer(
            format!("127.0.0.1:{}", receiver.port()),
            receiver.public_key().into(),
            quic_transport::PROTOCOL_VERSION,
        );
        let result = send_agent_request(
            &sender,
            &peer,
            &layout_a,
            "peer-a",
            sender.public_key(),
            "peer-b",
            AGENT_CAPABILITY_ECHO,
            serde_json::json!({ "ping": 42 }),
            Duration::from_secs(15),
        );

        let response = result.expect("echo 往返必须成功");
        assert_eq!(response.status, "ok", "echo 的 response 必须 status=ok");
        assert_eq!(
            response.payload,
            serde_json::json!({ "ping": 42 }),
            "echo 必须原样返回 payload"
        );

        sender.stop_and_wait().expect("发起端停止");
        receiver.stop_and_wait().expect("受控端停止");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_peer_rejects_agent_packet_gracefully() {
        // 旧版等价物：调度链各分支对 mykvm.agent.v1 包全部 decode 失败 →
        // on_stream 返回 false → QUIC 层回 reject ack。锁定三条性质：
        // 发送方收到 STREAM_REJECTED 错误（不挂起）、对端不 panic、
        // accept 循环不被阻塞（后续流仍被正常处理）。
        let dir = temp_identity_dir("legacy");
        let _ = std::fs::remove_dir_all(&dir);

        let receiver = quic_transport::start(
            free_quic_port(),
            dir.join("receiver"),
            Arc::new(|_, _| {}),
            Arc::new(|_, _| false),
        )
        .expect("旧版等价端 transport 启动");
        let sender = quic_transport::start(
            free_quic_port(),
            dir.join("sender"),
            Arc::new(|_, _| {}),
            Arc::new(|_, _| true),
        )
        .expect("发送端 transport 启动");

        let peer = sender.peer(
            format!("127.0.0.1:{}", receiver.port()),
            receiver.public_key().into(),
            quic_transport::PROTOCOL_VERSION,
        );
        let packet = authorized_packet("peer-a", sender.public_key());
        let bytes = crate::encode_wire_packet(&packet).expect("agent 包编码");

        let first = sender.send_stream_expect_ack(peer.clone(), bytes.clone());
        assert!(
            first
                .unwrap_err()
                .starts_with(quic_transport::STREAM_REJECTED),
            "旧版对端必须以 reject ack 拒绝 agent 包，发送方不得挂起"
        );

        // accept 循环未被阻塞：第二条流仍被正常处理（同样 reject，而非挂起/断连）。
        let second = sender.send_stream_expect_ack(peer.clone(), bytes);
        assert!(
            second
                .unwrap_err()
                .starts_with(quic_transport::STREAM_REJECTED),
            "拒绝后续的流必须仍被 accept 并处理，对端不得卡死"
        );

        sender.stop_and_wait().expect("发送端停止");
        receiver.stop_and_wait().expect("旧版等价端停止");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- f. 异步执行模型（P1a，RFC §6.3 执行模型）----

    /// 构造测试 runtime：capability 分发为注入的闭包。
    fn runtime_with_dispatch(
        slot: Arc<Mutex<Option<quic_transport::TransportHandle>>>,
        dispatch: impl Fn(&AgentChannelPacket) -> Result<Value, String> + Send + Sync + 'static,
    ) -> AgentChannelRuntime {
        AgentChannelRuntime {
            transport_slot: slot,
            dispatch: Arc::new(move |packet| dispatch(packet)),
        }
    }

    #[test]
    fn slow_capability_does_not_block_on_stream_ack() {
        // RFC §6.3 执行模型：request 鉴权/路由通过后 on_stream 必须立即返回
        // true（流 ack 即 accepted），capability 处理移出 stream slot——否则
        // 慢处理器会占着 MAX_CONCURRENT_STREAMS=8 里的一个 slot 最长 6s，
        // 与文件传输/剪贴板互饿。用 800ms 的伪 capability 验证 ack 毫秒级返回。
        init_test_logger();
        let slot: Arc<Mutex<Option<quic_transport::TransportHandle>>> = Arc::new(Mutex::new(None));
        let runtime = runtime_with_dispatch(slot, |_packet| {
            std::thread::sleep(Duration::from_millis(800));
            Ok(serde_json::json!({ "slow": true }))
        });
        let layout = test_agent_layout("server", Vec::new());
        let packet = authorized_packet("controller-1", "peer-key-1");

        let started = Instant::now();
        let accepted = handle_agent_stream_packet(
            &crate::encode_wire_packet(&packet).expect("编码"),
            &layout,
            "peer-local",
            &runtime,
            "127.0.0.1:65000".parse().expect("来源地址"),
        );
        let elapsed = started.elapsed();

        assert!(accepted, "已鉴权的 request 必须被 on_stream 接受");
        assert!(
            elapsed < Duration::from_millis(200),
            "on_stream 必须在慢 capability 执行完成前立即返回（实际耗时 {elapsed:?}）"
        );
    }

    #[test]
    fn async_request_eventually_sends_response_and_frees_thread() {
        // 异步模型的闭环性质：慢 capability 最终在独立线程执行完成并回发
        // response；随后 on_stream 恢复即时返回（线程不吞后续包的 ack）。
        init_test_logger();
        let slot: Arc<Mutex<Option<quic_transport::TransportHandle>>> = Arc::new(Mutex::new(None));
        let started_flag = Arc::new(AtomicU64::new(0));
        let flag_for_dispatch = Arc::clone(&started_flag);
        let runtime = runtime_with_dispatch(slot, move |_packet| {
            flag_for_dispatch.fetch_add(1, Ordering::Relaxed);
            std::thread::sleep(Duration::from_millis(150));
            Ok(Value::Null)
        });
        let layout = test_agent_layout("server", Vec::new());
        let packet = authorized_packet("controller-1", "peer-key-1");

        let started_at = Instant::now();
        assert!(
            handle_agent_stream_packet(
                &crate::encode_wire_packet(&packet).expect("编码"),
                &layout,
                "peer-local",
                &runtime,
                "127.0.0.1:65000".parse().expect("来源地址"),
            ),
            "request 必须被接受"
        );
        // 等待独立线程真正进入 capability 处理器。
        let wait_started = Instant::now();
        while started_flag.load(Ordering::Relaxed) == 0 {
            assert!(
                wait_started.elapsed() < Duration::from_secs(5),
                "异步线程必须在 5s 内开始执行 capability"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        // 处理器还在 sleep(150ms) 期间，后续包必须已被即时处理（不排队等待）。
        let second_started = Instant::now();
        assert!(
            handle_agent_stream_packet(
                &crate::encode_wire_packet(&packet).expect("编码"),
                &layout,
                "peer-local",
                &runtime,
                "127.0.0.1:65000".parse().expect("来源地址"),
            ),
            "后续包必须被继续接受"
        );
        assert!(
            second_started.elapsed() < Duration::from_millis(50),
            "第一个请求在后台执行时，on_stream 必须保持即时返回（实际 {:?}）",
            second_started.elapsed()
        );
        assert!(
            started_at.elapsed() < Duration::from_secs(5),
            "整个用例不得被 capability 的执行时长拖满"
        );
    }

    // ---- g. AgentChannelConfig 配置与 level gate（P1a，RFC §5.3/§6.3）----

    #[test]
    fn sanitize_clamps_invalid_level_and_cleans_lists() {
        let config = AgentChannelConfig {
            enabled: true,
            level: "  root  ".into(),
            file_roots: vec![
                "  ".into(),
                " /tmp/downloads ".into(),
                "/tmp/downloads".into(),
                "/tmp/logs".into(),
            ],
            cmd_whitelist: vec!["cargo test".into(), "  ".into(), " cargo test ".into()],
            approval_ttl_secs: 300,
        };
        let sanitized = config.sanitized();
        assert_eq!(
            sanitized.level, "readonly",
            "白名单外的 level（含大小写/空白变体）必须钳制回 readonly（fail closed）"
        );
        assert_eq!(
            sanitized.file_roots,
            vec!["/tmp/downloads".to_string(), "/tmp/logs".to_string()],
            "file_roots 必须去空白项、trim、去重并排序"
        );
        assert_eq!(
            sanitized.cmd_whitelist,
            vec!["cargo test".to_string()],
            "cmd_whitelist 必须去空白项、trim、去重并排序"
        );
        assert!(sanitized.enabled, "sanitize 不得改动 enabled（布尔无越界）");
        assert_eq!(
            sanitized.approval_ttl_secs, 300,
            "sanitize 不得改动 approval_ttl_secs（语义由 P3 消费时定）"
        );
    }

    #[test]
    fn sanitize_keeps_valid_levels_untouched() {
        for level in ["readonly", "files", "commands", "full"] {
            let config = AgentChannelConfig {
                level: level.into(),
                ..AgentChannelConfig::default()
            };
            assert_eq!(
                config.sanitized().level,
                level,
                "合法 level {level} 必须原样保留"
            );
        }
    }

    #[test]
    fn capability_allowed_follows_level_ladder() {
        // L0 及以上：echo/status.get/msg.send/log.read/file.stat/file.read。
        let l0_caps = [
            "echo",
            "status.get",
            "msg.send",
            "log.read",
            "file.stat",
            "file.read",
        ];
        let readonly = AgentChannelConfig::default();
        for capability in l0_caps {
            assert!(
                readonly.capability_allowed(capability),
                "readonly(L0) 必须放行 {capability}"
            );
        }
        assert!(
            !readonly.capability_allowed("file.write"),
            "readonly 不得放行 file.write（L1+）"
        );
        assert!(
            !readonly.capability_allowed("cmd.run"),
            "readonly 不得放行 cmd.run（L2+）"
        );

        let files = AgentChannelConfig {
            level: "files".into(),
            ..AgentChannelConfig::default()
        };
        assert!(
            files.capability_allowed("file.write"),
            "files(L1) 放行 file.write"
        );
        assert!(
            !files.capability_allowed("cmd.run"),
            "files(L1) 仍不得放行 cmd.run"
        );

        let commands = AgentChannelConfig {
            level: "commands".into(),
            ..AgentChannelConfig::default()
        };
        assert!(
            commands.capability_allowed("cmd.run"),
            "commands(L2) 放行 cmd.run"
        );

        let full = AgentChannelConfig {
            level: "full".into(),
            ..AgentChannelConfig::default()
        };
        assert!(full.capability_allowed("cmd.run"), "full(L3) 放行 cmd.run");

        assert!(
            !readonly.capability_allowed("human.ask"),
            "未在本阶段映射表中的 capability 一律拒绝"
        );
        assert!(!readonly.capability_allowed(""), "空 capability 一律拒绝");
    }

    #[test]
    fn default_config_is_enabled_readonly_and_survives_missing_fields() {
        let default = AgentChannelConfig::default();
        assert!(
            default.enabled,
            "默认必须启用通道（RFC §8 开放问题②的建议值）"
        );
        assert_eq!(default.level, "readonly", "默认级别必须是 L0");
        assert!(
            default.file_roots.is_empty(),
            "空 file_roots = 运行期默认目录"
        );
        assert!(default.cmd_whitelist.is_empty());
        assert_eq!(default.approval_ttl_secs, 300);

        // layout.json 缺失整段/缺失单字段时的 serde 默认（向前兼容旧快照）。
        let from_empty: AgentChannelConfig =
            serde_json::from_value(serde_json::json!({})).expect("空对象反序列化");
        assert_eq!(from_empty, default, "全字段缺失必须落回默认值");
        let partial: AgentChannelConfig = serde_json::from_value(serde_json::json!({
            "level": "files"
        }))
        .expect("部分字段反序列化");
        assert_eq!(partial.level, "files");
        assert!(partial.enabled, "缺失的 enabled 必须落默认 true");
    }
}
