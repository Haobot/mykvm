//! P1c 集成面：Tauri 命令 agent_channel_request 的纯函数辅助——返回结构、超时钳制与目标解析。

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;

// ---- P1c 集成面：Tauri 命令辅助（纯函数，可独立测试） ----

/// agent_channel_request 命令的返回结构（serde camelCase → 前端 wrapper）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AgentChannelResponse {
    /// response 的 status："ok" | "err"（对端返回什么就是什么）。
    pub status: String,
    pub payload: Value,
}

/// 请求超时钳制（P1c 命令）：默认 10s；Some(0) 视为未指定；上限 60s
///（防调用方误传超大值占住线程池——send_agent_request 是阻塞等待）。
pub(crate) fn clamped_agent_request_timeout(timeout_ms: Option<u64>) -> Duration {
    const DEFAULT_MS: u64 = 10_000;
    const MAX_MS: u64 = 60_000;
    match timeout_ms {
        None | Some(0) => Duration::from_millis(DEFAULT_MS),
        Some(ms) => Duration::from_millis(ms.min(MAX_MS)),
    }
}

/// agent_channel_request 的目标解析结果：不含凭据（cluster/secret 由
/// 命令从 layout 快照取），只含把 PeerEndpoint 拼出来所需的实时寻址。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AgentChannelTarget {
    /// 目标当前 announce 的 device id（内嵌 LAN 地址会漂移，发送时按它
    /// 填 target_id——与路由消歧的前缀放行语义一致，见 AGENTS.md 坑 6）。
    pub device_id: String,
    pub name: String,
    pub addr: String,
    pub transport_public_key: String,
    pub protocol_version: u16,
}

/// 目标解析（P1c 命令 agent_channel_request；模式照抄 lib.rs
/// `file_transfer_target_for_device` 先例，Agent 语境的两处差异）：
/// ① **不要求 input_ready**——agent 通道不注入输入，被控端
///    inputReady=false 只影响键鼠，不影响 agent 响应；
/// ② server 角色也**优先实时 peers 表**拿地址（id 内嵌 LAN 地址会漂移，
///    announce 表是最新真相源，见 AGENTS.md 坑 6），device 表兜底。
/// 错误文案为面向调用方的中文（未配对 / 不在线 / 版本过旧）。
pub(crate) fn agent_channel_target_for_device(
    layout: &crate::LayoutState,
    peers: &[crate::LanPeer],
    device_id: &str,
) -> Result<AgentChannelTarget, String> {
    if layout.cluster_id.trim().is_empty() || layout.pair_secret.trim().is_empty() {
        return Err("当前设备尚未完成配对，无法使用 Agent 通道。".into());
    }

    // client 角色唯一目标是其控制器（照 file_transfer_target_for_device：
    // devices 里可能残留做 server 时期的旧条目，先查控制器）。
    if layout.machine_role == "client" {
        if let Some(controller) = layout
            .paired_controllers
            .iter()
            .find(|controller| controller.id == device_id)
        {
            let peer = peers
                .iter()
                .find(|peer| {
                    peer.id == controller.id
                        || (!controller.transport_public_key.trim().is_empty()
                            && peer.transport_public_key == controller.transport_public_key)
                })
                .ok_or_else(|| format!("{} 当前不在线，无法使用 Agent 通道。", controller.name))?;
            if peer.protocol_version != crate::quic_transport::PROTOCOL_VERSION
                || peer.transport_public_key.trim().is_empty()
                || peer.quic_port == 0
            {
                return Err(format!(
                    "{} 的 MyKVM 版本过旧，请先升级后重试。",
                    controller.name
                ));
            }
            let host = if !peer.ip.trim().is_empty() {
                peer.ip.clone()
            } else {
                crate::file_transfer_host(&peer.host)
                    .or_else(|| crate::file_transfer_host(&controller.ip))
                    .or_else(|| crate::file_transfer_host(&controller.host))
                    .ok_or_else(|| format!("{} 缺少可用地址。", controller.name))?
            };
            return Ok(AgentChannelTarget {
                device_id: peer.id.clone(),
                name: controller.name.clone(),
                addr: format!("{}:{}", host, peer.quic_port),
                transport_public_key: peer.transport_public_key.clone(),
                protocol_version: peer.protocol_version,
            });
        }
    }

    // 实时 peers 表优先（同 id，或同 transport_public_key 钉扎的已保存
    // 设备——id 会随 IP 漂移，证书公钥才是稳定身份，与控制器查找同因）。
    if let Some(peer) = peers
        .iter()
        .find(|peer| peer.id == device_id || peer_matches_device(layout, peer, device_id))
    {
        if peer.protocol_version != crate::quic_transport::PROTOCOL_VERSION
            || peer.transport_public_key.trim().is_empty()
            || peer.quic_port == 0
        {
            return Err(format!("{} 的 MyKVM 版本过旧，请先升级后重试。", peer.name));
        }
        let host = if !peer.ip.trim().is_empty() {
            peer.ip.clone()
        } else {
            crate::file_transfer_host(&peer.host)
                .ok_or_else(|| format!("{} 缺少可用地址。", peer.name))?
        };
        return Ok(AgentChannelTarget {
            device_id: peer.id.clone(),
            name: peer.name.clone(),
            addr: format!("{}:{}", host, peer.quic_port),
            transport_public_key: peer.transport_public_key.clone(),
            protocol_version: peer.protocol_version,
        });
    }

    // peers 表没有（announce 刚过期等）：device 表兜底，但必须在线。
    if let Some(device) = layout
        .devices
        .iter()
        .find(|device| device.id == device_id && device.role != "local")
    {
        if !device.online {
            return Err(format!("{} 当前不在线，无法使用 Agent 通道。", device.name));
        }
        let quic_port = crate::normalize_quic_port(device.transport_port, device.quic_port);
        if device.protocol_version != crate::quic_transport::PROTOCOL_VERSION
            || device.transport_public_key.trim().is_empty()
            || quic_port == 0
        {
            return Err(format!(
                "{} 的 MyKVM 版本过旧，请先升级后重试。",
                device.name
            ));
        }
        let host = crate::file_transfer_host(&device.host)
            .ok_or_else(|| format!("{} 缺少可用地址。", device.name))?;
        return Ok(AgentChannelTarget {
            device_id: device.id.clone(),
            name: device.name.clone(),
            addr: format!("{}:{}", host, quic_port),
            transport_public_key: device.transport_public_key.clone(),
            protocol_version: device.protocol_version,
        });
    }

    Err("没有找到可使用 Agent 通道的目标设备。".into())
}

/// peer 的 transport_public_key 是否与 device_id 对应已保存设备的钉扎
/// 公钥一致（id 漂移场景的稳定身份匹配；先例见
/// file_transfer_target_for_device 的控制器分支同款匹配）。
fn peer_matches_device(
    layout: &crate::LayoutState,
    peer: &crate::LanPeer,
    device_id: &str,
) -> bool {
    if peer.transport_public_key.trim().is_empty() {
        return false;
    }
    layout.devices.iter().any(|device| {
        device.id == device_id
            && !device.transport_public_key.trim().is_empty()
            && device.transport_public_key == peer.transport_public_key
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamped_agent_request_timeout_defaults_to_10s_and_caps_at_60s() {
        // P1c 命令 agent_channel_request 的超时钳制：默认 10s，Some(0)
        // 视为未指定，上限 60s。
        assert_eq!(
            clamped_agent_request_timeout(None),
            Duration::from_millis(10_000)
        );
        assert_eq!(
            clamped_agent_request_timeout(Some(0)),
            Duration::from_millis(10_000),
            "Some(0) 视为未指定"
        );
        assert_eq!(
            clamped_agent_request_timeout(Some(500)),
            Duration::from_millis(500)
        );
        assert_eq!(
            clamped_agent_request_timeout(Some(600_000)),
            Duration::from_millis(60_000),
            "钳上限 60s"
        );
    }
}
