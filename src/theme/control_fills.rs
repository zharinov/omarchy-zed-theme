use super::borders::{Contact, ContactGroup, Purpose, fit};
use super::ui_policy::StructurePolicy;
use crate::Result;
use crate::color::{ColorMetrics, Rgba, oklab_to_oklch, parse_hex, tone};
use crate::search::MetricBand;
use std::collections::BTreeSet;

pub(super) fn tinted_hover(fill: &str, dark: bool) -> Result<String> {
    // Zed's Theme::darken subtracts HSL lightness, not OKLCH lightness.
    let color = parse_hex(fill)?;
    let minimum = color.r.min(color.g).min(color.b);
    let maximum = color.r.max(color.g).max(color.b);
    let lightness = (minimum + maximum) / 2.0;
    let next_lightness = (lightness - if dark { 0.2 } else { 0.05 }).max(0.0);
    let chroma = maximum - minimum;
    let saturation = if chroma == 0.0 {
        0.0
    } else {
        chroma / (1.0 - (2.0 * lightness - 1.0).abs())
    };
    let next_chroma = saturation * (1.0 - (2.0 * next_lightness - 1.0).abs());
    let channel = |value| {
        if chroma == 0.0 {
            next_lightness
        } else {
            next_lightness + ((value - minimum) / chroma - 0.5) * next_chroma
        }
    };
    Ok(Rgba {
        r: channel(color.r),
        g: channel(color.g),
        b: channel(color.b),
        a: color.a,
    }
    .hex())
}

struct ControlSurface {
    host: ColorMetrics,
    frame: ColorMetrics,
}

struct FillSource {
    color: ColorMetrics,
    chroma: f64,
    chromatic: bool,
    preferred_chroma: f64,
}

impl FillSource {
    fn new(seed: &str) -> Result<Self> {
        let color = ColorMetrics::from_hex(seed)?;
        let chroma = oklab_to_oklch(color.lab)[1];
        let chromatic = chroma >= 0.035;
        let preferred_chroma = if chromatic {
            (chroma * 0.35).clamp(0.030, 0.055)
        } else {
            chroma
        };

        Ok(Self {
            color,
            chroma,
            chromatic,
            preferred_chroma,
        })
    }

    fn candidates(&self, seed: &str) -> Result<BTreeSet<String>> {
        let chromas = if self.chromatic {
            vec![0.030, self.preferred_chroma, 0.045, 0.060, self.chroma]
        } else {
            vec![self.chroma]
        };
        let mut candidates = BTreeSet::from([seed.to_owned(), "#000000".into(), "#ffffff".into()]);

        for chroma in chromas {
            let chroma_scale = chroma / self.chroma.max(1e-12);
            for step in 0..=128 {
                candidates.insert(tone(seed, f64::from(step) / 128.0, chroma_scale)?);
            }
        }

        Ok(candidates)
    }
}

impl ControlSurface {
    fn evaluate(
        &self,
        fill: ColorMetrics,
        hover: ColorMetrics,
        band: MetricBand,
        chromatic: bool,
    ) -> (f64, f64, f64) {
        let native_distance = self.host.delta_e(fill);
        // Color can distinguish a tinted state at similar luminance.
        let separation = if chromatic {
            native_distance / 0.030
        } else {
            self.host.contrast(fill).ln() / 1.16_f64.ln()
        };
        let separation_deficit = (1.0 - separation).max(0.0);
        let ceiling = band.maximum().unwrap_or(1.8);
        let ceiling_excess = [fill, hover]
            .into_iter()
            .map(|state| {
                let contrast = self.host.contrast(state);
                (contrast.ln() / ceiling.ln() - 1.0).max(0.0)
            })
            .fold(0.0_f64, f64::max);
        let violation = separation_deficit.max(ceiling_excess);

        let preferred = band.preferred().unwrap_or(1.25).min(1.30);
        let contrast_deviation =
            (self.host.contrast(fill).ln() - preferred.ln()).abs() / preferred.ln();
        let framed_distance = self.host.delta_e(self.frame) + self.frame.delta_e(fill);
        let detour = (framed_distance - native_distance).max(0.0);
        let continuity_penalty = detour / native_distance.max(0.04);

        (violation, contrast_deviation, continuity_penalty)
    }
}

pub(super) struct ControlFills {
    surfaces: Vec<ControlSurface>,
    text: ColorMetrics,
    dark: bool,
}

impl ControlFills {
    pub(super) fn new(
        canvas: &str,
        hosts: &[String],
        text: &str,
        policy: &StructurePolicy,
        dark: bool,
    ) -> Result<Self> {
        // These neutral anchors describe the surfaces before semantic fills
        // exist. The final border pass adds every rendered control state.
        let contacts = hosts
            .iter()
            .map(|host| Contact::new(host, host, host, 1.0))
            .collect::<Result<Vec<_>>>()?;
        let frame = fit(
            canvas,
            &[ContactGroup {
                label: "control host separators",
                purpose: Purpose::Separator,
                band: policy.normal,
                contacts,
            }],
        )?;
        let frame = ColorMetrics::from_hex(&frame)?;
        let surfaces = hosts
            .iter()
            .map(|host| {
                let host = ColorMetrics::from_hex(host)?;
                let frame = ColorMetrics::blend_rgb24(host, frame.rgb24(), 153).metrics();
                Ok(ControlSurface { host, frame })
            })
            .collect::<Result<Vec<_>>>()?;

        Ok(Self {
            surfaces,
            text: ColorMetrics::from_hex(text)?,
            dark,
        })
    }

    pub(super) fn fit(
        &self,
        seed: &str,
        band: MetricBand,
        selected_text: Option<&str>,
    ) -> Result<String> {
        let source = FillSource::new(seed)?;
        let mut foregrounds = vec![self.text];
        if let Some(selected_text) = selected_text {
            foregrounds.push(ColorMetrics::from_hex(selected_text)?);
        }

        let mut best: Option<(String, [f64; 5])> = None;
        for candidate in source.candidates(seed)? {
            let fill = ColorMetrics::from_hex(&candidate)?;
            let hover = ColorMetrics::from_hex(&tinted_hover(&candidate, self.dark)?)?;
            let score = self.rank(fill, hover, &source, &foregrounds, band);
            if best
                .as_ref()
                .is_some_and(|(_, previous)| score >= *previous)
            {
                continue;
            }

            best = Some((candidate, score));
        }

        Ok(best.expect("control fill candidates include the seed").0)
    }

    fn rank(
        &self,
        fill: ColorMetrics,
        hover: ColorMetrics,
        source: &FillSource,
        foregrounds: &[ColorMetrics],
        band: MetricBand,
    ) -> [f64; 5] {
        let readability_deficit = foregrounds
            .iter()
            .flat_map(|foreground| {
                [fill, hover].map(|state| ((4.5 - foreground.contrast(state)) / 4.5).max(0.0))
            })
            .fold(0.0, f64::max);
        let chroma = oklab_to_oklch(fill.lab)[1];
        let identity_deficit = if source.chromatic {
            ((0.030 - chroma) / 0.030).max(0.0)
        } else {
            0.0
        };

        let mut worst_surface_violation = 0.0_f64;
        let mut surface_preference = 0.0;
        for surface in &self.surfaces {
            let (violation, contrast_deviation, continuity_penalty) =
                surface.evaluate(fill, hover, band, source.chromatic);
            worst_surface_violation = worst_surface_violation.max(violation);
            surface_preference += contrast_deviation;
            surface_preference += continuity_penalty;
        }

        let mean_surface_preference = surface_preference / self.surfaces.len().max(1) as f64;
        let chroma_deviation =
            (chroma - source.preferred_chroma).abs() / source.preferred_chroma.max(0.03);
        let preference = mean_surface_preference + chroma_deviation;
        [
            readability_deficit,
            identity_deficit,
            worst_surface_violation,
            preference,
            fill.delta_e(source.color),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(32))]

        #[test]
        fn hover_preserves_alpha_and_subtracts_hsl_lightness(
            rgba in any::<[u8; 4]>(),
            dark in any::<bool>(),
        ) {
            let [r, g, b, a] = rgba;
            let fill = format!("#{r:02x}{g:02x}{b:02x}{a:02x}");
            let hover = parse_hex(&tinted_hover(&fill, dark).unwrap()).unwrap();
            let input_lightness = (f64::from(r.max(g).max(b)) + f64::from(r.min(g).min(b))) / 510.0;
            let output_lightness = (hover.r.max(hover.g).max(hover.b) + hover.r.min(hover.g).min(hover.b)) / 2.0;
            let expected = (input_lightness - if dark { 0.20 } else { 0.05 }).max(0.0);

            prop_assert!((output_lightness - expected).abs() <= 0.5 / 255.0 + 1e-9);
            prop_assert!((hover.a - f64::from(a) / 255.0).abs() <= 1e-9);
        }

        #[test]
        fn hover_keeps_neutral_fills_achromatic(value in any::<u8>(), dark in any::<bool>()) {
            let fill = format!("#{value:02x}{value:02x}{value:02x}");
            let hover = parse_hex(&tinted_hover(&fill, dark).unwrap()).unwrap();
            prop_assert_eq!(hover.r, hover.g);
            prop_assert_eq!(hover.g, hover.b);
        }
    }
}
