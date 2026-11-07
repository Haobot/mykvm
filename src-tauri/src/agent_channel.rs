//! 跨机 Agent 互联通道（RFC：Doc/Rfc/跨机Agent互联与受控协作通道.md §6.1/§6.3）。
//!
//! P0 范围：协议包结构 + 鉴权 + pending 请求表 + 两段式 echo 闭环。
//! 两段式：request 与 response 是两条独立的 QUIC 流消息，靠 request_id 关联；
//! 应答由接收方反向新建流发回（ConnectionMap 对称，反向拨号命中既有信任面），
//! 不改动 quic_transport.rs 的 StreamHandler 签名。

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{HashMap, VecDeque},
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc, Arc, Mutex, OnceLock,
    },
    time::{Duration, Instant},
};

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

/// L0 能力：读取本机运行状态快照（P1a）。
pub(crate) const AGENT_CAPABILITY_STATUS_GET: &str = "status.get";

/// L0 能力：单向消息投递到本机收件箱（P1a）。
pub(crate) const AGENT_CAPABILITY_MSG_SEND: &str = "msg.send";

/// L0 能力：白名单根目录内 stat 文件/列目录（P1b）。
pub(crate) const AGENT_CAPABILITY_FILE_STAT: &str = "file.stat";

/// L0 能力：白名单根目录内内联读取小文件，一律 base64（P1b）。
pub(crate) const AGENT_CAPABILITY_FILE_READ: &str = "file.read";

/// L0 能力：读取本机日志尾部（P1b）。
pub(crate) const AGENT_CAPABILITY_LOG_READ: &str = "log.read";

/// file.read 的默认读取字节数（RFC §6.2：≤1MiB 内联）。
pub(crate) const AGENT_FILE_READ_DEFAULT_BYTES: u64 = 256 * 1024;

/// file.read 的参数上限：超过钳制到该值（而非拒绝）。
pub(crate) const AGENT_FILE_READ_MAX_BYTES: u64 = 1024 * 1024;

/// log.read 的默认尾部字节数。
pub(crate) const AGENT_LOG_READ_DEFAULT_BYTES: u64 = 64 * 1024;

/// log.read 的参数上限：超过钳制到该值（而非拒绝）。
pub(crate) const AGENT_LOG_READ_MAX_BYTES: u64 = 1024 * 1024;

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

/// 收件箱容量：内存环形 100 条，超限挤最老（不落盘——消息是瞬时协作信号，
/// 审计里已留有元数据痕迹）。
pub(crate) const AGENT_INBOX_CAPACITY: usize = 100;

/// msg.send 正文上限：4KiB（按 UTF-8 字节，截断在字符边界）。
pub(crate) const AGENT_MSG_TEXT_MAX_BYTES: usize = 4 * 1024;

/// 一条收件箱消息（RFC §6.2 msg.send：对端写收件箱）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AgentInboxMessage {
    pub ts_ms: u64,
    /// 发送方 device id（request 的 origin_id）。
    pub peer_id: String,
    pub title: Option<String>,
    pub text: String,
    /// text 超过 4KiB 被截断时为 true（任务书「截断并标注 truncated」）。
    pub truncated: bool,
}

/// agent 收件箱。生产用 [`global_agent_inbox`]；测试构造独立实例。
pub(crate) struct AgentInbox {
    messages: Mutex<VecDeque<AgentInboxMessage>>,
}

impl AgentInbox {
    pub(crate) fn new() -> Self {
        Self {
            messages: Mutex::new(VecDeque::new()),
        }
    }

    /// 投递一条消息（超限挤最老）。
    pub(crate) fn push(&self, message: AgentInboxMessage) {
        let Ok(mut messages) = self.messages.lock() else {
            return;
        };
        if messages.len() >= AGENT_INBOX_CAPACITY {
            messages.pop_front();
        }
        messages.push_back(message);
    }

    /// 收件箱快照（按时间升序）。生产读取方是 P1c 的 Tauri 命令，P1a 只有
    /// 测试消费——照 lib.rs DRAG_CONTROL_FAILED 的 cfg_attr 惯例压制该构建
    /// 下的 dead_code。
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn snapshot(&self) -> Vec<AgentInboxMessage> {
        self.messages
            .lock()
            .map(|messages| messages.iter().cloned().collect())
            .unwrap_or_default()
    }
}

/// 生产全局收件箱（OnceLock 单例）。测试不要用它（构造独立实例）。
pub(crate) fn global_agent_inbox() -> Arc<AgentInbox> {
    static INBOX: OnceLock<Arc<AgentInbox>> = OnceLock::new();
    INBOX.get_or_init(|| Arc::new(AgentInbox::new())).clone()
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
/// 结果写一条 out 审计（ok / err / timeout）。P1c 前仅供测试使用。
#[allow(clippy::too_many_arguments)]
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
    audit: &AgentAuditLog,
) -> Result<AgentResponse, String> {
    let started = Instant::now();
    let request_id = new_request_id();
    let params = payload_summary(&payload);
    // 审计需区分「等不到响应」(timeout) 与其它失败 (err)：标签块内各失败
    // 路径 break 出错误，超时路径额外置位。
    let mut timed_out = false;
    let outcome: Result<AgentResponse, String> = 'send: {
        if layout.cluster_id.trim().is_empty() || layout.pair_secret.trim().is_empty() {
            break 'send Err("当前设备尚未完成配对，无法使用 Agent 通道。".into());
        }

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
        let encoded = match crate::encode_wire_packet(&packet) {
            Ok(bytes) => bytes,
            Err(error) => {
                global_pending_table().cancel(&request_id);
                break 'send Err(error);
            }
        };

        // 流发送的 ack 只说明对端 handler 收下了包（≤6s）；结果本身等 response。
        if let Err(error) = transport.send_stream_expect_ack(peer.clone(), encoded) {
            global_pending_table().cancel(&request_id);
            break 'send Err(format!("agent 请求发送失败: {error}"));
        }

        match rx.recv_timeout(timeout + Duration::from_secs(1)) {
            Ok(result) => result,
            Err(_) => {
                global_pending_table().cancel(&request_id);
                timed_out = true;
                Err(format!("agent 请求超时（{timeout:?}）: 未收到响应"))
            }
        }
    };

    let (status, summary) = match &outcome {
        Ok(_) => ("ok", params),
        Err(message) if timed_out => ("timeout", format!("{message}；参数 {params}")),
        Err(message) => ("err", format!("{message}；参数 {params}")),
    };
    audit.record(AgentAuditEntry::new(
        "out",
        target_device_id,
        capability,
        &request_id,
        status,
        elapsed_ms(started),
        &summary,
    ));
    outcome
}

/// capability 处理器的执行上下文：一次 request 执行所需的本机信息。
/// 由 request 分支在异步线程内按请求构造（借用线程自有的 layout 快照）；
/// 收件箱以注入方式提供——生产为全局实例，测试注入独立实例防串线。
pub(crate) struct AgentCapabilityContext<'a> {
    /// on_stream 收包时刻的 layout 快照（与其它 stream 分支同一份快照语义）。
    pub layout: &'a crate::LayoutState,
    /// 本机当前 device id（`local_peer_from_layout(..).id`）。
    pub local_peer_id: &'a str,
    /// msg.send 的落点。
    pub inbox: &'a AgentInbox,
    /// P1b：file.stat/file.read 的默认可读根（config.file_roots 为空时兜底）。
    /// 生产 = [本机日志目录, 文件接收根]，见 [`AgentChannelRuntime::file_roots`]；
    /// 能力线程没有 AppHandle（Tauri 路径解析无法进测试），故照收件箱先例注入。
    pub default_file_roots: &'a [PathBuf],
    /// P1b：本机日志目录（log.read 在其中定位当前日志文件的唯一真相源，
    /// 语义同 lib.rs write_own_log_tail）；None = 日志能力不可用。
    pub log_dir: Option<&'a Path>,
}

/// capability 分发函数：按 packet.capability 执行并返回 response payload。
/// 以注入方式持有（而非写死调用），测试可替换为带延迟/计数的处理器来锁定
/// 执行模型的时序性质，生产恒为 [`dispatch_agent_capability`]。
pub(crate) type AgentCapabilityHandler = Arc<
    dyn Fn(&AgentCapabilityContext<'_>, &AgentChannelPacket) -> Result<Value, String> + Send + Sync,
>;

/// on_stream agent 分支的运行期依赖（RFC §6.3）。生产由 start_quic_transport
/// 构造一次、被 on_stream 闭包捕获（[`AgentChannelRuntime::production`]）；
/// 测试按用例构造独立实例，避免进程级全局状态串线。
#[derive(Clone)]
pub(crate) struct AgentChannelRuntime {
    /// response 回发用的 transport 槽位：on_stream 闭包先于 transport 创建，
    /// 只能捕获槽位，transport 就绪后由 start_quic_transport 填充。
    pub transport_slot: Arc<Mutex<Option<crate::quic_transport::TransportHandle>>>,
    pub dispatch: AgentCapabilityHandler,
    pub inbox: Arc<AgentInbox>,
    /// 审计落点（RFC §5.3 对策二）。
    pub audit: Arc<AgentAuditLog>,
    /// P1b：file.stat/file.read 的默认可读根（config.file_roots 为空时兜底）。
    /// 生产由 lib.rs start_quic_transport 解析 [app_log_dir, file_transfer_receive_root]
    /// 注入（能力线程无 AppHandle）；Arc 便于闭包捕获与线程转移。
    pub file_roots: Arc<Vec<PathBuf>>,
    /// P1b：本机日志目录（log.read 定位当前日志文件的唯一真相源目录）。
    /// 生产 = `app.path().app_log_dir()`；None/测试不注入时 log.read 返回 err。
    pub log_dir: Option<PathBuf>,
}

impl AgentChannelRuntime {
    /// 生产构造：capability 分发固定为 [`dispatch_agent_capability`]，
    /// 收件箱/审计为进程全局实例（审计落盘路径由 setup 的
    /// init_global_audit_log 注入）。
    pub(crate) fn production(
        transport_slot: Arc<Mutex<Option<crate::quic_transport::TransportHandle>>>,
    ) -> Self {
        Self {
            transport_slot,
            dispatch: Arc::new(dispatch_agent_capability),
            inbox: global_agent_inbox(),
            audit: global_audit_log(),
            file_roots: Arc::new(Vec::new()),
            log_dir: None,
        }
    }
}

/// on_stream 的 agent 分支入口（lib.rs 调度链在 pairing 之后、drag 之前调用）。
/// 返回 true = 该包属于 agent 协议且已处理；false = 交回调度链继续尝试
/// （decode 失败/协议不符/鉴权失败——与 clipboard 分支相同的静默拒绝策略）。
/// 鉴权失败对外保持静默（return false → QUIC reject），但在本机审计留痕
/// （RFC §5.3：只记来源地址，绝不写入凭据）。
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

    let kind = packet.kind.as_str();
    if !matches!(
        kind,
        AGENT_KIND_REQUEST | AGENT_KIND_RESPONSE | AGENT_KIND_CANCEL | AGENT_KIND_NOTIFY
    ) {
        // 未知 kind：拒绝（reject ack），为未来版本保留向前兼容语义——
        // 旧版对任何它不认识的 kind 也是同样反应。
        return false;
    }
    if !agent_packet_authorized(layout, &packet) {
        audit_denied(
            runtime,
            &packet,
            source,
            &format!("鉴权失败（kind={kind}）"),
        );
        return false;
    }

    match kind {
        AGENT_KIND_REQUEST => handle_agent_request(&packet, layout, local_peer_id, runtime, source),
        AGENT_KIND_RESPONSE => handle_agent_response(&packet),
        AGENT_KIND_CANCEL => {
            if global_pending_table().cancel(&packet.request_id) {
                log::info!(
                    "agent: 请求 {} 已被对端 {} 取消",
                    packet.request_id,
                    packet.origin_id
                );
            }
            true
        }
        _ => {
            log::info!(
                "agent: 收到来自 {} 的通知: {}",
                packet.origin_id,
                packet.payload
            );
            true
        }
    }
}

/// 记一条 (in, denied)：summary 只含原因 + 来源地址——不碰包内凭据字段。
fn audit_denied(
    runtime: &AgentChannelRuntime,
    packet: &AgentChannelPacket,
    source: SocketAddr,
    reason: &str,
) {
    runtime.audit.record(AgentAuditEntry::new(
        "in",
        &packet.origin_id,
        &packet.capability,
        &packet.request_id,
        "denied",
        0,
        &format!("{reason}；来源 {source}"),
    ));
}

/// 处理已鉴权的 request：路由 → level gate → 立即接受（返回 true，流 ack
/// 即 accepted），capability 分发 + response 回发在独立线程执行。
///
/// 执行模型（RFC §6.3，P0 验收结论）：on_stream 回调内同步执行会占住 QUIC
/// stream slot（并发上限 8）最长 6s，与文件传输/剪贴板互饿。本分支照 lib.rs
/// handle_peer_request_packet 的 Log/Clipboard 先例：先在回调内完成廉价的
/// 路由/gate 检查，之后 `std::thread::spawn` 做真正的工作。
/// 路由/gate 失败照抄 clipboard 分支策略：静默 return false（对端收到 QUIC 层
/// reject ack，不回 agent 层错误包，避免给对端任何确认信息）；gate 拒绝本机
/// 审计记 denied。
fn handle_agent_request(
    packet: &AgentChannelPacket,
    layout: &crate::LayoutState,
    local_peer_id: &str,
    runtime: &AgentChannelRuntime,
    source: SocketAddr,
) -> bool {
    if !agent_packet_targets_local(layout, packet, local_peer_id) {
        return false;
    }
    // 消费点再钳一次（照 focus_halo「存储钳制 + 消费点 sanitized」）：即便
    // layout 快照绕过了载入/保存钳制，非法 level 也按 readonly 判定。
    let config = layout.agent_channel.sanitized();
    if !config.enabled {
        audit_denied(runtime, packet, source, "agent 通道已关闭");
        return false;
    }
    if !config.capability_allowed(&packet.capability) {
        audit_denied(
            runtime,
            packet,
            source,
            &format!("级别 {} 不允许该能力", config.level),
        );
        return false;
    }

    // spawn 前把回调依赖克隆进线程（Arc clone 是廉价引用计数）；
    // layout 快照也随线程带走，回调线程不再持有任何本分支借用。
    let runtime = runtime.clone();
    let packet = packet.clone();
    let layout = layout.clone();
    let local_peer_id = local_peer_id.to_string();
    std::thread::spawn(move || {
        let started = Instant::now();
        let context = AgentCapabilityContext {
            layout: &layout,
            local_peer_id: &local_peer_id,
            inbox: &runtime.inbox,
            default_file_roots: runtime.file_roots.as_slice(),
            log_dir: runtime.log_dir.as_deref(),
        };
        let (status, payload, summary) = match (runtime.dispatch)(&context, &packet) {
            Ok(result) => ("ok", result, payload_summary(&packet.payload)),
            Err(error) => (
                "err",
                serde_json::json!({ "error": error }),
                format!("{error}；参数 {}", payload_summary(&packet.payload)),
            ),
        };
        runtime.audit.record(AgentAuditEntry::new(
            "in",
            &packet.origin_id,
            &packet.capability,
            &packet.request_id,
            status,
            elapsed_ms(started),
            &summary,
        ));
        send_agent_response(
            &runtime,
            &packet,
            status,
            payload,
            &layout,
            &local_peer_id,
            source,
        );
    });
    true
}

/// 异步线程内回发 response（两段式反向新建流），结果记一条 out 审计。
/// 失败只记 warn + 审计 err：请求已被消费，由发起方超时兜底重试。
fn send_agent_response(
    runtime: &AgentChannelRuntime,
    packet: &AgentChannelPacket,
    status: &str,
    payload: Value,
    layout: &crate::LayoutState,
    local_peer_id: &str,
    source: SocketAddr,
) {
    let started = Instant::now();
    let result = deliver_agent_response(
        runtime,
        packet,
        status,
        payload,
        layout,
        local_peer_id,
        source,
    );
    let (audit_status, summary) = match &result {
        Ok(()) => ("ok", format!("response({status}) 已回发至 {source}")),
        Err(error) => {
            log::warn!("agent: 响应 {} 回发失败: {error}", packet.request_id);
            ("err", format!("response({status}) 回发失败：{error}"))
        }
    };
    runtime.audit.record(AgentAuditEntry::new(
        "out",
        &packet.origin_id,
        &packet.capability,
        &packet.request_id,
        audit_status,
        elapsed_ms(started),
        &summary,
    ));
}

fn deliver_agent_response(
    runtime: &AgentChannelRuntime,
    packet: &AgentChannelPacket,
    status: &str,
    payload: Value,
    layout: &crate::LayoutState,
    local_peer_id: &str,
    source: SocketAddr,
) -> Result<(), String> {
    // transport 槽位尚空（启动竞态窗口）：请求已消费但响应无法回发。
    let transport = runtime
        .transport_slot
        .lock()
        .ok()
        .and_then(|slot| slot.clone())
        .ok_or_else(|| "QUIC transport 尚未就绪".to_string())?;

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
    let encoded = crate::encode_wire_packet(&response)?;
    // 反向新建流（两段式）：目标地址就是 request 的来源；证书钉扎用请求方
    // 自报的 transport 公钥。响应包带齐鉴权三件套（RFC §8 对策四：双向都不裸信任）。
    let peer = transport.peer(
        source.to_string(),
        packet.origin_transport_public_key.clone(),
        crate::quic_transport::PROTOCOL_VERSION,
    );
    transport.send_stream_expect_ack(peer, encoded)
}

/// 处理已鉴权的 response：按 request_id 完成 pending 等待者。
/// 查无此 id（超时回收/重放/重复响应）→ 拒绝（RFC §8 对策四）。
fn handle_agent_response(packet: &AgentChannelPacket) -> bool {
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

/// 审计用参数摘要：payload 的紧凑 JSON（AgentAuditEntry::new 负责 200 字封顶）。
fn payload_summary(payload: &Value) -> String {
    serde_json::to_string(payload).unwrap_or_default()
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// 生产 capability 分发（P1a：echo + status.get + msg.send；P1b：file.stat/
/// file.read/log.read）。
fn dispatch_agent_capability(
    context: &AgentCapabilityContext<'_>,
    packet: &AgentChannelPacket,
) -> Result<Value, String> {
    match packet.capability.as_str() {
        AGENT_CAPABILITY_ECHO => Ok(packet.payload.clone()),
        AGENT_CAPABILITY_STATUS_GET => Ok(agent_status_payload(context)),
        AGENT_CAPABILITY_MSG_SEND => agent_handle_msg_send(context, packet),
        AGENT_CAPABILITY_FILE_STAT => agent_handle_file_stat(context, packet),
        AGENT_CAPABILITY_FILE_READ => agent_handle_file_read(context, packet),
        AGENT_CAPABILITY_LOG_READ => agent_handle_log_read(context, packet),
        other => Err(format!("未支持的 agent capability: {other}")),
    }
}

/// file.stat / file.read 共用的可读根与路径解析：配置根（sanitized）非空用配置，
/// 否则回落注入的默认根；请求经 [`resolve_agent_readable_path`] 限根。
fn resolve_capability_path(
    context: &AgentCapabilityContext<'_>,
    requested: &str,
) -> Result<PathBuf, String> {
    let roots = crate::agent_file_access::agent_file_roots(
        &context.layout.agent_channel.sanitized(),
        context.default_file_roots,
    );
    crate::agent_file_access::resolve_agent_readable_path(&roots, requested)
}

/// file.stat：payload { path } → 安全层解析 → 文件/目录元信息（目录含按名称
/// 排序的 entries，封顶 [`AGENT_STAT_ENTRIES_LIMIT`] 条并标注截断）。
/// 根内不存在的路径返回 exists=false 零值而非 err（调用方「先 stat 再 read」
/// 的模式需要）；目录元数据读取失败 → err。
fn agent_handle_file_stat(
    context: &AgentCapabilityContext<'_>,
    packet: &AgentChannelPacket,
) -> Result<Value, String> {
    let requested = packet
        .payload
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| "file.stat 需要字符串字段 path".to_string())?;
    let path = resolve_capability_path(context, requested)?;
    let metadata = match std::fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(missing_stat_payload());
        }
        Err(error) => {
            return Err(format!(
                "file.stat 读取元数据失败（{}）: {error}",
                path.display()
            ));
        }
    };

    let is_dir = metadata.is_dir();
    let (entries, entries_truncated) = if is_dir {
        read_stat_entries(&path)?
    } else {
        (Vec::new(), false)
    };
    Ok(serde_json::json!({
        "exists": true,
        "isFile": metadata.is_file(),
        "isDir": is_dir,
        "sizeBytes": metadata.len(),
        "modifiedMs": modified_ms(&metadata),
        "entries": entries,
        "entriesTruncated": entries_truncated,
    }))
}

/// file.stat 对不存在路径的响应体（其余字段零值，不算错误）。
fn missing_stat_payload() -> Value {
    serde_json::json!({
        "exists": false,
        "isFile": false,
        "isDir": false,
        "sizeBytes": 0,
        "modifiedMs": 0,
        "entries": [],
        "entriesTruncated": false,
    })
}

/// file.stat 的目录列举上限（任务书规格：最多 200 条）。
const AGENT_STAT_ENTRIES_LIMIT: usize = 200;

/// 列举目录条目：按名称排序，封顶 [`AGENT_STAT_ENTRIES_LIMIT`] 条并标注截断。
/// read_dir 本身失败（权限等）→ err；单个条目取元数据失败按零值处理（尽力列举）。
fn read_stat_entries(path: &Path) -> Result<(Vec<Value>, bool), String> {
    let mut entries: Vec<(String, bool, u64)> = std::fs::read_dir(path)
        .map_err(|error| format!("file.stat 列举目录失败（{}）: {error}", path.display()))?
        .filter_map(|entry| entry.ok())
        .map(|entry| {
            let metadata = entry.metadata().ok();
            (
                entry.file_name().to_string_lossy().into_owned(),
                metadata.as_ref().is_some_and(|metadata| metadata.is_dir()),
                metadata.map(|metadata| metadata.len()).unwrap_or(0),
            )
        })
        .collect();
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    let truncated = entries.len() > AGENT_STAT_ENTRIES_LIMIT;
    entries.truncate(AGENT_STAT_ENTRIES_LIMIT);
    let payload = entries
        .into_iter()
        .map(|(name, is_dir, size_bytes)| {
            serde_json::json!({ "name": name, "isDir": is_dir, "sizeBytes": size_bytes })
        })
        .collect();
    Ok((payload, truncated))
}

/// mtime → UNIX 毫秒（早于纪元/时钟异常时 0，不因时间字段让整个 stat 失败）。
fn modified_ms(metadata: &std::fs::Metadata) -> u64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}

/// file.read：payload { path, maxBytes? } → 安全层解析 → 内联读取文件头部，
/// 一律 base64（不猜文本编码）。默认读 [`AGENT_FILE_READ_DEFAULT_BYTES`]，
/// maxBytes 超过 [`AGENT_FILE_READ_MAX_BYTES`] 时**钳制**（不拒绝）；文件大于
/// 读取上限时截断尾部——决策：读**头部**（truncate 尾部），日志类「看最新」
/// 场景应使用 log.read；offsetRead 字段名明确这一语义。目录/不存在 → err
/// （exists 语义属于 file.stat，调用方按「先 stat 再 read」协作）。
fn agent_handle_file_read(
    context: &AgentCapabilityContext<'_>,
    packet: &AgentChannelPacket,
) -> Result<Value, String> {
    let requested = packet
        .payload
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| "file.read 需要字符串字段 path".to_string())?;
    let max_bytes = match packet.payload.get("maxBytes") {
        None | Some(Value::Null) => AGENT_FILE_READ_DEFAULT_BYTES,
        Some(Value::Number(number)) => {
            let requested_bytes = number
                .as_u64()
                .ok_or_else(|| "file.read 的 maxBytes 必须是非负整数".to_string())?;
            requested_bytes.min(AGENT_FILE_READ_MAX_BYTES)
        }
        Some(_) => return Err("file.read 的 maxBytes 必须是数字".into()),
    };

    let path = resolve_capability_path(context, requested)?;
    let metadata = std::fs::metadata(&path).map_err(|error| {
        format!(
            "file.read 目标不存在或不可访问（{}）: {error}",
            path.display()
        )
    })?;
    if metadata.is_dir() {
        return Err(format!(
            "file.read 的目标是目录（{}），只支持读文件",
            path.display()
        ));
    }
    // 判定过的类型也可能在 stat 与 open 之间变化（TOCTOU）：读到的按实际内容算。
    let mut file = std::fs::File::open(&path)
        .map_err(|error| format!("file.read 打开失败（{}）: {error}", path.display()))?;
    let mut content = Vec::with_capacity(max_bytes.min(metadata.len()) as usize);
    let mut chunk = [0u8; 64 * 1024];
    let mut read_bytes = 0u64;
    loop {
        use std::io::Read as _;
        let read = file
            .read(&mut chunk)
            .map_err(|error| format!("file.read 读取失败（{}）: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        let take = read.min((max_bytes - read_bytes) as usize);
        content.extend_from_slice(&chunk[..take]);
        read_bytes += take as u64;
        if read_bytes >= max_bytes {
            break;
        }
    }

    use base64::Engine as _;
    Ok(serde_json::json!({
        "contentBase64": base64::engine::general_purpose::STANDARD.encode(&content),
        "sizeBytes": metadata.len(),
        "readBytes": read_bytes,
        "truncated": metadata.len() > read_bytes,
        "offsetRead": 0,
    }))
}

/// log.read：payload { tailBytes? } → 本机日志尾部。日志文件定位唯一真相源 =
/// lib.rs `write_own_log_tail`（10547-10571）的语义：日志目录内 mtime 最新的
/// .log 文件（tauri-plugin-log 轮转 KeepSome(5) 会换文件，不能写死文件名），
/// 复用 lib.rs 提取的 [`crate::newest_log_file_in`]；尾部字节复用
/// [`crate::tail_of_file`]。tailBytes 默认 [`AGENT_LOG_READ_DEFAULT_BYTES`]、
/// 超过 [`AGENT_LOG_READ_MAX_BYTES`] 钳制。尾部切掉第一个不完整行并标注
/// truncatedFromHead；文本 UTF-8 lossy 解码（日志本身是 UTF-8，损坏字节→
/// U+FFFD，不让一个坏字节让整个读取失败）。
fn agent_handle_log_read(
    context: &AgentCapabilityContext<'_>,
    packet: &AgentChannelPacket,
) -> Result<Value, String> {
    let tail_bytes = match packet.payload.get("tailBytes") {
        None | Some(Value::Null) => AGENT_LOG_READ_DEFAULT_BYTES,
        Some(Value::Number(number)) => {
            let requested_bytes = number
                .as_u64()
                .ok_or_else(|| "log.read 的 tailBytes 必须是非负整数".to_string())?;
            requested_bytes.min(AGENT_LOG_READ_MAX_BYTES)
        }
        Some(_) => return Err("log.read 的 tailBytes 必须是数字".into()),
    };
    let log_dir = context
        .log_dir
        .ok_or_else(|| "本机日志目录不可用（未注入），无法读取日志".to_string())?;
    let path = crate::newest_log_file_in(log_dir).map_err(|error| {
        // 目录不可读与「无 .log 文件」（首次运行尚未产生日志）各自带出原因。
        format!("log.read 定位日志失败: {error}")
    })?;
    let tail = crate::tail_of_file(&path, tail_bytes)
        .map_err(|error| format!("log.read 读取日志失败（{}）: {error}", path.display()))?;
    let size_bytes = std::fs::metadata(&path)
        .map(|metadata| metadata.len())
        .map_err(|error| format!("log.read 读取日志元数据失败: {error}"))?;

    // 只有读取确实截掉了头部（文件大于本次 tail 读取）时，tail 的第一段才可能是
    // 残行：丢弃到第一个换行为止（残行不完整，不能当日志给对端）。文件小于 tail
    // 读取时拿到的就是全文件，第一个换行是第一行的行尾，原样解码。
    let head_cut = u64::try_from(tail.len()).unwrap_or(u64::MAX) < size_bytes;
    let text = if head_cut {
        match tail.iter().position(|byte| *byte == b'\n') {
            Some(first_newline) => String::from_utf8_lossy(&tail[first_newline + 1..]).into_owned(),
            // 一次读取内没有任何完整行：整体视为残行丢弃。
            _ => String::new(),
        }
    } else {
        String::from_utf8_lossy(&tail).into_owned()
    };
    Ok(serde_json::json!({
        "text": text,
        "path": path.to_string_lossy(),
        "sizeBytes": size_bytes,
        "truncatedFromHead": head_cut,
    }))
}

/// status.get 的响应体（RFC §6.2 L0）：本机身份/配置快照。全部来源只读：
/// layout 快照 + crate::hostname()/current_platform() + 与 LanPeer 宣告同源的
/// APP_VERSION（setup 写入，未写入时兜底 CARGO_PKG_VERSION）。
/// agent 级别汇报 sanitized 后的**有效**级别（与 gate 判定一致）。
fn agent_status_payload(context: &AgentCapabilityContext<'_>) -> Value {
    let layout = context.layout;
    let agent = layout.agent_channel.sanitized();
    serde_json::json!({
        "machineRole": layout.machine_role,
        "inputMode": layout.input_mode,
        "agentEnabled": agent.enabled,
        "agentLevel": agent.level,
        "platform": crate::current_platform(),
        "hostname": crate::hostname().unwrap_or_default(),
        "deviceId": context.local_peer_id,
        "appVersion": crate::APP_VERSION
            .get()
            .cloned()
            .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string()),
        "quicPort": crate::normalize_quic_port(layout.transport_port, layout.quic_port),
    })
}

/// msg.send：payload { text, title? } → 写入收件箱 + log::info → ok。
/// text 超 4KiB 截断并标注 truncated；text 缺失/为空/非字符串、title 非字符串 → err。
fn agent_handle_msg_send(
    context: &AgentCapabilityContext<'_>,
    packet: &AgentChannelPacket,
) -> Result<Value, String> {
    let text = packet
        .payload
        .get("text")
        .and_then(Value::as_str)
        .ok_or_else(|| "msg.send 需要字符串字段 text".to_string())?;
    if text.trim().is_empty() {
        return Err("msg.send 的 text 不能为空".into());
    }
    let title = match packet.payload.get("title") {
        None | Some(Value::Null) => None,
        Some(Value::String(title)) => Some(title.clone()),
        Some(_) => return Err("msg.send 的 title 必须是字符串".into()),
    };
    let (text, truncated) = truncate_utf8_bytes(text, AGENT_MSG_TEXT_MAX_BYTES);

    log::info!(
        "agent: 收到来自 {} 的消息{}（{} 字节{}）",
        packet.origin_id,
        title
            .as_deref()
            .map(|title| format!("「{title}」"))
            .unwrap_or_default(),
        text.len(),
        if truncated { "，已截断" } else { "" }
    );
    context.inbox.push(AgentInboxMessage {
        ts_ms: crate::now_ms(),
        peer_id: packet.origin_id.clone(),
        title,
        text,
        truncated,
    });
    Ok(serde_json::json!({ "delivered": true, "truncated": truncated }))
}

/// 按 UTF-8 字节上限截断，切点回退到最近的字符边界（不切出半个字符）。
/// 返回（截断后文本, 是否发生截断）。
fn truncate_utf8_bytes(text: &str, max_bytes: usize) -> (String, bool) {
    if text.len() <= max_bytes {
        return (text.to_string(), false);
    }
    let mut cut = max_bytes;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    (text[..cut].to_string(), true)
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
            &AgentAuditLog::in_memory(),
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

    /// 构造测试 runtime：capability 分发为注入的闭包（收件箱为独立实例）。
    fn runtime_with_dispatch(
        slot: Arc<Mutex<Option<quic_transport::TransportHandle>>>,
        dispatch: impl Fn(&AgentCapabilityContext<'_>, &AgentChannelPacket) -> Result<Value, String>
            + Send
            + Sync
            + 'static,
    ) -> AgentChannelRuntime {
        AgentChannelRuntime {
            transport_slot: slot,
            dispatch: Arc::new(move |context, packet| dispatch(context, packet)),
            inbox: Arc::new(AgentInbox::new()),
            audit: Arc::new(AgentAuditLog::in_memory()),
            file_roots: Arc::new(Vec::new()),
            log_dir: None,
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
        let runtime = runtime_with_dispatch(slot, |_context, _packet| {
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
        let runtime = runtime_with_dispatch(slot, move |_context, _packet| {
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

    // ---- i. 首批 L0 能力：status.get / msg.send（P1a）----

    /// 构造一个 capability 请求包（鉴权字段齐全）。
    fn capability_packet(capability: &str, payload: Value) -> AgentChannelPacket {
        let mut packet = authorized_packet("controller-1", "peer-key-1");
        packet.capability = capability.into();
        packet.payload = payload;
        packet
    }

    /// 空注入的 capability 上下文（本机 id 固定、无默认根/日志目录）。
    /// 需要文件能力的用例改用 [`file_capability_context`] 注入临时目录。
    fn bare_capability_context<'a>(
        layout: &'a LayoutState,
        inbox: &'a AgentInbox,
    ) -> AgentCapabilityContext<'a> {
        AgentCapabilityContext {
            layout,
            local_peer_id: "peer-local-1",
            inbox,
            default_file_roots: &[],
            log_dir: None,
        }
    }

    /// P1b 文件能力用例的共享状态：layout + 收件箱 + 可读根/日志目录注入。
    /// 根与日志目录都指向沙箱（tempfile 自动清理），绝不依赖真实用户目录。
    struct FileCapabilityFixture {
        layout: LayoutState,
        inbox: AgentInbox,
        _guard: tempfile::TempDir,
        /// 测试根（沙箱内 root/）。
        root: PathBuf,
    }

    impl FileCapabilityFixture {
        fn new() -> Self {
            let guard = tempfile::tempdir().expect("创建临时目录");
            let base = guard.path().to_path_buf();
            let root = base.join("root");
            std::fs::create_dir_all(&root).expect("建测试根");
            let mut layout = test_agent_layout("server", Vec::new());
            layout.agent_channel.file_roots = vec![root.to_string_lossy().into_owned()];
            Self {
                layout,
                inbox: AgentInbox::new(),
                _guard: guard,
                root,
            }
        }

        fn context(&self) -> AgentCapabilityContext<'_> {
            AgentCapabilityContext {
                layout: &self.layout,
                local_peer_id: "peer-local-1",
                inbox: &self.inbox,
                default_file_roots: &[],
                log_dir: None,
            }
        }
    }

    #[test]
    fn status_get_reports_local_identity_and_agent_config() {
        let mut layout = test_agent_layout("client", Vec::new());
        layout.input_mode = "receive".into();
        layout.transport_port = 47833;
        layout.quic_port = 47834;
        let inbox = AgentInbox::new();
        let context = bare_capability_context(&layout, &inbox);

        let payload = dispatch_agent_capability(
            &context,
            &capability_packet(AGENT_CAPABILITY_STATUS_GET, Value::Null),
        )
        .expect("status.get 必须成功");

        assert_eq!(payload["machineRole"], "client");
        assert_eq!(payload["inputMode"], "receive");
        assert_eq!(payload["agentEnabled"], true);
        assert_eq!(payload["agentLevel"], "readonly");
        assert_eq!(payload["platform"], crate::current_platform());
        assert_eq!(
            payload["hostname"],
            crate::hostname().unwrap_or_default(),
            "hostname 必须来自 crate::hostname()"
        );
        assert_eq!(
            payload["deviceId"], "peer-local-1",
            "deviceId 必须是本机当前 id"
        );
        assert_eq!(
            payload["appVersion"],
            crate::APP_VERSION
                .get()
                .cloned()
                .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string()),
            "appVersion 必须与 LanPeer 宣告同源（APP_VERSION 兜底 CARGO_PKG_VERSION）"
        );
        assert_eq!(payload["quicPort"], 47834);
        assert!(
            !payload.to_string().contains(TEST_SECRET)
                && !payload.to_string().contains(TEST_CLUSTER),
            "status.get 不得泄漏配对凭据"
        );
    }

    #[test]
    fn status_get_reports_effective_level_for_invalid_config() {
        // 边界：layout 快照里若残留非法 level（钳制前的旧快照），status 汇报的
        // 必须是 gate 实际执行的有效级别 readonly，而不是原样回显非法值。
        let mut layout = test_agent_layout("server", Vec::new());
        layout.agent_channel.level = "root".into();
        layout.agent_channel.enabled = false;
        let inbox = AgentInbox::new();
        let context = bare_capability_context(&layout, &inbox);
        let payload = dispatch_agent_capability(
            &context,
            &capability_packet(AGENT_CAPABILITY_STATUS_GET, Value::Null),
        )
        .expect("status.get 必须成功");
        assert_eq!(payload["agentLevel"], "readonly");
        assert_eq!(payload["agentEnabled"], false);
    }

    #[test]
    fn msg_send_stores_message_in_inbox_and_acks() {
        let layout = test_agent_layout("server", Vec::new());
        let inbox = AgentInbox::new();
        let context = bare_capability_context(&layout, &inbox);

        let ack = dispatch_agent_capability(
            &context,
            &capability_packet(
                AGENT_CAPABILITY_MSG_SEND,
                serde_json::json!({ "text": "构建完成，请看日志", "title": "CI" }),
            ),
        )
        .expect("msg.send 必须成功");

        assert_eq!(ack["delivered"], true);
        assert_eq!(ack["truncated"], false);
        let messages = inbox.snapshot();
        assert_eq!(messages.len(), 1, "消息必须进入收件箱");
        assert_eq!(
            messages[0].peer_id, "controller-1",
            "peer_id 取 request 的 origin_id"
        );
        assert_eq!(messages[0].title.as_deref(), Some("CI"));
        assert_eq!(messages[0].text, "构建完成，请看日志");
        assert!(!messages[0].truncated);

        // title 可省略。
        dispatch_agent_capability(
            &context,
            &capability_packet(
                AGENT_CAPABILITY_MSG_SEND,
                serde_json::json!({ "text": "无标题" }),
            ),
        )
        .expect("缺省 title 必须成功");
        assert_eq!(inbox.snapshot()[1].title, None);
    }

    #[test]
    fn msg_send_rejects_empty_missing_or_non_string_text() {
        let layout = test_agent_layout("server", Vec::new());
        let inbox = AgentInbox::new();
        let context = bare_capability_context(&layout, &inbox);
        for payload in [
            serde_json::json!({ "text": "" }),
            serde_json::json!({ "text": "   " }),
            serde_json::json!({}),
            serde_json::json!({ "text": 42 }),
            serde_json::json!({ "text": "ok", "title": 7 }),
            Value::Null,
        ] {
            let result = dispatch_agent_capability(
                &context,
                &capability_packet(AGENT_CAPABILITY_MSG_SEND, payload.clone()),
            );
            assert!(result.is_err(), "非法 payload {payload} 必须返回 err");
        }
        assert!(inbox.snapshot().is_empty(), "被拒绝的消息不得进入收件箱");
    }

    #[test]
    fn msg_send_truncates_text_over_4kib_on_char_boundary() {
        let layout = test_agent_layout("server", Vec::new());
        let inbox = AgentInbox::new();
        let context = bare_capability_context(&layout, &inbox);
        // 3 字节汉字 × 2000 = 6000 字节 > 4096：截断必须落在字符边界上。
        let long_text = "汉".repeat(2000);
        let ack = dispatch_agent_capability(
            &context,
            &capability_packet(
                AGENT_CAPABILITY_MSG_SEND,
                serde_json::json!({ "text": long_text }),
            ),
        )
        .expect("超长消息截断后仍须成功");
        assert_eq!(ack["truncated"], true, "超长必须在 ack 里标注 truncated");

        let message = &inbox.snapshot()[0];
        assert!(message.truncated, "收件箱条目必须标注 truncated");
        assert!(
            message.text.len() <= AGENT_MSG_TEXT_MAX_BYTES,
            "截断后不得超过 4KiB（实际 {} 字节）",
            message.text.len()
        );
        assert!(
            message.text.len() > AGENT_MSG_TEXT_MAX_BYTES - 3,
            "截断应尽量保留到 4KiB 边界内最后一个完整字符"
        );
        assert!(
            message.text.chars().all(|character| character == '汉'),
            "不得切出半个字符"
        );

        // 恰好 4096 字节不截断（边界值）。
        let exact = "a".repeat(AGENT_MSG_TEXT_MAX_BYTES);
        let ack = dispatch_agent_capability(
            &context,
            &capability_packet(
                AGENT_CAPABILITY_MSG_SEND,
                serde_json::json!({ "text": exact }),
            ),
        )
        .expect("边界值必须成功");
        assert_eq!(ack["truncated"], false, "恰好 4KiB 不算超长");
    }

    #[test]
    fn inbox_caps_at_100_and_drops_oldest() {
        let inbox = AgentInbox::new();
        for index in 0..AGENT_INBOX_CAPACITY + 3 {
            inbox.push(AgentInboxMessage {
                ts_ms: index as u64,
                peer_id: "peer-a".into(),
                title: None,
                text: format!("msg-{index}"),
                truncated: false,
            });
        }
        let messages = inbox.snapshot();
        assert_eq!(
            messages.len(),
            AGENT_INBOX_CAPACITY,
            "收件箱必须封顶 100 条"
        );
        assert_eq!(messages[0].text, "msg-3", "最老的 3 条被挤出");
        assert_eq!(messages[messages.len() - 1].text, "msg-102");
    }

    // ---- j. level gate 接线 + 审计记录点（P1a，RFC §5.3 对策一/二）----

    /// 测试统一的伪来源地址（审计 summary 必须记录它）。
    const TEST_SOURCE: &str = "10.0.0.9:50000";

    fn test_source() -> SocketAddr {
        TEST_SOURCE.parse().expect("测试来源地址")
    }

    /// 生产分发 + 独立收件箱/审计的 runtime（不碰进程全局实例）。
    fn isolated_production_runtime(
        slot: Arc<Mutex<Option<quic_transport::TransportHandle>>>,
    ) -> AgentChannelRuntime {
        AgentChannelRuntime {
            transport_slot: slot,
            dispatch: Arc::new(dispatch_agent_capability),
            inbox: Arc::new(AgentInbox::new()),
            audit: Arc::new(AgentAuditLog::in_memory()),
            file_roots: Arc::new(Vec::new()),
            log_dir: None,
        }
    }

    /// 记录 capability 是否被执行的 runtime（gate 拒绝时必须为 0）。
    fn counting_runtime() -> (AgentChannelRuntime, Arc<AtomicU64>) {
        let dispatched = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&dispatched);
        let runtime = runtime_with_dispatch(Arc::new(Mutex::new(None)), move |_context, packet| {
            counter.fetch_add(1, Ordering::Relaxed);
            Ok(packet.payload.clone())
        });
        (runtime, dispatched)
    }

    /// 轮询审计直到出现满足条件的条目（异步线程写入），超时返回 false。
    fn wait_for_audit(
        audit: &AgentAuditLog,
        predicate: impl Fn(&AgentAuditEntry) -> bool,
        timeout: Duration,
    ) -> bool {
        let started = Instant::now();
        while started.elapsed() < timeout {
            if audit.snapshot().iter().any(&predicate) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    fn deliver(
        packet: &AgentChannelPacket,
        layout: &LayoutState,
        runtime: &AgentChannelRuntime,
    ) -> bool {
        handle_agent_stream_packet(
            &crate::encode_wire_packet(packet).expect("编码"),
            layout,
            "peer-local",
            runtime,
            test_source(),
        )
    }

    #[test]
    fn readonly_rejects_cmd_run_and_audits_denied() {
        let (runtime, dispatched) = counting_runtime();
        let layout = test_agent_layout("server", Vec::new()); // 默认 readonly
        let packet = capability_packet("cmd.run", serde_json::json!({ "cmd": "whoami" }));

        assert!(
            !deliver(&packet, &layout, &runtime),
            "readonly 下 cmd.run 必须被拒（对外静默：QUIC reject）"
        );
        let entries = runtime.audit.snapshot();
        assert_eq!(entries.len(), 1, "level 拒绝必须恰好留一条审计");
        assert_eq!(entries[0].direction, "in");
        assert_eq!(entries[0].status, "denied");
        assert_eq!(entries[0].capability, "cmd.run");
        assert_eq!(entries[0].peer_id, "controller-1");
        assert!(
            entries[0].summary.contains(TEST_SOURCE),
            "denied 摘要必须含来源地址（实际 {:?}）",
            entries[0].summary
        );
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            dispatched.load(Ordering::Relaxed),
            0,
            "被 gate 拒绝的请求不得进入 capability 执行"
        );
    }

    #[test]
    fn readonly_allows_status_get_and_audits_execution() {
        let runtime = isolated_production_runtime(Arc::new(Mutex::new(None)));
        let layout = test_agent_layout("server", Vec::new());
        let packet = capability_packet(AGENT_CAPABILITY_STATUS_GET, Value::Null);

        assert!(
            deliver(&packet, &layout, &runtime),
            "readonly 必须放行 status.get"
        );
        assert!(
            wait_for_audit(
                &runtime.audit,
                |entry| entry.direction == "in"
                    && entry.status == "ok"
                    && entry.capability == AGENT_CAPABILITY_STATUS_GET
                    && entry.request_id == "1-abcdef"
                    && entry.peer_id == "controller-1",
                Duration::from_secs(5),
            ),
            "执行完成必须记 (in, ok)，实际 {:?}",
            runtime.audit.snapshot()
        );
        // transport 槽位为空：response 无法回发，必须记 (out, err)。
        assert!(
            wait_for_audit(
                &runtime.audit,
                |entry| entry.direction == "out"
                    && entry.status == "err"
                    && entry.capability == AGENT_CAPABILITY_STATUS_GET,
                Duration::from_secs(5),
            ),
            "response 回发失败必须记 (out, err)，实际 {:?}",
            runtime.audit.snapshot()
        );
    }

    #[test]
    fn disabled_channel_rejects_every_capability_and_audits_denied() {
        let (runtime, dispatched) = counting_runtime();
        let mut layout = test_agent_layout("server", Vec::new());
        layout.agent_channel.enabled = false;
        layout.agent_channel.level = "full".into(); // 熔断开关优先于级别

        for capability in [
            AGENT_CAPABILITY_ECHO,
            AGENT_CAPABILITY_STATUS_GET,
            AGENT_CAPABILITY_MSG_SEND,
        ] {
            let packet = capability_packet(capability, serde_json::json!({ "text": "hi" }));
            assert!(
                !deliver(&packet, &layout, &runtime),
                "enabled=false 时 {capability} 必须被拒"
            );
        }
        let entries = runtime.audit.snapshot();
        assert_eq!(entries.len(), 3, "每个被拒请求各留一条审计");
        assert!(
            entries
                .iter()
                .all(|entry| entry.direction == "in" && entry.status == "denied"),
            "全部记 (in, denied)：{entries:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            dispatched.load(Ordering::Relaxed),
            0,
            "熔断后不得执行任何能力"
        );
    }

    #[test]
    fn auth_failure_is_audited_with_source_but_no_credentials() {
        let dir = std::env::temp_dir().join(format!(
            "mykvm-audit-auth-{}-{}",
            std::process::id(),
            crate::random_hex(4)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        let audit_path = dir.join("agent-audit.jsonl");

        let (mut runtime, dispatched) = counting_runtime();
        runtime.audit = Arc::new(AgentAuditLog::with_rotation_threshold(
            Some(audit_path.clone()),
            AGENT_AUDIT_MAX_FILE_BYTES,
        ));
        let layout = test_agent_layout(
            "client",
            vec![test_controller("controller-1", "peer-key-1")],
        );
        let forged_secret = "forged-secret-value-123";
        let mut packet = authorized_packet("controller-1", "peer-key-1");
        packet.pair_secret = forged_secret.into();

        assert!(
            !deliver(&packet, &layout, &runtime),
            "鉴权失败对外必须保持静默拒绝（return false → QUIC reject）"
        );
        let entries = runtime.audit.snapshot();
        assert_eq!(entries.len(), 1, "鉴权失败必须在本机留痕");
        assert_eq!(entries[0].direction, "in");
        assert_eq!(entries[0].status, "denied");
        assert!(
            entries[0].summary.contains(TEST_SOURCE),
            "鉴权失败摘要必须记来源地址（实际 {:?}）",
            entries[0].summary
        );

        // 内存条目 + 落盘文件：任何凭据值（本机真值、对端伪造值、cluster）都不得出现。
        let mut serialized: String = entries
            .iter()
            .map(|entry| serde_json::to_string(entry).expect("序列化"))
            .collect();
        serialized.push_str(&std::fs::read_to_string(&audit_path).expect("审计必须落盘"));
        for secret in [TEST_SECRET, TEST_CLUSTER, forged_secret] {
            assert!(
                !serialized.contains(secret),
                "审计文本不得含凭据值 {secret:?}：{serialized}"
            );
        }
        assert_eq!(dispatched.load(Ordering::Relaxed), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn forged_response_failing_auth_is_audited() {
        let (runtime, _dispatched) = counting_runtime();
        let layout = test_agent_layout(
            "client",
            vec![test_controller("controller-1", "peer-key-1")],
        );
        let mut packet = authorized_packet("controller-1", "peer-key-1");
        packet.kind = AGENT_KIND_RESPONSE.into();
        packet.status = Some("ok".into());
        packet.cluster_id = "cluster-forged".into();

        assert!(
            !deliver(&packet, &layout, &runtime),
            "伪造 response 必须被拒"
        );
        let entries = runtime.audit.snapshot();
        assert_eq!(entries.len(), 1, "response 鉴权失败同样留痕");
        assert_eq!(entries[0].status, "denied");
        assert!(entries[0].summary.contains(TEST_SOURCE));
        assert!(
            !serde_json::to_string(&entries[0])
                .expect("序列化")
                .contains("cluster-forged"),
            "伪造的 cluster_id 不得写入审计"
        );
    }

    #[test]
    fn forged_oversized_fields_are_bounded_in_audit() {
        // 未鉴权的伪造包可携带超长字段：审计必须封顶，否则 2000 条环形 +
        // 落盘文件会被单个大包撑爆（鉴权失败留痕引入的新攻击面）。
        let (runtime, _dispatched) = counting_runtime();
        let layout = test_agent_layout("server", Vec::new());
        let mut packet = authorized_packet(&"o".repeat(100_000), "peer-key-1");
        packet.capability = "c".repeat(100_000);
        packet.request_id = "r".repeat(100_000);
        packet.pair_secret = "wrong".into();

        assert!(!deliver(&packet, &layout, &runtime));
        let entries = runtime.audit.snapshot();
        assert_eq!(entries.len(), 1);
        for (name, value) in [
            ("peer_id", &entries[0].peer_id),
            ("capability", &entries[0].capability),
            ("request_id", &entries[0].request_id),
        ] {
            assert!(
                value.chars().count() <= AGENT_AUDIT_FIELD_MAX_CHARS + 1,
                "{name} 必须封顶在 {AGENT_AUDIT_FIELD_MAX_CHARS} 字符内（实际 {}）",
                value.chars().count()
            );
        }
    }

    /// 起一对真实 QUIC loopback 端点：A 发起、B 受控（client 白名单只认 A）。
    /// 返回 (A transport, B transport, A runtime, B runtime, A layout, 临时目录)。
    fn quic_pair(
        tag: &str,
        dispatch_b: Option<AgentCapabilityHandler>,
    ) -> (
        quic_transport::TransportHandle,
        quic_transport::TransportHandle,
        AgentChannelRuntime,
        AgentChannelRuntime,
        LayoutState,
        std::path::PathBuf,
    ) {
        init_test_logger();
        let dir = temp_identity_dir(tag);
        let _ = std::fs::remove_dir_all(&dir);

        let layout_a = test_agent_layout("server", Vec::new());
        let slot_a: Arc<Mutex<Option<quic_transport::TransportHandle>>> =
            Arc::new(Mutex::new(None));
        let runtime_a = isolated_production_runtime(Arc::clone(&slot_a));
        let runtime_a_stream = runtime_a.clone();
        let layout_a_stream = layout_a.clone();
        let sender = quic_transport::start(
            free_quic_port(),
            dir.join("sender"),
            Arc::new(|_, _| {}),
            Arc::new(move |payload: Vec<u8>, source: SocketAddr| {
                handle_agent_stream_packet(
                    &payload,
                    &layout_a_stream,
                    "peer-a",
                    &runtime_a_stream,
                    source,
                )
            }),
        )
        .expect("发起端 transport 启动");
        *slot_a.lock().expect("槽位锁") = Some(sender.clone());

        let layout_b = test_agent_layout(
            "client",
            vec![test_controller("peer-a", sender.public_key())],
        );
        let slot_b: Arc<Mutex<Option<quic_transport::TransportHandle>>> =
            Arc::new(Mutex::new(None));
        let mut runtime_b = isolated_production_runtime(Arc::clone(&slot_b));
        if let Some(dispatch) = dispatch_b {
            runtime_b.dispatch = dispatch;
        }
        let runtime_b_stream = runtime_b.clone();
        let receiver = quic_transport::start(
            free_quic_port(),
            dir.join("receiver"),
            Arc::new(|_, _| {}),
            Arc::new(move |payload: Vec<u8>, source: SocketAddr| {
                handle_agent_stream_packet(&payload, &layout_b, "peer-b", &runtime_b_stream, source)
            }),
        )
        .expect("受控端 transport 启动");
        *slot_b.lock().expect("槽位锁") = Some(receiver.clone());

        (sender, receiver, runtime_a, runtime_b, layout_a, dir)
    }

    #[test]
    fn echo_roundtrip_records_audit_on_both_sides() {
        let (sender, receiver, runtime_a, runtime_b, layout_a, dir) = quic_pair("audit-echo", None);
        let peer = sender.peer(
            format!("127.0.0.1:{}", receiver.port()),
            receiver.public_key().into(),
            quic_transport::PROTOCOL_VERSION,
        );
        let response = send_agent_request(
            &sender,
            &peer,
            &layout_a,
            "peer-a",
            sender.public_key(),
            "peer-b",
            AGENT_CAPABILITY_ECHO,
            serde_json::json!({ "ping": 7 }),
            Duration::from_secs(15),
            &runtime_a.audit,
        )
        .expect("echo 往返必须成功");
        assert_eq!(response.payload, serde_json::json!({ "ping": 7 }));

        // 发起端 A：本地发起请求的最终结果 (out, ok)，peer 为目标设备。
        let a_entries = runtime_a.audit.snapshot();
        assert!(
            a_entries.iter().any(|entry| entry.direction == "out"
                && entry.status == "ok"
                && entry.capability == AGENT_CAPABILITY_ECHO
                && entry.peer_id == "peer-b"),
            "发起端必须记 (out, ok)：{a_entries:?}"
        );
        // 受控端 B：执行完成 (in, ok) + response 回发完成 (out, ok)。
        for direction in ["in", "out"] {
            assert!(
                wait_for_audit(
                    &runtime_b.audit,
                    |entry| entry.direction == direction
                        && entry.status == "ok"
                        && entry.capability == AGENT_CAPABILITY_ECHO
                        && entry.peer_id == "peer-a",
                    Duration::from_secs(5),
                ),
                "受控端必须记 ({direction}, ok)：{:?}",
                runtime_b.audit.snapshot()
            );
        }

        sender.stop_and_wait().expect("发起端停止");
        receiver.stop_and_wait().expect("受控端停止");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn local_request_timeout_is_audited_as_timeout() {
        // 受控端处理 2.5s，发起端只等 300ms(+1s 宽限)：发起端必须记 (out, timeout)。
        let slow: AgentCapabilityHandler = Arc::new(|_context, packet| {
            std::thread::sleep(Duration::from_millis(2500));
            Ok(packet.payload.clone())
        });
        let (sender, receiver, runtime_a, _runtime_b, layout_a, dir) =
            quic_pair("audit-timeout", Some(slow));
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
            Value::Null,
            Duration::from_millis(300),
            &runtime_a.audit,
        );
        assert!(result.is_err(), "超时必须返回错误");
        let entries = runtime_a.audit.snapshot();
        assert!(
            entries.iter().any(|entry| entry.direction == "out"
                && entry.status == "timeout"
                && entry.peer_id == "peer-b"),
            "发起端超时必须记 (out, timeout)：{entries:?}"
        );

        sender.stop_and_wait().expect("发起端停止");
        receiver.stop_and_wait().expect("受控端停止");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn local_request_rejected_by_peer_is_audited_as_err() {
        // 对端（旧版等价物）reject：发送阶段即失败，发起端必须记 (out, err)。
        let dir = temp_identity_dir("audit-reject");
        let _ = std::fs::remove_dir_all(&dir);
        let receiver = quic_transport::start(
            free_quic_port(),
            dir.join("receiver"),
            Arc::new(|_, _| {}),
            Arc::new(|_, _| false),
        )
        .expect("拒绝端 transport 启动");
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
        let audit = AgentAuditLog::in_memory();
        let result = send_agent_request(
            &sender,
            &peer,
            &test_agent_layout("server", Vec::new()),
            "peer-a",
            sender.public_key(),
            "peer-b",
            AGENT_CAPABILITY_ECHO,
            Value::Null,
            Duration::from_secs(5),
            &audit,
        );
        assert!(result.is_err(), "对端拒绝必须返回错误");
        let entries = audit.snapshot();
        assert!(
            entries
                .iter()
                .any(|entry| entry.direction == "out" && entry.status == "err"),
            "发送失败必须记 (out, err)：{entries:?}"
        );

        sender.stop_and_wait().expect("发送端停止");
        receiver.stop_and_wait().expect("拒绝端停止");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- k. P1b 只读能力：file.stat / file.read / log.read ----

    impl FileCapabilityFixture {
        /// 在根内写一个内容为 `bytes` 的文件并返回其绝对路径字符串。
        fn write_root_file(&self, name: &str, bytes: &[u8]) -> String {
            let path = self.root.join(name);
            std::fs::write(&path, bytes).expect("写测试文件");
            path.to_str().expect("测试根必须是 UTF-8").to_string()
        }

        /// 在根内建一个子目录（含中间层）并返回其绝对路径字符串。
        fn make_root_dir(&self, relative: &str) -> String {
            let path = self.root.join(relative);
            std::fs::create_dir_all(&path).expect("建测试目录");
            path.to_str().expect("测试根必须是 UTF-8").to_string()
        }

        /// 根外文件的绝对路径字符串（写入 base 下、root 外）。
        fn outside_path(&self, name: &str) -> String {
            let path = self._guard.path().join(name);
            std::fs::write(&path, b"outside").expect("写根外文件");
            path.to_str().expect("沙箱路径必须是 UTF-8").to_string()
        }
    }

    fn stat_request(path: &str) -> AgentChannelPacket {
        capability_packet(
            AGENT_CAPABILITY_FILE_STAT,
            serde_json::json!({ "path": path }),
        )
    }

    fn read_request(path: &str, max_bytes: Option<u64>) -> AgentChannelPacket {
        capability_packet(
            AGENT_CAPABILITY_FILE_READ,
            match max_bytes {
                Some(bytes) => serde_json::json!({ "path": path, "maxBytes": bytes }),
                None => serde_json::json!({ "path": path }),
            },
        )
    }

    fn log_read_request(tail_bytes: Option<u64>) -> AgentChannelPacket {
        capability_packet(
            AGENT_CAPABILITY_LOG_READ,
            match tail_bytes {
                Some(bytes) => serde_json::json!({ "tailBytes": bytes }),
                None => serde_json::json!({}),
            },
        )
    }

    #[test]
    fn file_stat_reports_existing_file_fields() {
        let fixture = FileCapabilityFixture::new();
        let path = fixture.write_root_file("hello.txt", b"hello world");
        let payload = dispatch_agent_capability(&fixture.context(), &stat_request(&path))
            .expect("file.stat 根内文件必须成功");

        assert_eq!(payload["exists"], true);
        assert_eq!(payload["isFile"], true);
        assert_eq!(payload["isDir"], false);
        assert_eq!(payload["sizeBytes"], 11);
        assert!(
            payload["modifiedMs"].as_u64().unwrap_or(0) > 0,
            "modifiedMs 必须是文件的真实 mtime 毫秒（实际 {}）",
            payload["modifiedMs"]
        );
        assert_eq!(
            payload["entries"],
            serde_json::json!([]),
            "文件的 entries 必须为空"
        );
        assert_eq!(payload["entriesTruncated"], false);
    }

    #[test]
    fn file_stat_lists_directory_entries_sorted_with_cap() {
        let fixture = FileCapabilityFixture::new();
        let dir = fixture.make_root_dir("folder");

        let payload = dispatch_agent_capability(&fixture.context(), &stat_request(&dir))
            .expect("file.stat 目录必须成功");
        assert_eq!(payload["exists"], true);
        assert_eq!(payload["isFile"], false);
        assert_eq!(payload["isDir"], true);
        assert_eq!(
            payload["entries"],
            serde_json::json!([]),
            "空目录 entries 为空"
        );
        assert_eq!(payload["entriesTruncated"], false);

        // 先填一个文件再建目录：非按名称序的创建顺序，验证排序按名称。
        fixture.write_root_file("folder/z.txt", b"z");
        fixture.make_root_dir("folder/a-dir");
        fixture.write_root_file("folder/m.txt", b"mm");
        let payload = dispatch_agent_capability(&fixture.context(), &stat_request(&dir))
            .expect("列目录必须成功");
        let entries = payload["entries"].as_array().expect("entries 数组");
        let names: Vec<&str> = entries
            .iter()
            .map(|entry| entry["name"].as_str().expect("条目必须有 name"))
            .collect();
        assert_eq!(
            names,
            vec!["a-dir", "m.txt", "z.txt"],
            "entries 必须按名称排序"
        );
        assert_eq!(entries[0]["isDir"], true, "目录条目标注 isDir");
        assert_eq!(entries[0]["sizeBytes"], 0);
        assert_eq!(entries[1]["sizeBytes"], 2, "文件条目带真实大小");
        assert_eq!(entries[2]["isDir"], false);
        assert_eq!(payload["entriesTruncated"], false);
    }

    #[test]
    fn file_stat_truncates_entries_at_200() {
        let fixture = FileCapabilityFixture::new();
        let dir = fixture.make_root_dir("many");
        for index in 0..250 {
            fixture.write_root_file(&format!("many/{index:03}.txt"), b"x");
        }

        let payload = dispatch_agent_capability(&fixture.context(), &stat_request(&dir))
            .expect("列大目录必须成功");
        let entries = payload["entries"].as_array().expect("entries 数组");
        assert_eq!(entries.len(), 200, "entries 必须封顶 200 条");
        assert_eq!(entries[0]["name"], "000.txt", "截断保留名称排序的前 200 条");
        assert_eq!(entries[199]["name"], "199.txt");
        assert_eq!(payload["entriesTruncated"], true, "超限必须标注截断");
    }

    #[test]
    fn file_stat_missing_path_reports_exists_false_with_zero_fields() {
        let fixture = FileCapabilityFixture::new();
        let missing = fixture.root.join("ghost.txt");
        let payload = dispatch_agent_capability(
            &fixture.context(),
            &stat_request(missing.to_str().expect("测试路径")),
        )
        .expect("根内不存在的路径不算错误（调用方先 stat 再 read）");

        assert_eq!(payload["exists"], false);
        assert_eq!(payload["isFile"], false);
        assert_eq!(payload["isDir"], false);
        assert_eq!(payload["sizeBytes"], 0);
        assert_eq!(payload["modifiedMs"], 0);
        assert_eq!(payload["entries"], serde_json::json!([]));
        assert_eq!(payload["entriesTruncated"], false);
    }

    #[test]
    fn file_stat_rejects_paths_outside_roots() {
        let fixture = FileCapabilityFixture::new();
        let outside = fixture.outside_path("secret.txt");

        let error = dispatch_agent_capability(&fixture.context(), &stat_request(&outside))
            .expect_err("根外路径必须拒绝");
        assert!(error.contains("根目录"), "错误须说明原因（实际 {error:?}）");

        // `..` 穿越同样落到安全层拒绝。
        let traversal = format!("{}/../secret.txt", fixture.root.to_str().expect("测试根"));
        assert!(
            dispatch_agent_capability(&fixture.context(), &stat_request(&traversal)).is_err(),
            "`..` 穿越必须拒绝"
        );

        // 相对路径/空路径拒绝。
        assert!(dispatch_agent_capability(&fixture.context(), &stat_request("inner.txt")).is_err());
        assert!(dispatch_agent_capability(&fixture.context(), &stat_request("")).is_err());
    }

    #[test]
    fn file_stat_uses_configured_roots_over_defaults() {
        // layout 配置了根时用配置根：注入的默认根（生产 = 日志目录/接收根）即便
        // 存在也不参与，默认根内的文件必须拒绝；配置根内的文件照常放行。
        let fixture = FileCapabilityFixture::new();
        let default_root = fixture._guard.path().join("default-root");
        std::fs::create_dir_all(&default_root).expect("建默认根目录");
        let default_file = default_root.join("f.txt");
        std::fs::write(&default_file, b"other").expect("写默认根内文件");

        // 模拟生产注入：默认根指向 default-root（不在 layout 配置里）。
        let default_roots = vec![default_root.clone()];
        let inbox = AgentInbox::new();
        let context = AgentCapabilityContext {
            layout: &fixture.layout,
            local_peer_id: "peer-local-1",
            inbox: &inbox,
            default_file_roots: &default_roots,
            log_dir: None,
        };

        let in_config_root = fixture.root.join("inner.txt");
        std::fs::write(&in_config_root, b"cfg").expect("写配置根内文件");
        let payload = dispatch_agent_capability(
            &context,
            &stat_request(in_config_root.to_str().expect("沙箱路径")),
        )
        .expect("配置根内的文件必须放行");
        assert_eq!(payload["exists"], true);
        assert_eq!(payload["isFile"], true);

        let error = dispatch_agent_capability(
            &context,
            &stat_request(default_file.to_str().expect("沙箱路径")),
        )
        .expect_err("配置根非空时，默认根内的文件必须拒绝（默认根不参与）");
        assert!(error.contains("根目录"), "错误须说明原因（实际 {error:?}）");
    }

    #[test]
    fn file_read_returns_base64_content_of_small_file() {
        let fixture = FileCapabilityFixture::new();
        let path = fixture.write_root_file("data.bin", b"\x00\x01\xffbinary");
        let payload = dispatch_agent_capability(&fixture.context(), &read_request(&path, None))
            .expect("file.read 根内小文件必须成功");

        use base64::Engine as _;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(payload["contentBase64"].as_str().expect("contentBase64"))
            .expect("contentBase64 必须是合法 base64");
        assert_eq!(
            decoded, b"\x00\x01\xffbinary",
            "解码内容必须与原文件一致（不猜文本编码）"
        );
        assert_eq!(payload["sizeBytes"], 9, "sizeBytes 是文件实际大小");
        assert_eq!(payload["readBytes"], 9);
        assert_eq!(payload["truncated"], false);
        assert_eq!(
            payload["offsetRead"], 0,
            "决策：读头部（truncate 尾部），字段名明确语义"
        );
    }

    #[test]
    fn file_read_clamps_oversized_max_bytes_to_1mib() {
        // 请求 2MiB：钳制到 1MiB 放行（而非拒绝），truncated 标注供调用方知晓。
        let fixture = FileCapabilityFixture::new();
        let payload_2mib = dispatch_agent_capability(
            &fixture.context(),
            &read_request(
                &fixture.write_root_file("big.bin", &vec![0u8; 2 * 1024 * 1024]),
                Some(2 * 1024 * 1024),
            ),
        )
        .expect("maxBytes 超上限必须钳制而非拒绝");
        assert_eq!(
            payload_2mib["readBytes"], AGENT_FILE_READ_MAX_BYTES,
            "请求 2MiB 必须被钳制到恰好 1MiB"
        );
        assert_eq!(payload_2mib["sizeBytes"], 2 * 1024 * 1024);
        assert_eq!(payload_2mib["truncated"], true);
    }

    #[test]
    fn file_read_truncates_file_bigger_than_max_bytes() {
        let fixture = FileCapabilityFixture::new();
        let path = fixture.write_root_file("ten.bin", b"0123456789");
        let payload = dispatch_agent_capability(&fixture.context(), &read_request(&path, Some(4)))
            .expect("按 maxBytes 读取必须成功");
        assert_eq!(payload["readBytes"], 4);
        assert_eq!(payload["sizeBytes"], 10);
        assert_eq!(payload["truncated"], true, "文件大于 maxBytes 必须标注截断");
        assert_eq!(payload["offsetRead"], 0);
    }

    #[test]
    fn file_read_rejects_directory_and_missing_file() {
        let fixture = FileCapabilityFixture::new();
        let dir = fixture.make_root_dir("a-folder");
        let error = dispatch_agent_capability(&fixture.context(), &read_request(&dir, None))
            .expect_err("目录必须返回 err");
        assert!(
            error.contains("目录"),
            "目录错误须说明原因（实际 {error:?}）"
        );

        let missing = fixture.root.join("ghost.bin");
        let error = dispatch_agent_capability(
            &fixture.context(),
            &read_request(missing.to_str().expect("测试路径"), None),
        )
        .expect_err("决策：read 不存在 → err 带明确信息（exists 语义属于 file.stat）");
        assert!(
            error.contains("不存在"),
            "不存在错误须说明原因（实际 {error:?}）"
        );
    }

    #[test]
    fn file_read_rejects_paths_outside_roots() {
        let fixture = FileCapabilityFixture::new();
        let outside = fixture.outside_path("secret.bin");
        assert!(
            dispatch_agent_capability(&fixture.context(), &read_request(&outside, None)).is_err(),
            "file.read 与 file.stat 共用同一条安全层边界"
        );
    }

    fn log_fixture() -> (FileCapabilityFixture, PathBuf) {
        let fixture = FileCapabilityFixture::new();
        let log_dir = fixture._guard.path().join("logs");
        std::fs::create_dir_all(&log_dir).expect("建日志目录");
        (fixture, log_dir)
    }

    /// 构造注入了日志目录的上下文（fixture.context() 的日志目录变体）。
    fn log_capability_context<'a>(
        fixture: &'a FileCapabilityFixture,
        log_dir: &'a Path,
    ) -> AgentCapabilityContext<'a> {
        AgentCapabilityContext {
            layout: &fixture.layout,
            local_peer_id: "peer-local-1",
            inbox: &fixture.inbox,
            default_file_roots: &[],
            log_dir: Some(log_dir),
        }
    }

    #[test]
    fn log_read_returns_tail_of_newest_log_file() {
        let (fixture, log_dir) = log_fixture();
        // 旧日志（更早 mtime）+ 当前日志（最新 mtime）：必须选中后者。
        let old = log_dir.join("mykvm-old.log");
        std::fs::write(&old, "旧轮转日志\n").expect("写旧日志");
        std::thread::sleep(Duration::from_millis(20));
        let new = log_dir.join("mykvm.log");
        std::fs::write(&new, "第一行\n第二行\n第三行\n").expect("写当前日志");

        let payload = dispatch_agent_capability(
            &log_capability_context(&fixture, &log_dir),
            &log_read_request(None),
        )
        .expect("log.read 必须成功");
        assert_eq!(payload["text"], "第一行\n第二行\n第三行\n");
        assert_eq!(
            payload["path"],
            new.to_str().expect("沙箱路径").to_string(),
            "必须选 mtime 最新的 .log 文件（唯一真相源语义，同 write_own_log_tail）"
        );
        assert_eq!(
            payload["sizeBytes"],
            std::fs::metadata(&new).expect("元数据").len()
        );
        assert_eq!(payload["truncatedFromHead"], false);
    }

    #[test]
    fn log_read_drops_partial_head_line_and_flags_truncation() {
        let (fixture, log_dir) = log_fixture();
        let mut content = "a".repeat(40) + "\n";
        content.push_str("完整行\n不完整的尾部行");
        std::fs::write(log_dir.join("mykvm.log"), &content).expect("写日志");

        // tailBytes 恰好切到「完整行」中间：头部被切的不完整行必须丢弃并标注。
        let payload = dispatch_agent_capability(
            &log_capability_context(&fixture, &log_dir),
            &log_read_request(Some(45)),
        )
        .expect("log.read 尾部读取必须成功");
        assert_eq!(payload["truncatedFromHead"], true);
        assert!(
            payload["text"]
                .as_str()
                .expect("text")
                .starts_with("完整行"),
            "尾部必须切成完整行（实际 {:?}）",
            payload["text"]
        );
    }

    #[test]
    fn log_read_clamps_tail_bytes_to_1mib() {
        let (fixture, log_dir) = log_fixture();
        let line = "x".repeat(99) + "\n";
        let content = line.repeat(20_000); // ≈2MiB
        std::fs::write(log_dir.join("mykvm.log"), &content).expect("写 2MiB 日志");

        let payload = dispatch_agent_capability(
            &log_capability_context(&fixture, &log_dir),
            &log_read_request(Some(2 * 1024 * 1024)),
        )
        .expect("超限 tailBytes 必须钳制而非拒绝");
        let text = payload["text"].as_str().expect("text");
        assert!(
            text.len() <= AGENT_LOG_READ_MAX_BYTES as usize,
            "钳制后不得超过 1MiB（实际 {}）",
            text.len()
        );
        assert_eq!(payload["truncatedFromHead"], true);
        assert!(
            text.starts_with("x"),
            "丢弃头部残行后仍以完整行开头（实际前 20 字 {:?}）",
            text.chars().take(20).collect::<String>()
        );
    }

    #[test]
    fn log_read_errors_without_log_dir_or_log_file() {
        let (fixture, _log_dir) = log_fixture();
        let error = dispatch_agent_capability(&fixture.context(), &log_read_request(None))
            .expect_err("未注入日志目录（None）必须 err 而不是 panic");
        assert!(error.contains("日志"), "错误须说明原因（实际 {error:?}）");

        let (fixture, empty_log_dir) = log_fixture();
        // 首次运行：日志目录存在但还没有任何 .log 文件。
        let error = dispatch_agent_capability(
            &log_capability_context(&fixture, &empty_log_dir),
            &log_read_request(None),
        )
        .expect_err("目录里没有日志文件必须 err");
        assert!(
            error.contains("不存在") || error.contains("日志"),
            "错误须说明原因（实际 {error:?}）"
        );
    }

    #[test]
    fn log_read_survives_non_utf8_bytes_via_lossy_decoding() {
        let (fixture, log_dir) = log_fixture();
        let mut bytes = "正常行\n".to_string().into_bytes();
        bytes.extend_from_slice(&[0xff, 0xfe, b'\n']); // 非法 UTF-8 序列
        bytes.extend_from_slice("尾行\n".as_bytes());
        std::fs::write(log_dir.join("mykvm.log"), &bytes).expect("写含坏字节日志");

        let payload = dispatch_agent_capability(
            &log_capability_context(&fixture, &log_dir),
            &log_read_request(None),
        )
        .expect("损坏字节必须 lossy 解码而不是报错");
        let text = payload["text"].as_str().expect("text");
        assert!(
            text.contains("正常行") && text.contains("尾行"),
            "合法内容保留"
        );
        assert!(
            text.contains('\u{FFFD}'),
            "损坏字节替换为 U+FFFD（实际 {text:?}）"
        );
    }

    #[test]
    fn dispatch_wires_file_log_capabilities_with_injected_context() {
        // 接线确认：三个新能力在 dispatch 分支上可达（能力名 → 安全层 → 本机路径）。
        let fixture = FileCapabilityFixture::new();
        let path = fixture.write_root_file("wired.txt", b"wired");
        let stat = dispatch_agent_capability(&fixture.context(), &stat_request(&path))
            .expect("file.stat 分支必须接线");
        assert_eq!(stat["exists"], true);
        let read = dispatch_agent_capability(&fixture.context(), &read_request(&path, None))
            .expect("file.read 分支必须接线");
        assert_eq!(read["sizeBytes"], 5);
    }

    #[test]
    fn default_file_roots_fallback_via_agent_file_access_layer() {
        // config.file_roots 为空时用注入的默认根（生产 = [日志目录, 接收根]），
        // 跨模块确认 agent_file_roots 的回落逻辑被 dispatch 消费。
        let guard = tempfile::tempdir().expect("创建临时目录");
        let base = guard.path().to_path_buf();
        let log_dir = base.join("logs");
        let transfers = base.join("transfers");
        std::fs::create_dir_all(&log_dir).expect("建日志目录");
        std::fs::create_dir_all(&transfers).expect("建接收根");
        let in_transfers = transfers.join("received.txt");
        std::fs::write(&in_transfers, b"received").expect("写接收文件");

        let mut layout = test_agent_layout("server", Vec::new());
        layout.agent_channel.file_roots = Vec::new(); // 空 = 默认根
        let inbox = AgentInbox::new();
        let default_roots =
            crate::agent_file_access::default_agent_file_roots(Some(&log_dir), Some(&transfers));
        let context = AgentCapabilityContext {
            layout: &layout,
            local_peer_id: "peer-local-1",
            inbox: &inbox,
            default_file_roots: &default_roots,
            log_dir: None,
        };

        let payload = dispatch_agent_capability(
            &context,
            &stat_request(in_transfers.to_str().expect("沙箱路径")),
        )
        .expect("默认根内的路径必须放行");
        assert_eq!(payload["exists"], true);
    }
}
