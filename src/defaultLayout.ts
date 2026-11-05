import type { LayoutState } from './types'

const platformName = navigator.platform.toLowerCase()
const browserPlatform = platformName.includes('mac')
  ? 'macos'
  : platformName.includes('win')
    ? 'windows'
    : 'unknown'
const browserScreen = window.screen as typeof window.screen & {
  availLeft?: number
  availTop?: number
}
const browserWidth = Math.max(window.screen.width, 1)
const browserHeight = Math.max(window.screen.height, 1)

export const defaultLayout: LayoutState = {
  activeDeviceId: 'local-device',
  selectedScreenId: 'browser-display-1',
  inputMode: 'control',
  machineRole: 'unset',
  clusterId: 'browser-cluster',
  pairSecret: 'browser-secret',
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
  screenSwitchHotkeys: { left: 'alt+left', right: 'alt+right', up: 'alt+up', down: 'alt+down' },
  focusHalo: {
    enabled: false,
    dwellMs: 900,
    settleMs: 150,
    enterSpeed: 0.15,
    cooldownMs: 500,
    undoMs: 5000,
    deadZonePercent: 5,
    gain: 1,
    haloDiameter: 160,
    haloOpacity: 0.55,
    showProgress: true,
    switchZone: 'anywhere',
    cameraPosition: 'top',
    invertPitch: false,
    keyboardSuspends: true,
    titleBlacklist: [],
    processBlacklist: [],
    emergencyStopHotkey: 'disabled',
    undoHotkey: 'disabled',
    recenterHotkey: 'disabled',
  },
  devices: [
    {
      id: 'local-device',
      name: 'Desktop fallback',
      platform: browserPlatform,
      host: window.location.hostname || 'localhost',
      transportPort: 47833,
      quicPort: 47834,
      transportPublicKey: '',
      protocolVersion: 1,
      color: '#2f7af8',
      online: true,
      inputReady: false,
      role: 'local',
      source: 'detected',
      screens: [
        {
          id: 'browser-display-1',
          deviceId: 'local-device',
          name: 'Browser display',
          x: browserScreen.availLeft ?? 0,
          y: browserScreen.availTop ?? 0,
          width: Math.round(browserWidth),
          height: Math.round(browserHeight),
          scale: window.devicePixelRatio || 1,
          isPrimary: true,
        },
      ],
    },
  ],
}
