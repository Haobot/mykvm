import test from 'node:test';
import assert from 'node:assert/strict';
import {
  AGENT_CHANNEL_LEVELS,
  agentAuditDirectionLabel,
  agentAuditStatusLabel,
  agentChannelInboxClearVia,
  agentChannelAuditVia,
  agentChannelInboxVia,
  agentChannelRequestVia,
  resolveAgentChannelConfig,
  setAgentChannelLevel,
  toggleAgentChannel,
} from './agentChannelPanel.ts';
import type { AgentChannelConfig } from './types.ts';

// —— desktopApi wrapper 的参数契约（锁定 snake_case 命令名 + camelCase 参数，
//    与 Rust 端 #[tauri::command] 的参数解析约定一致） ——

test('agentChannelRequestVia 调用 snake_case 命令且参数为 camelCase', async () => {
  const calls: Array<{ command: string; args: Record<string, unknown> }> = [];
  const invoke = async (command: string, args?: Record<string, unknown>) => {
    calls.push({ command, args: args ?? {} });
    return { status: 'ok', payload: { deviceId: 'peer-1' } };
  };

  const response = (await agentChannelRequestVia(
    invoke,
    'peer-1',
    'log.read',
    { tailBytes: 1024 },
    5000,
  )) as { status: string; payload: unknown };

  assert.equal(calls.length, 1);
  assert.equal(calls[0].command, 'agent_channel_request');
  assert.deepEqual(calls[0].args, {
    deviceId: 'peer-1',
    capability: 'log.read',
    payload: { tailBytes: 1024 },
    timeoutMs: 5000,
  });
  assert.equal(response.status, 'ok');
});

test('agentChannelRequestVia 的 timeoutMs 缺省传 null（后端按默认 10s 处理）', async () => {
  const calls: Array<Record<string, unknown> | undefined> = [];
  const invoke = async (_command: string, args?: Record<string, unknown>) => {
    calls.push(args);
    return { status: 'ok', payload: {} };
  };

  await agentChannelRequestVia(invoke, 'peer-1', 'status.get', {});
  assert.equal(calls[0]?.timeoutMs, null, '缺省超时必须显式传 null');
});

test('agentChannelAuditVia / agentChannelInboxVia / agentChannelInboxClearVia 契约', async () => {
  const audit = [{ tsMs: 1, direction: 'out', peerId: 'p', capability: 'log.read', requestId: '1-a', status: 'ok', durationMs: 3, summary: 's' }];
  const inbox = [{ tsMs: 2, peerId: 'p', title: null, text: 'hello', truncated: false }];

  assert.deepEqual(await agentChannelAuditVia(async () => audit, 50), audit);
  const auditCalls: Array<Record<string, unknown> | undefined> = [];
  await agentChannelAuditVia(async (_c, args) => {
    auditCalls.push(args);
    return [];
  }, 50);
  assert.deepEqual(auditCalls[0], { limit: 50 }, 'audit 的 limit 参数为 camelCase');

  assert.deepEqual(await agentChannelInboxVia(async () => inbox), inbox);
  assert.deepEqual(await agentChannelInboxVia(async () => []), []);

  assert.equal(await agentChannelInboxClearVia(async () => 3), 3);
  const clearCommands: string[] = [];
  await agentChannelInboxClearVia(async (command) => {
    clearCommands.push(command);
    return 0;
  });
  assert.deepEqual(clearCommands, ['agent_channel_inbox_clear']);
});

// —— Agent 面板 UI 的可测逻辑 ——

test('resolveAgentChannelConfig 对旧快照缺省回 Rust 默认（enabled=true, readonly）', () => {
  const resolved = resolveAgentChannelConfig(undefined);
  assert.deepEqual(resolved, {
    enabled: true,
    level: 'readonly',
    fileRoots: [],
    cmdWhitelist: [],
    approvalTtlSecs: 300,
  });
  // 已有配置原样保留（不吞用户意图）。
  const existing: AgentChannelConfig = {
    enabled: false,
    level: 'full',
    fileRoots: ['/tmp'],
    cmdWhitelist: ['cargo'],
    approvalTtlSecs: 60,
  };
  assert.deepEqual(resolveAgentChannelConfig(existing), existing);
});

test('toggleAgentChannel 只改 enabled，不触碰其余字段（不可变更新）', () => {
  const config = resolveAgentChannelConfig(undefined);
  const next = toggleAgentChannel(config, false);
  assert.equal(next.enabled, false);
  assert.equal(next.level, config.level);
  assert.notEqual(next, config, '必须返回新对象（React 不可变更新）');
});

test('setAgentChannelLevel 只改 level', () => {
  const config = resolveAgentChannelConfig(undefined);
  const next = setAgentChannelLevel(config, 'commands');
  assert.equal(next.level, 'commands');
  assert.equal(next.enabled, config.enabled);
  assert.notEqual(next, config);
});

test('级别选项四档齐全，files/commands/full 标注后续开放', () => {
  assert.deepEqual(
    AGENT_CHANNEL_LEVELS.map((option) => option.value),
    ['readonly', 'files', 'commands', 'full'],
  );
  assert.equal(AGENT_CHANNEL_LEVELS.find((o) => o.value === 'readonly')?.deferred, false);
  assert.equal(AGENT_CHANNEL_LEVELS.find((o) => o.value === 'files')?.deferred, true);
  assert.equal(AGENT_CHANNEL_LEVELS.find((o) => o.value === 'commands')?.deferred, true);
  assert.equal(AGENT_CHANNEL_LEVELS.find((o) => o.value === 'full')?.deferred, true);
});

test('审计方向/状态标签映射为中文', () => {
  assert.equal(agentAuditDirectionLabel('in'), '收');
  assert.equal(agentAuditDirectionLabel('out'), '发');
  assert.equal(agentAuditDirectionLabel('weird'), 'weird', '未知值原样展示不崩');

  assert.equal(agentAuditStatusLabel('ok'), '成功');
  assert.equal(agentAuditStatusLabel('err'), '失败');
  assert.equal(agentAuditStatusLabel('denied'), '拒绝');
  assert.equal(agentAuditStatusLabel('timeout'), '超时');
  assert.equal(agentAuditStatusLabel('weird'), 'weird');
});
