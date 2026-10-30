import { flattenScreens, getLayoutBounds } from "./layout.ts";
import type { LanPeer, LanPeerScreen } from "./runtime.ts";
import type { Device, LayoutState, Platform, Screen } from "./types.ts";

const DEVICE_COLORS = [
  "#2f7af8",
  "#0f766e",
  "#b45309",
  "#7c3aed",
  "#be123c",
  "#0891b2",
];

/**
 * 对端（discovery peer）与已保存设备之间的纯逻辑：身份匹配、重新配对的
 * 地址候选，以及把实时对端合并回布局设备。抽成独立模块以便 node --test
 * 直接单测（App.tsx 依赖 React/CSS，无法在测试进程里加载）。
 *
 * 背景：设备列表来自 runtime.discovery.peers，布局设备是持久化数据。对端
 * DHCP 换址或传输证书轮换后两者不再相等，布局里的设备会一直显示离线；此时
 * 若"重新配对"继续用设备保存的旧 host，就会打到旧地址并报
 * "No pairing challenge received from <旧IP>"。因此这里的身份匹配按凭证
 * 强度分层：传输公钥 > 设备 id > 同 cluster 的 host/ip > 名称（仅作兜底）。
 */

export function sanitizeId(value: string): string {
  return value
    .trim()
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "");
}

export function peerDeviceId(peer: LanPeer): string {
  return sanitizeId(peer.id || peer.name || peer.ip) || "peer-device";
}

export function deviceMatchesPeer(
  layout: LayoutState,
  device: Device,
  peer: LanPeer,
): boolean {
  if (device.transportPublicKey.trim().length > 0) {
    return device.transportPublicKey === peer.transportPublicKey;
  }
  return (
    device.id === peerDeviceId(peer) ||
    sameClusterHost(layout, device, peer)
  );
}

export function findPeerDevice(
  layout: LayoutState,
  peer: LanPeer,
): Device | undefined {
  return layout.devices.find((device) =>
    deviceMatchesPeer(layout, device, peer),
  );
}

/// Stable identity of a live peer inside one presence pass.
export function peerKey(peer: LanPeer): string {
  return peer.id.trim() || peerDeviceId(peer);
}

/// Pass-level claims for the weak match (P0-2a): strong matches always win,
/// the weak pass may only look at what they left behind.
export interface WeakMatchPass {
  /// Devices participating in this pass (local devices never weak-match).
  devices?: Device[];
  /// Peers participating in this pass.
  peers?: LanPeer[];
  /// Devices already claimed by strong matches (or earlier weak matches).
  claimedDeviceIds?: ReadonlySet<string>;
  /// Peers already claimed (keyed by peerKey).
  claimedPeerIds?: ReadonlySet<string>;
}

/// 弱匹配的「信号」条件（大小写不敏感、trim）：名称相同，或设备 host 按 "/"
/// 拆段后任一段等于对端自报 host。与 Rust weak_device_match 同语义。
function weakSignalMatches(device: Device, peer: LanPeer): boolean {
  return (
    textEquals(device.name, peer.name) || hostSegmentEqualsPeerHost(device.host, peer.host)
  );
}

function hostSegmentEqualsPeerHost(deviceHost: string, peerHost: string): boolean {
  const target = peerHost.trim().toLowerCase();
  if (!target) {
    return false;
  }
  return deviceHost
    .split("/")
    .some((part) => part.trim().toLowerCase() === target);
}

/// 弱匹配核心条件：同 cluster（非空）且对端不处于待配对状态，加上信号匹配。
function weakCoreMatch(
  layout: LayoutState,
  device: Device,
  peer: LanPeer,
): boolean {
  return (
    layout.clusterId.trim().length > 0 &&
    !peer.pairingRequired &&
    peer.clusterId === layout.clusterId &&
    weakSignalMatches(device, peer)
  );
}

/**
 * 弱匹配（P0-2a，与 Rust weak_device_match 同语义）：仅用于恢复
 * online/inputReady 显示，绝不改写 key/host/ports/id（安全边界）。
 * 生效条件全部满足：
 * (a) 同 cluster 且非空、对端非 pairingRequired；
 * (b) 信号匹配（名称，或设备 host 段等于对端 host）；
 * (c) 双向唯一：设备与 peer 在本次 presence pass 内都未被强匹配认领，
 *     且两侧都不存在第二个弱候选。
 * pass 省略时以 layout.devices × [peer] 为本次 pass、无已认领集合。
 */
export function weakMatchesPeer(
  layout: LayoutState,
  device: Device,
  peer: LanPeer,
  pass?: WeakMatchPass,
): boolean {
  if (device.role === "local") {
    return false;
  }
  if (pass?.claimedDeviceIds?.has(device.id)) {
    return false;
  }
  if (pass?.claimedPeerIds?.has(peerKey(peer))) {
    return false;
  }

  const devices = (pass?.devices ?? layout.devices).filter(
    (candidate) =>
      candidate.role !== "local" &&
      !pass?.claimedDeviceIds?.has(candidate.id),
  );
  const peers = (pass?.peers ?? [peer]).filter(
    (candidate) => !pass?.claimedPeerIds?.has(peerKey(candidate)),
  );
  if (!devices.some((candidate) => candidate.id === device.id)) {
    return false;
  }
  if (!peers.some((candidate) => peerKey(candidate) === peerKey(peer))) {
    return false;
  }
  if (!weakCoreMatch(layout, device, peer)) {
    return false;
  }
  // 强匹配优先：任一强匹配命中都让弱匹配让路（防抢占）。
  if (peers.some((candidate) => deviceMatchesPeer(layout, device, candidate))) {
    return false;
  }
  if (devices.some((candidate) => deviceMatchesPeer(layout, candidate, peer))) {
    return false;
  }

  // 双向唯一：设备侧与 peer 侧都必须是唯一弱候选。
  const deviceSide = devices.filter((candidate) =>
    weakCoreMatch(layout, candidate, peer),
  );
  if (deviceSide.length !== 1 || deviceSide[0].id !== device.id) {
    return false;
  }
  const peerSide = peers.filter((candidate) =>
    weakCoreMatch(layout, device, candidate),
  );
  return peerSide.length === 1 && peerKey(peerSide[0]) === peerKey(peer);
}

/**
 * 第二遍弱匹配：对「剩余未认领设备 × 未认领 peer」做双向唯一匹配。
 * 返回 device.id -> 唯一弱命中 peer；仅更新 online/inputReady 用。
 */
export function uniqueWeakMatches(
  layout: LayoutState,
  peers: LanPeer[],
  claimedDeviceIds: ReadonlySet<string>,
  claimedPeerIds: ReadonlySet<string>,
): Map<string, LanPeer> {
  const result = new Map<string, LanPeer>();
  const devices = layout.devices.filter(
    (device) => device.role !== "local" && !claimedDeviceIds.has(device.id),
  );
  const freePeers = peers.filter((peer) => !claimedPeerIds.has(peerKey(peer)));
  if (devices.length === 0 || freePeers.length === 0) {
    return result;
  }

  const peerCandidates = new Map<string, Device[]>();
  const deviceCandidates = new Map<string, LanPeer[]>();
  for (const device of devices) {
    for (const peer of freePeers) {
      if (!weakCoreMatch(layout, device, peer)) {
        continue;
      }
      const forDevice = deviceCandidates.get(device.id) ?? [];
      forDevice.push(peer);
      deviceCandidates.set(device.id, forDevice);
      const forPeer = peerCandidates.get(peerKey(peer)) ?? [];
      forPeer.push(device);
      peerCandidates.set(peerKey(peer), forPeer);
    }
  }

  for (const device of devices) {
    const candidates = deviceCandidates.get(device.id) ?? [];
    if (candidates.length !== 1) {
      continue;
    }
    const peer = candidates[0];
    if ((peerCandidates.get(peerKey(peer)) ?? []).length !== 1) {
      continue;
    }
    result.set(device.id, peer);
  }
  return result;
}

/**
 * 找出与某台已保存设备对应的实时对端，按匹配强度从高到低：
 * 1. 传输公钥相等（证书仍是同一份，只是地址变了）；
 * 2. 设备 id 相同（对端 id / 由 id 派生的设备 id）；
 * 3. 同 cluster 且 host/ip 相同（旧版无证书设备的兜底）；
 * 4. 名称相同（大小写不敏感；证书轮换后只剩这一个弱线索可用）。
 */
export function findLivePeerForDevice(
  layout: LayoutState,
  device: Device,
  peers: LanPeer[],
): LanPeer | undefined {
  return (
    findPeerByTransportKey(device, peers) ??
    findPeerByIdentity(device, peers) ??
    peers.find((peer) => sameClusterHost(layout, device, peer)) ??
    findPeerByName(device, peers)
  );
}

/**
 * 重新配对时要尝试的地址，优先级从高到低：
 * 1. 实时对端上报的 IP（UDP 包的源地址，是当前真实地址；为空时用其 host）；
 * 2. 设备保存的 host，按 "/" 与 "," 拆成各段（可能是 "主机名 / IP, IP"）；
 * 3. 实时对端自报的 host（其中可能带主机名，后端可再解析）。
 * 结果去重且不含空串。缺省没有 layout 时退化为按公钥/id/名称找实时对端。
 */
export function repairHostCandidates(
  device: Device,
  peers: LanPeer[],
  layout?: LayoutState,
): string[] {
  const livePeer = layout
    ? findLivePeerForDevice(layout, device, peers)
    : (findPeerByTransportKey(device, peers) ??
      findPeerByIdentity(device, peers) ??
      findPeerByName(device, peers));

  const candidates: string[] = [];
  const seen = new Set<string>();
  const push = (value: string) => {
    for (const part of splitHostCandidates(value)) {
      const key = part.toLowerCase();
      if (seen.has(key)) {
        continue;
      }
      seen.add(key);
      candidates.push(part);
    }
  };

  if (livePeer) {
    push(livePeer.ip);
    if (!livePeer.ip.trim()) {
      // 没有源地址时先顶上对端自报的 host，保证实时地址永远排在最前。
      push(livePeer.host);
    }
  }
  push(device.host);
  if (livePeer) {
    push(livePeer.host);
  }

  return candidates;
}

/**
 * Devices 页用：实时对端匹配不到任何已保存设备时，找一台明显是同一台机器
 * 的离线设备（名称相同，或 host 里的主机名相同），把它原地重新配对，而不是
 * 新增一台重复设备。已被其它实时对端认领的设备不会被抢走。
 */
export function findRepairDeviceForPeer(
  layout: LayoutState,
  peers: LanPeer[],
  peer: LanPeer,
): Device | undefined {
  if (
    layout.devices.some((device) => deviceMatchesPeer(layout, device, peer))
  ) {
    return undefined;
  }

  const available = layout.devices.filter(
    (device) =>
      device.role !== "local" &&
      !device.online &&
      !peers.some(
        (other) =>
          other !== peer && deviceMatchesPeer(layout, device, other),
      ),
  );
  if (available.length === 0) {
    return undefined;
  }

  return (
    available.find((device) => textEquals(device.name, peer.name)) ??
    available.find((device) => hostMatchesPeer(device.host, peer))
  );
}

/// 证书轮换自动弹窗（P0-3）的判定结果：设备与实时对端身份（传输公钥）漂移。
export type IdentityDriftReason = "rotated" | "missingKey"

export interface IdentityDrift {
  device: Device
  peer: LanPeer
  reason: IdentityDriftReason
}

/// 漂移指纹：同一 (device.id, peer 传输公钥) 组合视为同一次漂移（同因只弹一次）。
export function identityDriftFingerprint(peer: LanPeer): string {
  return peer.transportPublicKey.trim()
}

function identityDriftReason(
  device: Device,
  peer: LanPeer,
): IdentityDriftReason | null {
  const deviceKey = device.transportPublicKey.trim()
  if (!deviceKey) {
    // 设备从未记录证书而对端有证书：输入同样会被拒，需要走配对码补录。
    return peer.transportPublicKey.trim() ? "missingKey" : null
  }
  return deviceKey === peer.transportPublicKey ? null : "rotated"
}

/**
 * 找出所有「设备身份已漂移」的 (device, peer)（P0-3 判定函数）：
 * - 第一遍强匹配认领（含空 key 设备按 id/host 命中——key 为空即漂移
 *   missingKey）；
 * - 第二遍对剩余设备做唯一弱匹配，再不行用 findLivePeerForDevice 兜底，
 *   但都不抢第一遍已认领的设备与 peer（安全边界：不触碰信任）。
 */
export function listIdentityDrifts(
  layout: LayoutState,
  peers: LanPeer[],
): IdentityDrift[] {
  const drifts: IdentityDrift[] = []
  const claimedDeviceIds = new Set<string>()
  const claimedPeerIds = new Set<string>()

  for (const peer of peers) {
    for (const device of layout.devices) {
      if (device.role === "local" || claimedDeviceIds.has(device.id)) {
        continue
      }
      if (!deviceMatchesPeer(layout, device, peer)) {
        continue
      }
      claimedDeviceIds.add(device.id)
      claimedPeerIds.add(peerKey(peer))
      const reason = identityDriftReason(device, peer)
      if (reason) {
        drifts.push({ device, peer, reason })
      }
    }
  }

  const weak = uniqueWeakMatches(
    layout,
    peers,
    claimedDeviceIds,
    claimedPeerIds,
  )
  for (const device of layout.devices) {
    if (device.role === "local" || claimedDeviceIds.has(device.id)) {
      continue
    }
    const freePeers = peers.filter(
      (peer) => !claimedPeerIds.has(peerKey(peer)),
    )
    const peer =
      weak.get(device.id) ??
      (freePeers.length > 0
        ? findLivePeerForDevice(layout, device, freePeers)
        : undefined)
    if (!peer) {
      continue
    }
    const reason = identityDriftReason(device, peer)
    if (reason) {
      drifts.push({ device, peer, reason })
    }
  }
  return drifts
}

/// 自动弹窗用：第一条身份漂移（多台同时漂移不堆叠，只显示第一条）。
export function findIdentityDriftPeer(
  layout: LayoutState,
  peers: LanPeer[],
): IdentityDrift | undefined {
  return listIdentityDrifts(layout, peers)[0]
}

/**
 * 把实时对端写入布局。命中 preferredDeviceId（重新配对）时原位更新该设备，
 * 保留 id、颜色、指针/滚轮速度与屏幕摆放位置；否则沿用原有的按公钥/id 匹配。
 */
export function upsertPeerDevice(
  layout: LayoutState,
  peer: LanPeer,
  alias = "",
  manual = false,
  preferredDeviceId = "",
): LayoutState {
  const preferredId = preferredDeviceId.trim();
  let existingIndex = preferredId
    ? layout.devices.findIndex((device) => device.id === preferredId)
    : -1;
  const repairInPlace = existingIndex >= 0;
  if (!repairInPlace) {
    existingIndex = layout.devices.findIndex(
      (device) =>
        device.id === peerDeviceId(peer) ||
        (device.transportPublicKey.trim().length > 0 &&
          device.transportPublicKey === peer.transportPublicKey),
    );
  }

  const existingDevice =
    existingIndex >= 0 ? layout.devices[existingIndex] : undefined;
  const nextDevice = createDeviceFromPeer(
    layout,
    peer,
    alias,
    existingDevice,
    manual,
    repairInPlace,
  );
  const devices =
    existingIndex >= 0
      ? layout.devices.map((device, index) =>
          index === existingIndex ? nextDevice : device,
        )
      : [...layout.devices, nextDevice];

  return {
    ...layout,
    devices,
    inputMode: "control",
    activeDeviceId: nextDevice.id,
    selectedScreenId:
      nextDevice.screens[0]?.id ?? layout.selectedScreenId,
  };
}

function createDeviceFromPeer(
  layout: LayoutState,
  peer: LanPeer,
  alias = "",
  existingDevice?: Device,
  manual = false,
  repair = false,
): Device {
  // P0-2b.4：非 manual 设备的 id 自动跟随对端（与后端 peer_device_id 同规则），
  // 彻底关闭「换 IP 后 id/host 双失配、设备永久离线」；manual 设备保留既有 id，
  // 用户引用稳定。冲突防护：新 id 已被另一台设备占用时保留旧 id，绝不合并两台设备。
  const followedId = peerDeviceId(peer);
  const manualDevice = manual || existingDevice?.source === "manual";
  const idTakenByOther =
    !manualDevice &&
    layout.devices.some(
      (device) => device !== existingDevice && device.id === followedId,
    );
  const id = manualDevice
    ? existingDevice?.id ?? followedId
    : idTakenByOther
      ? existingDevice?.id ?? followedId
      : followedId;
  // 名称始终优先保留用户可见的既有名称（手动添加的别名），重新配对只刷新
  // 地址/证书/端口/屏幕，避免设备被悄悄改名（与 #26 的既有行为一致）。
  const name = alias || existingDevice?.name || peer.name;
  // #26：手动添加的设备保留用户显式填写的 host（可能是 NAT 转发地址），
  // 不被 discovery 自动覆盖。但"重新配对"是用户主动发起的显式修复，对端
  // 往往已经换 IP/证书，继续沿用旧地址会让设备永久离线，所以这里按约定
  // 改用对端的实时地址（peer.ip 优先）；对端没给地址时保留旧 host。
  const host = repair
    ? peer.ip || peer.host || existingDevice?.host || ""
    : !manual && existingDevice?.source === "manual"
      ? existingDevice.host
      : peer.ip || peer.host;

  return {
    id,
    name,
    platform: normalizePlatform(peer.platform),
    host,
    transportPort: peer.transportPort,
    quicPort: peer.quicPort,
    transportPublicKey: peer.transportPublicKey,
    protocolVersion: peer.protocolVersion,
    color: existingDevice?.color ?? nextDeviceColor(layout),
    online: true,
    inputReady: peer.inputReady,
    role: "client",
    source: manualDevice ? "manual" : "detected",
    ...(existingDevice?.pointerSpeed === undefined
      ? {}
      : { pointerSpeed: existingDevice.pointerSpeed }),
    ...(existingDevice?.scrollSpeed === undefined
      ? {}
      : { scrollSpeed: existingDevice.scrollSpeed }),
    screens: createScreensFromPeer(layout, id, peer.screens, existingDevice),
  };
}

function createScreensFromPeer(
  layout: LayoutState,
  deviceId: string,
  peerScreens: LanPeerScreen[],
  existingDevice?: Device,
): Screen[] {
  if (peerScreens.length === 0) {
    return [];
  }

  const localScreens =
    layout.devices.find((device) => device.role === "local")?.screens ?? [];
  const localBounds = getLayoutBounds(
    localScreens.length > 0 ? localScreens : [fallbackScreen()],
  );
  const layoutScreens = flattenScreens(layout);
  const layoutBounds = getLayoutBounds(
    layoutScreens.length > 0 ? layoutScreens : [fallbackScreen()],
  );
  const peerMinX = Math.min(...peerScreens.map((screen) => screen.x));
  const peerMinY = Math.min(...peerScreens.map((screen) => screen.y));
  const startX = layoutBounds.maxX;

  return peerScreens.map((screen, index) => {
    const id = uniqueScreenId(deviceId, screen, index);
    const existingScreen =
      existingDevice?.screens.find((candidate) => candidate.id === id) ??
      // P0-2b：设备 id 跟随后屏幕 id 前缀变化，按 id 找不到旧屏；数量相同则
      // 按 index 对位兜底取原坐标，用户摆放不回默认（与后端 screens_from_peer
      // 的 index 对位兜底一致）。
      (existingDevice && existingDevice.screens.length === peerScreens.length
        ? existingDevice.screens[index]
        : undefined);

    return {
      id,
      deviceId,
      name: screen.name || `Display ${index + 1}`,
      x: existingScreen?.x ?? startX + (screen.x - peerMinX),
      y: existingScreen?.y ?? localBounds.minY + (screen.y - peerMinY),
      width: screen.width,
      height: screen.height,
      scale: screen.scale,
      isPrimary: screen.isPrimary,
    };
  });
}

function uniqueScreenId(
  deviceId: string,
  screen: LanPeerScreen,
  index: number,
) {
  return `${deviceId}-${sanitizeId(screen.id || screen.name || `display-${index + 1}`)}`;
}

function nextDeviceColor(layout: LayoutState) {
  return DEVICE_COLORS[layout.devices.length % DEVICE_COLORS.length];
}

export function normalizePlatform(platform: string): Platform {
  if (platform === "windows" || platform === "macos") {
    return platform;
  }

  return "unknown";
}

export function fallbackScreen(): Screen {
  return {
    id: "fallback",
    deviceId: "fallback",
    name: "Fallback",
    x: 0,
    y: 0,
    width: 1,
    height: 1,
    scale: 1,
    isPrimary: true,
  };
}

function splitHostCandidates(host: string): string[] {
  return host
    .split(/[/,]/)
    .map((part) => part.trim())
    .filter((part) => part.length > 0);
}

function findPeerByTransportKey(
  device: Device,
  peers: LanPeer[],
): LanPeer | undefined {
  const key = device.transportPublicKey.trim();
  if (!key) {
    return undefined;
  }
  return peers.find(
    (peer) =>
      peer.transportPublicKey.trim().length > 0 &&
      peer.transportPublicKey === key,
  );
}

function findPeerByIdentity(
  device: Device,
  peers: LanPeer[],
): LanPeer | undefined {
  return peers.find(
    (peer) => device.id === peer.id || device.id === peerDeviceId(peer),
  );
}

function findPeerByName(
  device: Device,
  peers: LanPeer[],
): LanPeer | undefined {
  const name = device.name.trim().toLowerCase();
  if (!name) {
    return undefined;
  }
  return peers.find((peer) => peer.name.trim().toLowerCase() === name);
}

function hostMatchesPeer(deviceHost: string, peer: LanPeer): boolean {
  const peerHosts = new Set(
    [...splitHostCandidates(peer.host), ...splitHostCandidates(peer.ip)]
      .map((part) => part.toLowerCase())
      .filter((part) => part.length > 0),
  );
  if (peerHosts.size === 0) {
    return false;
  }

  return splitHostCandidates(deviceHost).some((part) => {
    const host = part.toLowerCase();
    // 手动地址可能带显式端口（host:port），主机名部分仍视为同一台机器。
    return peerHosts.has(host) || peerHosts.has(host.replace(/:\d+$/, ""));
  });
}

function sameClusterHost(
  layout: LayoutState,
  device: Device,
  peer: LanPeer,
) {
  return (
    layout.clusterId.trim().length > 0 &&
    !peer.pairingRequired &&
    peer.clusterId === layout.clusterId &&
    (sameHost(device.host, peer.host) || sameHost(device.host, peer.ip))
  );
}

function sameHost(value: string, host: string) {
  const normalizedHost = host.trim().toLowerCase();
  return (
    normalizedHost.length > 0 &&
    value
      .split("/")
      .map((part) => part.trim().toLowerCase())
      .some((part) => part === normalizedHost)
  );
}

function textEquals(left: string, right: string) {
  const first = left.trim().toLowerCase();
  const second = right.trim().toLowerCase();
  return first.length > 0 && first === second;
}
