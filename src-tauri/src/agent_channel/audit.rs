//! Agent 动作审计（RFC §5.3 对策二）：内存环形 + JSONL 落盘轮转、全局实例与审计快照。

use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{Mutex, OnceLock},
};

/// 审计环形缓冲上限（RFC §8 对策五：环形 + 落盘截断，不无限增长）。
pub(crate) const AGENT_AUDIT_RING_CAPACITY: usize = 2000;

/// 审计落盘单文件上限：超过即轮转为 `agent-audit.jsonl.1`（覆盖旧 .1）。
pub(crate) const AGENT_AUDIT_MAX_FILE_BYTES: u64 = 5 * 1024 * 1024;

/// 审计摘要的最大字符数（按 Unicode 字符计，参数摘要 ≤200 字）。
pub(crate) const AGENT_AUDIT_SUMMARY_MAX_CHARS: usize = 200;

/// 审计条目里 peer/capability/request_id 的最大字符数。鉴权失败也要留痕，
/// 而这些字段来自未鉴权的对端——不设上限的话，伪造包可用超长字段把内存
/// 环形（2000 条）与落盘文件撑爆。
pub(crate) const AGENT_AUDIT_FIELD_MAX_CHARS: usize = 128;

/// 一条 agent 动作审计记录（RFC §5.3 对策二）。summary 由各记录点构造，
/// 只含参数摘要/来源地址——**任何路径都不得写入 pair_secret/cluster_id**。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AgentAuditEntry {
    pub ts_ms: u64,
    /// "in" = 本机收到并处理；"out" = 本机发起/回发。
    pub direction: String,
    pub peer_id: String,
    pub capability: String,
    pub request_id: String,
    /// "ok" | "err" | "denied" | "timeout"
    pub status: String,
    pub duration_ms: u64,
    pub summary: String,
}

impl AgentAuditEntry {
    /// 构造并封顶：peer/capability/request_id 来自未鉴权的对端（伪造包可带
    /// 超长字段），统一按 [`AGENT_AUDIT_FIELD_MAX_CHARS`] 封顶；摘要按
    /// [`AGENT_AUDIT_SUMMARY_MAX_CHARS`] 封顶。
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        direction: &str,
        peer_id: &str,
        capability: &str,
        request_id: &str,
        status: &str,
        duration_ms: u64,
        summary: &str,
    ) -> Self {
        Self {
            ts_ms: crate::now_ms(),
            direction: direction.into(),
            peer_id: truncate_chars(peer_id, AGENT_AUDIT_FIELD_MAX_CHARS),
            capability: truncate_chars(capability, AGENT_AUDIT_FIELD_MAX_CHARS),
            request_id: truncate_chars(request_id, AGENT_AUDIT_FIELD_MAX_CHARS),
            status: status.into(),
            duration_ms,
            summary: truncate_summary(summary),
        }
    }
}

/// 摘要截断：按字符取前 [`AGENT_AUDIT_SUMMARY_MAX_CHARS`] 个，截断时补 `…`。
pub(crate) fn truncate_summary(summary: &str) -> String {
    truncate_chars(summary, AGENT_AUDIT_SUMMARY_MAX_CHARS)
}

/// 通用字符封顶：取前 `max_chars` 个字符，截断补 `…`（结果最多 max_chars+1）。
fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut truncated: String = text.chars().take(max_chars).collect();
    truncated.push('…');
    truncated
}

/// agent 通道审计日志：内存环形 2000 条 + JSONL 追加落盘（超过阈值轮转）。
/// 生产用 [`global_audit_log`]（OnceLock 全局，lib.rs setup 时注入配置目录）；
/// 测试构造独立实例（注入路径/阈值），避免进程级状态串线。
pub(crate) struct AgentAuditLog {
    ring: Mutex<VecDeque<AgentAuditEntry>>,
    /// 落盘文件路径；None = 仅内存（全局未初始化/测试）。
    file_path: Option<PathBuf>,
    /// 轮转阈值；生产为 [`AGENT_AUDIT_MAX_FILE_BYTES`]，测试注入小阈值。
    max_file_bytes: u64,
}

impl AgentAuditLog {
    /// 纯内存实例（测试/全局兜底）。
    pub(crate) fn in_memory() -> Self {
        Self::with_rotation_threshold(None, AGENT_AUDIT_MAX_FILE_BYTES)
    }

    /// 注入路径与轮转阈值的构造（测试用小阈值验证轮转）。
    pub(crate) fn with_rotation_threshold(file_path: Option<PathBuf>, max_file_bytes: u64) -> Self {
        Self {
            ring: Mutex::new(VecDeque::new()),
            file_path,
            max_file_bytes,
        }
    }

    /// 记录一条审计：入环形（超限挤最老）+ 尽力追加落盘。落盘失败只记
    /// warn 不上抛——审计不得反过来影响 agent 通道本身。
    pub(crate) fn record(&self, entry: AgentAuditEntry) {
        // 序列化在持锁区内完成：并发请求线程各自记录自己那条，避免
        // append 阶段经 back() 读到他人条目造成丢行/重复行。
        let line = {
            let Ok(mut ring) = self.ring.lock() else {
                return;
            };
            if ring.len() >= AGENT_AUDIT_RING_CAPACITY {
                ring.pop_front();
            }
            ring.push_back(entry);
            match serde_json::to_string(ring.back().expect("刚 push 必有尾元素")) {
                Ok(line) => line,
                Err(error) => {
                    log::warn!("agent: 审计条目序列化失败: {error}");
                    return;
                }
            }
        };
        self.append_line_to_file(&line);
    }

    /// 环形快照（按时间升序）。生产读取方是 P1c 的审计查看 Tauri 命令，
    /// P1a 只有测试消费——照 lib.rs DRAG_CONTROL_FAILED 的 cfg_attr 惯例
    /// 压制该构建下的 dead_code。
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn snapshot(&self) -> Vec<AgentAuditEntry> {
        self.ring
            .lock()
            .map(|ring| ring.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// 追加一行到落盘文件（尽力而为）：先按需轮转。
    fn append_line_to_file(&self, line: &str) {
        let Some(path) = &self.file_path else {
            return;
        };
        self.rotate_if_needed();
        let append = || -> std::io::Result<()> {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)?;
            file.write_all(line.as_bytes())?;
            file.write_all(b"\n")
        };
        if let Err(error) = append() {
            log::warn!("agent: 审计落盘失败（{}）: {error}", path.display());
        }
    }

    /// 轮转：主文件超过阈值时改名为 `<path>.1`（覆盖旧 .1）。检查点在
    /// 每次 append 之前，用元数据大小判断——多进程同时写同一目录不是
    /// 本场景（单应用单进程），无需文件锁。
    fn rotate_if_needed(&self) {
        let Some(path) = &self.file_path else {
            return;
        };
        let oversized = std::fs::metadata(path)
            .map(|metadata| metadata.len() > self.max_file_bytes)
            .unwrap_or(false);
        if !oversized {
            return;
        }
        let rotated_path = path.with_extension(format!(
            "{}.1",
            path.extension()
                .map(|extension| extension.to_string_lossy().into_owned())
                .unwrap_or_default()
        ));
        // rename 的目标既有语义：Windows MoveFileEx(REPLACE_EXISTING) 与
        // POSIX rename(2) 都直接覆盖既有文件——旧 .1 即被覆盖（规格如此）。
        if let Err(error) = std::fs::rename(path, &rotated_path) {
            log::warn!("agent: 审计轮转失败（{}）: {error}", path.display());
        }
    }
}

/// 生产全局审计（OnceLock；lib.rs setup 用 init_global_audit_log 注入配置
/// 目录，未初始化时退化为仅内存环形）。测试不要用它（构造独立实例）。
pub(crate) fn global_audit_log() -> std::sync::Arc<AgentAuditLog> {
    global_audit_slot()
        .get_or_init(|| std::sync::Arc::new(AgentAuditLog::in_memory()))
        .clone()
}

/// setup 期注入审计落盘路径（配置目录下 agent-audit.jsonl）。若全局实例已
/// 先被取用（退化为仅内存），注入失败并留 warn——落盘缺失必须可被发现。
pub(crate) fn init_global_audit_log(config_dir: &std::path::Path) {
    let path = config_dir.join("agent-audit.jsonl");
    let installed = global_audit_slot().set(std::sync::Arc::new(
        AgentAuditLog::with_rotation_threshold(Some(path.clone()), AGENT_AUDIT_MAX_FILE_BYTES),
    ));
    if installed.is_err() {
        log::warn!(
            "agent: 审计全局实例已先行初始化，落盘路径 {} 未生效",
            path.display()
        );
    }
}

fn global_audit_slot() -> &'static OnceLock<std::sync::Arc<AgentAuditLog>> {
    static SLOT: OnceLock<std::sync::Arc<AgentAuditLog>> = OnceLock::new();
    &SLOT
}

// ---- P1c 集成面：Tauri 命令辅助（纯函数，可独立测试） ----

/// 审计快照（P1c 命令 agent_channel_audit）：**最新在前**（倒序）；
/// limit 缺省 100、显式 0 视为未指定（避免调用方误传 0 拿到空列表
/// 误判「没有审计」）、超过环形容量按容量封顶。
pub(crate) fn recent_audit_snapshot(
    log: &AgentAuditLog,
    limit: Option<usize>,
) -> Vec<AgentAuditEntry> {
    const DEFAULT_LIMIT: usize = 100;
    let cap = limit.filter(|limit| *limit > 0).unwrap_or(DEFAULT_LIMIT);
    let mut entries = log.snapshot();
    entries.reverse();
    entries.truncate(cap.min(AGENT_AUDIT_RING_CAPACITY));
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::agent_channel::test_support::{TEST_CLUSTER, TEST_SECRET};

    // ---- h. 审计（P1a，RFC §5.3 对策二）----

    /// 构造一条测试审计记录。
    fn test_audit_entry(request_id: usize, summary: &str) -> AgentAuditEntry {
        AgentAuditEntry::new(
            "in",
            "peer-a",
            "echo",
            &format!("req-{request_id}"),
            "ok",
            5,
            summary,
        )
    }

    #[test]
    fn audit_ring_caps_at_2000_and_drops_oldest() {
        let log = AgentAuditLog::in_memory();
        for request_id in 0..AGENT_AUDIT_RING_CAPACITY + 5 {
            log.record(test_audit_entry(request_id, "参数摘要"));
        }
        let snapshot = log.snapshot();
        assert_eq!(
            snapshot.len(),
            AGENT_AUDIT_RING_CAPACITY,
            "环形缓冲必须封顶在 2000 条"
        );
        assert_eq!(snapshot[0].request_id, "req-5", "最老的 5 条必须被挤出");
        assert_eq!(
            snapshot[snapshot.len() - 1].request_id,
            "req-2004",
            "最新记录保留在队尾"
        );
    }

    #[test]
    fn audit_summary_is_truncated_at_200_chars() {
        let long = "字".repeat(500);
        let entry = AgentAuditEntry::new("in", "peer-a", "msg.send", "req-1", "ok", 1, &long);
        assert!(
            entry.summary.chars().count() <= AGENT_AUDIT_SUMMARY_MAX_CHARS + 1,
            "摘要必须截断在 200 字内（含省略号）"
        );
        assert!(entry.summary.ends_with('…'), "截断必须打标记");
        assert_eq!(
            AgentAuditEntry::new("in", "p", "c", "r", "ok", 0, "短摘要").summary,
            "短摘要",
            "未超长摘要原样保留"
        );
    }

    #[test]
    fn audit_jsonl_appends_and_rotates_at_injected_threshold() {
        let dir = std::env::temp_dir().join(format!(
            "mykvm-audit-rotate-{}-{}",
            std::process::id(),
            crate::random_hex(4)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        let path = dir.join("agent-audit.jsonl");

        // 注入小阈值（1200 字节 ≈ 8 条记录）恰好触发一次轮转，验证轮转语义。
        // （更小的阈值会触发多次轮转，而轮转按规格覆盖旧 .1——多次覆盖下
        // 只有最后一次轮转的内容保留，那是设计行为不是丢失。）
        let log = AgentAuditLog::with_rotation_threshold(Some(path.clone()), 1200);
        let long_summary = "x".repeat(80);
        for request_id in 0..12 {
            log.record(test_audit_entry(request_id, &long_summary));
        }

        // 主文件 + 轮转出的 .1 必须都存在，且每行都是合法 JSON 条目。
        let rotated_path = dir.join("agent-audit.jsonl.1");
        assert!(path.exists(), "审计主文件必须落盘");
        assert!(rotated_path.exists(), "超过阈值必须轮转出 .1 文件");
        for file_path in [&rotated_path, &path] {
            let content = std::fs::read_to_string(file_path).expect("读取审计文件");
            let lines: Vec<&str> = content.lines().filter(|line| !line.is_empty()).collect();
            assert!(!lines.is_empty(), "{file_path:?} 必须有内容");
            for line in &lines {
                let entry: AgentAuditEntry =
                    serde_json::from_str(line).expect("每行必须是合法 AgentAuditEntry");
                assert_eq!(entry.capability, "echo");
            }
        }
        // 12 条记录全部落在两个文件里（append 不丢行），.1 持有较老的那段。
        let mut all_ids: Vec<String> = Vec::new();
        for file_path in [&rotated_path, &path] {
            let content = std::fs::read_to_string(file_path).expect("读取");
            for line in content.lines().filter(|line| !line.is_empty()) {
                let entry: AgentAuditEntry = serde_json::from_str(line).expect("解析");
                all_ids.push(entry.request_id);
            }
        }
        all_ids.sort();
        all_ids.dedup();
        assert_eq!(all_ids.len(), 12, "12 条记录必须全部落盘且不重复");
        let rotated_first: AgentAuditEntry = serde_json::from_str(
            std::fs::read_to_string(&rotated_path)
                .expect("读取 .1")
                .lines()
                .next()
                .expect(".1 非空"),
        )
        .expect("解析");
        assert_eq!(
            rotated_first.request_id, "req-0",
            ".1 必须从最老的记录开始（一次轮转切出前段）"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn audit_second_rotation_overwrites_old_rotated_file() {
        // 规格：超过阈值轮转为 .1 且覆盖旧 .1（最多保留两个文件，不无限增长）。
        // 阈值 1 字节 ⇒ 每次 append 前都轮转：第 3 条写入时 .1 被第 2 条覆盖。
        let dir = std::env::temp_dir().join(format!(
            "mykvm-audit-overwrite-{}-{}",
            std::process::id(),
            crate::random_hex(4)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        let path = dir.join("agent-audit.jsonl");
        let log = AgentAuditLog::with_rotation_threshold(Some(path.clone()), 1);
        for request_id in 0..3 {
            log.record(test_audit_entry(request_id, "摘要"));
        }

        let read_ids = |file: &std::path::Path| -> Vec<String> {
            std::fs::read_to_string(file)
                .expect("读取")
                .lines()
                .filter(|line| !line.is_empty())
                .map(|line| {
                    serde_json::from_str::<AgentAuditEntry>(line)
                        .expect("解析")
                        .request_id
                })
                .collect()
        };
        assert_eq!(
            read_ids(&dir.join("agent-audit.jsonl.1")),
            vec!["req-1".to_string()],
            "旧 .1（req-0）必须被第二次轮转覆盖"
        );
        assert_eq!(read_ids(&path), vec!["req-2".to_string()]);
        assert!(
            !dir.join("agent-audit.jsonl.2").exists(),
            "不得产生 .2 等额外轮转文件"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn audit_serialization_never_contains_pair_credentials() {
        // RFC §5.3：审计是排障面，不得成为第二份凭据副本——即便调用方
        // 误传，序列化文本里也不能出现 pair_secret/cluster_id 的值。
        let log = AgentAuditLog::in_memory();
        log.record(AgentAuditEntry::new(
            "in",
            "peer-a",
            "echo",
            "req-1",
            "denied",
            0,
            "10.0.0.9:50000 鉴权失败",
        ));
        let serialized: String = log
            .snapshot()
            .iter()
            .map(|entry| serde_json::to_string(entry).expect("条目序列化"))
            .collect();
        assert!(
            !serialized.contains(TEST_SECRET),
            "审计文本不得含 pair_secret"
        );
        assert!(
            !serialized.contains(TEST_CLUSTER),
            "审计文本不得含 cluster_id"
        );
        assert!(
            serialized.contains("10.0.0.9:50000"),
            "denied 摘要必须记来源地址"
        );
    }

    #[test]
    fn audit_write_failure_does_not_break_recording() {
        // 落盘路径指向一个文件（不是目录）：append 必失败，record 不得
        // panic，环形照常工作——审计故障不得影响 agent 通道。
        let dir = std::env::temp_dir().join(format!(
            "mykvm-audit-badpath-{}-{}",
            std::process::id(),
            crate::random_hex(4)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        let blocker = dir.join("blocker");
        std::fs::write(&blocker, b"not a dir").expect("预置文件");
        let bad_path = blocker.join("child").join("agent-audit.jsonl");

        let log = AgentAuditLog::with_rotation_threshold(Some(bad_path), 300);
        log.record(test_audit_entry(1, "摘要"));
        log.record(test_audit_entry(2, "摘要"));
        assert_eq!(log.snapshot().len(), 2, "落盘失败不得影响环形记录");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recent_audit_snapshot_returns_newest_first_with_clamped_limit() {
        // P1c 命令 agent_channel_audit 的实现：最新在前（倒序），
        // limit 缺省 100、显式 0 视为未指定、超大值钳到环形容量。
        let log = AgentAuditLog::in_memory();
        for index in 0..3 {
            log.record(AgentAuditEntry::new(
                "in",
                "peer-1",
                "log.read",
                &format!("req-{index}"),
                "ok",
                0,
                "",
            ));
        }

        let recent = recent_audit_snapshot(&log, None);
        assert_eq!(recent.len(), 3, "默认 limit=100 不截短 3 条");
        assert_eq!(recent[0].request_id, "req-2", "最新在前");
        assert_eq!(recent[2].request_id, "req-0");

        let recent = recent_audit_snapshot(&log, Some(2));
        assert_eq!(recent.len(), 2, "显式 limit 截短");
        assert_eq!(recent[0].request_id, "req-2", "截短后仍最新在前");

        let recent = recent_audit_snapshot(&log, Some(0));
        assert_eq!(recent.len(), 3, "Some(0) 视为未指定，按默认 100 处理");

        let recent = recent_audit_snapshot(&log, Some(usize::MAX));
        assert_eq!(recent.len(), 3, "超大 limit 钳到 2000，实际以环形条数为准");
    }
}
