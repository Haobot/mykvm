//! Agent 通道鉴权与路由消歧（语义照抄 clipboard/input 分支）。

use super::protocol::AgentChannelPacket;

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

#[cfg(test)]
mod tests {
    use super::*;

    use crate::agent_channel::test_support::{
        authorized_packet, test_agent_layout, test_controller, TEST_SECRET,
    };
    use crate::{quic_transport, Device};

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
}
