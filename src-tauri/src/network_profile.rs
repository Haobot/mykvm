//! 按网络（接口名+子网指纹）分组的设备配置记忆与离线设备自动清理的纯逻辑。
//!
//! 发现循环是本模块唯一的调用方：每 10s 的身份刷新块里先用
//! `NetworkKeyStability` 判定指纹是否稳定，稳定后 `swap_network_profile`
//! 切换档案；同一块里 `OfflineProbeTracker::evaluate` 对仍离线的非手动
//! 设备计一轮 miss，满 `OFFLINE_PROBE_MAX_MISSES` 由调用方移除。所有
//! 发包与落盘都留在 lib.rs，本模块只做可单测的决策。

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

use super::{now_ms, Device, LayoutState, NetworkProfile};

/// 换网判定要求指纹连续观测到的相同轮数（发现循环 10s 一轮，两轮≈20s
/// 防抖：WiFi 切换过程中接口先 down 后 up、短暂拿到地址又丢失的瞬态
/// 不足以触发档案切换）。
const NETWORK_KEY_STABLE_ROUNDS: u32 = 2;
/// 离线重试轮间隔；发现循环以同节奏（身份刷新 10s 块）驱动。
pub(crate) const OFFLINE_PROBE_INTERVAL: Duration = Duration::from_secs(10);
/// 离线重试：连续多少轮仍离线即视为不可达、静默删除。
pub(crate) const OFFLINE_PROBE_MAX_MISSES: u32 = 5;

/// 一个网络指纹条目：接口名 + IPv4 地址 + 子网掩码。
struct FingerprintEntry {
    interface: String,
    ip: Ipv4Addr,
    netmask: Ipv4Addr,
}

/// 从指纹条目计算稳定指纹：过滤虚拟接口与不可用地址后，每接口产出
/// `iface=网络地址/前缀`（如 `en0=192.168.1.0/24`），排序去重拼接。
/// DHCP 在同一子网内换地址不换指纹；换 WiFi/插网线（子网或接口名变化）
/// 才变。空条目/全被过滤返回空串——「未知网络」，调用方不得据此换网。
fn fingerprint_from(entries: &[FingerprintEntry]) -> String {
    let mut parts: Vec<String> = entries
        .iter()
        .filter(|entry| {
            !super::is_excluded_interface_name(&entry.interface)
                && super::usable_discovery_ipv4(entry.ip)
                && !entry.netmask.is_unspecified()
        })
        .map(|entry| {
            let network = u32::from(entry.ip) & u32::from(entry.netmask);
            let prefix = u32::from(entry.netmask).count_ones();
            format!("{}={}/{}", entry.interface, Ipv4Addr::from(network), prefix)
        })
        .collect();
    parts.sort();
    parts.dedup();
    parts.join(",")
}

/// 采集当前本机地址并计算网络指纹；get_if_addrs 失败按「无网络」返回空串。
pub(crate) fn network_fingerprint() -> String {
    let Ok(interfaces) = if_addrs::get_if_addrs() else {
        return String::new();
    };
    let entries: Vec<FingerprintEntry> = interfaces
        .iter()
        .filter_map(|interface| match &interface.addr {
            if_addrs::IfAddr::V4(address) => Some(FingerprintEntry {
                interface: interface.name.clone(),
                ip: address.ip,
                netmask: address.netmask,
            }),
            if_addrs::IfAddr::V6(_) => None,
        })
        .collect();
    fingerprint_from(&entries)
}

/// 指纹稳定性观测器：空指纹（网络未知）清空候选；指纹变化重新计数；
/// 连续 `NETWORK_KEY_STABLE_ROUNDS` 轮相同后每轮返回稳定键（此后调用方
/// 反复交给 `swap_network_profile`，键相同会自然 no-op）。
pub(crate) struct NetworkKeyStability {
    pending: Option<String>,
    rounds: u32,
}

impl NetworkKeyStability {
    pub(crate) fn new() -> Self {
        Self {
            pending: None,
            rounds: 0,
        }
    }

    pub(crate) fn observe(&mut self, fingerprint: &str) -> Option<&str> {
        if fingerprint.is_empty() {
            self.pending = None;
            self.rounds = 0;
            return None;
        }
        match self.pending.as_deref() {
            Some(current) if current == fingerprint => self.rounds += 1,
            _ => {
                self.pending = Some(fingerprint.to_string());
                self.rounds = 1;
            }
        }
        if self.rounds >= NETWORK_KEY_STABLE_ROUNDS {
            self.pending.as_deref()
        } else {
            None
        }
    }
}

/// 设备增删后修正 `active_device_id`/`selected_screen_id` 指向——与前端
/// `handleRemoveDevice` 同语义：失效则落到剩余列表第一个设备/其第一块
/// 屏幕；列表为空时保持原值不动。
fn repair_active_selection(layout: &mut LayoutState) {
    if !layout
        .devices
        .iter()
        .any(|device| device.id == layout.active_device_id)
    {
        if let Some(first) = layout.devices.first() {
            layout.active_device_id = first.id.clone();
        }
    }
    let selected_exists = layout.devices.iter().any(|device| {
        device
            .screens
            .iter()
            .any(|screen| screen.id == layout.selected_screen_id)
    });
    if !selected_exists {
        if let Some(screen) = layout
            .devices
            .first()
            .and_then(|device| device.screens.first())
        {
            layout.selected_screen_id = screen.id.clone();
        }
    }
}

/// 换网档案切换：把非 local/非 manual 设备暂存进旧键档案，载入新键档案
/// 的设备（按 id 去重合并，local/manual 恒保留）。旧键为空 = 首次采用，
/// 当前设备原地归属新键、只记键不暂存（升级自旧版时无法判定设备属于
/// 哪个网络，留在原地由离线重试自行清理）。返回是否真的动了列表
/// （新键为空或与当前相同返回 false，不自增 revision）。
pub(crate) fn swap_network_profile(layout: &mut LayoutState, new_key: &str) -> bool {
    if new_key.is_empty() || layout.active_network_key == new_key {
        return false;
    }
    let old_key = std::mem::replace(&mut layout.active_network_key, new_key.to_string());

    // 首次采用（旧键为空）：当前设备列表本就属于这个网络，原地归属、
    // 只记键不暂存不重建——升级自旧版时无法判定设备属于哪个网络，
    // 留在原地由离线重试自行清理。设备列表没动，不自增 revision。
    if old_key.is_empty() {
        return true;
    }

    let stash: Vec<Device> = layout
        .devices
        .iter()
        .filter(|device| device.role != "local" && device.source != "manual")
        .cloned()
        .collect();
    layout.network_profiles.insert(
        old_key,
        NetworkProfile {
            devices: stash,
            saved_at_ms: now_ms(),
        },
    );

    let mut next: Vec<Device> = layout
        .devices
        .iter()
        .filter(|device| device.role == "local" || device.source == "manual")
        .cloned()
        .collect();
    if let Some(profile) = layout.network_profiles.get(new_key) {
        for device in &profile.devices {
            if !next.iter().any(|existing| existing.id == device.id) {
                next.push(device.clone());
            }
        }
    }
    layout.devices = next;
    repair_active_selection(layout);
    layout.layout_revision += 1;
    true
}

/// 按 id 移除设备（调用方负责判定可删），修正 active/selected 指向并
/// 自增 `layout_revision`——save_layout 的陈旧快照闸门据此拦截前端回写。
/// 返回实际移除的 (id, name)，空表示没动列表（不自增 revision）。
pub(crate) fn remove_devices_by_id(
    layout: &mut LayoutState,
    ids: &[String],
) -> Vec<(String, String)> {
    if ids.is_empty() {
        return Vec::new();
    }
    let mut removed = Vec::new();
    layout.devices.retain(|device| {
        if ids.contains(&device.id) {
            removed.push((device.id.clone(), device.name.clone()));
            false
        } else {
            true
        }
    });
    if !removed.is_empty() {
        repair_active_selection(layout);
        layout.layout_revision += 1;
    }
    removed
}

/// 离线设备的重试计数器（运行时状态，不落盘；只活在发现循环里——
/// 循环停摆即冻结，绝不因循环卡死而误删）。
pub(crate) struct OfflineProbeTracker {
    misses: HashMap<String, u32>,
}

impl OfflineProbeTracker {
    pub(crate) fn new() -> Self {
        Self {
            misses: HashMap::new(),
        }
    }

    /// 一轮离线判定：在线/豁免（local、manual、upgrading）清零计数；仍离线
    /// 计一轮 miss 并列入本轮探测；满上限的转入删除列表（探测与删除同轮，
    /// 探测包是最后尽力一发，回复到达时设备已移除也无害——回复只合并 peer）。
    /// 已从设备表消失的 id 计数一并回收，防泄漏。
    pub(crate) fn evaluate(&mut self, devices: &[Device]) -> (Vec<String>, Vec<(String, u32)>) {
        let mut probes = Vec::new();
        let mut removals = Vec::new();
        for device in devices {
            if device.role == "local" || device.source == "manual" || device.upgrading {
                self.misses.remove(&device.id);
                continue;
            }
            if device.online {
                self.misses.remove(&device.id);
                continue;
            }
            let misses = self.misses.entry(device.id.clone()).or_insert(0);
            *misses += 1;
            probes.push(device.id.clone());
            if *misses >= OFFLINE_PROBE_MAX_MISSES {
                removals.push((device.id.clone(), *misses));
            }
        }
        self.misses
            .retain(|id, _| devices.iter().any(|device| device.id == *id));
        (probes, removals)
    }

    /// 换网后清空：新档案载入的设备从零开始计，不携带旧网络的欠账。
    pub(crate) fn reset(&mut self) {
        self.misses.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::test_screen;

    fn entry(interface: &str, ip: [u8; 4], mask: [u8; 4]) -> FingerprintEntry {
        FingerprintEntry {
            interface: interface.into(),
            ip: Ipv4Addr::from(ip),
            netmask: Ipv4Addr::from(mask),
        }
    }

    fn device(id: &str, role: &str, source: &str, online: bool) -> Device {
        Device {
            id: id.into(),
            name: id.into(),
            platform: "macos".into(),
            host: "10.0.0.2".into(),
            transport_port: 47833,
            quic_port: 47834,
            transport_public_key: String::new(),
            protocol_version: 1,
            color: "#0f766e".into(),
            online,
            input_ready: false,
            upgrading: false,
            upgrading_until_ms: 0,
            role: role.into(),
            source: source.into(),
            screens: vec![test_screen(id)],
            pointer_speed: 1.0,
            scroll_speed: 1.0,
        }
    }

    fn layout_with(devices: Vec<Device>) -> LayoutState {
        let mut layout = crate::tests::test_layout();
        layout.devices = devices;
        layout.active_device_id = layout
            .devices
            .first()
            .map(|device| device.id.clone())
            .unwrap_or_default();
        layout.selected_screen_id = layout
            .devices
            .first()
            .and_then(|device| device.screens.first())
            .map(|screen| screen.id.clone())
            .unwrap_or_default();
        layout
    }

    #[test]
    fn network_fingerprint_stable_across_dhcp_renewal() {
        // 同接口同子网、主机位变了（DHCP 续租）：指纹不变。
        let before = fingerprint_from(&[entry("en0", [192, 168, 1, 23], [255, 255, 255, 0])]);
        let after = fingerprint_from(&[entry("en0", [192, 168, 1, 77], [255, 255, 255, 0])]);
        assert_eq!(before, "en0=192.168.1.0/24");
        assert_eq!(before, after);
    }

    #[test]
    fn network_fingerprint_distinguishes_iface_and_subnet() {
        let wifi = fingerprint_from(&[entry("en0", [192, 168, 1, 5], [255, 255, 255, 0])]);
        let other_wifi = fingerprint_from(&[entry("en0", [10, 0, 0, 5], [255, 255, 255, 0])]);
        let wired = fingerprint_from(&[entry("en5", [192, 168, 1, 5], [255, 255, 255, 0])]);
        assert_ne!(wifi, other_wifi, "不同子网必须是不同网络");
        assert_ne!(wifi, wired, "同子网不同接口（WiFi vs 有线）必须分开");
    }

    #[test]
    fn network_fingerprint_empty_when_no_usable_address() {
        assert_eq!(fingerprint_from(&[]), "");
        // 虚拟接口与不可用地址全部过滤后同样为空——未知网络不换档。
        assert_eq!(
            fingerprint_from(&[
                entry("utun3", [10, 8, 0, 2], [255, 255, 255, 0]),
                entry("en0", [169, 254, 1, 1], [255, 255, 0, 0]),
                entry("en0", [0, 0, 0, 0], [0, 0, 0, 0]),
            ]),
            ""
        );
    }

    #[test]
    fn network_key_stability_requires_two_rounds_and_resets_on_gap() {
        let mut stability = NetworkKeyStability::new();
        assert_eq!(
            stability.observe("en0=192.168.1.0/24"),
            None,
            "首轮只记候选"
        );
        assert_eq!(
            stability.observe("en0=192.168.1.0/24"),
            Some("en0=192.168.1.0/24"),
            "次轮相同才稳定"
        );
        // 网络断开（空指纹）清空候选，恢复后要重新攒两轮。
        assert_eq!(stability.observe(""), None);
        assert_eq!(stability.observe("en0=192.168.1.0/24"), None);
        // 指纹变化同样重新计数。
        assert_eq!(
            stability.observe("en0=192.168.1.0/24"),
            Some("en0=192.168.1.0/24")
        );
        assert_eq!(stability.observe("en5=10.0.0.0/24"), None);
    }

    #[test]
    fn network_swap_stashes_remote_devices_and_restores_target_profile() {
        let mut layout = layout_with(vec![
            device("local-device", "local", "detected", true),
            device("peer-a", "client", "detected", true),
        ]);
        layout.active_network_key = "en0=192.168.1.0/24".into();

        assert!(swap_network_profile(&mut layout, "en5=10.0.0.0/24"));
        // 旧网络的远程设备进了档案，新网络无档案 → 列表只剩 local。
        let stashed = &layout.network_profiles["en0=192.168.1.0/24"];
        assert_eq!(stashed.devices.len(), 1);
        assert_eq!(stashed.devices[0].id, "peer-a");
        assert_eq!(layout.devices.len(), 1);
        assert_eq!(layout.devices[0].id, "local-device");
        assert_eq!(layout.active_network_key, "en5=10.0.0.0/24");

        // 切回旧网络：peer-a 原样回来。
        assert!(swap_network_profile(&mut layout, "en0=192.168.1.0/24"));
        assert_eq!(layout.devices.len(), 2);
        assert!(layout.devices.iter().any(|device| device.id == "peer-a"));
        assert_eq!(layout.active_network_key, "en0=192.168.1.0/24");
    }

    #[test]
    fn network_swap_keeps_manual_and_local_devices() {
        let mut layout = layout_with(vec![
            device("local-device", "local", "detected", true),
            device("manual-1", "client", "manual", false),
            device("peer-a", "client", "detected", true),
        ]);
        layout.active_network_key = "en0=192.168.1.0/24".into();

        assert!(swap_network_profile(&mut layout, "en5=10.0.0.0/24"));
        let ids: Vec<&str> = layout
            .devices
            .iter()
            .map(|device| device.id.as_str())
            .collect();
        assert!(ids.contains(&"local-device"), "本地设备必须保留");
        assert!(ids.contains(&"manual-1"), "手动设备全局保留、不进档案");
        assert!(!ids.contains(&"peer-a"), "detected 设备按网络分组暂存");
        // 手动设备不进档案。
        let stashed = &layout.network_profiles["en0=192.168.1.0/24"];
        assert_eq!(stashed.devices.len(), 1);
        assert_eq!(stashed.devices[0].id, "peer-a");
    }

    #[test]
    fn network_swap_repairs_active_device_id_fallback() {
        let mut layout = layout_with(vec![
            device("local-device", "local", "detected", true),
            device("peer-a", "client", "detected", true),
        ]);
        layout.active_network_key = "en0=192.168.1.0/24".into();
        layout.active_device_id = "peer-a".into();
        layout.selected_screen_id = "peer-a-screen".into();

        assert!(swap_network_profile(&mut layout, "en5=10.0.0.0/24"));
        assert_eq!(
            layout.active_device_id, "local-device",
            "active 指向被换走的设备时回退到剩余首个设备"
        );
        assert_eq!(
            layout.selected_screen_id, "local-device-display-1",
            "selected 回退到剩余首个设备的首块屏幕"
        );
    }

    #[test]
    fn network_swap_first_adoption_keeps_devices_and_records_key() {
        let mut layout = layout_with(vec![
            device("local-device", "local", "detected", true),
            device("peer-a", "client", "detected", true),
        ]);
        // active_network_key 为空 = 首次采用：只记键，不暂存不换列表。
        assert!(swap_network_profile(&mut layout, "en0=192.168.1.0/24"));
        assert_eq!(layout.devices.len(), 2);
        assert!(layout.network_profiles.is_empty());
        assert_eq!(layout.active_network_key, "en0=192.168.1.0/24");
        // 同键再换是 no-op：不动列表也不自增 revision。
        let revision = layout.layout_revision;
        assert!(!swap_network_profile(&mut layout, "en0=192.168.1.0/24"));
        assert!(!swap_network_profile(&mut layout, ""));
        assert_eq!(layout.layout_revision, revision);
    }

    #[test]
    fn offline_device_removed_after_five_failed_probes() {
        let mut tracker = OfflineProbeTracker::new();
        let offline = [device("peer-a", "client", "detected", false)];
        let mut last = 0;
        for round in 1..=OFFLINE_PROBE_MAX_MISSES {
            let (probes, removals) = tracker.evaluate(&offline);
            assert!(
                probes.contains(&"peer-a".to_string()),
                "第 {round} 轮要探测"
            );
            if round < OFFLINE_PROBE_MAX_MISSES {
                assert!(removals.is_empty(), "第 {round} 轮不该删");
                last = round;
            } else {
                assert_eq!(removals.len(), 1);
                assert_eq!(removals[0].0, "peer-a");
                assert_eq!(removals[0].1, OFFLINE_PROBE_MAX_MISSES);
                last = round;
            }
        }
        assert_eq!(last, OFFLINE_PROBE_MAX_MISSES);
    }

    #[test]
    fn offline_probe_counter_resets_when_device_returns_online() {
        let mut tracker = OfflineProbeTracker::new();
        let mut online = [device("peer-a", "client", "detected", false)];
        tracker.evaluate(&online);
        tracker.evaluate(&online);
        let (_, removals) = tracker.evaluate(&online);
        assert!(removals.is_empty());

        // 回到在线：计数清零，再离线从头攒。
        online[0].online = true;
        let (probes, _) = tracker.evaluate(&online);
        assert!(probes.is_empty(), "在线设备不探测");
        online[0].online = false;
        for _ in 0..OFFLINE_PROBE_MAX_MISSES - 1 {
            let (_, removals) = tracker.evaluate(&online);
            assert!(removals.is_empty(), "重新离线应重新攒满五轮");
        }
        let (_, removals) = tracker.evaluate(&online);
        assert_eq!(removals.len(), 1, "第五轮才删");
    }

    #[test]
    fn offline_probe_skips_local_manual_and_upgrading_devices() {
        let mut tracker = OfflineProbeTracker::new();
        let mut devices = vec![
            device("local-device", "local", "detected", false),
            device("manual-1", "client", "manual", false),
            device("peer-upgrading", "client", "detected", false),
        ];
        devices[2].upgrading = true;
        for _ in 0..OFFLINE_PROBE_MAX_MISSES + 2 {
            let (probes, removals) = tracker.evaluate(&devices);
            assert!(probes.is_empty(), "豁免设备不探测不计数");
            assert!(removals.is_empty(), "豁免设备绝不删除");
        }
        // 升级宽限结束后重新计数。
        devices[2].upgrading = false;
        let (probes, removals) = tracker.evaluate(&devices);
        assert!(probes.contains(&"peer-upgrading".to_string()));
        assert!(removals.is_empty());
    }

    #[test]
    fn offline_probe_counter_drops_devices_gone_from_list() {
        let mut tracker = OfflineProbeTracker::new();
        tracker.evaluate(&[device("peer-a", "client", "detected", false)]);
        tracker.evaluate(&[device("peer-a", "client", "detected", false)]);
        // peer-a 被用户手动删除后列表不再包含它：计数随之回收。
        tracker.evaluate(&[]);
        let (probes, removals) = tracker.evaluate(&[device("peer-b", "client", "detected", false)]);
        assert!(!probes.contains(&"peer-a".to_string()));
        assert!(removals.is_empty());
    }

    #[test]
    fn remove_devices_by_id_repairs_selection_and_bumps_revision() {
        let mut layout = layout_with(vec![
            device("local-device", "local", "detected", true),
            device("peer-a", "client", "detected", false),
        ]);
        layout.active_device_id = "peer-a".into();
        layout.selected_screen_id = "peer-a-screen".into();
        let revision = layout.layout_revision;

        let removed = remove_devices_by_id(&mut layout, &["peer-a".into(), "ghost".into()]);
        assert_eq!(removed, vec![("peer-a".to_string(), "peer-a".to_string())]);
        assert_eq!(layout.devices.len(), 1);
        assert_eq!(layout.active_device_id, "local-device");
        assert!(layout.selected_screen_id.starts_with("local-device"));
        assert_eq!(layout.layout_revision, revision + 1);

        // 空列表/未命中：不动 revision。
        let revision = layout.layout_revision;
        assert!(remove_devices_by_id(&mut layout, &[]).is_empty());
        assert!(remove_devices_by_id(&mut layout, &["ghost".into()]).is_empty());
        assert_eq!(layout.layout_revision, revision);
    }

    #[test]
    fn screen_helper_ids_match_device_prefix() {
        // 防御性：上面用例依赖 test_screen(id) 的 id 形态。
        let screen = test_screen("peer-a");
        assert!(screen.id.starts_with("peer-a"));
    }
}
