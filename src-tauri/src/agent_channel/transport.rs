//! 两段式收发与 on_stream agent 分支（RFC §6.3 执行模型）：发起请求、接收调度、
//! gate、异步执行与响应回发。

use serde_json::Value;
use std::{
    net::SocketAddr,
    time::{Duration, Instant},
};

use super::audit::{AgentAuditEntry, AgentAuditLog};
use super::auth::{agent_packet_authorized, agent_packet_targets_local};
use super::capabilities::{AgentCapabilityContext, AgentChannelRuntime};
use super::pending::{global_pending_table, new_request_id, AgentResponse};
use super::protocol::{
    AgentChannelPacket, AGENT_KIND_CANCEL, AGENT_KIND_NOTIFY, AGENT_KIND_REQUEST,
    AGENT_KIND_RESPONSE, AGENT_PROTOCOL,
};

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
    use std::sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, OnceLock,
    };

    use crate::agent_channel::audit::{AGENT_AUDIT_FIELD_MAX_CHARS, AGENT_AUDIT_MAX_FILE_BYTES};
    use crate::agent_channel::capabilities::{
        dispatch_agent_capability, AgentCapabilityHandler, AGENT_CAPABILITY_ECHO,
        AGENT_CAPABILITY_MSG_SEND, AGENT_CAPABILITY_STATUS_GET,
    };
    use crate::agent_channel::inbox::AgentInbox;
    use crate::agent_channel::test_support::{
        authorized_packet, capability_packet, test_agent_layout, test_controller, TEST_CLUSTER,
        TEST_SECRET,
    };
    use crate::{quic_transport, LayoutState};

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
}
