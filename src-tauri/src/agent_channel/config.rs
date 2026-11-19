//! Agent 通道用户意图配置（RFC §6.3）：开关 + 能力级别 + 白名单、level gate 与能力宣告。

use serde::{Deserialize, Serialize};

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

/// LanPeer 随 discovery announce 携带的 Agent 通道能力宣告（RFC §6.3 P1c）。
/// 对端据此得知「此端是否具备 agent 通道、开放到哪一级」；旧版对端的包
/// 没有该字段，LanPeer 侧 serde(default) 落 None（跨版本兼容已用字节层
/// 测试锁定，见 lib.rs 的跨版本双向探测）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AgentChannelAnnounce {
    pub enabled: bool,
    pub level: String,
}

/// 从用户意图配置构造宣告。enabled=false 也照常宣告（enabled:false +
/// sanitized 级别）——语义是「此端已显式关闭」，与「旧版对端（无字段）」
/// 区分开；level 在此钳制为有效值，与 gate 判定一致（调用方不必记得
/// 先 sanitized，钳制内聚在本函数）。
pub(crate) fn announce_from_config(config: &AgentChannelConfig) -> AgentChannelAnnounce {
    let sanitized = config.sanitized();
    AgentChannelAnnounce {
        enabled: sanitized.enabled,
        level: sanitized.level,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn announce_from_config_clamps_level_and_announces_disabled() {
        // P1c：enabled=false 也宣告（enabled:false），level 取钳制后的有效值，
        // 不外泄脏值（与 gate 判定一致）。
        let config = AgentChannelConfig {
            enabled: false,
            level: "bogus".into(),
            ..AgentChannelConfig::default()
        };
        let announce = announce_from_config(&config);
        assert!(!announce.enabled);
        assert_eq!(announce.level, "readonly");

        let config = AgentChannelConfig {
            enabled: true,
            level: "files".into(),
            ..AgentChannelConfig::default()
        };
        let announce = announce_from_config(&config);
        assert!(announce.enabled);
        assert_eq!(announce.level, "files");
    }
}
