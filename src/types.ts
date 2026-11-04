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
  showProgress: boolean
  switchZone: 'anywhere' | 'titlebar'
  cameraPosition: 'top' | 'bottom'
  keyboardSuspends: boolean
  titleBlacklist: string[]
  processBlacklist: string[]
  emergencyStopHotkey: string
  undoHotkey: string
  recenterHotkey: string
  /** Captured calibration; absent until the first successful capture. */
  calibration?: HaloCalibration | null
}

/** Mirrors the Rust `HaloStatus` (serde camelCase). */
export interface HaloStatus {
  running: boolean
  cameraOk: boolean
  reason: string | null
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
}

export interface InputProtection {
  localOnly: boolean
  protectFullscreen: boolean
  blockedApplications: string[]
  lockHotkey: string
}
