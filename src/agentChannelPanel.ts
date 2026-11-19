/**
 * 跨机 Agent 通道（P1c）Settings 区块的可测纯逻辑。
 *
 * 仓库的前端测试是 node:test 纯逻辑测试（无组件渲染设施），照
 * layoutPersist / runtimeBlocker 的「逻辑独立模块 + 注入式测试」先例：
 * - desktopApi wrapper 的 Tauri 分支经由 `*Via(invoke, ...)` 锁定命令名与
 *   camelCase 参数名（snake_case 命令 + camelCase 参数是仓库 invoke 约定，
 *   走错命名后端会静默收到 undefined，必须用测试钉住）；
 * - 组件（agentChannelPanel.tsx）只做渲染，状态更新语义在这里锁定。
 */
import type { AgentChannelConfig, AgentChannelLevel } from './types'

/** 与 node 端测试对齐的 invoke 形状（@tauri-apps/api/core 的子集）。 */
export type TauriInvoker = (
  command: string,
  args?: Record<string, unknown>,
) => Promise<unknown>

/** 发起 agent 请求（命令名/参数名契约）。timeoutMs 缺省显式传 null。 */
export function agentChannelRequestVia(
  invoke: TauriInvoker,
  deviceId: string,
  capability: string,
  payload: unknown,
  timeoutMs?: number,
): Promise<unknown> {
  return invoke('agent_channel_request', {
    deviceId,
    capability,
    payload,
    timeoutMs: timeoutMs ?? null,
  })
}

/** 审计快照（最新在前）。limit 缺省不传（后端默认 100、钳 2000）。 */
export function agentChannelAuditVia(
  invoke: TauriInvoker,
  limit?: number,
): Promise<unknown> {
  return invoke('agent_channel_audit', limit === undefined ? {} : { limit })
}

/** 收件箱快照。 */
export function agentChannelInboxVia(invoke: TauriInvoker): Promise<unknown> {
  return invoke('agent_channel_inbox', {})
}

/** 清空收件箱，返回清掉的条数。 */
export function agentChannelInboxClearVia(invoke: TauriInvoker): Promise<unknown> {
  return invoke('agent_channel_inbox_clear', {})
}

/** 级别选项（RFC §5.3 L0-L3）。deferred = 对应能力后续版本开放（当前仅 L0 生效）。 */
export const AGENT_CHANNEL_LEVELS: Array<{
  value: AgentChannelLevel
  deferred: boolean
}> = [
  { value: 'readonly', deferred: false },
  { value: 'files', deferred: true },
  { value: 'commands', deferred: true },
  { value: 'full', deferred: true },
]

/**
 * 旧快照缺省兜底：与 Rust `AgentChannelConfig::default()` 一致
 * （enabled=true, readonly，见 agent_channel/config.rs Default impl）。
 */
export function resolveAgentChannelConfig(
  config: AgentChannelConfig | undefined,
): AgentChannelConfig {
  return (
    config ?? {
      enabled: true,
      level: 'readonly',
      fileRoots: [],
      cmdWhitelist: [],
      approvalTtlSecs: 300,
    }
  )
}

/** 总开关切换（不可变更新，只改 enabled）。 */
export function toggleAgentChannel(
  config: AgentChannelConfig,
  enabled: boolean,
): AgentChannelConfig {
  return { ...config, enabled }
}

/** 级别切换（不可变更新，只改 level）。 */
export function setAgentChannelLevel(
  config: AgentChannelConfig,
  level: AgentChannelLevel,
): AgentChannelConfig {
  return { ...config, level }
}

/** 审计方向的中文标签；未知值原样展示（不因新枚举值崩 UI）。 */
export function agentAuditDirectionLabel(direction: string): string {
  if (direction === 'in') return '收'
  if (direction === 'out') return '发'
  return direction
}

/** 审计状态的中文标签；未知值原样展示。 */
export function agentAuditStatusLabel(status: string): string {
  if (status === 'ok') return '成功'
  if (status === 'err') return '失败'
  if (status === 'denied') return '拒绝'
  if (status === 'timeout') return '超时'
  return status
}

/**
 * 审计/收件箱时间戳 → 本地时区 HH:MM:SS（审计条目都在当天附近，日期由
 * 会话上下文补足）。缺省 0 渲染占位符，不渲染误导性的 1970。
 */
export function formatAgentAuditTime(tsMs: number): string {
  if (!Number.isFinite(tsMs) || tsMs <= 0) return '--:--:--'
  const date = new Date(tsMs)
  const pad = (value: number) => String(value).padStart(2, '0')
  return `${pad(date.getHours())}:${pad(date.getMinutes())}:${pad(date.getSeconds())}`
}
