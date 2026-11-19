//! msg.send 收件箱：内存环形、全局实例与清空辅助。

use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, OnceLock},
};

/// 收件箱容量：内存环形 100 条，超限挤最老（不落盘——消息是瞬时协作信号，
/// 审计里已留有元数据痕迹）。
pub(crate) const AGENT_INBOX_CAPACITY: usize = 100;

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

// ---- P1c 集成面：Tauri 命令辅助（纯函数，可独立测试） ----

/// 清空收件箱并返回清掉的条数（P1c 命令 agent_channel_inbox_clear）。
pub(crate) fn drain_agent_inbox(inbox: &AgentInbox) -> usize {
    let Ok(mut messages) = inbox.messages.lock() else {
        return 0;
    };
    let cleared = messages.len();
    messages.clear();
    cleared
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn drain_agent_inbox_returns_cleared_count_and_empties() {
        let inbox = AgentInbox::new();
        inbox.push(AgentInboxMessage {
            ts_ms: 1,
            peer_id: "peer-1".into(),
            title: None,
            text: "a".into(),
            truncated: false,
        });
        inbox.push(AgentInboxMessage {
            ts_ms: 2,
            peer_id: "peer-1".into(),
            title: None,
            text: "b".into(),
            truncated: false,
        });

        assert_eq!(drain_agent_inbox(&inbox), 2, "返回清掉的条数");
        assert!(inbox.snapshot().is_empty(), "清空后收件箱为空");
        assert_eq!(drain_agent_inbox(&inbox), 0, "再清一次为 0");
    }
}
