import assert from "node:assert/strict";
import { test } from "node:test";
import {
  findIdentityDriftPeer,
  findLivePeerForDevice,
  findRepairDeviceForPeer,
  identityDriftFingerprint,
  listIdentityDrifts,
  peerKey,
  repairHostCandidates,
  uniqueWeakMatches,
  upsertPeerDevice,
  weakMatchesPeer,
} from "./pairingRepair.ts";
// 身份漂移弹窗通道（P0-3）住在 runtimeBlocker.ts；它的单测文件不在本次允许
// 写入范围内，所以这里一并覆盖「同因只弹一次 / 解除自动关 / 不堆叠」。
import {
  createIdentityDriftTracker,
  dismissIdentityDrift,
  trackIdentityDrift,
} from "./runtimeBlocker.ts";
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
  // P0-2b.4 新语义（原断言「原设备应保留自己的 id / activeDeviceId=old-abc」
  // 按新规则更新，理由：非 manual 设备修复后 id 自动跟随对端派生规则
  // peerDeviceId(peer)，与后端 retarget 同源，关闭「换 IP 后 id/host 双失配」；
  // 原 id 不再存在，且不产生重复设备）。
  const updated = next.devices.find((candidate) => candidate.id === "new-xyz");
  assert.ok(updated, "非 manual 设备修复后应采用对端派生的新 id");
  assert.equal(
    next.devices.some((candidate) => candidate.id === "old-abc"),
    false,
    "旧 id 应被替换掉而不是残留成第二台设备",
  );
  assert.equal(updated.host, "192.168.3.99");
  assert.equal(updated.transportPublicKey, "rotated-key");
  assert.equal(updated.color, "#be123c");
  assert.equal(updated.pointerSpeed, 1.5);
  assert.equal(updated.scrollSpeed, 2);
  // activeDeviceId 跟随新 id：仍是同一台设备的引用。
  assert.equal(next.activeDeviceId, "new-xyz");
  // 屏幕摆放不丢：设备 id 变化导致屏幕 id 前缀变化，按 index 对位保留原坐标
  // （新语义新增断言，防止 id 跟随把用户摆放重置回默认）。
  assert.equal(updated.screens[0]?.deviceId, "new-xyz");
  assert.equal(updated.screens[0]?.x, device.screens[0].x);
  assert.equal(updated.screens[0]?.y, device.screens[0].y);
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

// ---------- P0-2a 弱匹配（仅恢复状态显示，不触碰信任） ----------

test("弱匹配：证书轮换后同 cluster 同名设备唯一命中", () => {
  const device = makeDevice({ online: false, transportPublicKey: "old-key" });
  const peer = makePeer({ transportPublicKey: "new-key" });
  const layout = makeLayout([device]);

  // 公钥不一致 → 强匹配失败；名称/cluster 信号唯一 → 弱匹配命中。
  assert.equal(weakMatchesPeer(layout, device, peer), true);
  const weak = uniqueWeakMatches(layout, [peer], new Set(), new Set());
  assert.equal(weak.get(device.id), peer);
});

test("弱匹配：名称大小写与空白不敏感，host 按 '/' 拆段也能命中", () => {
  const byName = makeDevice({ name: "  windows box ", online: false });
  const namePeer = makePeer({ name: "Windows Box" });
  assert.equal(weakMatchesPeer(makeLayout([byName]), byName, namePeer), true);

  const renamed = makeDevice({
    id: "renamed-device",
    name: "工作机",
    host: "Win-Box / 192.168.3.44",
    online: false,
  });
  const hostPeer = makePeer({ id: "peer-b", name: "其它名字", host: "win-box" });
  assert.equal(weakMatchesPeer(makeLayout([renamed]), renamed, hostPeer), true);
});

test("弱匹配：已被强匹配认领的设备不会被其它 peer 抢走（防抢占）", () => {
  const device = makeDevice({
    online: false,
    transportPublicKey: "existing-key",
  });
  const claimingPeer = makePeer({
    id: "claiming",
    transportPublicKey: "existing-key",
  });
  const driftPeer = makePeer({ id: "drifted", transportPublicKey: "new-key" });
  const layout = makeLayout([device]);

  const claimedDeviceIds = new Set([device.id]);
  const claimedPeerIds = new Set([peerKey(claimingPeer)]);
  assert.equal(
    weakMatchesPeer(layout, device, driftPeer, {
      claimedDeviceIds,
      claimedPeerIds,
      peers: [claimingPeer, driftPeer],
    }),
    false,
    "设备已被 claimingPeer 强匹配认领，弱匹配不得抢占",
  );
  assert.equal(
    uniqueWeakMatches(
      layout,
      [claimingPeer, driftPeer],
      claimedDeviceIds,
      claimedPeerIds,
    ).size,
    0,
    "认领后的 pass 里不应再产生弱匹配",
  );
});

test("弱匹配：设备侧或 peer 侧存在第二个弱候选时双向都不命中", () => {
  // 设备侧歧义：两台同名设备对着一个 peer。
  const twinA = makeDevice({ id: "twin-a", name: "Twin", online: false });
  const twinB = makeDevice({ id: "twin-b", name: "Twin", online: false });
  const twinPeer = makePeer({ id: "twin-peer", name: "Twin" });
  const twinLayout = makeLayout([twinA, twinB]);
  assert.equal(weakMatchesPeer(twinLayout, twinA, twinPeer), false);
  assert.equal(weakMatchesPeer(twinLayout, twinB, twinPeer), false);
  assert.equal(
    uniqueWeakMatches(twinLayout, [twinPeer], new Set(), new Set()).size,
    0,
  );

  // peer 侧歧义：一台设备对着两个同名 peer。
  const solo = makeDevice({ id: "solo", name: "Solo", online: false });
  const peerA = makePeer({ id: "peer-a", name: "Solo" });
  const peerB = makePeer({ id: "peer-b", name: "Solo" });
  const soloLayout = makeLayout([solo]);
  assert.equal(
    weakMatchesPeer(soloLayout, solo, peerA, { peers: [peerA, peerB] }),
    false,
  );
  assert.equal(
    uniqueWeakMatches(soloLayout, [peerA, peerB], new Set(), new Set()).size,
    0,
  );
});

test("弱匹配：cluster 不同、cluster 为空或对端待配对时不生效", () => {
  const device = makeDevice({ online: false });
  const layout = makeLayout([device]);

  assert.equal(weakMatchesPeer(layout, device, makePeer({ clusterId: "cluster-b" })), false);
  assert.equal(weakMatchesPeer(layout, device, makePeer({ pairingRequired: true })), false);
  const emptyCluster = { ...makeLayout([device]), clusterId: "" };
  assert.equal(weakMatchesPeer(emptyCluster, device, makePeer()), false);
});

test("弱匹配：本机（local）设备与已被本机强匹配的场景不参与", () => {
  const local = makeDevice({ id: "local", role: "local", online: true });
  assert.equal(weakMatchesPeer(makeLayout([local]), local, makePeer()), false);
});

// ---------- P0-2b.4 设备 id 自动跟随（前端规则） ----------

test("P0-2b.4：manual 设备重新配对保留旧 id（用户引用稳定）", () => {
  const device = makeDevice({ id: "manual-fixed", source: "manual" });
  const peer = makePeer();

  const next = upsertPeerDevice(
    makeLayout([device]),
    peer,
    "",
    true,
    "manual-fixed",
  );

  assert.equal(next.devices.length, 1);
  assert.equal(
    next.devices[0].id,
    "manual-fixed",
    "manual 设备 id 永不自动变，非 manual 才跟随对端",
  );
});

test("P0-2b.4：新 id 已被另一台设备占用时保留旧 id，绝不合并两台设备", () => {
  const device = makeDevice({ id: "old-abc" });
  const occupant = makeDevice({
    id: "new-xyz",
    name: "另一台机器",
    transportPublicKey: "other-key",
  });
  const peer = makePeer({ id: "new-xyz" });
  const layout = makeLayout([device, occupant]);

  const next = upsertPeerDevice(layout, peer, "", false, "old-abc");

  const updated = next.devices.find((candidate) => candidate.id === "old-abc");
  assert.ok(updated, "冲突时保留旧 id，不改写成已占用的新 id");
  assert.equal(next.devices.length, 2, "两台设备绝不合并");
  assert.equal(next.activeDeviceId, "old-abc");
});

// ---------- P0-3 证书轮换身份漂移判定 ----------

test("P0-3：证书轮换（弱匹配命中）检出漂移，key 一致的强匹配设备不报", () => {
  const rotated = makeDevice({
    id: "rotated",
    name: "Win-Box",
    online: false,
    transportPublicKey: "old-key",
  });
  const peer = makePeer({ id: "win-box", name: "Win-Box", transportPublicKey: "new-key" });
  const layout = makeLayout([rotated]);

  const drifts = listIdentityDrifts(layout, [peer]);
  assert.equal(drifts.length, 1);
  assert.equal(drifts[0].device.id, "rotated");
  assert.equal(drifts[0].reason, "rotated");
  assert.equal(findIdentityDriftPeer(layout, [peer])?.device.id, "rotated");
  // 指纹 = 对端传输公钥（trim 后），同 (device.id, 指纹) 即同因。
  assert.equal(identityDriftFingerprint(peer), "new-key");

  const healthy = makeDevice({ online: true, transportPublicKey: "new-key" });
  assert.equal(
    listIdentityDrifts(makeLayout([healthy]), [makePeer()]).length,
    0,
  );
});

test("P0-3：设备 key 为空而对端有证书时报 missingKey（强匹配命中也不放过）", () => {
  const device = makeDevice({ transportPublicKey: "", online: true });
  const peer = makePeer();

  const drifts = listIdentityDrifts(makeLayout([device]), [peer]);
  assert.equal(drifts.length, 1);
  assert.equal(drifts[0].reason, "missingKey");
});

test("P0-3：已被其它 peer 强匹配认领的设备不被同名 peer 报成漂移（防抢占）", () => {
  const device = makeDevice({
    online: true,
    transportPublicKey: "existing-key",
  });
  const claimingPeer = makePeer({
    id: "claiming",
    transportPublicKey: "existing-key",
  });
  const lookalike = makePeer({ id: "lookalike", transportPublicKey: "new-key" });

  const drifts = listIdentityDrifts(makeLayout([device]), [
    claimingPeer,
    lookalike,
  ]);
  assert.equal(
    drifts.length,
    0,
    "设备已被 key 一致的 peer 认领，同名 lookalike 不得制造假漂移",
  );
});

// ---------- P0-3 弹窗通道：同因只弹一次、解除自动关、不堆叠 ----------

test("P0-3 弹窗：同一漂移只弹一次，轮询重复检测绝不重开", () => {
  const signal = { deviceId: "dev-a", fingerprint: "key-1" };
  const first = trackIdentityDrift(createIdentityDriftTracker(), [signal]);
  assert.equal(first.show?.deviceId, "dev-a");
  assert.equal(first.show?.fingerprint, "key-1");
  assert.equal(first.clear, false);

  const second = trackIdentityDrift(first.tracker, [signal]);
  assert.equal(second.show, null, "2s 轮询再次看到同一漂移不再弹");
  assert.equal(second.clear, false);
});

test("P0-3 弹窗：稍后处理后本次漂移不再弹，同设备新指纹可再弹", () => {
  const shown = trackIdentityDrift(createIdentityDriftTracker(), [
    { deviceId: "dev-a", fingerprint: "key-1" },
  ]);
  const dismissed = dismissIdentityDrift(shown.tracker);
  assert.equal(dismissed.shown, null);
  assert.equal(dismissed.dismissed["dev-a"], "key-1");

  const quiet = trackIdentityDrift(dismissed, [
    { deviceId: "dev-a", fingerprint: "key-1" },
  ]);
  assert.equal(quiet.show, null, "稍后处理后同一指纹保持安静");

  // 漂移消失（修复成功/peer 离线）→ tracker 复位；同设备新证书再漂移可再弹。
  const cleared = trackIdentityDrift(quiet.tracker, []);
  assert.deepEqual(cleared.tracker, createIdentityDriftTracker());
  const again = trackIdentityDrift(cleared.tracker, [
    { deviceId: "dev-a", fingerprint: "key-2" },
  ]);
  assert.equal(again.show?.fingerprint, "key-2");
});

test("P0-3 弹窗：正在显示的漂移消失时自动收起（解除自动关）", () => {
  const shown = trackIdentityDrift(createIdentityDriftTracker(), [
    { deviceId: "dev-a", fingerprint: "key-1" },
  ]);
  const cleared = trackIdentityDrift(shown.tracker, []);
  assert.equal(cleared.clear, true, "漂移解除要自动关弹窗");
  assert.equal(cleared.show, null);
  assert.deepEqual(cleared.tracker, createIdentityDriftTracker());
});

test("P0-3 弹窗：多台同时漂移不堆叠，只显示第一条且不被轮询换掉", () => {
  const a = { deviceId: "dev-a", fingerprint: "key-a" };
  const b = { deviceId: "dev-b", fingerprint: "key-b" };
  const first = trackIdentityDrift(createIdentityDriftTracker(), [a, b]);
  assert.equal(first.show?.deviceId, "dev-a", "只显示第一条");

  const reordered = trackIdentityDrift(first.tracker, [b, a]);
  assert.equal(reordered.show, null, "已显示的漂移持续存在时不换弹窗");
  assert.equal(reordered.clear, false);

  // 第一条解除：自动收起，并立即显示仍在漂移的第二条。
  const aResolved = trackIdentityDrift(first.tracker, [b]);
  assert.equal(aResolved.clear, true);
  assert.equal(aResolved.show?.deviceId, "dev-b");
});
