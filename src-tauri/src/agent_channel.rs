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
        mpsc, Mutex, OnceLock,
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

/// 本机历代 device id 的稳定前缀 `peer-{hostname}-`（语义与 input.rs
/// local_peer_host_prefix 一致；该函数是 input 私有的，这里用 crate 根的
/// hostname/sanitize 重新推导，避免改动 input.rs）。
fn local_peer_host_prefix() -> Option<String> {
    let host = crate::hostname()?;
    let normalized = crate::sanitize_peer_id_part(&host);
    (!normalized.is_empty()).then(|| format!("peer-{normalized}-"))
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
    if packet.target_id.trim().is_empty() {
        return true;
    }
    if packet.target_id == local_peer_id {
        return true;
    }
    if let Some(prefix) = local_peer_host_prefix() {
        if packet.target_id.starts_with(&prefix) {
            return true;
        }
    }
    layout
        .devices
        .iter()
        .any(|device| device.role == "local" && device.id == packet.target_id)
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

#[cfg(test)]
mod tests {
    use super::*;
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
}
