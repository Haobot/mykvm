//! capability 执行面：能力名与参数上限、执行上下文/运行期依赖、生产分发与
//! 各能力处理器（P1a：echo/status.get/msg.send；P1b：file.stat/file.read/log.read）。

use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use super::audit::{global_audit_log, AgentAuditLog};
use super::inbox::{global_agent_inbox, AgentInbox, AgentInboxMessage};
use super::protocol::AgentChannelPacket;

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

/// msg.send 正文上限：4KiB（按 UTF-8 字节，截断在字符边界）。
pub(crate) const AGENT_MSG_TEXT_MAX_BYTES: usize = 4 * 1024;

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

/// 生产 capability 分发（P1a：echo + status.get + msg.send；P1b：file.stat/
/// file.read/log.read）。
pub(super) fn dispatch_agent_capability(
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
/// 场景应使用 log.read；offsetRead 字段名明确这一语义。requestedBytes 回显
/// 钳制后本次请求读取的上限（配 truncated 供调用方知晓截断）。目录/不存在
/// → err（exists 语义属于 file.stat，调用方按「先 stat 再 read」协作）。
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
        // 本次请求读取的字节数（maxBytes 钳制后的上限）：readBytes == requestedBytes
        // 说明读到了上限，可能还有更多。
        "requestedBytes": max_bytes,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use crate::agent_channel::test_support::{
        capability_packet, test_agent_layout, TEST_CLUSTER, TEST_SECRET,
    };
    use crate::LayoutState;

    // ---- i. 首批 L0 能力：status.get / msg.send（P1a）----

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
        assert_eq!(
            payload["requestedBytes"], AGENT_FILE_READ_DEFAULT_BYTES,
            "缺省时回显钳制后的默认读取上限"
        );
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
        assert_eq!(
            payload_2mib["requestedBytes"], AGENT_FILE_READ_MAX_BYTES,
            "requestedBytes 回显钳制后的上限（1MiB），而不是调用方请求的 2MiB"
        );
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
