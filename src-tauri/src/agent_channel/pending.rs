//! 两段式 RPC 的挂起请求表：request_id ↔ 等待者，以及 request_id 生成。

use serde_json::Value;
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc, Mutex, OnceLock,
    },
    time::{Duration, Instant},
};

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

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造 status=ok 的响应结果（pending 表测试用）。
    fn ok_response(payload: Value) -> Result<AgentResponse, String> {
        Ok(AgentResponse {
            status: "ok".into(),
            payload,
        })
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
