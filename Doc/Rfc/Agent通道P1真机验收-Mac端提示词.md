# Agent 通道 P1 真机验收 —— Mac 端执行提示词

- 状态：待执行
- 日期：2026-11-09
- 适用：Mac 端 AI 助手（一次性任务：拉取、验证、部署、配合联调、回报证据）
- 背景：`feat/agent-channel` 分支实现了跨机 Agent 互联通道的 P0+P1（协议 `mykvm.agent.v1`、鉴权、L0 只读能力 `echo/status.get/msg.send/log.read/file.stat/file.read`、审计、Settings 面板）。现需双端部署后与 Windows 端联调验收。

## 纪律（先读）

1. 全程简体中文（思考、输出、提交信息）。
2. **所有结论必须附真实命令输出原文**；失败如实报告并停止，禁止伪造、禁止静默换方案。
3. 本任务是验收，**禁止修改任何代码、禁止 push**；遇到文档与实际冲突，停下报告，等待人工决策。
4. 参考仓库根 AGENTS.md（已知系统坑清单）。

## 第一步：拉取分支并核对基线

```bash
cd <本机 mykvm 仓库路径>
git fetch origin
git checkout feat/agent-channel
git log --oneline -1   # 期望 HEAD 为 44b0922（chore(构建): 开启 withGlobalTauri…）或更新
git status             # 必须干净（允许存在未跟踪的本地文件）
```

## 第二步：环境与测试基线

```bash
export PATH="$HOME/.cargo/bin:$PATH"
rustc --version        # 记录版本
cd src-tauri && cargo test --lib 2>&1 | tail -2   # 期望 0 failed（总数约 597；Unix 侧 symlink 用例生效，数目允许与 Win 端有平台差异——以 0 failed 为准，如实报告实际数字）
cd .. && npm install 2>&1 | tail -1               # 依赖已就绪则很快
npm test 2>&1 | tail -6                             # 期望 pass 63 / fail 0
```

## 第三步：部署（验收形态 = dev 模式）

说明：验收需要在 webview devtools console 里执行 `invoke`，因此用 dev 模式（debug 构建才带 devtools；正式版无）。**先正常退出正式版 mykvm.app**（托盘/菜单退出；不要强杀进程，更不要动 input 相关服务——见 AGENTS.md 重启坑）。

```bash
npm run tauri:dev      # 或 sh scripts/run-tauri-dev.sh
```

首次编译需数分钟。窗口出现后自检：

1. 日志：`tail -f ~/Library/Logs/com.xzhpl.mykvm/mykvm.log` 应有 `discovery started`，数秒内出现 `discovery peer updated`（Windows 对端需在线）。若重装过正式版导致权限/会话扰动，重启一次 dev 实例再观察。
2. UI：设置页出现「**Agent 通道**」区块（总开关默认开、级别默认 readonly、审计折叠列表、收件箱列表）。
3. devtools：webview 内右键 →「检查/Inspect」→ Console 输入 `window.__TAURI__.core` 回显对象即说明全局注入生效（本次验收的调用通道）。

## 第四步：配合 Windows 端联调（被动核对）

Windows 端将依次发起 4 个请求（echo / status.get / log.read / msg.send）。收到后核对：

1. 审计落盘：`cat ~/Library/Application\ Support/com.xzhpl.mykvm/agent-audit.jsonl` 应出现 4 条 `"direction":"in"` 记录（capability 与状态对得上，summary 不含任何凭据）。
2. UI：设置页 Agent 区块「最近审计」刷新可见同样记录；收件箱出现 Windows 端发来的消息文本。
3. 日志无 `agent` 相关 warn/error。

## 第五步（Mac 为 server 角色时可选）：主动发起一次反向验证

```js
// webview Console（deviceId 换成 Windows 端设备 id，见下方获取方法）
const state = await window.__TAURI__.core.invoke('read_app_state');
// 从 state 中找到对端 device 的 id 后：
await window.__TAURI__.core.invoke('agent_channel_request', {
  deviceId: '<Win端设备id>',
  capability: 'echo',
  payload: { from: 'mac-p1-verify' },
})
// 再试：capability: 'status.get', payload: {}；capability: 'log.read', payload: { tailBytes: 65536 }
```

期望：echo 原样回显；status.get 返回 Win 端 `{machineRole, inputMode, agentEnabled, agentLevel, platform, hostname, deviceId, appVersion, quicPort}`；log.read 返回日志尾部文本。任何「对端不在线 / 版本过旧 / 超时」错误原样回报。

## 第六步：证据回报清单

1. `git log --oneline -1` 与 `git status --short`。
2. `cargo test` 与 `npm test` 的统计行原文。
3. dev 启动后日志中 `discovery started` / `discovery peer updated` 行原文。
4. `agent-audit.jsonl` 的 in 记录原文（脱敏无需——设计上就不含凭据）。
5. 若执行了第五步：各 capability 的返回 JSON 原文。
6. 任何异常/与本文档不符之处，如实列出。
