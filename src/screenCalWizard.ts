// 多屏区域校准（转头切屏）向导的纯逻辑：计划排序、采集角推进、完成条件、
// 拓扑警示。后端（halo_screen_cal_* 命令）持有权威状态，这里只做展示层
// 推导，两个 `屏幕 k / 第 c 角` 推导必须与后端 corner 枚举（tl→tr→br→bl）
// 保持一致。

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

/** Mirrors the Rust `HaloScreenCalPlanItem` (serde camelCase). */
export interface HaloScreenCalPlanItem {
  screenIndex: number
  screenId: string
  label: number
  x: number
  y: number
  width: number
  height: number
  /** S4: the screen lives on an online peer device (no local badge). */
  remote: boolean
  /** S4: owning device's display name ("Mac mini"), empty for local. */
  deviceName: string
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

/** Capture corner names sent to the backend; the wizard walks them in order. */
export const SCREEN_CAL_CORNER_ORDER = ['tl', 'tr', 'br', 'bl'] as const
export type ScreenCalCorner = (typeof SCREEN_CAL_CORNER_ORDER)[number]

/** Sorting contract shared with the backend: board x first, then y. */
export function sortScreensByBoard(
  screens: { x: number; y: number }[],
): { x: number; y: number }[] {
  return [...screens].sort((a, b) => a.x - b.x || a.y - b.y)
}

/**
 * The corner the wizard should ask for next given how many corners of the
 * current screen are already committed (0-3). Wraps across screens via the
 * caller, which resets it on each accepted region.
 */
export function nextCorner(cornersDoneInScreen: number): ScreenCalCorner {
  return SCREEN_CAL_CORNER_ORDER[cornersDoneInScreen % SCREEN_CAL_CORNER_ORDER.length]
}

/** 1-based "第 c 角" label for the UI progress line. */
export function cornerOrdinal(cornersDoneInScreen: number): number {
  return (cornersDoneInScreen % SCREEN_CAL_CORNER_ORDER.length) + 1
}

/** 完成校准 button enablement: every planned screen has an accepted region. */
export function canFinishScreenCal(progress: HaloScreenCalProgress): boolean {
  return (
    progress.totalScreens > 0 && progress.acceptedScreens === progress.totalScreens
  )
}

/** All four corners of the current screen are committed. */
export function currentScreenComplete(progress: HaloScreenCalProgress): boolean {
  return (
    progress.cornersDone > 0 && progress.cornersDone % SCREEN_CAL_CORNER_ORDER.length === 0
  )
}

/**
 * 拓扑警示条件：后端存了校准且拓扑已变（`screenTopologyOk === false`）。
 * `null`/undefined（未校准）与 true 都不警示。
 */
export function showScreenCalTopologyWarning(
  screenTopologyOk: boolean | null | undefined,
): boolean {
  return screenTopologyOk === false
}
