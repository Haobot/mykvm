//! 跨机 Agent 互联通道（RFC：Doc/Rfc/跨机Agent互联与受控协作通道.md §6.1/§6.3）。
//!
//! P0 范围：协议包结构 + 鉴权 + pending 请求表 + 两段式 echo 闭环。
//! 两段式：request 与 response 是两条独立的 QUIC 流消息，靠 request_id 关联；
//! 应答由接收方反向新建流发回（ConnectionMap 对称，反向拨号命中既有信任面），
//! 不改动 quic_transport.rs 的 StreamHandler 签名。

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{mpsc, Mutex},
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

/// 一条已注册请求的等待句柄：响应到达（或取消/超时）时经此通道完成等待者。
pub(crate) type PendingReplyRx = mpsc::Receiver<Result<Value, String>>;

/// request_id ↔ 等待者 的挂起表。生产路径用 [`global_pending_table`]，
/// 单元测试构造独立实例（避免进程级全局状态串线）。
pub(crate) struct AgentPendingTable {
    entries: Mutex<HashMap<String, PendingEntry>>,
}

struct PendingEntry {
    created_at: Instant,
    timeout: Duration,
    reply: mpsc::SyncSender<Result<Value, String>>,
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
        todo!("红灯阶段：pending 注册未实现")
    }

    /// 按 request_id 完成等待者并移除条目。返回 false = 表中查无此 id
    /// （超时已回收/重复响应/重放），调用方应拒绝该包。
    pub(crate) fn resolve(&self, request_id: &str, result: Result<Value, String>) -> bool {
        todo!("红灯阶段：pending 完成未实现")
    }

    /// 主动移除挂起请求（kind=cancel / 发送失败清理）。
    pub(crate) fn cancel(&self, request_id: &str) -> bool {
        todo!("红灯阶段：pending 取消未实现")
    }
}

/// 生产路径的全局挂起表。进程级单例；测试不要用它（构造独立实例）。
pub(crate) fn global_pending_table() -> &'static AgentPendingTable {
    todo!("红灯阶段：全局表未实现")
}

/// 生成 request_id：进程内单调计数 + 随机 nonce（防重放/防碰撞）。
pub(crate) fn new_request_id() -> String {
    todo!("红灯阶段：request_id 生成未实现")
}

/// 鉴权（照抄 lib.rs clipboard_packet_authorized 的模式）：
/// cluster 等值 + pair_secret 恒时比较 + 已配对控制器白名单
/// （transport_public_key 优先、device_id 兜底、legacy local-device 放宽）。
pub(crate) fn agent_packet_authorized(
    layout: &crate::LayoutState,
    packet: &AgentChannelPacket,
) -> bool {
    todo!("红灯阶段：鉴权未实现")
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
    todo!("红灯阶段：路由未实现")
}

/// 向对端发出 agent request 并等待对应 response（两段式，阻塞至超时）。
/// P0 仅供测试与后续 Tauri 命令接线使用。
pub(crate) fn send_agent_request(
    transport: &crate::quic_transport::TransportHandle,
    peer: &crate::quic_transport::PeerEndpoint,
    layout: &crate::LayoutState,
    local_peer_id: &str,
    local_transport_public_key: &str,
    capability: &str,
    payload: Value,
    timeout: Duration,
) -> Result<AgentResponse, String> {
    let _ = (
        transport,
        peer,
        layout,
        local_peer_id,
        local_transport_public_key,
        capability,
        payload,
        timeout,
    );
    todo!("红灯阶段：两段式请求未实现")
}

/// send_agent_request 的成功返回：response 的 status 与 payload。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AgentResponse {
    pub status: String,
    pub payload: Value,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{quic_transport, Device, LayoutState, PairedController};

    const TEST_CLUSTER: &str = "cluster-test";
    const TEST_SECRET: &str = "secret-test";

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
            table.resolve("req-1", Ok(serde_json::json!({ "echo": true }))),
            "已注册的 request_id 必须能被响应完成"
        );
        let result = rx
            .recv_timeout(Duration::from_secs(1))
            .expect("等待者必须被唤醒");
        assert_eq!(result.unwrap(), serde_json::json!({ "echo": true }));
    }

    #[test]
    fn resolve_unknown_request_id_is_rejected() {
        let table = AgentPendingTable::new();
        assert!(
            !table.resolve("ghost", Ok(serde_json::json!(null))),
            "查无此 id（超时回收/重放）的响应必须被拒绝"
        );
    }

    #[test]
    fn expired_entry_is_reaped_and_late_response_rejected() {
        let table = AgentPendingTable::new();
        let _rx = table.register("req-2".into(), Duration::from_millis(30));
        std::thread::sleep(Duration::from_millis(80));
        assert!(
            !table.resolve("req-2", Ok(serde_json::json!(null))),
            "过期条目回收后，迟到响应必须被拒绝"
        );
    }

    #[test]
    fn duplicate_request_id_does_not_cross_wires() {
        let table = AgentPendingTable::new();
        let first = table.register("req-3".into(), Duration::from_secs(2));
        let second = table.register("req-3".into(), Duration::from_secs(2));
        assert!(
            table.resolve("req-3", Ok(serde_json::json!("second"))),
            "重复注册后响应完成的是最新等待者"
        );
        assert_eq!(
            second
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .unwrap(),
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
