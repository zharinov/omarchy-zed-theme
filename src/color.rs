use crate::{Error, Result};
use std::f64::consts::TAU;
use std::sync::OnceLock;

const CBRT_TABLE_BITS: u32 = 11;
const CBRT_TABLE_SIZE: usize = 1 << CBRT_TABLE_BITS;
const CBRT_EXPONENT_REMAINDERS: [f64; 3] = [1.0, 1.259_921_049_894_873_2, 1.587_401_051_968_199_4];

// libm cbrt dominated cold profiles; this bounded table covers the positive
// linear-RGB domain while keeping the uncommon numeric cases exact.

const fn cbrt_reference(value: f64) -> f64 {
    let mut estimate = 1.0;
    let mut iteration = 0;
    while iteration < 12 {
        estimate = (2.0 * estimate + value / (estimate * estimate)) / 3.0;
        iteration += 1;
    }
    estimate
}

const fn cbrt_table() -> [f64; CBRT_TABLE_SIZE + 1] {
    let mut table = [0.0; CBRT_TABLE_SIZE + 1];
    let mut index = 0;
    while index <= CBRT_TABLE_SIZE {
        table[index] = cbrt_reference(1.0 + index as f64 / CBRT_TABLE_SIZE as f64);
        index += 1;
    }
    table
}

static CBRT_MANTISSA: [f64; CBRT_TABLE_SIZE + 1] = cbrt_table();

#[inline]
fn color_cbrt(value: f64) -> f64 {
    if value == 0.0 {
        return 0.0;
    }

    if !value.is_normal() || value < 0.0 {
        return value.cbrt();
    }

    let bits = value.to_bits();
    let exponent = ((bits >> 52) & 0x7ff) as i32 - 1023;
    let fraction = bits & ((1_u64 << 52) - 1);
    let index = (fraction >> (52 - CBRT_TABLE_BITS)) as usize;
    let remainder_mask = (1_u64 << (52 - CBRT_TABLE_BITS)) - 1;
    let interpolation =
        (fraction & remainder_mask) as f64 / (1_u64 << (52 - CBRT_TABLE_BITS)) as f64;
    let lower = CBRT_MANTISSA[index];
    let root = lower + (CBRT_MANTISSA[index + 1] - lower) * interpolation;

    let exponent_quotient = exponent.div_euclid(3);
    let exponent_remainder = exponent.rem_euclid(3) as usize;
    let exponent_scale = f64::from_bits(((exponent_quotient + 1023) as u64) << 52);
    root * exponent_scale * CBRT_EXPONENT_REMAINDERS[exponent_remainder]
}

#[inline]
fn luminance_contrast_at_least(left: f64, right: f64, target: f64) -> bool {
    left.max(right) + 0.05 >= target * (left.min(right) + 0.05)
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct Rgb24(u32);

impl Rgb24 {
    pub(crate) const BLACK: Self = Self(0x000000);
    pub(crate) const WHITE: Self = Self(0xffffff);

    pub(crate) fn from_rgba(value: Rgba) -> Self {
        Self(
            (u32::from(byte(value.r)) << 16)
                | (u32::from(byte(value.g)) << 8)
                | u32::from(byte(value.b)),
        )
    }

    fn from_linear([r, g, b]: [f64; 3]) -> Self {
        Self(
            (u32::from(linear_byte(r)) << 16)
                | (u32::from(linear_byte(g)) << 8)
                | u32::from(linear_byte(b)),
        )
    }

    fn channel(self, shift: u32) -> u8 {
        (self.0 >> shift) as u8
    }

    fn blend(self, overlay: Self, alpha: u8) -> Self {
        let blend_channel = |shift| {
            let base = u32::from(self.channel(shift));
            let overlay = u32::from(overlay.channel(shift));
            let alpha = u32::from(alpha);
            // Zed consumes byte colors, so integer compositing preserves the
            // exact rounded output without repeated float conversions.
            (base * (255 - alpha) + overlay * alpha + 127) / 255
        };
        Self((blend_channel(16) << 16) | (blend_channel(8) << 8) | blend_channel(0))
    }

    pub(crate) fn hex(self) -> String {
        format!("#{:06x}", self.0)
    }
}

#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct Rgba32(u32);

impl Rgba32 {
    pub(crate) fn from_rgba(value: Rgba) -> Self {
        Self(
            (u32::from(byte(value.r)) << 24)
                | (u32::from(byte(value.g)) << 16)
                | (u32::from(byte(value.b)) << 8)
                | u32::from(byte(value.a)),
        )
    }

    pub(crate) fn from_rgb_alpha(rgb: Rgb24, alpha: u8) -> Self {
        Self((rgb.0 << 8) | u32::from(alpha))
    }

    pub(crate) fn rgba(self) -> Rgba {
        Rgba {
            r: f64::from((self.0 >> 24) as u8) / 255.0,
            g: f64::from((self.0 >> 16) as u8) / 255.0,
            b: f64::from((self.0 >> 8) as u8) / 255.0,
            a: f64::from(self.0 as u8) / 255.0,
        }
    }

    pub(crate) fn rgb24(self) -> Rgb24 {
        Rgb24(self.0 >> 8)
    }

    pub(crate) fn hex(self) -> String {
        if self.0 as u8 == u8::MAX {
            format!("#{:06x}", self.0 >> 8)
        } else {
            format!("#{:08x}", self.0)
        }
    }

    pub(crate) fn alpha(self) -> u8 {
        self.0 as u8
    }

    pub(crate) fn hex_cmp(self, other: Self) -> std::cmp::Ordering {
        self.rgb24().cmp(&other.rgb24()).then_with(|| {
            let left_alpha = self.0 as u8;
            let right_alpha = other.0 as u8;
            match (left_alpha == u8::MAX, right_alpha == u8::MAX) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => left_alpha.cmp(&right_alpha),
            }
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rgba {
    pub(crate) r: f64,
    pub(crate) g: f64,
    pub(crate) b: f64,
    pub(crate) a: f64,
}

impl Rgba {
    pub fn alpha(self) -> f64 {
        self.a
    }

    pub fn opaque_hex(self) -> String {
        format!(
            "#{:02x}{:02x}{:02x}",
            byte(self.r),
            byte(self.g),
            byte(self.b)
        )
    }

    pub fn hex(self) -> String {
        let opaque = self.opaque_hex();
        if self.a >= 1.0 {
            opaque
        } else {
            format!("{opaque}{:02x}", byte(self.a))
        }
    }
}

#[derive(Clone, Copy)]
pub struct ColorMetrics {
    pub(crate) rgba: Rgba32,
    pub lab: [f64; 3],
    pub luminance: f64,
}

#[derive(Clone, Copy)]
pub struct PreparedColor {
    rgba: Rgba32,
    linear: [f64; 3],
    pub luminance: f64,
}

impl PreparedColor {
    pub fn contrast(self, other: ColorMetrics) -> f64 {
        (self.luminance.max(other.luminance) + 0.05) / (self.luminance.min(other.luminance) + 0.05)
    }

    #[inline]
    pub(crate) fn contrast_at_least(self, other: ColorMetrics, target: f64) -> bool {
        luminance_contrast_at_least(self.luminance, other.luminance, target)
    }

    pub fn metrics(self) -> ColorMetrics {
        ColorMetrics {
            rgba: self.rgba,
            lab: linear_rgb_to_oklab(self.linear),
            luminance: self.luminance,
        }
    }
}

impl ColorMetrics {
    pub fn from_hex(value: &str) -> Result<Self> {
        Ok(Self::from_rgba(parse_hex(value)?))
    }

    pub fn from_rgba(rgba: Rgba) -> Self {
        Self::prepare(rgba).metrics()
    }

    pub(crate) fn from_rgb24(rgb: Rgb24) -> Self {
        Self::prepare_packed(Rgba32::from_rgb_alpha(rgb, u8::MAX)).metrics()
    }

    pub(crate) fn blend_rgb24(base: Self, overlay: Rgb24, alpha: u8) -> PreparedColor {
        Self::prepare_packed(Rgba32::from_rgb_alpha(
            base.rgb24().blend(overlay, alpha),
            u8::MAX,
        ))
    }

    pub(crate) fn rgb24(self) -> Rgb24 {
        self.rgba.rgb24()
    }

    pub fn prepare(rgba: Rgba) -> PreparedColor {
        Self::prepare_packed(Rgba32::from_rgba(rgba))
    }

    fn prepare_packed(rgba: Rgba32) -> PreparedColor {
        let rgb = linear_rgb24(rgba.rgb24());
        PreparedColor {
            rgba,
            linear: rgb,
            luminance: 0.2126 * rgb[0] + 0.7152 * rgb[1] + 0.0722 * rgb[2],
        }
    }

    pub fn blend(base: Self, overlay: Rgba) -> Self {
        Self::blend_prepared(base, overlay).metrics()
    }

    pub fn blend_prepared(base: Self, overlay: Rgba) -> PreparedColor {
        let base = base.rgba.rgba();
        let rgba = if overlay.a >= 1.0 {
            Rgba {
                a: base.a,
                ..overlay
            }
        } else if overlay.a <= 0.0 {
            base
        } else {
            Rgba {
                r: base.r * (1.0 - overlay.a) + overlay.r * overlay.a,
                g: base.g * (1.0 - overlay.a) + overlay.g * overlay.a,
                b: base.b * (1.0 - overlay.a) + overlay.b * overlay.a,
                a: base.a,
            }
        };
        Self::prepare_packed(Rgba32::from_rgba(rgba))
    }

    pub fn contrast(self, other: Self) -> f64 {
        (self.luminance.max(other.luminance) + 0.05) / (self.luminance.min(other.luminance) + 0.05)
    }

    #[inline]
    pub(crate) fn contrast_at_least(self, other: Self, target: f64) -> bool {
        luminance_contrast_at_least(self.luminance, other.luminance, target)
    }

    pub fn delta_e(self, other: Self) -> f64 {
        self.lab
            .into_iter()
            .zip(other.lab)
            .map(|(left, right)| (left - right).powi(2))
            .sum::<f64>()
            .sqrt()
    }
}

fn byte(value: f64) -> u8 {
    (value.clamp(0.0, 1.0) * 255.0 + 0.5).floor() as u8
}

fn linear_channel_table() -> &'static [f64; 256] {
    static TABLE: OnceLock<[f64; 256]> = OnceLock::new();
    TABLE.get_or_init(|| std::array::from_fn(|value| srgb_to_linear(value as f64 / 255.0)))
}

fn linear_byte_boundary(value: u8) -> f64 {
    // The analytic inverse can land a few binary64 values away from the old
    // forward-transfer boundary, so calibrate it against that exact operation.
    let mut boundary = srgb_to_linear((f64::from(value) + 0.5) / 255.0);
    if byte(linear_to_srgb(boundary)) <= value {
        loop {
            boundary = f64::from_bits(boundary.to_bits() + 1);
            if byte(linear_to_srgb(boundary)) > value {
                return boundary;
            }
        }
    }

    loop {
        let previous = f64::from_bits(boundary.to_bits() - 1);
        if byte(linear_to_srgb(previous)) <= value {
            return boundary;
        }
        boundary = previous;
    }
}

fn linear_byte(channel: f64) -> u8 {
    static THRESHOLDS: OnceLock<[f64; 255]> = OnceLock::new();
    let thresholds =
        THRESHOLDS.get_or_init(|| std::array::from_fn(|value| linear_byte_boundary(value as u8)));
    thresholds.partition_point(|threshold| *threshold <= channel.clamp(0.0, 1.0)) as u8
}

pub(crate) fn linear_rgb(rgba: Rgba) -> [f64; 3] {
    linear_rgb24(Rgb24::from_rgba(rgba))
}

fn linear_rgb24(rgb: Rgb24) -> [f64; 3] {
    let linear = linear_channel_table();
    [
        linear[usize::from(rgb.channel(16))],
        linear[usize::from(rgb.channel(8))],
        linear[usize::from(rgb.channel(0))],
    ]
}

pub fn parse_hex(value: &str) -> Result<Rgba> {
    let bytes = value.as_bytes();
    if (bytes.len() != 7 && bytes.len() != 9)
        || bytes.first() != Some(&b'#')
        || !bytes[1..].iter().all(u8::is_ascii_hexdigit)
    {
        return Err(Error::invalid(format!("invalid hex color: {value:?}")));
    }

    let parse = |range: std::ops::Range<usize>| {
        u8::from_str_radix(&value[range], 16)
            .map(|part| part as f64 / 255.0)
            .map_err(|_| Error::invalid(format!("invalid hex color: {value:?}")))
    };
    Ok(Rgba {
        r: parse(1..3)?,
        g: parse(3..5)?,
        b: parse(5..7)?,
        a: if bytes.len() == 9 { parse(7..9)? } else { 1.0 },
    })
}

pub(crate) fn validate_opaque_hex(value: &str, label: impl std::fmt::Display) -> Result<()> {
    if value.len() != 7
        || !value.starts_with('#')
        || !value[1..].bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(Error::invalid(format!(
            "{label} must be a six-digit hex color, got {value:?}"
        )));
    }
    Ok(())
}

pub fn normalize_hex(value: &str, key: &str) -> Result<String> {
    validate_opaque_hex(value, format_args!("resolved palette key {key:?}"))?;
    Ok(value.to_ascii_lowercase())
}

pub fn srgb_to_linear(channel: f64) -> f64 {
    if channel <= 0.04045 {
        channel / 12.92
    } else {
        ((channel + 0.055) / 1.055).powf(2.4)
    }
}

pub fn linear_to_srgb(channel: f64) -> f64 {
    let channel = channel.clamp(0.0, 1.0);
    if channel <= 0.0031308 {
        12.92 * channel
    } else {
        1.055 * channel.powf(1.0 / 2.4) - 0.055
    }
}

pub fn rgb_to_oklab(color: Rgba) -> [f64; 3] {
    linear_rgb_to_oklab([
        srgb_to_linear(color.r),
        srgb_to_linear(color.g),
        srgb_to_linear(color.b),
    ])
}

fn linear_rgb_to_oklab([r, g, b]: [f64; 3]) -> [f64; 3] {
    let l = 0.412_221_470_8 * r + 0.536_332_536_3 * g + 0.051_445_992_9 * b;
    let m = 0.211_903_498_2 * r + 0.680_699_545_1 * g + 0.107_396_956_6 * b;
    let s = 0.088_302_461_9 * r + 0.281_718_837_6 * g + 0.629_978_700_5 * b;
    let cube_root = if r == g && g == b {
        f64::cbrt
    } else {
        color_cbrt
    };
    let [l, m, s] = [cube_root(l), cube_root(m), cube_root(s)];
    [
        0.210_454_255_3 * l + 0.793_617_785 * m - 0.004_072_046_8 * s,
        1.977_998_495_1 * l - 2.428_592_205 * m + 0.450_593_709_9 * s,
        0.025_904_037_1 * l + 0.782_771_766_2 * m - 0.808_675_766 * s,
    ]
}

pub(crate) fn oklab_to_linear_rgb([lightness, a, b]: [f64; 3]) -> [f64; 3] {
    let l = lightness + 0.396_337_777_4 * a + 0.215_803_757_3 * b;
    let m = lightness - 0.105_561_345_8 * a - 0.063_854_172_8 * b;
    let s = lightness - 0.089_484_177_5 * a - 1.291_485_548 * b;
    let [l, m, s] = [l * l * l, m * m * m, s * s * s];
    [
        4.076_741_662_1 * l - 3.307_711_591_3 * m + 0.230_969_929_2 * s,
        -1.268_438_004_6 * l + 2.609_757_401_1 * m - 0.341_319_396_5 * s,
        -0.004_196_086_3 * l - 0.703_418_614_7 * m + 1.707_614_701 * s,
    ]
}

pub(crate) fn oklab_to_rgb(lab: [f64; 3], alpha: f64) -> Rgba {
    assert!(
        lab.into_iter().all(f64::is_finite),
        "internal OKLab color must have finite components"
    );
    assert!(
        alpha.is_finite() && (0.0..=1.0).contains(&alpha),
        "internal alpha must be finite and in 0..=1"
    );
    let [r, g, b] = oklab_to_linear_rgb(lab);
    Rgba {
        r: linear_to_srgb(r),
        g: linear_to_srgb(g),
        b: linear_to_srgb(b),
        a: alpha,
    }
}

pub fn oklab_to_oklch([lightness, a, b]: [f64; 3]) -> [f64; 3] {
    let chroma = a.hypot(b);
    let hue = if chroma > 1e-12 {
        b.atan2(a).rem_euclid(TAU)
    } else {
        0.0
    };
    [lightness, chroma, hue]
}

pub fn oklch_to_oklab([lightness, chroma, hue]: [f64; 3]) -> [f64; 3] {
    [lightness, chroma * hue.cos(), chroma * hue.sin()]
}

fn in_gamut(lab: [f64; 3]) -> bool {
    oklab_to_linear_rgb(lab)
        .into_iter()
        .all(|channel| (-1e-9..=1.0 + 1e-9).contains(&channel))
}

fn smoothstep(edge0: f64, edge1: f64, value: f64) -> f64 {
    if edge0 == edge1 {
        return 0.0;
    }
    let position = ((value - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    position * position * (3.0 - 2.0 * position)
}

pub fn endpoint_chroma_taper(lightness: f64) -> f64 {
    smoothstep(0.0, 0.12, lightness).min(smoothstep(0.0, 0.12, 1.0 - lightness))
}

pub fn gamut_map_oklch(lightness: f64, chroma: f64, hue: f64) -> Result<Rgba> {
    finite_unit_interval("OKLCH lightness", lightness)?;
    if !chroma.is_finite() || chroma < 0.0 {
        return Err(Error::invalid(format!(
            "OKLCH chroma must be finite and non-negative, got {chroma:?}"
        )));
    }
    if !hue.is_finite() {
        return Err(Error::invalid(format!(
            "OKLCH hue must be finite, got {hue:?}"
        )));
    }
    Ok(gamut_map_oklch_unchecked(lightness, chroma, hue))
}

pub(crate) fn gamut_map_oklch_unchecked(lightness: f64, chroma: f64, hue: f64) -> Rgba {
    assert!(
        lightness.is_finite() && (0.0..=1.0).contains(&lightness),
        "internal OKLCH lightness must be finite and in 0..=1"
    );
    assert!(
        chroma.is_finite() && chroma >= 0.0,
        "internal OKLCH chroma must be finite and non-negative"
    );
    assert!(hue.is_finite(), "internal OKLCH hue must be finite");
    let candidate = oklch_to_oklab([lightness, chroma, hue]);
    if in_gamut(candidate) {
        return oklab_to_rgb(candidate, 1.0);
    }
    gamut_map_oklch_with_limit(lightness, chroma, hue, gamut_chroma_limit(lightness, hue))
}

pub(crate) fn gamut_chroma_limit(lightness: f64, hue: f64) -> f64 {
    gamut_chroma_limit_with_components(lightness, hue.cos(), hue.sin())
}

pub(crate) fn gamut_chroma_limit_with_components(
    lightness: f64,
    hue_cos: f64,
    hue_sin: f64,
) -> f64 {
    let lightness = lightness.clamp(0.0, 1.0);
    let mut low = 0.0;
    let mut high = 0.5;

    while high < 4.0 && in_gamut([lightness, high * hue_cos, high * hue_sin]) {
        low = high;
        high *= 2.0;
    }

    for _ in 0..32 {
        let middle = (low + high) / 2.0;
        if in_gamut([lightness, middle * hue_cos, middle * hue_sin]) {
            low = middle;
        } else {
            high = middle;
        }
    }

    low
}

pub(crate) fn oklch_in_gamut_with_components(
    lightness: f64,
    chroma: f64,
    hue_cos: f64,
    hue_sin: f64,
) -> bool {
    in_gamut([
        lightness.clamp(0.0, 1.0),
        chroma.max(0.0) * hue_cos,
        chroma.max(0.0) * hue_sin,
    ])
}

pub(crate) fn gamut_map_oklch_with_limit(
    lightness: f64,
    chroma: f64,
    hue: f64,
    chroma_limit: f64,
) -> Rgba {
    assert!(
        lightness.is_finite() && (0.0..=1.0).contains(&lightness),
        "internal OKLCH lightness must be finite and in 0..=1"
    );
    assert!(
        chroma.is_finite() && chroma >= 0.0,
        "internal OKLCH chroma must be finite and non-negative"
    );
    assert!(hue.is_finite(), "internal OKLCH hue must be finite");
    assert!(
        !chroma_limit.is_nan() && chroma_limit >= 0.0,
        "internal gamut chroma limit must be non-negative and not NaN"
    );
    gamut_map_oklch_with_components(lightness, chroma, hue.cos(), hue.sin(), chroma_limit)
}

pub(crate) fn gamut_map_oklch_with_components(
    lightness: f64,
    chroma: f64,
    hue_cos: f64,
    hue_sin: f64,
    chroma_limit: f64,
) -> Rgba {
    assert!(
        lightness.is_finite() && (0.0..=1.0).contains(&lightness),
        "internal OKLCH lightness must be finite and in 0..=1"
    );
    assert!(
        chroma.is_finite() && chroma >= 0.0,
        "internal OKLCH chroma must be finite and non-negative"
    );
    assert!(
        hue_cos.is_finite() && hue_sin.is_finite(),
        "internal hue components must be finite"
    );
    assert!(
        !chroma_limit.is_nan() && chroma_limit >= 0.0,
        "internal gamut chroma limit must be non-negative and not NaN"
    );
    oklab_to_rgb(
        [
            lightness,
            chroma.min(chroma_limit) * hue_cos,
            chroma.min(chroma_limit) * hue_sin,
        ],
        1.0,
    )
}

pub(crate) fn gamut_map_oklch_rgb24_with_components(
    lightness: f64,
    chroma: f64,
    hue_cos: f64,
    hue_sin: f64,
    chroma_limit: f64,
) -> Rgb24 {
    let lightness = lightness.clamp(0.0, 1.0);
    let chroma = chroma.max(0.0).min(chroma_limit);
    Rgb24::from_linear(oklab_to_linear_rgb([
        lightness,
        chroma * hue_cos,
        chroma * hue_sin,
    ]))
}

pub fn lab(value: &str) -> Result<[f64; 3]> {
    Ok(rgb_to_oklab(parse_hex(value)?))
}

pub fn lightness(value: &str) -> Result<f64> {
    Ok(lab(value)?[0])
}

pub fn delta_e(first: &str, second: &str) -> Result<f64> {
    let (first, second) = (lab(first)?, lab(second)?);
    Ok(first
        .into_iter()
        .zip(second)
        .map(|(left, right)| (left - right).powi(2))
        .sum::<f64>()
        .sqrt())
}

pub fn relative_luminance(value: &str) -> Result<f64> {
    let color = parse_hex(value)?;
    Ok(0.2126 * srgb_to_linear(color.r)
        + 0.7152 * srgb_to_linear(color.g)
        + 0.0722 * srgb_to_linear(color.b))
}

pub fn contrast_ratio(first: &str, second: &str) -> Result<f64> {
    let (first, second) = (relative_luminance(first)?, relative_luminance(second)?);
    Ok((first.max(second) + 0.05) / (first.min(second) + 0.05))
}

pub(crate) fn geometric_contrast(color: &str, backgrounds: &[String]) -> Result<f64> {
    if backgrounds.is_empty() {
        return Err(Error::invalid(
            "geometric contrast requires at least one background",
        ));
    }
    let mean_log = backgrounds
        .iter()
        .map(|background| contrast_ratio(color, background).map(f64::ln))
        .sum::<Result<f64>>()?
        / backgrounds.len() as f64;
    Ok(mean_log.exp())
}

pub fn gpui_blend(base: &str, overlay: &str) -> Result<Rgba> {
    let (base, overlay) = (parse_hex(base)?, parse_hex(overlay)?);
    if overlay.a >= 1.0 {
        return Ok(Rgba {
            a: base.a,
            ..overlay
        });
    }

    if overlay.a <= 0.0 {
        return Ok(base);
    }

    Ok(Rgba {
        r: base.r * (1.0 - overlay.a) + overlay.r * overlay.a,
        g: base.g * (1.0 - overlay.a) + overlay.g * overlay.a,
        b: base.b * (1.0 - overlay.a) + overlay.b * overlay.a,
        a: base.a,
    })
}

pub fn render_layers(base: &str, overlays: &[&str]) -> Result<String> {
    overlays
        .iter()
        .try_fold(base.to_owned(), |rendered, overlay| {
            Ok(gpui_blend(&rendered, overlay)?.opaque_hex())
        })
}

pub fn with_alpha(value: &str, alpha: f64) -> Result<String> {
    finite_unit_interval("alpha", alpha)?;
    Ok(Rgba {
        a: alpha,
        ..parse_hex(value)?
    }
    .hex())
}

pub fn apply_opacity(value: &str, factor: f64) -> Result<String> {
    finite_unit_interval("opacity factor", factor)?;
    let color = parse_hex(value)?;
    Ok(Rgba {
        a: color.a * factor,
        ..color
    }
    .hex())
}

pub fn tone(value: &str, target_lightness: f64, chroma_scale: f64) -> Result<String> {
    finite_unit_interval("target lightness", target_lightness)?;
    if !chroma_scale.is_finite() || chroma_scale < 0.0 {
        return Err(Error::invalid(format!(
            "chroma scale must be finite and non-negative, got {chroma_scale:?}"
        )));
    }
    let [_, chroma, hue] = oklab_to_oklch(lab(value)?);
    Ok(gamut_map_oklch_unchecked(
        target_lightness,
        chroma * chroma_scale * endpoint_chroma_taper(target_lightness),
        hue,
    )
    .opaque_hex())
}

fn finite_unit_interval(name: &str, value: f64) -> Result<()> {
    if !value.is_finite() || !(0.0..=1.0).contains(&value) {
        return Err(Error::invalid(format!(
            "{name} must be finite and in 0..=1, got {value:?}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_vectors() {
        assert_eq!(rgb_to_oklab(parse_hex("#000000").unwrap()), [0.0, 0.0, 0.0]);
        assert!((rgb_to_oklab(parse_hex("#ffffff").unwrap())[0] - 1.0).abs() < 1e-7);
        assert_eq!(
            oklab_to_rgb(lab("#449dab").unwrap(), 1.0).opaque_hex(),
            "#449dab"
        );
        assert!((contrast_ratio("#000000", "#ffffff").unwrap() - 21.0).abs() < 1e-12);
        assert_eq!(
            gpui_blend("#000000", "#ffffff80").unwrap().opaque_hex(),
            "#808080"
        );
    }

    #[test]
    fn malformed_non_ascii_hex_is_rejected() {
        assert!(parse_hex("#€éa").is_err());
    }

    #[test]
    fn scalar_color_operations_reject_invalid_numeric_domains() {
        for error in [
            with_alpha("#112233", f64::NAN).unwrap_err(),
            with_alpha("#112233", 1.1).unwrap_err(),
            apply_opacity("#112233", -0.1).unwrap_err(),
            tone("#112233", 0.5, f64::NAN).unwrap_err(),
            gamut_map_oklch(f64::NAN, 0.1, 0.0).unwrap_err(),
            gamut_map_oklch(0.5, -0.1, 0.0).unwrap_err(),
            gamut_map_oklch(0.5, 0.1, f64::INFINITY).unwrap_err(),
        ] {
            assert_eq!(error.kind(), crate::ErrorKind::InvalidInput);
        }
    }

    #[test]
    fn color_cube_root_matches_the_standard_library() {
        for exponent in -32..=0 {
            for mantissa_index in 0..=4096 {
                let mantissa = 1.0 + (f64::from(mantissa_index) + 0.37) / 4097.0;
                let value = mantissa * 2.0_f64.powi(exponent);
                let expected = value.cbrt();
                assert!((color_cbrt(value) - expected).abs() <= 7e-9, "{value}");
            }
        }
    }

    #[test]
    fn linear_byte_matches_srgb_quantization() {
        for index in 0..=100_000 {
            let channel = f64::from(index) / 100_000.0;
            assert_eq!(linear_byte(channel), byte(linear_to_srgb(channel)));
        }

        let thresholds = std::array::from_fn::<_, 255, _>(|value| {
            let mut low = 0.0_f64.to_bits();
            let mut high = 1.0_f64.to_bits();
            while low + 1 < high {
                let middle = low + (high - low) / 2;
                if byte(linear_to_srgb(f64::from_bits(middle))) <= value as u8 {
                    low = middle;
                } else {
                    high = middle;
                }
            }
            f64::from_bits(high)
        });
        for threshold in thresholds {
            for bits in threshold.to_bits() - 1..=threshold.to_bits() + 1 {
                let channel = f64::from_bits(bits);
                assert_eq!(linear_byte(channel), byte(linear_to_srgb(channel)));
            }
        }
    }

    #[test]
    fn contrast_threshold_check_matches_ratio_comparison() {
        let targets = [1.0, 1.001, 1.01, 1.08, 1.12, 1.5, 3.0, 4.5, 7.0, 21.0];
        for &left in linear_channel_table() {
            for &right in linear_channel_table() {
                let ratio = (left.max(right) + 0.05) / (left.min(right) + 0.05);
                for target in targets {
                    assert_eq!(
                        luminance_contrast_at_least(left, right, target),
                        ratio >= target,
                        "left={left} right={right} target={target} ratio={ratio}",
                    );
                }
            }
        }
    }

    #[test]
    fn direct_rgb24_gamut_mapping_matches_rgba_mapping() {
        for lightness_index in 0..=32 {
            let lightness = f64::from(lightness_index) / 32.0;
            for chroma_index in 0..=16 {
                let chroma = f64::from(chroma_index) * 0.025;
                for hue_index in 0..36 {
                    let hue = f64::from(hue_index) * TAU / 36.0;
                    let (hue_cos, hue_sin) = (hue.cos(), hue.sin());
                    let limit = gamut_chroma_limit_with_components(lightness, hue_cos, hue_sin);
                    for chroma_limit in [limit, f64::INFINITY] {
                        let direct = gamut_map_oklch_rgb24_with_components(
                            lightness,
                            chroma,
                            hue_cos,
                            hue_sin,
                            chroma_limit,
                        );
                        let rgba = Rgb24::from_rgba(gamut_map_oklch_with_components(
                            lightness,
                            chroma,
                            hue_cos,
                            hue_sin,
                            chroma_limit,
                        ));
                        assert_eq!(direct, rgba, "L={lightness} C={chroma} h={hue}");
                    }
                }
            }
        }
    }

    #[test]
    fn packed_blend_matches_quantized_floating_point() {
        for base in 0..=u8::MAX {
            for overlay in 0..=u8::MAX {
                for alpha in 0..=u8::MAX {
                    let base_rgb = Rgb24(u32::from(base));
                    let overlay_rgb = Rgb24(u32::from(overlay));
                    let actual = base_rgb.blend(overlay_rgb, alpha).channel(0);
                    let expected = byte(
                        f64::from(base) / 255.0 * (1.0 - f64::from(alpha) / 255.0)
                            + f64::from(overlay) / 255.0 * f64::from(alpha) / 255.0,
                    );
                    assert_eq!(
                        actual, expected,
                        "base={base} overlay={overlay} alpha={alpha}"
                    );
                }
            }
        }
    }
}
