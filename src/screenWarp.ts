// 转头切屏（S3）设置 UI 的纯逻辑：滑杆范围、共享平移补偿系数、
// 「未校准不生效」提示条件。后端 FocusHaloSettings.sanitized() 的范围
// 与这里必须一致（regionSettleMs 150–800 / regionMargin 1.0–1.5 /
// transAlpha 0–120），前端先夹一次让滑杆手感与落盘值一致。

/** 转头切屏滑杆范围与默认值；须与 Rust `params.rs` 的 sanitize 范围一致。 */
export const SCREEN_WARP_RANGES = {
  /** 新区域需持续注视多久才切换（毫秒）。 */
  regionSettleMs: { min: 150, max: 800, step: 50, default: 300 },
  /** 姿态落在所有区域外时的扩容系数（1.0 = 不容忍）。 */
  regionMargin: { min: 1.0, max: 1.5, step: 0.05, default: 1.15 },
  /** 平移补偿系数（度/单位）；一个滑杆同时写 transAlphaYaw/Pitch。 */
  transAlpha: { min: 0, max: 120, step: 5, default: 60 },
} as const

/** Clamp a slider value into its documented range (defensive vs old configs). */
export function clampToRange(
  range: { min: number; max: number },
  value: number,
): number {
  if (!Number.isFinite(value)) {
    return range.min
  }
  return Math.min(range.max, Math.max(range.min, value))
}

/**
 * 共享「平移补偿系数」滑杆的落盘结果：同时写 transAlphaYaw 与
 * transAlphaPitch 两个字段（水平/垂直各自拟合是后续微移拟合的事）。
 */
export function sharedTransAlpha(value: number): {
  transAlphaYaw: number
  transAlphaPitch: number
} {
  const clamped = clampToRange(SCREEN_WARP_RANGES.transAlpha, value)
  return { transAlphaYaw: clamped, transAlphaPitch: clamped }
}

/**
 * 「转头切屏」开关打开但没有可用的多屏校准时，主区需要灰字提示
 * 「需先完成多屏区域校准，当前不会生效」。开关关闭时不提示（关着本来
 * 就不生效）。
 */
export function screenWarpNeedsCalibration(settings: {
  screenWarpEnabled?: boolean
  screenCalibration?: { regions: unknown[] } | null
}): boolean {
  return (
    settings.screenWarpEnabled === true &&
    !(settings.screenCalibration && settings.screenCalibration.regions.length > 0)
  )
}
