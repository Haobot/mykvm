import assert from "node:assert/strict";
import { test } from "node:test";
import {
  findLivePeerForDevice,
  findRepairDeviceForPeer,
  repairHostCandidates,
  upsertPeerDevice,
} from "./pairingRepair.ts";
import type { LanPeer } from "./runtime.ts";
import type { Device, LayoutState, Screen } from "./types.ts";

function makeScreen(deviceId: string, index = 0): Screen {
  return {
    id: `${deviceId}-display-${index + 1}`,
    deviceId,
    name: `Display ${index + 1}`,
    x: index * 100,
    y: 0,
    width: 1920,
    height: 1080,
    scale: 1,
    isPrimary: index === 0,
  };
}

function makeDevice(overrides: Partial<Device> = {}): Device {
  const id = overrides.id ?? "macbook-pro";
  return {
    id,
    name: "MacBook Pro",
    platform: "macos",
    host: "192.168.3.44",
    transportPort: 47831,
    quicPort: 47832,
    transportPublicKey: "old-key",
    protocolVersion: 3,
    color: "#0f766e",
    online: false,
    inputReady: false,
    role: "client",
    source: "detected",
    screens: [makeScreen(id)],
    ...overrides,
  };
}

function makePeer(overrides: Partial<LanPeer> = {}): LanPeer {
  return {
    id: "macbook-pro",
    name: "MacBook Pro",
    platform: "macos",
    machineRole: "client",
    clusterId: "cluster-a",
    pairingRequired: false,
    host: "MacBook-Pro / 192.168.3.55",
    ip: "192.168.3.55",
    transportPort: 47831,
    quicPort: 47832,
    transportPublicKey: "new-key",
    protocolVersion: 3,
    screenCount: 1,
    inputReady: true,
    screens: [
      {
        id: "display-1",
        name: "Built-in",
        x: 0,
        y: 0,
        width: 1512,
        height: 982,
        scale: 2,
        isPrimary: true,
      },
    ],
    appVersion: "0.9.13",
    lastSeenMs: 1,
    ...overrides,
  };
}

function makeLayout(devices: Device[]): LayoutState {
  return {
    devices,
    activeDeviceId: devices[0]?.id ?? "",
    selectedScreenId: devices[0]?.screens[0]?.id ?? "",
    inputMode: "control",
    machineRole: "server",
    clusterId: "cluster-a",
    pairSecret: "pair-secret",
    pairedControllers: [],
    clipboardSync: true,
    fileTransferEnabled: true,
    language: "cn",
    themeMode: "system",
    performanceMonitor: false,
    transportPortMode: "auto",
    transportPort: 47831,
    quicPort: 47832,
    modifierRemap: false,
    modifierMap: { control: "control", alt: "alt", meta: "meta" },
    edgeSwitchHotkey: "",
    screenSwitchHotkeys: { left: "", right: "", up: "", down: "" },
  };
}

test("重新配对候选地址优先使用实时对端 IP，而不是设备保存的旧 host", () => {
  const device = makeDevice({ host: "192.168.3.44" });
  const peer = makePeer();
  const candidates = repairHostCandidates(device, [peer], makeLayout([device]));

  assert.equal(candidates[0], "192.168.3.55");
  assert.ok(candidates.includes("192.168.3.44"), "旧 host 仍作为回退候选保留");
  assert.equal(
    new Set(candidates).size,
    candidates.length,
    "候选地址不应重复",
  );
});

test("设备 host 里的主机名与多个地址会被拆开并去重", () => {
  const device = makeDevice({
    id: "win-box",
    name: "Windows",
    host: "Windows / 192.168.3.44, 10.0.0.7",
    transportPublicKey: "old-key",
  });
  const peer = makePeer({
    id: "unrelated",
    name: "Other machine",
    transportPublicKey: "another-key",
  });

  assert.deepEqual(repairHostCandidates(device, [peer]), [
    "Windows",
    "192.168.3.44",
    "10.0.0.7",
  ]);
});

test("manual 设备没有实时对端时回退到保存的 host", () => {
  const device = makeDevice({
    name: "NAT 转发机",
    source: "manual",
    host: "203.0.113.7:47833",
    transportPublicKey: "old-key",
  });

  assert.deepEqual(repairHostCandidates(device, []), ["203.0.113.7:47833"]);
});

test("证书轮换后仍能按名称找到同名离线设备用于重新配对", () => {
  const device = makeDevice({
    online: false,
    transportPublicKey: "rotated-key",
    host: "192.168.3.44",
  });
  const peer = makePeer();
  const layout = makeLayout([device]);

  assert.equal(findRepairDeviceForPeer(layout, [peer], peer)?.id, device.id);
  assert.equal(findLivePeerForDevice(layout, device, [peer])?.id, peer.id);
});

test("对端改名后仍能按 host 中的主机名找回离线设备", () => {
  const device = makeDevice({
    name: "工作机",
    host: "MacBook-Pro / 192.168.3.44",
    online: false,
    transportPublicKey: "rotated-key",
  });
  const peer = makePeer({
    id: "renamed-peer",
    name: "已改名的机器",
    ip: "192.168.3.55",
    host: "MacBook-Pro / 192.168.3.55",
  });

  assert.equal(
    findRepairDeviceForPeer(makeLayout([device]), [peer], peer)?.id,
    device.id,
  );
});

test("已经能匹配到设备时不返回重新配对目标（避免误伤在线设备）", () => {
  const peer = makePeer();
  const device = makeDevice({
    online: true,
    transportPublicKey: peer.transportPublicKey,
  });

  assert.equal(
    findRepairDeviceForPeer(makeLayout([device]), [peer], peer),
    undefined,
  );
  assert.equal(
    findRepairDeviceForPeer(makeLayout([device]), [peer], makePeer({ id: "other" })),
    undefined,
  );
});

test("重新配对成功后原位更新设备，不产生重复并保留速度与颜色", () => {
  const device = makeDevice({
    name: "工作 MacBook",
    host: "192.168.3.44",
    online: false,
    pointerSpeed: 1.5,
    scrollSpeed: 2,
    color: "#be123c",
  });
  const peer = makePeer();

  const next = upsertPeerDevice(makeLayout([device]), peer, "", false, device.id);

  assert.equal(next.devices.length, 1, "不应新增重复设备");
  const updated = next.devices[0];
  assert.equal(updated.id, device.id);
  assert.equal(updated.name, "工作 MacBook", "重新配对不应覆盖用户可见名称");
  assert.equal(updated.host, "192.168.3.55");
  assert.equal(updated.transportPublicKey, "new-key");
  assert.equal(updated.online, true);
  assert.equal(updated.pointerSpeed, 1.5);
  assert.equal(updated.scrollSpeed, 2);
  assert.equal(updated.color, "#be123c");
  assert.equal(next.activeDeviceId, device.id);
  assert.equal(next.selectedScreenId, device.screens[0].id);
  assert.equal(updated.screens[0]?.x, device.screens[0].x);
});

test("重新配对成功时 manual 设备也改用对端实时地址（显式修复）", () => {
  const device = makeDevice({
    source: "manual",
    host: "203.0.113.7:47833",
  });
  const peer = makePeer();

  const next = upsertPeerDevice(makeLayout([device]), peer, "", true, device.id);

  assert.equal(next.devices.length, 1);
  assert.equal(next.devices[0].host, "192.168.3.55");
  assert.equal(next.devices[0].source, "manual");
});

test("普通发现合并仍保留手动设备地址（#26 语义不回退）", () => {
  const device = makeDevice({
    source: "manual",
    host: "203.0.113.7:47833",
    transportPublicKey: "",
  });
  const peer = makePeer();

  const next = upsertPeerDevice(makeLayout([device]), peer);

  assert.equal(next.devices.length, 1);
  assert.equal(next.devices[0].host, "203.0.113.7:47833");
});

// 以下用例专门锁住"只能靠"的匹配分支：对端 id、传输公钥、cluster、host
// 全部不同，删掉对应逻辑会直接变红（verifier 灵敏度分析要求）。

test("IP/证书/对端 id 全变后只有 preferredDeviceId 能原位更新，不新增重复设备", () => {
  const device = makeDevice({
    id: "old-abc",
    name: "工作 MacBook",
    host: "192.168.3.44",
    transportPublicKey: "old-key",
    color: "#be123c",
    pointerSpeed: 1.5,
    scrollSpeed: 2,
    online: false,
  });
  const peer = makePeer({
    id: "new-xyz",
    name: "工作 MacBook",
    ip: "192.168.3.99",
    host: "工作 MacBook / 192.168.3.99",
    transportPublicKey: "rotated-key",
  });
  const layout = makeLayout([device]);

  // 没有 preferredDeviceId 时：id 与公钥都对不上，只能新增一台重复设备。
  const withoutPreferred = upsertPeerDevice(layout, peer);
  assert.equal(
    withoutPreferred.devices.length,
    2,
    "该 fixture 下旧兜底不命中，才会新增重复设备",
  );

  const next = upsertPeerDevice(layout, peer, "", false, "old-abc");

  assert.equal(next.devices.length, 1, "preferredDeviceId 应原位更新而不是新增");
  const updated = next.devices.find((candidate) => candidate.id === "old-abc");
  assert.ok(updated, "原设备应保留自己的 id");
  assert.equal(updated.host, "192.168.3.99");
  assert.equal(updated.transportPublicKey, "rotated-key");
  assert.equal(updated.color, "#be123c");
  assert.equal(updated.pointerSpeed, 1.5);
  assert.equal(updated.scrollSpeed, 2);
  assert.equal(next.activeDeviceId, "old-abc");
});

test("id/cluster/host 都不同、仅名称相同时 findLivePeerForDevice 仍能命中并优先用其 IP", () => {
  const device = makeDevice({
    id: "old-abc",
    name: "MacBook Pro",
    host: "192.168.3.44",
    transportPublicKey: "old-key",
  });
  const peer = makePeer({
    id: "new-xyz",
    name: "MacBook Pro",
    ip: "192.168.3.77",
    host: "MacBook-Pro / 192.168.3.77",
    transportPublicKey: "rotated-key",
    clusterId: "cluster-b",
  });
  const layout = makeLayout([device]);

  assert.equal(findLivePeerForDevice(layout, device, [peer])?.id, "new-xyz");
  assert.equal(
    repairHostCandidates(device, [peer], layout)[0],
    "192.168.3.77",
    "名称兜底命中后，重新配对应首选实时对端 IP 而不是旧 host",
  );
});

test("在线设备不会被当作重新配对目标（避免把正常设备改坏）", () => {
  const device = makeDevice({
    name: "MacBook Pro",
    online: true,
    transportPublicKey: "rotated-key",
  });
  const peer = makePeer();

  // 名称/主机名一致，但设备在线：它不是"离线残留"，不应成为 repair 目标。
  assert.equal(findRepairDeviceForPeer(makeLayout([device]), [peer], peer), undefined);
});

test("已被其它实时对端认领的设备不会被抢作重新配对目标", () => {
  const device = makeDevice({
    id: "old-abc",
    name: "MacBook Pro",
    online: false,
    transportPublicKey: "existing-key",
  });
  const claimingPeer = makePeer({
    id: "claiming-peer",
    name: "MacBook Pro",
    transportPublicKey: "existing-key",
  });
  const targetPeer = makePeer({
    id: "target-peer",
    name: "MacBook Pro",
    ip: "192.168.3.55",
    transportPublicKey: "new-key",
  });
  const layout = makeLayout([device]);

  // claimingPeer 按公钥认领了该设备，targetPeer 只是同名，不应抢走它。
  assert.equal(
    findRepairDeviceForPeer(layout, [claimingPeer, targetPeer], targetPeer),
    undefined,
  );
});
