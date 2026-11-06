# 跨机 Agent 互联与受控协作通道方案

- 状态：待评审
- 日期：2026-11-07
- 背景：跨主机（Win 开发机 ↔ Mac 开发机）联调已成为日常主力工作流（互控、halo、抗干扰均双端联调验证）。两端各驻一个 AI Agent（ZCode 等），但 Agent 之间无法直接通信——日志、构建结果、diff、结论目前靠「剪贴板 + 人转头搬运」，人成为两台 AI 之间的网桥。

## 1. 需求分析

### 1.1 场景分解（按真实联调工作流推导）

| 场景 | 现状（人肉桥接） | 期望 |
|---|---|---|
| S1 消息互通 | Mac 端 agent 说「跑一下 cargo test」，人复制到 Win 端 | A 端 agent 直接向 B 端发送结构化消息/请求，取回结果 |
| S2 文件交换 | 抓 `mykvm.log` 靠剪贴板文件/手动 `fetch_client_log`，产物（安装包/截图/diff）手动搬 | agent 按需读写对端指定文件，双向、带进度、断点续传 |
| S3 命令控制 | 人在 B 机上替 A 机 agent 敲命令（cargo test / npm test / 重启服务） | A 端 agent 请求 B 端执行**受控**命令并取回结构化输出（exit code / stdout / stderr） |
| S4 人工介入闭环 | agent 需要人审批/肉眼验证时，人自己转头/切屏 | agent 请求人工介入 → 光环/切屏把人带到对应机器 → 结果回传（MyKVM 独有，见 §5.3） |

### 1.2 非目标

- 不做云中转/公网穿透（沿用[网络抗干扰 RFC](网络抗干扰与多接口持续连接.md)非目标：同网段 LAN）。
- 不做 LLM 推理中转、不替代 Agent 自身的会话/工具体系——只提供「对端机器的结构化触达能力」。
- 不发明通用 RPC 框架；能力面收敛为消息、文件、命令、人工介入四类。
- 不做无人值守优先（headless 服务模式暂不纳入，见 §7 P5）。

## 2. 现状盘点：已有的地基（可行性依据）

| 资产 | 证据 | 对 Agent 通道的意义 |
|---|---|---|
| QUIC 双向对称连接 | `quic_transport.rs:433-467` 每节点同时绑定 ServerConfig 的 Endpoint；`:972-985` 任一侧可主动拨号；ConnectionMap 按 PeerKey 对称键控 | 双向 RPC 的传输地基已存在，**无需新端口、无需新连接管理** |
| 三层信任（发现钉扎 + TLS1.3 + pair_secret） | `PinnedCertVerifier`（quic_transport.rs:614-668）逐字节比对 announce 证书；`packet_authorized_fields`（input.rs:3060-3087）cluster 等值 + `consttime_eq` 恒时比较 + 白名单匹配 | 跨机身份验证——Agent 通信方案里最难做对的部分**已经解决且经受过实战** |
| 包级权限门范式 | `PeerRequest` 枚举 `Log/Update/Lock/Clipboard` + `authorize_peer_request`（lib.rs:10557-10614，注释 "What a paired peer may ask of this device"），按「协议名 × 本机开关 × 角色方向 × 白名单」四层 gate | **这就是 Agent 能力授权的原型**，扩展枚举即可 |
| 分块文件传输 | `FileTransferPacket` 256KiB/chunk、单文件 2GiB、断点续传（lib.rs:9688-9789、10766+） | 大 payload 直接复用；agent 请求里放 transfer_id 引用即可 |
| 消息封装与向前兼容 | MessagePack named（`lib.rs:11137`），`serde(default)` + 旧解码器忽略未知字段，跨版本字节兼容测试已锁定（lib.rs:17193-17371） | 新协议字符串 `mykvm.agent.v1` 对旧版无破坏 |
| 能力宣告 | `LanPeer`（lib.rs:384-418）已是能力载体，`ips` 字段证明「serde(default) 字段随 announce 传播」模式可行 | 对端是否具备 agent 能力、版本如何，3s 内可知 |
| 审批 UI 范式 | `PairingChallenge`（挂起表 + TTL + attempts + 6 位码，lib.rs:13442）；`runtimeBlocker.ts` 的「同因只弹一次、解除自动关」纪律 | 逐命令审批窗有完整先例可抄 |
| 剪贴板/文件/输入共享同一连接 | 同一 QUIC endpoint、同一 ConnectionMap、同一观测面 | Agent 通道天然继承抗干扰成果（多地址 directed、健康度、看门狗） |

### 3 缺口（需新建的部分）

1. **RPC 应答路径**：`on_stream` handler 只能回 `ok/reject` 两字节（quic_transport.rs:779-781），不支持在原流上回载荷。**决策：两段式**——应答作为独立新包反向新建流发回（ConnectionMap 对称，反向拨号命中缓存连接），`request_id` 多路分复用；长任务先回 `accepted`、完成后独立回结果。现有 Log 请求已是此形态（lib.rs:10674-10693 收到请求后 spawn 线程、用文件传输回传），证明模式成立。
2. **命令执行面**：白名单、工作目录约束、超时、输出截断、编码（Windows 控制台统一按 UTF-8 约定）——全新模块，但授权门照抄 `authorize_peer_request`。
3. **审计**：现状零审计（成功传输仅失败有 warn）。Agent 通道从第一天起带环形审计日志。
4. **Agent 集成面**：本机 MCP server（stdio），见 §5.4。
5. **并发预算**：`MAX_CONCURRENT_STREAMS = 8`（quic_transport.rs:47），agent 请求与文件传输/剪贴板竞争，需评估提升或配额。

## 4. 可行性结论

**可行，且成本远低于从零建设。** 传输、鉴权、文件分块、能力宣告、审批范式五块地基全部现成；净新增工作是协议结构 + 命令执行面 + 审批/审计 UI + MCP 桥，均为应用层逻辑，不触碰 `quic_transport.rs` 核心路径（两段式应答避免了扩展 StreamHandler 签名的侵入）。

跨版本混跑安全性：旧版对端收到 `mykvm.agent.v1` 包会在调度链各分支 decode 失败而落入默认拒绝路径（P0 验收项：旧版优雅拒绝不崩、不挂起）；新版发起方按 `LanPeer` 能力宣告 gate，不对无能力对端发起。

## 5. 必要性评估（含替代方案对比）

### 5.1 替代方案

| 方案 | 结论 |
|---|---|
| SSH/WinRM + scp | 技术上可行，但：密钥分发与轮换、Windows OpenSSH 权限/非交互 shell 坑、macOS 侧 Remote Management 配置，**每对机器都要重新做一遍身份信任**——而这正是 MyKVM 配对已一次性解决的；且 SSH 会话与 MyKVM 键鼠/剪贴板是两个割裂的信任域与观测面 |
| Tailscale/云隧道 | 引入外部依赖与延迟；用户环境已有 Tailscale 但它解决「网络可达」不解决「能力语义」（结构化命令/审批/审计） |
| 云协作（GitHub PR/issue 中转） | 异步、延迟高、无法承载实时联调（跑测试→看日志→改→再跑的分钟级循环） |
| 维持人肉桥接 | 现状。每次往返成本 = 复制 + 转头 + 粘贴 + 等待 + 转回，打断两端的 agent 会话心流；这是当前联调效率的第一瓶颈 |

### 5.2 判断

**必要，但范围要克制。** 价值排序：

1. **S1+S2（消息 + 文件）**：高价值、低风险（不超出剪贴板已有的读写面），优先做。
2. **S3 白名单命令**：高价值、中风险（自动化执行），配审计 + 熔断。
3. **S3 任意命令**：低频场景，默认关闭，逐次审批制。
4. **S4 人工介入联动**：差异化增值，随 P4 顺带。

### 5.3 安全模型：信任域论证（本方案的核心安全依据）

**配对后的信任域已经是「完全控制对方机器」级别**——键鼠注入可以做人在场时能做的一切（包括打开终端执行任意命令）。因此 Agent 通道**不提升权限上界**；它提升的是**自动化程度**（无人在场时高速批量执行），新增风险相应为：

- 对策一（分级 capability）：L0 只读 → L1 文件写（限白名单目录）→ L2 白名单命令 → L3 任意命令（逐次审批或短 TTL 窗口），默认 L0。
- 对策二（审计）：所有 agent 动作全量环形日志（来源/能力/参数摘要/结果状态/耗时），前端可查。
- 对策三（熔断）：agent 通道开关独立于 `input_mode`，一键禁用不影响键鼠/剪贴板；L3 审批窗展示完整命令、工作目录、来源设备，杜绝「钓鱼式审批」。
- 对策四（防重放）：`request_id` 单调 + TTL，响应包同样过 `packet_authorized` 鉴权（双向都不是裸信任）。

## 6. 技术方案

### 6.1 协议：`mykvm.agent.v1`

```rust
struct AgentChannelPacket {
    protocol: &'static str,     // "mykvm.agent.v1"
    kind: String,               // "request" | "response" | "notify" | "cancel" | "approval"
    request_id: String,         // 多路分复用键，含单调计数 + 随机 nonce
    origin_id: String,          // 复用 packet_targets_local 路由语义（input.rs:3170）
    target_id: String,
    cluster_id: String,         // 鉴权三件套，照抄 clipboard_packet_authorized（lib.rs:9379）
    pair_secret: String,
    capability: String,         // 见 6.2
    payload: serde_json::Value, // 参数/结果；大载荷放 transfer_id 引用
    status: Option<String>,     // response 专用: ok | err | denied | accepted | pending_approval
}
```

- 承载平面：QUIC stream（`send_stream_expect_ack`，≤48MiB/包）；不占用输入 datagram 热路径。
- 调度接入：`on_stream`（lib.rs:986-1069）在 pairing 之后插 `handle_agent_packet` 分支——**唯一必改的调度点**。
- 大载荷：payload 内只放 `transfer_id`，实体复用 `FileTransferPacket` 分块协议。

### 6.2 能力面（capability 枚举）

| capability | 级别 | 语义 |
|---|---|---|
| `status.get` | L0 | 对端运行状态（peer 快照、输入链路、halo 状态） |
| `log.read` | L0 | 抓取对端日志（`fetch_client_log` 的推广，lib.rs:5280 先例） |
| `file.stat` / `file.read` | L0 | 白名单目录内列目录/读文件（读走 transfer 引用） |
| `msg.send` | L0 | 单向通知（对端弹气泡/写收件箱） |
| `file.write` | L1 | 写文件到白名单目录（默认项目目录 + Downloads） |
| `cmd.run` | L2/L3 | 执行命令：L2 白名单前缀匹配 + 目录约束 + 超时 + 输出截断；L3 任意命令需对端逐次审批 |
| `human.ask` | L0 | 请求人工介入：对端光环提示 + 可选切屏（S4） |

### 6.3 模块划分

```
src-tauri/src/agent_channel.rs        # 新模块：协议结构、鉴权、请求分发、pending 表（HashMap<request_id, oneshot>）、审计环形日志
src-tauri/src/agent_exec.rs           # 命令执行面：白名单匹配、spawn、超时、UTF-8 输出采集（P3）
lib.rs                                # on_stream 插一个分支；LayoutState 加 agent_channel 配置；新增 Tauri 命令
src/types.ts + Settings 页             # 配置 UI（开关/级别/白名单/审计查看）
src/agentApproval.ts + 审批 modal      # L3 审批（照 runtimeBlocker「同因只弹一次」纪律 + PairingChallenge 挂起表）
mykvm-agent-mcp（子命令，P4）          # stdio MCP server，见 6.4
```

配置挂载（照现有惯例）：

- `LayoutState` 新增 `#[serde(default)] agent_channel: AgentChannelConfig { enabled, level, cmd_whitelist, approval_ttl }`；权限字段归入 `merge_runtime_owned_layout_fields`（lib.rs:3924）防前端陈旧快照回滚。
- `LanPeer` 新增 `#[serde(default)] agent_channel: Option<AgentCapabilityAnnounce>`，随 announce 传播。

Tauri 命令（照 `fetch_client_log` 先例）：`agent_channel_send` / `agent_channel_status` / `agent_channel_cancel` / `agent_approve_request` / `agent_read_audit`；后端→前端事件 `agent-request-pending`、`agent-audit-appended`。

### 6.4 Agent 集成面：本机 MCP 网桥（P4）

不要求各 Agent 工具原生理解 MyKVM 协议；两端 MyKVM 各暴露一个 **stdio MCP server**（`mykvm --agent-mcp`），把 6.2 能力面翻译为 MCP 工具：

```
A 机 Agent(ZCode) ─stdio─ A 机 MCP server ─QUIC mykvm.agent.v1─ B 机 MyKVM ─执行─┐
                                    ▲                                            │
                                    └────────────── response（原路返回）──────────┘
```

Agent 侧工具命名建议：`peer_status` / `peer_log` / `peer_file_read` / `peer_file_put` / `peer_exec` / `peer_ask_human`（`peer_*` 前缀，多对端时带 device 参数）。任何支持 MCP 的工具（ZCode / Claude Code / Cursor）零改动接入。

### 6.5 与 halo 的联动（S4，差异化）

`human.ask` 触发对端：halo 光环在对端屏幕边缘高亮 + 主窗口气泡（照 runtimeBlocker 弹窗纪律）；可选（配置项）执行 `halo` 切屏把操作者视野带到请求机器——**AI 请求人工介入 → 硬件级注意力引导**，这是纯网络方案（SSH/云）无法提供的闭环，也是本方案区别于「又一个远程执行工具」的核心价值。

## 7. 阶段规划与验收

| 阶段 | 内容 | 验收标准 |
|---|---|---|
| P0 协议骨架 | `agent_channel.rs` + 协议包 + on_stream 分支 + 鉴权 + 两段式 RPC 闭环（echo 能力）+ 跨版本混跑测试 | 双端真实 QUIC 上 echo 往返；**旧版对端收到 agent 包优雅拒绝不崩不挂起**；`cargo test --lib` 全绿；新增单测覆盖鉴权否定路径（错 cluster/secret/白名单外） |
| P1 消息与只读 | `status.get` / `log.read` / `file.stat` / `file.read`（小文件）/ `msg.send` + 审计日志 + Tauri 命令 | A 端（通过手动 invoke）能拉到 B 端日志与状态；审计记录完整；L0 为默认级别 |
| P2 文件交换 | `file.write` + transfer_id 大载荷集成 + 进度 UI（复用 `file-transfer-progress`） | ≥100MiB 文件双向传输带进度与断点续传；与剪贴板并发不互饿（并发预算处理） |
| P3 命令控制 | `cmd.run` L2 白名单 + L3 审批 UI + 熔断开关 | 白名单外命令被拒并审计；L3 全流程：请求→对端弹窗（完整命令可见）→批准/拒绝→结果回传；熔断后键鼠/剪贴板不受影响 |
| P4 Agent 集成 | MCP stdio server + `human.ask` halo 联动 | ZCode 在 A 机通过 MCP 工具驱动 B 机完成一次真实联调循环（拉日志→改→构建→回传产物）；`human.ask` 光环提示可见 |
| P5（可选）无人值守 | 授权纯函数抽取 + headless_client 注册 on_stream | 服务模式下 agent 通道可用（独立评估安全影响后再决策是否做） |

实施顺序建议在 halo-screen-warp 分支合流后另开 `feat/agent-channel`，P0-P1 一个 PR，P2、P3 各一个 PR（P3 涉及审批 UI 需真机验收）。

## 8. 风险与开放问题

1. **审批窗钓鱼**：L3 审批只显示命令文本可能被 `cmd /c echo ...` 类混淆欺骗 → 审批窗显示**解析后的完整 argv + 工作目录 + 来源**，禁止仅显示一行字符串。
2. **输出编码**：Windows 控制台输出非 UTF-8（GBK 代码页）→ `cmd.run` 约定以 UTF-8 采集（`[Console]::OutputEncoding` 预设），失败时降级标注 `encoding: "lossy-utf8"`。
3. **并发预算**：8 条 stream 上限对「命令执行 + 文件传输 + 剪贴板」三业务可能不够 → P2 实测后决定提升上限还是按 capability 配额。
4. **request_id 防重放**：响应包同样过鉴权；request_id 带 TTL，重放旧请求在 pending 表查不到即拒绝。
5. **审计膨胀**：环形缓冲（如 2000 条）+ 落盘截断，不无限增长。
6. **开放问题（评审定）**：① `cmd.run` 是否允许交互式命令（默认否，仅批处理式）；② agent 通道是否随配对自动开启 L0（建议：是，因 L0 不超出 log/file 只读面，与现有 `fetch_client_log` 等价）；③ MCP server 是否需要鉴权本机调用者（建议：仅 localhost stdio，继承 OS 会话隔离，暂不加）。

## 9. 决策记录

| 决策 | 结论 | 理由 |
|---|---|---|
| 传输复用 47834 QUIC | 是 | 配对/钉扎/抗干扰成果直接继承；连接管理对称双向已支持 |
| RPC 形态 | 两段式（独立响应流 + request_id），不扩展 StreamHandler | 侵入最小；Log 请求先例已验证；天然支持长任务 |
| 集成面 | 底层自定义协议 + 上层 MCP stdio 网桥 | 底层保持 MyKVM 轻量鉴权模式；MCP 让存量 Agent 工具零改动接入 |
| 命令控制 | L0-L3 分级，默认 L0，L3 逐次审批 | 配对信任域已等于完全控制，但自动化程度需独立约束（§5.3） |
| 无人值守 | 暂不纳入（P5 独立评估） | 与剪贴板/文件通道一致（headless_client.rs:33 已声明该边界） |
