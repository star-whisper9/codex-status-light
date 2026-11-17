//! VIA 颜色输入的统一转换与状态调色板。
pub(super) const BLUE: u32 = 0x0000ff;
pub(super) const GREEN: u32 = 0x01ff8a;
pub(super) const RED: u32 = 0xff0000;
pub(super) const ORANGE: u32 = 0xff6701;

/// 与 VIA 十六进制颜色输入相同，转换为 0..255 的色相、饱和度并四舍五入。
/// 亮度是独立设置，不取 RGB 的 value 覆盖键盘当前亮度。
/// 算法来源：<https://github.com/the-via/app/blob/main/src/utils/color-math.ts>。
pub(super) fn via_color(rgb: u32) -> (u8, u8) {
    let r = ((rgb >> 16) & 255) as f64;
    let g = ((rgb >> 8) & 255) as f64;
    let b = (rgb & 255) as f64;
    let max = r.max(g).max(b);
    let delta = max - r.min(g).min(b);
    let hue = if delta == 0.0 {
        0.0
    } else if max == r {
        ((g - b) / delta).rem_euclid(6.0)
    } else if max == g {
        (b - r) / delta + 2.0
    } else {
        (r - g) / delta + 4.0
    };
    let saturation = if max == 0.0 { 0.0 } else { delta / max };
    (
        (255.0 * hue / 6.0).round() as u8,
        (255.0 * saturation).round() as u8,
    )
}
