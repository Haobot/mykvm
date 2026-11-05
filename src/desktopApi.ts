import { invoke, isTauri } from '@tauri-apps/api/core'
import { APP_VERSION, RELEASES_URL, REPOSITORY_URL } from './constants'
import { defaultLayout } from './defaultLayout'
import type {
  AppStateSnapshot,
  DiagnosticInfo,
  DiscoveryStatus,
  InputServiceStatus,
  PerformanceSample,
  RuntimeStatus,
} from './runtime'
import type { LayoutState } from './types'
import type { HaloDebugSnapshot, HaloStatus } from './types'

export interface AppUpdateInfo {
  version: string
  currentVersion?: string
  date?: string
  body?: string
}

export interface AppUpdateCheckResult {
  available: boolean
  update?: AppUpdateInfo
}

export interface FileTransferSummary {
  targetName: string
  fileCount: number
  byteCount: number
}

// ponytail: static stopped-state stub so `npm run dev` in a plain browser still
// renders the layout editor; the real runtime lives in the Tauri backend.
const STUB_DETAIL = 'Available only in the Tauri desktop runtime.'

export async function setHotkeyRecording(recording: boolean, captured: string | null = null): Promise<void> {
  if (isTauri()) await invoke('set_hotkey_recording', { recording, captured });
}

const BROWSER_RUNTIME: RuntimeStatus = {
  started: false,
  transport: { state: 'stubbed', detail: STUB_DETAIL },
  capture: { state: 'stubbed', detail: STUB_DETAIL },
  inject: { state: 'stubbed', detail: STUB_DETAIL },
  clipboard: { state: 'stubbed', detail: STUB_DETAIL },
  privilege: { isElevated: false, canElevate: false, detail: STUB_DETAIL },
  inputService: {
    installed: false,
    running: false,
    workerSessionId: null,
    pipeAvailable: false,
    sasAvailable: false,
    detail: STUB_DETAIL,
  },
  discovery: {
    state: 'idle',
    detail: STUB_DETAIL,
    port: defaultLayout.transportPort,
    localPeer: {
      id: 'browser-preview',
      name: 'Browser preview',
      platform: navigator.platform,
      machineRole: defaultLayout.machineRole,
      clusterId: defaultLayout.clusterId,
      pairingRequired: false,
      host: 'localhost',
      ip: '127.0.0.1',
      transportPort: defaultLayout.transportPort,
      quicPort: defaultLayout.quicPort,
      transportPublicKey: '',
      protocolVersion: 1,
      screenCount: 0,
      inputReady: false,
      screens: [],
      appVersion: APP_VERSION,
      lastSeenMs: 0,
    },
    peers: [],
  },
  pairing: {
    state: 'idle',
    code: '',
    requesterName: '',
    requesterIp: '',
    expiresAtMs: 0,
    detail: '',
  },
}

let browserLayout = defaultLayout

export async function loadAppState(): Promise<AppStateSnapshot> {
  if (!isTauri()) {
    return {
      layout: browserLayout,
      runtime: BROWSER_RUNTIME,
    }
  }

  return invoke<AppStateSnapshot>('load_app_state')
}

export async function saveLayout(layout: LayoutState): Promise<AppStateSnapshot> {
  if (!isTauri()) {
    browserLayout = layout
    return {
      layout,
      runtime: BROWSER_RUNTIME,
    }
  }

  return invoke<AppStateSnapshot>('save_layout', { layout })
}

export async function resetPairing(): Promise<AppStateSnapshot> {
  if (!isTauri()) {
    return {
      layout: { ...defaultLayout, pairedControllers: [] },
      runtime: BROWSER_RUNTIME,
    }
  }

  return invoke<AppStateSnapshot>('reset_pairing')
}

export async function isAutostartEnabled(): Promise<boolean> {
  if (!isTauri()) {
    return false
  }

  return invoke<boolean>('is_autostart_enabled')
}

export async function setAutostart(enabled: boolean): Promise<boolean> {
  if (!isTauri()) {
    return enabled
  }

  return invoke<boolean>('set_autostart', { enabled })
}

export async function startRuntime(): Promise<RuntimeStatus> {
  if (!isTauri()) {
    return BROWSER_RUNTIME
  }

  return invoke<RuntimeStatus>('start_runtime')
}

export async function readRuntimeStatus(): Promise<RuntimeStatus> {
  if (!isTauri()) {
    return BROWSER_RUNTIME
  }

  return invoke<RuntimeStatus>('read_runtime_status')
}

export async function readDiagnosticInfo(): Promise<DiagnosticInfo> {
  if (!isTauri()) {
    return {
      report: STUB_DETAIL,
      appVersion: APP_VERSION,
      platform: navigator.platform,
      role: defaultLayout.machineRole,
      runtimeStarted: false,
      localName: BROWSER_RUNTIME.discovery.localPeer.name,
      localIp: BROWSER_RUNTIME.discovery.localPeer.ip,
      discoveryPort: BROWSER_RUNTIME.discovery.port,
      quicPort: BROWSER_RUNTIME.discovery.localPeer.quicPort,
      peerCount: 0,
      logDir: '',
      configDir: '',
      networkHint: STUB_DETAIL,
      firewallHint: STUB_DETAIL,
    }
  }

  return invoke<DiagnosticInfo>('read_diagnostic_info')
}

export async function openLogDirectory(): Promise<void> {
  if (!isTauri()) {
    return
  }

  await invoke('open_log_directory')
}

export async function stopRuntime(): Promise<RuntimeStatus> {
  if (!isTauri()) {
    return BROWSER_RUNTIME
  }

  return invoke<RuntimeStatus>('stop_runtime')
}

export async function pasteRemoteClipboard(): Promise<void> {
  if (!isTauri()) return
  await invoke('paste_remote_clipboard')
}

export async function cancelFileTransfer(transferId: string): Promise<boolean> {
  if (!isTauri()) return false
  return invoke<boolean>('cancel_file_transfer', { transferId })
}

export async function scanLanPeers(): Promise<DiscoveryStatus> {
  if (!isTauri()) {
    return BROWSER_RUNTIME.discovery
  }

  return invoke<DiscoveryStatus>('scan_lan_peers')
}

export async function probeLanPeer(host: string) {
  if (!isTauri()) {
    throw new Error('Direct peer probing is available only in the Tauri desktop runtime.')
  }

  return invoke<DiscoveryStatus['localPeer']>('probe_lan_peer', { host })
}

// deviceId 可选：重新配对已保存设备时传入布局设备 id。后端会据此在实时
// peer 快照里重新解析该机器的当前地址（设备保存的 host 可能已过期），并回退
// 到调用方传入的 host。未传时 Tauri 侧按 null 处理。
export async function requestLanPairing(host: string, deviceId?: string) {
  if (!isTauri()) {
    throw new Error('LAN pairing is available only in the Tauri desktop runtime.')
  }

  return invoke<DiscoveryStatus['localPeer']>('request_lan_pairing', {
    host,
    deviceId: deviceId?.trim() || null,
  })
}

export async function confirmLanPairing(host: string, code: string, deviceId?: string) {
  if (!isTauri()) {
    throw new Error('LAN pairing is available only in the Tauri desktop runtime.')
  }

  return invoke<DiscoveryStatus['localPeer']>('confirm_lan_pairing', {
    host,
    code,
    deviceId: deviceId?.trim() || null,
  })
}

export async function dismissPairingRequest(): Promise<RuntimeStatus> {
  if (!isTauri()) {
    return BROWSER_RUNTIME
  }

  return invoke<RuntimeStatus>('dismiss_pairing_request')
}

export async function writeClipboardText(text: string): Promise<void> {
  if (!isTauri()) {
    return
  }

  await invoke('write_clipboard_text', { text })
}

export async function readPerformanceSample(): Promise<PerformanceSample> {
  if (!isTauri()) {
    return {
      timestampMs: Date.now(),
      appCpuPercent: 0,
      appMemoryMb: 0,
      transportPackets: 0,
      inputEvents: 0,
      clipboardPackets: 0,
      admittedCredentialessInputPackets: 0,
    }
  }

  return invoke<PerformanceSample>('read_performance_sample')
}

export async function restartAsAdmin(): Promise<void> {
  if (!isTauri()) {
    return
  }

  await invoke('restart_as_admin')
}

export async function readInputServiceStatus(): Promise<InputServiceStatus> {
  if (!isTauri()) {
    return BROWSER_RUNTIME.inputService
  }

  return invoke<InputServiceStatus>('read_input_service_status')
}

export async function installInputService(): Promise<InputServiceStatus> {
  if (!isTauri()) {
    return BROWSER_RUNTIME.inputService
  }

  return invoke<InputServiceStatus>('install_input_service')
}

export async function uninstallInputService(): Promise<InputServiceStatus> {
  if (!isTauri()) {
    return BROWSER_RUNTIME.inputService
  }

  return invoke<InputServiceStatus>('uninstall_input_service')
}

export async function sendFilesToDevice(deviceId: string, paths: string[]): Promise<FileTransferSummary> {
  if (!isTauri()) {
    return {
      targetName: 'Desktop fallback',
      fileCount: paths.length,
      byteCount: 0,
    }
  }

  return invoke<FileTransferSummary>('send_files_to_device', { deviceId, paths })
}

export async function requestClientUpdate(deviceId: string): Promise<void> {
  if (!isTauri()) {
    return
  }

  await invoke('request_client_update', { deviceId })
}

export async function fetchClientLog(deviceId: string): Promise<string> {
  if (!isTauri()) {
    return ''
  }

  return invoke<string>('fetch_client_log', { deviceId })
}

export async function relaunchApp(): Promise<void> {
  if (!isTauri()) {
    window.location.reload()
    return
  }

  const { relaunch } = await import('@tauri-apps/plugin-process')
  await relaunch()
}

export async function syncWindowChrome(theme: 'dark' | 'light'): Promise<void> {
  if (!isTauri()) {
    return
  }

  await invoke('sync_window_chrome', { theme })
}

export async function minimizeMainWindow(): Promise<void> {
  if (!isTauri()) {
    return
  }

  await invoke('minimize_main_window')
}

export async function hideMainWindow(): Promise<void> {
  if (!isTauri()) {
    return
  }

  await invoke('hide_main_window')
}

export async function startWindowDrag(): Promise<void> {
  if (!isTauri()) {
    return
  }

  await invoke('start_window_drag')
}

export async function toggleMaximizeMainWindow(): Promise<void> {
  if (!isTauri()) {
    return
  }

  await invoke('toggle_maximize_main_window')
}

export async function openRepositoryUrl(): Promise<void> {
  if (!isTauri()) {
    window.open(REPOSITORY_URL, '_blank', 'noopener,noreferrer')
    return
  }

  await invoke('open_repository_url')
}

export async function openUpdateReleasePage(): Promise<void> {
  if (!isTauri()) {
    window.open(RELEASES_URL, '_blank', 'noopener,noreferrer')
    return
  }

  await invoke('open_releases_url')
}

export async function isPortableMode(): Promise<boolean> {
  if (!isTauri()) {
    return false
  }

  return invoke<boolean>('is_portable_mode')
}

export type HaloVisual = 'idle' | 'candidate' | 'pulse' | 'suspended'

export interface HaloTestOverlayOptions {
  screenId: string
  /** Screen-local logical point (halo center). */
  x: number
  y: number
  visual: HaloVisual
  /** 0..1 progress arc, only used when visual === 'candidate'. */
  progress?: number
  /** Debug-only halo diameter override (logical px); exercises the hot-resize path. */
  diameter?: number
}

// FocusHalo P0 debug entry: draw/hide one halo frame on a local screen.
export async function haloTestOverlay(options: HaloTestOverlayOptions): Promise<void> {
  if (!isTauri()) {
    return
  }

  await invoke('halo_test_overlay_show', {
    screenId: options.screenId,
    x: options.x,
    y: options.y,
    visual: options.visual,
    progress: options.progress ?? null,
    diameter: options.diameter ?? null,
  })
}

export async function haloHideOverlay(): Promise<void> {
  if (!isTauri()) {
    return
  }

  await invoke('halo_test_overlay_hide')
}

// FocusHalo P1: tracking pipeline control + calibration flow.

/** Calibration capture steps, in the order the backend expects. */
export const HALO_CALIBRATION_POINTS = ['center', 'left', 'right', 'up', 'down'] as const
export type HaloCalibrationPoint = (typeof HALO_CALIBRATION_POINTS)[number] | 'finish'

/**
 * Enable/disable the tracking pipeline. May take seconds (camera start);
 * resolves with a backend status string or rejects with a user-facing reason.
 * The caller persists the `enabled` flag afterwards via saveLayout.
 */
export async function haloSetEnabled(enabled: boolean): Promise<string> {
  if (!isTauri()) {
    return enabled ? 'running' : 'stopped'
  }

  return invoke<string>('halo_set_enabled', { enabled })
}

export async function haloGetStatus(): Promise<HaloStatus> {
  if (!isTauri()) {
    return { running: false, cameraOk: false, reason: null }
  }

  return invoke<HaloStatus>('halo_get_status')
}

export async function haloCaptureCalibrationPoint(point: HaloCalibrationPoint): Promise<void> {
  if (!isTauri()) {
    return
  }

  await invoke('halo_capture_calibration_point', { point })
}

export async function haloRecenter(): Promise<void> {
  if (!isTauri()) {
    return
  }

  await invoke('halo_recenter')
}

// Button-click fallback for the undo-switch global hotkey.
export async function haloUndoSwitch(): Promise<void> {
  if (!isTauri()) {
    return
  }

  await invoke('halo_undo_switch')
}

// Button-click fallback for the emergency-stop global hotkey.
export async function haloEmergencyStop(): Promise<void> {
  if (!isTauri()) {
    return
  }

  await invoke('halo_emergency_stop')
}

// FocusHalo S2: multi-screen region calibration wizard (转头切屏).

/** Mirrors the Rust `HaloScreenCalPlanItem` (serde camelCase). */
export interface HaloScreenCalPlanItem {
  screenIndex: number
  screenId: string
  label: number
  x: number
  y: number
  width: number
  height: number
}

/** Mirrors the Rust `HaloScreenCalProgress` (serde camelCase). */
export interface HaloScreenCalProgress {
  acceptedScreens: number
  totalScreens: number
  cornersDone: number
}

/** Mirrors the Rust `HaloScreenCalExtent` (serde camelCase). */
export interface HaloScreenCalExtent {
  screenIndex: number
  screenId: string
  label: number
  widthDeg: number
  heightDeg: number
}

/** Mirrors the Rust `HaloScreenCalSummary` (serde camelCase). */
export interface HaloScreenCalSummary {
  totalScreens: number
  capturedAtMs: number
  refU: number
  refV: number
  transAlphaYaw: number
  transAlphaPitch: number
  extents: HaloScreenCalExtent[]
}

export type HaloScreenCalCorner = 'tl' | 'tr' | 'br' | 'bl'

/**
 * Start the wizard: builds the plan (board x then y, numbered 1..N), shows
 * the native badge layer, and returns the plan. Rejects when FocusHalo is
 * not running (user-facing Chinese reason).
 */
export async function haloScreenCalBegin(): Promise<HaloScreenCalPlanItem[]> {
  if (!isTauri()) {
    return []
  }

  return invoke<HaloScreenCalPlanItem[]>('halo_screen_cal_begin')
}

/** Highlight `corner` of the screen at `screenIndex` on the badge layer. */
export async function haloScreenCalSetStep(
  screenIndex: number,
  corner: HaloScreenCalCorner,
): Promise<void> {
  if (!isTauri()) {
    return
  }

  await invoke('halo_screen_cal_set_step', { screenIndex, corner })
}

/** Capture the currently highlighted corner; validates on the backend. */
export async function haloScreenCalCapture(
  screenIndex: number,
  corner: HaloScreenCalCorner,
): Promise<HaloScreenCalProgress> {
  if (!isTauri()) {
    return { acceptedScreens: 0, totalScreens: 0, cornersDone: 0 }
  }

  return invoke<HaloScreenCalProgress>('halo_screen_cal_capture', { screenIndex, corner })
}

/** Persist the finished calibration into focus_halo and tear the wizard down. */
export async function haloScreenCalFinish(): Promise<HaloScreenCalSummary> {
  if (!isTauri()) {
    throw new Error('Available only in the Tauri desktop runtime.')
  }

  return invoke<HaloScreenCalSummary>('halo_screen_cal_finish')
}

/** Cancel the wizard: hide the badges, drop plan/corners. */
export async function haloScreenCalCancel(): Promise<void> {
  if (!isTauri()) {
    return
  }

  await invoke('halo_screen_cal_cancel')
}

/**
 * Subscribe to the FocusHalo debug stream (5 Hz while tracking runs).
 * Returns an unlisten function; resolves to a no-op outside the Tauri
 * runtime. The listener is registered through a dynamic import so the
 * browser preview never loads the event API eagerly.
 */
export async function listenHaloDebug(
  callback: (snapshot: HaloDebugSnapshot) => void,
): Promise<() => void> {
  if (!isTauri()) {
    return () => {}
  }

  const { listen } = await import('@tauri-apps/api/event')
  return listen<HaloDebugSnapshot>('halo-debug', ({ payload }) => callback(payload))
}

export async function checkForAppUpdate(): Promise<AppUpdateCheckResult> {
  if (!isTauri()) {
    return { available: false }
  }

  const { check } = await import('@tauri-apps/plugin-updater')
  // A stalled manifest fetch left the check spinning (PR #22).
  const update = await check({ timeout: 20_000 })

  if (!update) {
    return { available: false }
  }

  return {
    available: true,
    update: {
      version: update.version,
      currentVersion: update.currentVersion,
      date: update.date,
      body: update.body,
    },
  }
}

export async function setAppUpgrading(enabled: boolean): Promise<void> {
  if (!isTauri()) return
  await invoke('set_app_upgrading', { enabled })
}

export async function installAppUpdate(): Promise<void> {
  if (!isTauri()) {
    return
  }

  const [{ check }, { relaunch }] = await Promise.all([
    import('@tauri-apps/plugin-updater'),
    import('@tauri-apps/plugin-process'),
  ])
  // A stalled manifest fetch left the check spinning (PR #22).
  const update = await check({ timeout: 20_000 })

  if (!update) {
    return
  }

  await setAppUpgrading(true).catch(() => {})
  let handedOff = false
  try {
    await update.download()
    // On Windows install() ends this process without the exit hooks: close
    // the network first so the controller reconnects at once (to the input
    // service while the installer runs) instead of timing out.
    await invoke('prepare_update_install').catch(() => {})
    handedOff = true
    await update.install()
  } catch (error) {
    await setAppUpgrading(false).catch(() => {})
    if (handedOff) {
      await startRuntime().catch(() => {})
    }
    throw error
  }
  await relaunch()
}
