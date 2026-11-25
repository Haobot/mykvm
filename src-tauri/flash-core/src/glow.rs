//! 提示光圈的双层虚化光晕剖面：两平台共用的纯几何/合成模型。
//!
//! Linux 是空实现平台（other_imp），剖面只被 macOS/Windows 的绘制器与
//! 单测消费，模块级豁免该平台的死代码告警。
//!
//! 原生重绘 screen-indicator.html 时代的 CSS 双层光晕：
//! 外层 `.glow` 大半径模糊弥散光晕 + 内层 `.ring` 轻虚化亮芯。
//! CSS 已随 WebView 实现删除，数值以那时的规范为锚点：
//!
//! - 脊线 = 两层描边的共同中心线，距屏幕边缘 12px
//!   （.glow 边框带 4..20px、.ring 边框带 8..16px，中心线都在 12px）；
//!   圆角 18（22 - 8/2 = 26 - 16/2 = 18）。
//! - 亮芯 = 8px 边框 rgba(130,195,255,0.85) + blur(3)。
//! - 光晕 = 16px 边框 rgba(56,150,255,0.35) + blur(18) + 外阴影 28px。
//!
//! 模糊用"同轴多层描边"近似：每个壳层是一圈固定宽度/透明度的描边，
//! 逐层 source-over 叠加（覆盖率 = 1-∏(1-α)）形成平滑衰减裙边。
//! macOS 把壳层画成叠放的 CAShapeLayer，Windows 按同一剖面逐像素填充，
//! [`composite_pixel`] 的合成模型与 CA 的 source-over 逐层合成一致。

/// 脊线距窗口边缘的内缩量（pt）。
pub(crate) const SPINE_INSET: f64 = 12.0;

/// 脊线圆角半径（pt）。
pub(crate) const SPINE_RADIUS: f64 = 18.0;

/// 光晕颜色 `rgba(56, 150, 255)`（通道 0-255）。透明度由壳层剖面给出。
pub(crate) const GLOW_RGB: (f64, f64, f64) = (56.0, 150.0, 255.0);

/// 亮芯颜色 `rgba(130, 195, 255)`。透明度由壳层剖面给出。
pub(crate) const CORE_RGB: (f64, f64, f64) = (130.0, 195.0, 255.0);

/// 外层光晕壳层剖面：(描边全宽 pt, 单层 alpha)，由外到内。
/// 7 层低透明度叠出大半径弥散裙边，模拟 blur(18px) 的高斯衰减。
pub(crate) const GLOW_SHELLS: [(f64, f64); 7] = [
    (42.0, 0.02),
    (36.0, 0.03),
    (30.0, 0.045),
    (25.0, 0.06),
    (20.0, 0.075),
    (15.0, 0.095),
    (10.0, 0.12),
];

/// 内层亮芯壳层剖面：(描边全宽 pt, 单层 alpha)，由外到内。
/// 细亮芯 + 轻虚化肩，模拟 8px 边框 blur(3px)。
pub(crate) const CORE_SHELLS: [(f64, f64); 7] = [
    (13.0, 0.06),
    (11.0, 0.08),
    (9.0, 0.11),
    (7.0, 0.15),
    (5.0, 0.2),
    (3.5, 0.26),
    (2.0, 0.4),
];

/// 距脊线 `|sd|` 处某族壳层的累计覆盖率：所有宽到盖住该处的壳层
/// source-over 叠加 = 1-∏(1-α)。壳层由外到内排列，宽到窄，一旦
/// 盖不住即可提前收工。
#[cfg_attr(not(any(target_os = "windows", test)), allow(dead_code))]
pub(crate) fn coverage(sd: f64, shells: &[(f64, f64)]) -> f64 {
    let sd = sd.abs();
    let mut miss = 1.0_f64;
    for (width, alpha) in shells {
        if width / 2.0 < sd {
            break;
        }
        miss *= 1.0 - alpha;
    }
    1.0 - miss
}

/// 点到圆角矩形描边（脊线）的有符号距离（pt，外正内负）。
/// 容忍退化输入（w/h ≤ 0、圆角过大），与 halo 的 inside_rounded 同策略。
#[cfg_attr(not(any(target_os = "windows", test)), allow(dead_code))]
pub(crate) fn sd_rounded_rect(px: f64, py: f64, x: f64, y: f64, w: f64, h: f64, r: f64) -> f64 {
    if w <= 0.0 || h <= 0.0 {
        return f64::INFINITY;
    }
    let r = r.max(0.0).min(w / 2.0).min(h / 2.0);
    let qx = (px - x - w / 2.0).abs() - (w / 2.0 - r);
    let qy = (py - y - h / 2.0).abs() - (h / 2.0 - r);
    let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
    let inside = qx.max(qy).min(0.0);
    outside + inside - r
}

/// 亮芯 over 光晕的像素合成（premultiplied BGRA，Windows 分层窗格式）。
/// `ac`/`ag` 是两族壳层的覆盖率，`pulse` 是脉冲透明度（整体缩放，与
/// CSS 逐元素 opacity 动画一致）。macOS 不调用本函数——CA 逐层
/// source-over 与这里的合成模型等价。
#[cfg_attr(not(any(target_os = "windows", test)), allow(dead_code))]
pub(crate) fn composite_pixel(ac: f64, ag: f64, pulse: f64) -> u32 {
    let ac = (ac * pulse).clamp(0.0, 1.0);
    let ag = (ag * pulse).clamp(0.0, 1.0);
    let out_a = ac + ag * (1.0 - ac);
    let (cr, cg, cb) = CORE_RGB;
    let (gr, gg, gb) = GLOW_RGB;
    let over = 1.0 - ac;
    let byte = |channel: f64, mul: f64| ((channel / 255.0) * mul * 255.0).round() as u32;
    let a8 = (out_a * 255.0).round() as u32;
    (a8 << 24)
        | ((byte(cr, ac) + byte(gr, ag * over)) << 16)
        | ((byte(cg, ac) + byte(gg, ag * over)) << 8)
        | (byte(cb, ac) + byte(gb, ag * over))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 壳层剖面由外到内有序：宽度递减、alpha 在 (0,1)、宽度为正。
    fn assert_profile_ordered(shells: &[(f64, f64)]) {
        for pair in shells.windows(2) {
            assert!(pair[0].0 > pair[1].0, "壳层宽度必须由外到内递减: {pair:?}");
        }
        for (width, alpha) in shells {
            assert!(*width > 0.0, "壳层宽度必须为正: {shells:?}");
            assert!(
                (0.0..1.0).contains(alpha),
                "壳层 alpha 必须落在 (0,1): {shells:?}"
            );
        }
    }

    #[test]
    fn shell_profiles_are_ordered() {
        assert_profile_ordered(&GLOW_SHELLS);
        assert_profile_ordered(&CORE_SHELLS);
    }

    #[test]
    fn coverage_peaks_match_the_css_layer_alphas() {
        // 逐层 source-over 累计的峰值分别落在 CSS 两层描边的 alpha 附近：
        // 光晕 ≈ 0.35、亮芯 ≈ 0.85（壳层化后略降）。
        let glow_peak = coverage(0.0, &GLOW_SHELLS);
        let core_peak = coverage(0.0, &CORE_SHELLS);
        assert!((0.30..=0.45).contains(&glow_peak), "glow peak {glow_peak}");
        assert!((0.70..=0.90).contains(&core_peak), "core peak {core_peak}");
    }

    #[test]
    fn coverage_is_symmetric_and_fades_to_zero() {
        for shells in [&GLOW_SHELLS[..], &CORE_SHELLS[..]] {
            let outermost = shells[0].0 / 2.0;
            assert_eq!(coverage(outermost + 0.01, shells), 0.0, "裙边外必须归零");
            assert_eq!(coverage(1000.0, shells), 0.0);
            let left = coverage(-3.0, shells);
            let right = coverage(3.0, shells);
            assert_eq!(left, right, "覆盖率必须对称");
            // 随 |sd| 单调不增（采样验证）。
            let mut prev = coverage(0.0, shells);
            for step in 1..=20 {
                let now = coverage(f64::from(step) * outermost / 20.0, shells);
                assert!(now <= prev + 1e-12, "覆盖率必须单调不增");
                prev = now;
            }
        }
    }

    #[test]
    fn coverage_matches_explicit_source_over_product() {
        // 手工展开一层：覆盖处 = 1-(1-0.2)(1-0.5) = 0.6。
        let shells = [(10.0, 0.2), (4.0, 0.5)];
        assert!((coverage(2.0, shells.as_slice()) - 0.6).abs() < 1e-12);
        // 只有外层够得着。
        assert!((coverage(3.0, shells.as_slice()) - 0.2).abs() < 1e-12);
    }

    #[test]
    fn sd_rounded_rect_measures_from_the_outline() {
        // 矩形 (0,0)-(20,20)、圆角 4：右缘 x=20，外侧 +2 → 2，内侧 -3。
        assert!((sd_rounded_rect(22.0, 10.0, 0.0, 0.0, 20.0, 20.0, 4.0) - 2.0).abs() < 1e-9);
        assert!((sd_rounded_rect(17.0, 10.0, 0.0, 0.0, 20.0, 20.0, 4.0) + 3.0).abs() < 1e-9);
        // 直边中点在描边（sd=0）上。
        assert!(sd_rounded_rect(20.0, 10.0, 0.0, 0.0, 20.0, 20.0, 4.0).abs() < 1e-9);
        // 角外（0,0 方向）到圆角弧心 (4,4) 距离 5√2，弧外 5√2-4。
        let corner = sd_rounded_rect(-1.0, -1.0, 0.0, 0.0, 20.0, 20.0, 4.0);
        assert!((corner - (5.0 * 2.0_f64.sqrt() - 4.0)).abs() < 1e-9);
    }

    #[test]
    fn sd_rounded_rect_tolerates_degenerate_input() {
        assert!(sd_rounded_rect(0.0, 0.0, 0.0, 0.0, 0.0, 10.0, 4.0).is_infinite());
        // 圆角过大按半宽钳制，不产生 NaN。
        assert!(sd_rounded_rect(3.0, 3.0, 0.0, 0.0, 4.0, 4.0, 99.0).is_finite());
    }

    #[test]
    fn composite_pixel_endpoints_follow_the_model() {
        // 纯亮芯（ag=0、ac=1、pulse=1）→ CORE_RGB 全量预乘。
        let core_only = composite_pixel(1.0, 0.0, 1.0);
        assert_eq!(core_only >> 24, 255);
        assert_eq!((core_only >> 16) & 0xFF, 130);
        assert_eq!((core_only >> 8) & 0xFF, 195);
        assert_eq!(core_only & 0xFF, 255);
        // 无覆盖 → 全透明。
        assert_eq!(composite_pixel(0.0, 0.0, 1.0), 0);
        // pulse=0 → 全透明（脉冲整体缩放透明度）。
        assert_eq!(composite_pixel(1.0, 1.0, 0.0), 0);
    }

    #[test]
    fn composite_pixel_layers_core_over_glow() {
        // ac=ag=1/2：out_a = 0.5+0.5*0.5 = 0.75 → 191。
        let pixel = composite_pixel(0.5, 0.5, 1.0);
        let a = pixel >> 24;
        assert!((a as i64 - 191).abs() <= 1, "alpha {a}");
        // 亮芯在前：红通道 = 130*0.5 + 56*0.5*0.5 = 79。
        let r = (pixel >> 16) & 0xFF;
        assert!((r as i64 - 79).abs() <= 1, "red {r}");
    }
}
