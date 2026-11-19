export type Platform = 'windows' | 'macos' | 'unknown'

export type MachineRole = 'unset' | 'server' | 'client'

export type AppLanguage = 'cn' | 'en'

export type ThemeMode = 'system' | 'dark' | 'light'

export type TransportPortMode = 'auto' | 'fixed'

export type ModifierTarget = 'control' | 'alt' | 'meta' | 'same'

export interface ModifierMap {
  control: ModifierTarget
  alt: ModifierTarget
  meta: ModifierTarget
}

export interface ScreenSwitchHotkeys {
  left: string
  right: string
  up: string
  down: string
}

/** Mirrors the Rust `halo::CalibrationProfile` (serde camelCase). */
export interface HaloCalibration {
  centerYaw: number
  centerPitch: number
  leftYaw: number
  rightYaw: number
  upPitch: number
  downPitch: number
}

/** Mirrors the Rust `halo::screen_map::PosePoint` (serde camelCase). */
export interface HaloPosePoint {
  yaw: number
  pitch: number
  u: number
  v: number
}

/** Mirrors the Rust `halo::screen_map::BoardRect` (serde camelCase). */
export interface HaloBoardRect {
  screenId: string
  x: number
  y: number
  width: number
  height: number
}

/** Mirrors the Rust `halo::screen_map::ScreenRegion` (serde camelCase). */
export interface HaloScreenRegion {
  screenId: string
  board: HaloBoardRect
  tl: HaloPosePoint
  tr: HaloPosePoint
  br: HaloPosePoint
  bl: HaloPosePoint
}

/** Mirrors the Rust `halo::screen_map::ScreenCalibration` (serde camelCase). */
export interface HaloScreenCalibration {
  version: number
  capturedAtMs: number
  regions: HaloScreenRegion[]
  refU: number
  refV: number
  transAlphaYaw: number
  transAlphaPitch: number
}

/** Mirrors the Rust `halo::FocusHaloSettings` (serde camelCase). */
export interface FocusHaloSettings {
  enabled: boolean
  dwellMs: number
  settleMs: number
  enterSpeed: number
  cooldownMs: number
  undoMs: number
  deadZonePercent: number
  gain: number
  haloDiameter: number
  haloOpacity: number
  /** Render-servo settle time in ms: smaller = snappier, larger = calmer. */
  smoothMs: number
  showProgress: boolean
  switchZone: 'anywhere' | 'titlebar'
  cameraPosition: 'top' | 'bottom'
  /** Flip the vertical axis when the halo still moves opposite to the gaze. */
  invertPitch?: boolean
  keyboardSuspends: boolean
  titleBlacklist: string[]
  processBlacklist: string[]
  emergencyStopHotkey: string
  undoHotkey: string
  recenterHotkey: string
  /** Captured calibration; absent until the first successful capture. */
  calibration?: HaloCalibration | null
  /** 转头切屏 master switch (S1 schema stability; consumed in S2+). */
  screenWarpEnabled?: boolean
  showHalo?: boolean
  /** New-region hysteresis window in ms. */
  regionSettleMs?: number
  /** Dilatation factor around a region centroid (1.0 = no tolerance). */
  regionMargin?: number
  /** Seat-translation compensation, deg per normalized unit. */
  transAlphaYaw?: number
  transAlphaPitch?: number
  /** Per-screen pose quads; absent until the first multi-screen calibration. */
  screenCalibration?: HaloScreenCalibration | null
}

/** Mirrors the Rust `HaloStatus` (serde camelCase). */
export interface HaloStatus {
  running: boolean
  cameraOk: boolean
  reason: string | null
  /** Multi-screen calibration vs live topology: null before any calibration. */
  screenTopologyOk?: boolean | null
}

/** Mirrors the Rust `halo::DebugSnapshot` (serde camelCase, phase snake_case). */
export interface HaloDebugSnapshot {
  tsMs: number
  faceDetected: boolean
  rawYaw: number
  rawPitch: number
  filteredYaw: number
  filteredPitch: number
  normX: number
  normY: number
  speedNorm: number
  phase: 'idle' | 'suspended_face' | 'suspended_input'
  /** 转头切屏：当前生效区域对应的屏幕 id（单屏模式为 null）。 */
  activeScreenId?: string | null
  /** 转头切屏：本快照窗口内发生了区域切换（引擎仅在提交 tick 置位）。 */
  screenSwitched?: boolean
  /** S4 跨机切屏：本快照 tick 锁存的远程落点（仅提交 tick 出现）。 */
  remoteSwitch?: {
    screenId: string
    boardX: number
    boardY: number
  } | null
}

export interface PairedController {
  id: string
  name: string
  host: string
  ip: string
  transportPublicKey: string
  protocolVersion: number
  clusterId: string
  pairedAtMs: number
}

export interface Screen {
  id: string
  deviceId: string
  name: string
  x: number
  y: number
  width: number
  height: number
  scale: number
  isPrimary: boolean
}

export interface Device {
  id: string
  name: string
  platform: Platform
  host: string
  transportPort: number
  quicPort: number
  transportPublicKey: string
  protocolVersion: number
  color: string
  online: boolean
  inputReady: boolean
  upgrading?: boolean
  /** Multipliers this controller applies when driving the device. */
  pointerSpeed?: number
  scrollSpeed?: number
  role: 'local' | 'server' | 'client'
  source?: 'detected' | 'manual'
  screens: Screen[]
}

export interface LayoutState {
  devices: Device[]
  activeDeviceId: string
  selectedScreenId: string
  inputMode: 'control' | 'receive'
  machineRole: MachineRole
  clusterId: string
  pairSecret: string
  pairedControllers: PairedController[]
  clipboardSync: boolean
  clipboardOnDemand?: boolean
  clipboardPasteHotkey?: string
  fileTransferEnabled: boolean
  /** Controller: locking this machine locks its online clients too. */
  lockSync?: boolean
  inputProtection?: InputProtection
  language: AppLanguage
  themeMode: ThemeMode
  performanceMonitor: boolean
  transportPortMode: TransportPortMode
  transportPort: number
  quicPort: number
  modifierRemap: boolean
  modifierMap: ModifierMap
  edgeSwitchHotkey: string
  screenSwitchHotkeys: ScreenSwitchHotkeys
  focusHalo?: FocusHaloSettings
  /** 跨机 Agent 通道配置；旧快照缺省时后端落默认（enabled=true, readonly）。 */
  agentChannel?: AgentChannelConfig
}

export interface InputProtection {
  localOnly: boolean
  protectFullscreen: boolean
  blockedApplications: string[]
  lockHotkey: string
}

/** 跨机 Agent 通道能力级别（RFC §5.3 L0-L3；P1a 仅 readonly 生效）。 */
export type AgentChannelLevel = 'readonly' | 'files' | 'commands' | 'full'

/** Mirrors the Rust `agent_channel::AgentChannelConfig` (serde camelCase)。 */
export interface AgentChannelConfig {
  enabled: boolean
  level: AgentChannelLevel
  /** 文件白名单根目录；空 = 运行期默认 [日志目录, Downloads]。 */
  fileRoots: string[]
  /** L2 命令白名单（P3 消费）。 */
  cmdWhitelist: string[]
  /** L3 审批窗口秒数（P3 消费）。 */
  approvalTtlSecs: number
}

/** Mirrors the Rust `agent_channel::AgentChannelAnnounce` (serde camelCase)。 */
export interface AgentChannelAnnounce {
  enabled: boolean
  level: AgentChannelLevel
}

/** Mirrors the Rust `agent_channel::AgentAuditEntry` (serde camelCase)。 */
export interface AgentChannelAuditEntry {
  tsMs: number
  /** "in" = 本机收到并处理；"out" = 本机发起/回发。 */
  direction: string
  peerId: string
  capability: string
  requestId: string
  /** "ok" | "err" | "denied" | "timeout" */
  status: string
  durationMs: number
  summary: string
}

/** Mirrors the Rust `agent_channel::AgentInboxMessage` (serde camelCase)。 */
export interface AgentChannelInboxMessage {
  tsMs: number
  /** 发送方 device id。 */
  peerId: string
  title: string | null
  text: string
  /** text 超过 4KiB 被截断时为 true。 */
  truncated: boolean
}

/** Mirrors the Rust `agent_channel::AgentChannelResponse` (serde camelCase)。 */
export interface AgentChannelResponse {
  status: string
  payload: unknown
}
