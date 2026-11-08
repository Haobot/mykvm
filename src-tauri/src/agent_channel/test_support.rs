//! agent_channel 各子模块单元测试共享的构造辅助（仅测试构建编译）。

use serde_json::Value;

use super::capabilities::AGENT_CAPABILITY_ECHO;
use super::config::AgentChannelConfig;
use super::protocol::{AgentChannelPacket, AGENT_KIND_REQUEST, AGENT_PROTOCOL};
use crate::{quic_transport, LayoutState, PairedController};

pub(super) const TEST_CLUSTER: &str = "cluster-test";
pub(super) const TEST_SECRET: &str = "secret-test";

/// 精简 layout 构造：只填鉴权/路由涉及的语义字段，其余用安全默认值。
pub(super) fn test_agent_layout(
    machine_role: &str,
    controllers: Vec<PairedController>,
) -> LayoutState {
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

pub(super) fn test_controller(id: &str, transport_public_key: &str) -> PairedController {
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

/// 构造一个凭据齐全的请求包，供各用例按需改坏某个字段。
pub(super) fn authorized_packet(origin_id: &str, origin_key: &str) -> AgentChannelPacket {
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

/// 构造一个 capability 请求包（鉴权字段齐全）。
pub(super) fn capability_packet(capability: &str, payload: Value) -> AgentChannelPacket {
    let mut packet = authorized_packet("controller-1", "peer-key-1");
    packet.capability = capability.into();
    packet.payload = payload;
    packet
}
