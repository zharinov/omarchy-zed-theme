//! Theme-relative presentation policy for non-syntax UI roles.
//!
//! Palettes describe relationships rather than a complete Zed theme. This module
//! measures the relationships in the resolved palette, then constrains each UI
//! role to a bounded range. Readability and state separation remain floors; the
//! preferred value and ceiling preserve whether the source theme is quiet or bold.

use crate::Result;
use crate::color::{contrast_ratio, delta_e, lightness};
use crate::palette::ResolvedPalette;
use crate::search::MetricBand;

#[derive(Clone, Copy, Debug)]
pub(crate) struct VisualBand {
    pub(crate) contrast: MetricBand,
    pub(crate) delta_e: MetricBand,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct SurfacePolicy {
    pub(crate) lower_depth: f64,
    pub(crate) upper_depth: f64,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ContentPolicy {
    pub(crate) muted_saliency: f64,
    pub(crate) placeholder_saliency: f64,
    pub(crate) disabled_saliency: f64,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct InteractionPolicy {
    pub(crate) hover: VisualBand,
    pub(crate) active: VisualBand,
    pub(crate) selected: VisualBand,
    pub(crate) adjacent_contrast: f64,
    pub(crate) adjacent_delta_e: f64,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct StructurePolicy {
    pub(crate) passive: MetricBand,
    pub(crate) normal: MetricBand,
    pub(crate) active_guide: MetricBand,
    pub(crate) focus: MetricBand,
    pub(crate) status_border: MetricBand,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ScrollPolicy {
    pub(crate) idle: VisualBand,
    pub(crate) hover: VisualBand,
    pub(crate) active: VisualBand,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct TerminalPolicy {
    pub(crate) dim_saliency: f64,
    pub(crate) bright_saliency: f64,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct UiPolicy {
    pub(crate) surfaces: SurfacePolicy,
    pub(crate) content: ContentPolicy,
    pub(crate) interactions: InteractionPolicy,
    pub(crate) structure: StructurePolicy,
    pub(crate) scroll: ScrollPolicy,
    pub(crate) terminal: TerminalPolicy,
}

fn color<'a>(palette: &'a ResolvedPalette, key: &str) -> &'a str {
    palette
        .colors
        .get(key)
        .expect("validated palette color must be present")
}

fn projected_contrast(reference: f64, saliency: f64) -> f64 {
    (reference.ln() * saliency).exp()
}

fn source_saliency(source: &str, reference: &str, background: &str) -> Result<f64> {
    let reference_contrast = contrast_ratio(reference, background)?;
    let source_contrast = contrast_ratio(source, background)?;
    Ok(source_contrast.ln() / reference_contrast.ln().max(1e-12))
}

fn visual_band(contrast: (f64, f64, f64), distance: (f64, f64, f64)) -> VisualBand {
    VisualBand {
        contrast: MetricBand::bounded(contrast.0, contrast.1, contrast.2),
        delta_e: MetricBand::bounded(distance.0, distance.1, distance.2),
    }
}

impl UiPolicy {
    pub(crate) fn derive(palette: &ResolvedPalette) -> Result<Self> {
        let background = color(palette, "background");
        let foreground = color(palette, "foreground");
        let foreground_contrast = contrast_ratio(foreground, background)?;
        let selection_contrast = contrast_ratio(color(palette, "selection"), background)?;
        let selection_distance = delta_e(color(palette, "selection"), background)?;

        let background_lightness = lightness(background)?;
        let mut lower_depth = 0.0_f64;
        let mut upper_depth = 0.0_f64;
        let mut source_surface_contrast = 1.0_f64;
        for key in ["darker_background", "dark_background", "lighter_background"] {
            let source = color(palette, key);
            let offset = lightness(source)? - background_lightness;
            lower_depth = lower_depth.max((-offset).max(0.0));
            upper_depth = upper_depth.max(offset.max(0.0));
            source_surface_contrast =
                source_surface_contrast.max(contrast_ratio(source, background)?);
        }
        if lower_depth <= 1e-6 {
            lower_depth = (upper_depth * 0.65).max(0.010);
        }
        if upper_depth <= 1e-6 {
            upper_depth = (lower_depth * 0.65).max(0.008);
        }
        let surfaces = SurfacePolicy {
            lower_depth: lower_depth.clamp(0.010, 0.085),
            upper_depth: upper_depth.clamp(0.008, 0.075),
        };

        let muted_saliency =
            source_saliency(color(palette, "muted"), foreground, background)?.clamp(0.42, 0.78);
        let source_placeholder_saliency =
            source_saliency(color(palette, "dark_foreground"), foreground, background)?;
        let placeholder_saliency = source_placeholder_saliency
            .clamp(0.38, 0.70)
            .min((muted_saliency - 0.04).max(0.38));
        let disabled_saliency = source_placeholder_saliency
            .clamp(0.30, 0.60)
            .min((placeholder_saliency - 0.05).max(0.30));

        let hover_preferred = projected_contrast(selection_contrast, 0.45).clamp(1.16, 1.30);
        let active_preferred = projected_contrast(selection_contrast, 0.70).clamp(1.24, 1.45);
        let selected_preferred = selection_contrast.clamp(1.25, 1.55);
        let hover_distance = (selection_distance * 0.45).clamp(0.040, 0.075);
        let active_distance = (selection_distance * 0.70).clamp(0.060, 0.105);
        let selected_distance = selection_distance.clamp(0.080, 0.140);
        let interactions = InteractionPolicy {
            hover: visual_band(
                (1.12, hover_preferred, (hover_preferred + 0.12).min(1.42)),
                (0.030, hover_distance, 0.350),
            ),
            active: visual_band(
                (1.20, active_preferred, (active_preferred + 0.16).min(1.65)),
                (0.045, active_distance, 0.400),
            ),
            selected: visual_band(
                (
                    1.20,
                    selected_preferred,
                    (selected_preferred + 0.20).min(1.80),
                ),
                (0.055, selected_distance, 0.450),
            ),
            adjacent_contrast: 1.03,
            adjacent_delta_e: 0.015,
        };

        let normal_structure = projected_contrast(source_surface_contrast, 0.58).clamp(1.16, 1.42);
        let passive_structure = projected_contrast(source_surface_contrast, 0.34).clamp(1.08, 1.20);
        let focus = projected_contrast(foreground_contrast, 0.55).clamp(3.02, 4.00);
        let structure = StructurePolicy {
            passive: MetricBand::bounded(1.05, passive_structure, 1.30),
            normal: MetricBand::bounded(1.10, normal_structure, 1.80),
            active_guide: MetricBand::bounded(
                1.30,
                projected_contrast(source_surface_contrast, 0.90).clamp(1.40, 1.90),
                2.25,
            ),
            focus: MetricBand::bounded(3.02, focus, 4.50),
            status_border: MetricBand::bounded(1.30, normal_structure.max(1.55), 2.40),
        };

        let scroll_idle = projected_contrast(foreground_contrast, 0.28).clamp(1.55, 2.25);
        let scroll_hover =
            projected_contrast(foreground_contrast, 0.38).clamp(scroll_idle + 0.25, 3.05);
        let scroll_active =
            projected_contrast(foreground_contrast, 0.48).clamp(scroll_hover + 0.25, 4.00);
        let scroll = ScrollPolicy {
            idle: visual_band(
                (1.45, scroll_idle, (scroll_idle + 0.30).min(2.55)),
                (0.045, 0.070, 0.450),
            ),
            hover: visual_band(
                (
                    scroll_idle + 0.15,
                    scroll_hover,
                    (scroll_hover + 0.35).min(3.40),
                ),
                (0.060, 0.095, 0.500),
            ),
            active: visual_band(
                (
                    scroll_hover + 0.15,
                    scroll_active,
                    (scroll_active + 0.40).min(4.40),
                ),
                (0.080, 0.125, 0.550),
            ),
        };

        let dim_saliency =
            source_saliency(color(palette, "dark_foreground"), foreground, background)?
                .clamp(0.55, 0.82);
        let bright_saliency =
            source_saliency(color(palette, "bright_foreground"), foreground, background)?
                .clamp(1.02, 1.16);

        Ok(Self {
            surfaces,
            content: ContentPolicy {
                muted_saliency,
                placeholder_saliency,
                disabled_saliency,
            },
            interactions,
            structure,
            scroll,
            terminal: TerminalPolicy {
                dim_saliency,
                bright_saliency,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::CANONICAL_COLOR_KEYS;
    use crate::palette::Provenance;
    use std::collections::BTreeMap;

    fn palette(selection: &str, muted: &str) -> ResolvedPalette {
        let value = |key: &str| match key {
            "background" | "color0" => "#101418",
            "dark_background" => "#0d1114",
            "darker_background" | "color8" => "#080b0d",
            "lighter_background" => "#252b31",
            "foreground" | "color7" => "#d8dee9",
            "dark_foreground" => "#7f8995",
            "bright_foreground" | "light_foreground" | "selection_foreground" | "color15" => {
                "#ffffff"
            }
            "selection" | "selection_background" => selection,
            "muted" => muted,
            "accent" | "blue" | "bright_blue" | "color4" | "color12" => "#74a7d8",
            "cursor" | "magenta" | "bright_magenta" | "color5" | "color13" => "#c58bd8",
            "red" | "bright_red" | "color1" | "color9" => "#db7b82",
            "yellow" | "bright_yellow" | "color3" | "color11" => "#d8b56a",
            "orange" | "brown" => "#d99865",
            "green" | "bright_green" | "color2" | "color10" => "#83bd77",
            "cyan" | "bright_cyan" | "color6" | "color14" => "#73b8b0",
            _ => unreachable!("missing test color for {key}"),
        };
        ResolvedPalette {
            mode: "dark".into(),
            colors: CANONICAL_COLOR_KEYS
                .iter()
                .map(|key| ((*key).to_owned(), value(key).to_owned()))
                .collect::<BTreeMap<_, _>>(),
            provenance: CANONICAL_COLOR_KEYS
                .iter()
                .map(|key| ((*key).to_owned(), Provenance::Direct))
                .collect(),
        }
    }

    #[test]
    fn source_interaction_strength_changes_targets_without_weakening_floors() {
        let quiet = UiPolicy::derive(&palette("#1b232a", "#46515b")).unwrap();
        let bold = UiPolicy::derive(&palette("#52697b", "#8995a1")).unwrap();

        assert!(
            quiet.interactions.hover.contrast.preferred()
                < bold.interactions.hover.contrast.preferred()
        );
        assert!(
            quiet.interactions.active.contrast.preferred()
                < bold.interactions.active.contrast.preferred()
        );
        assert_eq!(
            quiet.interactions.hover.contrast.minimum(),
            bold.interactions.hover.contrast.minimum()
        );
        assert!(
            bold.interactions.active.contrast.preferred()
                <= bold.interactions.active.contrast.maximum()
        );
    }

    #[test]
    fn source_content_hierarchy_is_clamped_to_a_usable_order() {
        let ordinary = UiPolicy::derive(&palette("#28333d", "#52606c")).unwrap();
        let inverted = UiPolicy::derive(&palette("#28333d", "#ffffff")).unwrap();

        for policy in [ordinary, inverted] {
            assert!(policy.content.muted_saliency > policy.content.placeholder_saliency);
            assert!(policy.content.placeholder_saliency > policy.content.disabled_saliency);
            assert!(policy.content.muted_saliency <= 0.78);
        }
    }
}
