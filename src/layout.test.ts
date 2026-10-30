import test from 'node:test';
import assert from 'node:assert/strict';
import { flattenScreens, separateOverlappingScreens } from './layout.ts';
import type { Device, LayoutState, Screen } from './types.ts';

function screen(
  id: string,
  deviceId: string,
  x: number,
  y: number,
  width: number,
  height: number,
): Screen {
  return { id, deviceId, name: id, x, y, width, height, scale: 1, isPrimary: false };
}

function device(id: string, role: Device['role'], screens: Screen[]): Device {
  return {
    id,
    name: id,
    platform: role === 'local' ? 'macos' : 'windows',
    host: role === 'local' ? '127.0.0.1' : '10.0.0.2',
    transportPort: 47833,
    quicPort: 47834,
    transportPublicKey: '',
    protocolVersion: 1,
    color: '#2f7af8',
    online: true,
    inputReady: true,
    role,
    source: 'detected',
    screens,
  };
}

function layoutWith(localScreens: Screen[], peerScreens: Screen[]): LayoutState {
  return {
    activeDeviceId: 'local-device',
    selectedScreenId: localScreens[0].id,
    inputMode: 'control',
    machineRole: 'server',
    clusterId: 'cluster',
    pairSecret: 'secret',
    pairedControllers: [],
    clipboardSync: false,
    fileTransferEnabled: true,
    language: 'cn',
    themeMode: 'system',
    performanceMonitor: false,
    transportPortMode: 'auto',
    transportPort: 47833,
    quicPort: 47834,
    modifierRemap: true,
    modifierMap: { control: 'meta', alt: 'same', meta: 'control' },
    edgeSwitchHotkey: 'alt+shift+k',
    screenSwitchHotkeys: {
      left: 'alt+left',
      right: 'alt+right',
      up: 'alt+up',
      down: 'alt+down',
    },
    devices: [
      device('local-device', 'local', localScreens),
      device('peer-device', 'client', peerScreens),
    ],
  };
}

function overlappingPairs(layout: LayoutState): string[] {
  const screens = flattenScreens(layout);
  const pairs: string[] = [];
  for (let i = 0; i < screens.length; i += 1) {
    for (let j = i + 1; j < screens.length; j += 1) {
      const a = screens[i];
      const b = screens[j];
      if (
        a.x < b.x + b.width &&
        a.x + a.width > b.x &&
        a.y < b.y + b.height &&
        a.y + a.height > b.y
      ) {
        pairs.push(`${a.id}|${b.id}`);
      }
    }
  }
  return pairs;
}

test('完全重叠的屏幕被推开成恰好相切，不再重叠', () => {
  const local = screen('local-display-1', 'local-device', 0, 0, 1920, 1080);
  const peer = screen('peer-device-display-1', 'peer-device', 0, 0, 1920, 1080);
  const layout = layoutWith([local], [peer]);

  const next = separateOverlappingScreens(layout, peer.id);
  const moved = next.devices[1].screens[0];

  assert.deepEqual(overlappingPairs(next), []);
  // 沿重叠较小的 Y 轴分离，两屏上/下边恰好相切。
  assert.equal(moved.x, 0);
  assert.equal(moved.y, -1080);
  assert.equal(moved.y + moved.height, local.y);
});

test('部分重叠沿重叠较小的轴分离，位移最小', () => {
  const local = screen('local-display-1', 'local-device', 0, 0, 1920, 1080);
  const peer = screen('peer-device-display-1', 'peer-device', 1800, 200, 1920, 1080);
  const layout = layoutWith([local], [peer]);

  const next = separateOverlappingScreens(layout, peer.id);
  const moved = next.devices[1].screens[0];

  assert.deepEqual(overlappingPairs(next), []);
  // X 向重叠仅 120、Y 向 880，取 X 轴：贴到本机右边缘。
  assert.equal(moved.x, 1920);
  assert.equal(moved.y, 200);
  assert.equal(moved.x, local.x + local.width);
});

test('连锁重叠时迭代收敛，最终无两两重叠', () => {
  const localA = screen('local-display-1', 'local-device', 0, 0, 1000, 1000);
  const localB = screen('local-display-2', 'local-device', 1000, 0, 1000, 1000);
  const peer = screen('peer-device-display-1', 'peer-device', 900, 900, 1000, 1000);
  const layout = layoutWith([localA, localB], [peer]);

  const next = separateOverlappingScreens(layout, peer.id);
  const moved = next.devices[1].screens[0];

  assert.deepEqual(overlappingPairs(next), []);
  assert.equal(moved.y, 1000);
});
