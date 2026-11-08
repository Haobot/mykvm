//! 跨机 Agent 互联通道（RFC：Doc/Rfc/跨机Agent互联与受控协作通道.md §6.1/§6.3）。
//!
//! P0 范围：协议包结构 + 鉴权 + pending 请求表 + 两段式 echo 闭环。
//! 两段式：request 与 response 是两条独立的 QUIC 流消息，靠 request_id 关联；
//! 应答由接收方反向新建流发回（ConnectionMap 对称，反向拨号命中既有信任面），
//! 不改动 quic_transport.rs 的 StreamHandler 签名。
//!
//! 子模块（纯移动拆分自原单文件 agent_channel.rs，行为零变更）：
//! - [`protocol`]：协议字符串、kind 常量与线上包结构；
//! - [`auth`]：鉴权与路由消歧；
//! - [`pending`]：request_id ↔ 等待者挂起表与 request_id 生成；
//! - [`config`]：用户意图配置、level gate 与 LanPeer 能力宣告；
//! - [`audit`]：审计条目、内存环形 + JSONL 落盘与全局实例；
//! - [`inbox`]：msg.send 收件箱与全局实例；
//! - [`transport`]：两段式收发、on_stream 分支入口与异步执行模型；
//! - [`capabilities`]：能力名/参数上限、执行上下文/运行期依赖与各能力处理器；
//! - [`command`]：P1c Tauri 命令 agent_channel_request 的辅助（返回结构、超时钳制、目标解析）。
//!
//! 本模块外部（lib.rs 等）引用的项在下方 re-export，`agent_channel::X` 路径保持不变。

pub(crate) mod audit;
pub(crate) mod auth;
pub(crate) mod capabilities;
pub(crate) mod command;
pub(crate) mod config;
pub(crate) mod inbox;
pub(crate) mod pending;
pub(crate) mod protocol;
pub(crate) mod transport;

#[cfg(test)]
mod test_support;

pub(crate) use audit::{
    global_audit_log, init_global_audit_log, recent_audit_snapshot, AgentAuditEntry,
};
pub(crate) use capabilities::AgentChannelRuntime;
pub(crate) use command::{
    agent_channel_target_for_device, clamped_agent_request_timeout, AgentChannelResponse,
};
pub(crate) use config::{
    announce_from_config, default_agent_channel, AgentChannelAnnounce, AgentChannelConfig,
};
pub(crate) use inbox::{drain_agent_inbox, global_agent_inbox, AgentInboxMessage};
pub(crate) use transport::{handle_agent_stream_packet, send_agent_request};
