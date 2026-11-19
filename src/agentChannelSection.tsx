import { useCallback, useEffect, useState } from 'react'
import {
  AGENT_CHANNEL_LEVELS,
  agentAuditDirectionLabel,
  agentAuditStatusLabel,
  formatAgentAuditTime,
  resolveAgentChannelConfig,
  setAgentChannelLevel,
  toggleAgentChannel,
} from './agentChannelPanel'
import { agentChannelAudit, agentChannelInbox, agentChannelInboxClear } from './desktopApi'
import type { AppText } from './i18n'
import type {
  AgentChannelAuditEntry,
  AgentChannelConfig,
  AgentChannelInboxMessage,
} from './types'

interface AgentChannelPanelProps {
  /** layout.agentChannel；旧快照可能缺省（undefined → 兜底 Rust 默认）。 */
  config: AgentChannelConfig | undefined
  /** 变更回传：App 侧经 updateLayout 走既有 save_layout 链路。 */
  onChange: (config: AgentChannelConfig) => void
  ui: AppText
}

/**
 * 设置页「Agent 通道」区块（P1c，RFC §6.3）：总开关 + 级别 + 审计查看 +
 * 收件箱。克制版：不挂轮询（审计手动刷新；收件箱挂载拉一次），收到消息
 * 不做系统通知（P4 的 human.ask 才做提示）。
 */
export function AgentChannelPanel({ config, onChange, ui }: AgentChannelPanelProps) {
  const resolved = resolveAgentChannelConfig(config)
  const [auditEntries, setAuditEntries] = useState<AgentChannelAuditEntry[]>([])
  const [auditLoaded, setAuditLoaded] = useState(false)
  const [auditError, setAuditError] = useState('')
  const [inbox, setInbox] = useState<AgentChannelInboxMessage[]>([])
  const [inboxLoaded, setInboxLoaded] = useState(false)
  const [inboxError, setInboxError] = useState('')
  const [pending, setPending] = useState(false)

  const refreshAudit = useCallback(async () => {
    setPending(true)
    try {
      setAuditEntries(await agentChannelAudit())
      setAuditError('')
      setAuditLoaded(true)
    } catch (error) {
      setAuditError(error instanceof Error ? error.message : String(error))
    } finally {
      setPending(false)
    }
  }, [])

  // 挂载拉一次收件箱（不做轮询；后续消息靠手动刷新或重进页面查看）。
  // setState 只出现在 promise 回调里（照 App.tsx haloGetStatus 先例）。
  useEffect(() => {
    let active = true
    agentChannelInbox()
      .then((messages) => {
        if (!active) return
        setInbox(messages)
        setInboxError('')
        setInboxLoaded(true)
      })
      .catch((error) => {
        if (!active) return
        setInboxError(error instanceof Error ? error.message : String(error))
      })
    return () => {
      active = false
    }
  }, [])

  const clearInbox = useCallback(async () => {
    setPending(true)
    try {
      await agentChannelInboxClear()
      setInbox([])
      setInboxLoaded(true)
    } catch (error) {
      setInboxError(error instanceof Error ? error.message : String(error))
    } finally {
      setPending(false)
    }
  }, [])

  return (
    <section className="surface-card settings-card">
      <div className="card-title-row">
        <h2>{ui.settings.agentChannel}</h2>
        <span className={`update-status-badge ${resolved.enabled ? 'idle' : 'failed'}`}>
          {resolved.enabled ? ui.common.enabled : ui.common.disabled}
        </span>
      </div>
      <p className="muted-copy">{ui.settings.agentChannelCopy}</p>

      <div className="settings-control-row">
        <span>{ui.settings.agentChannelSwitch}</span>
        <div className="segmented-control">
          <button
            type="button"
            className={resolved.enabled ? 'active' : ''}
            onClick={() => onChange(toggleAgentChannel(resolved, true))}
          >
            {ui.settings.autostartOn}
          </button>
          <button
            type="button"
            className={!resolved.enabled ? 'active' : ''}
            onClick={() => onChange(toggleAgentChannel(resolved, false))}
          >
            {ui.settings.autostartOff}
          </button>
        </div>
      </div>

      <div className="settings-control-row">
        <span>{ui.settings.agentChannelLevel}</span>
        <div className="segmented-control">
          {AGENT_CHANNEL_LEVELS.map((option) => (
            <button
              key={option.value}
              type="button"
              className={resolved.level === option.value ? 'active' : ''}
              onClick={() => onChange(setAgentChannelLevel(resolved, option.value))}
            >
              {option.value}
              {option.deferred ? '*' : ''}
            </button>
          ))}
        </div>
      </div>
      <p className="muted-copy">{ui.settings.agentChannelLevelNote}</p>

      <details
        className="agent-audit-details"
        onToggle={(event) => {
          // 展开时拉一次审计（原生 details 的 toggle 语义，免额外 state）。
          if ((event.target as HTMLDetailsElement).open && !auditLoaded && !pending) {
            void refreshAudit()
          }
        }}
      >
        <summary>{ui.settings.agentChannelAuditTitle}</summary>
        <div className="inline-actions">
          <button
            type="button"
            className="secondary-button compact-button"
            disabled={pending}
            onClick={() => void refreshAudit()}
          >
            {pending ? ui.common.refreshing : ui.settings.agentChannelAuditRefresh}
          </button>
        </div>
        {auditError ? <p className="muted-copy">{auditError}</p> : null}
        {auditLoaded && auditEntries.length === 0 && !auditError ? (
          <p className="muted-copy">{ui.settings.agentChannelAuditEmpty}</p>
        ) : null}
        <ul className="agent-audit-list">
          {auditEntries.map((entry) => (
            <li key={`${entry.tsMs}-${entry.requestId}`}>
              <span className="agent-audit-time">{formatAgentAuditTime(entry.tsMs)}</span>
              <span className="agent-audit-direction">
                {agentAuditDirectionLabel(entry.direction)}
              </span>
              <span className="agent-audit-peer">{entry.peerId}</span>
              <span className="agent-audit-capability">{entry.capability}</span>
              <span className="agent-audit-status">{agentAuditStatusLabel(entry.status)}</span>
              <span className="agent-audit-summary">{entry.summary}</span>
            </li>
          ))}
        </ul>
      </details>

      <div className="card-title-row agent-inbox-row">
        <h3>{ui.settings.agentChannelInboxTitle}</h3>
        <button
          type="button"
          className="secondary-button compact-button"
          disabled={pending || inbox.length === 0}
          onClick={() => void clearInbox()}
        >
          {ui.settings.agentChannelInboxClear}
        </button>
      </div>
      {inboxError ? <p className="muted-copy">{inboxError}</p> : null}
      {inboxLoaded && inbox.length === 0 && !inboxError ? (
        <p className="muted-copy">{ui.settings.agentChannelInboxEmpty}</p>
      ) : null}
      <ul className="agent-inbox-list">
        {inbox.map((message) => (
          <li key={`${message.tsMs}-${message.peerId}`}>
            <span className="agent-inbox-meta">
              {formatAgentAuditTime(message.tsMs)} · {message.peerId}
            </span>
            {message.title ? (
              <strong className="agent-inbox-title">{message.title}</strong>
            ) : null}
            <span className="agent-inbox-text">
              {message.text}
              {message.truncated ? '…' : ''}
            </span>
          </li>
        ))}
      </ul>
    </section>
  )
}
