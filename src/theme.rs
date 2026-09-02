//! Builds a complete Zed theme from every valid Omarchy palette.
//!
//! Omarchy supplies every UI color. Built-in colors may repair syntax roles only.
//! Visual targets rank candidates; they never prevent generation.

use self::tokens::{
    ContentTokens, DerivedTokens, InteractionTokens, OpaqueColor, OverlayColor, RoleColor,
    StatusChannel, StatusTokens, SurfaceTokens, ThemeTokens,
};
use crate::color::{
    apply_opacity, contrast_ratio, delta_e, gamut_map_oklch_unchecked, geometric_contrast,
    gpui_blend, lab, lightness, oklab_to_oklch, parse_hex, render_layers, tone, with_alpha,
};
use crate::constants::*;
use crate::palette::ResolvedPalette;
use crate::saliency::{
    HOVER_LINE_NUMBER_SALIENCY, INACTIVE_LINE_NUMBER_SALIENCY, PRIMARY_SALIENCY, SaliencyRequest,
    fit_relative,
};
use crate::search::{
    FitBounds, MetricBand, OverlayFitRequest, OverlayPairRequest, PairConstraints, Search,
    StateFitRequest, cvd_distance, cvd_greedy_order,
};
use crate::syntax::{SyntaxContexts, build_syntax};
use crate::{Error, Result};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet, btree_map::Entry};

mod tokens;
mod ui_policy;

use self::ui_policy::{TerminalPolicy, UiPolicy, VisualBand};

fn color<'a>(palette: &'a ResolvedPalette, key: &str) -> &'a str {
    palette
        .colors
        .get(key)
        .expect("validated palette color must be present")
}

fn is_dark_mode(mode: &str) -> bool {
    match mode {
        "dark" => true,
        "light" => false,
        _ => panic!("validated palette mode must be dark or light, got {mode:?}"),
    }
}

fn unique(values: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    values
        .into_iter()
        .filter(|value| seen.insert(value.clone()))
        .collect()
}

fn render_on_bases(bases: &[String], overlays: &[&str]) -> Result<Vec<String>> {
    bases
        .iter()
        .map(|base| render_layers(base, overlays))
        .collect()
}

fn render_with_bounded_generic_highlights(
    bases: &[String],
    highlights: &[&str],
) -> Result<Vec<String>> {
    // Zed permits several unordered generic highlights. Depth one is the largest
    // bounded stage for which every built-in palette can preserve all text contracts.
    let mut scenes = bases.to_vec();
    for first in highlights {
        scenes.extend(render_on_bases(bases, &[*first])?);
    }
    Ok(scenes)
}

fn opacity_byte(opacity: f64) -> u8 {
    (opacity * 255.0).round() as u8
}

fn bounded_overlay_request<'a>(
    backgrounds: &'a [String],
    band: VisualBand,
) -> OverlayFitRequest<'a> {
    OverlayFitRequest::new(backgrounds, band.contrast, band.delta_e)
}

fn fit_bounded_color(
    search: &mut Search,
    seed: &str,
    backgrounds: &[String],
    band: MetricBand,
) -> Result<String> {
    search.fit_color_bounded(seed, backgrounds, &[], FitBounds::new(band))
}

fn retained_tint_chroma(seed: &str) -> Result<f64> {
    let chroma = oklab_to_oklch(lab(seed)?)[1];
    // Requiring tiny incidental chroma jumps neutral sources to the first
    // chromatic lattice step, so only authored-looking tints retain a floor.
    Ok(if chroma >= 0.035 {
        chroma.min(0.040)
    } else {
        0.0
    })
}

fn quality_shortfall(actual: f64, target: f64) -> f64 {
    ((target - actual) / target.max(1e-12)).max(0.0)
}

const PREFERRED_HIGHLIGHT_MAX_ALPHA: u8 = 166;

fn fit_highlight_strict(
    search: &mut Search,
    role: &str,
    seed: &str,
    request: OverlayFitRequest<'_>,
) -> Result<Option<String>> {
    search
        .try_fit_readable_overlay_preferred(
            seed,
            request,
            PREFERRED_HIGHLIGHT_MAX_ALPHA,
            OVERLAY_MAX_ALPHA,
        )
        .map_err(|error| error.context(role))
}

fn fit_highlight(
    search: &mut Search,
    role: &str,
    seed: &str,
    request: OverlayFitRequest<'_>,
) -> Result<String> {
    search
        .fit_readable_overlay_preferred(
            seed,
            request,
            PREFERRED_HIGHLIGHT_MAX_ALPHA,
            OVERLAY_MAX_ALPHA,
        )
        .map_err(|error| error.context(role))
}

#[derive(Default)]
struct StyleBuilder(BTreeMap<String, String>);

impl StyleBuilder {
    fn insert(&mut self, role: RoleColor) {
        let (name, value) = role.into_parts();
        match self.0.entry(name) {
            Entry::Vacant(entry) => {
                entry.insert(value);
            }
            Entry::Occupied(entry) => {
                panic!("theme role {} was generated twice", entry.key())
            }
        }
    }

    fn insert_opaque(&mut self, role: impl Into<String>, color: String) -> Result<()> {
        self.insert(RoleColor::opaque_value(role, color)?);
        Ok(())
    }

    fn insert_overlay(&mut self, role: impl Into<String>, color: String) -> Result<()> {
        self.insert(RoleColor::overlay_value(role, color)?);
        Ok(())
    }

    fn extend(&mut self, roles: impl IntoIterator<Item = RoleColor>) {
        for role in roles {
            self.insert(role);
        }
    }

    fn extend_opaque(&mut self, roles: impl IntoIterator<Item = (String, String)>) -> Result<()> {
        for (role, color) in roles {
            self.insert_opaque(role, color)?;
        }
        Ok(())
    }

    fn append_to(self, style: &mut Map<String, Value>) {
        for (role, color) in self.0 {
            if style.contains_key(&role) {
                panic!("theme role {role} was generated twice");
            }
            style.insert(role, color.into());
        }
    }
}

fn derive_surfaces(
    palette: &ResolvedPalette,
    policy: &UiPolicy,
) -> Result<BTreeMap<String, String>> {
    let canvas = color(palette, "background");
    let canvas_lightness = lightness(canvas)?;
    let offsets = if is_dark_mode(&palette.mode) {
        [
            ("chrome", -policy.surfaces.lower_depth * 0.55),
            ("surface", policy.surfaces.upper_depth * 0.35),
            ("elevated", policy.surfaces.upper_depth),
        ]
    } else {
        [
            ("chrome", -policy.surfaces.lower_depth * 0.65),
            ("surface", -policy.surfaces.lower_depth * 0.35),
            ("elevated", policy.surfaces.upper_depth),
        ]
    };
    let authored = [
        "darker_background",
        "dark_background",
        "lighter_background",
        "background",
    ]
    .into_iter()
    .map(|key| {
        let value = color(palette, key);
        Ok((key, value, lightness(value)?))
    })
    .collect::<Result<Vec<_>>>()?;

    let mut surfaces = BTreeMap::from([("canvas".into(), canvas.to_owned())]);
    let mut used = BTreeSet::from([canvas.to_owned()]);
    let mut previous = f64::NEG_INFINITY;

    for (role, offset) in offsets {
        let target = (canvas_lightness + offset).clamp(0.0, 1.0);
        let lower_side = target < canvas_lightness;
        let mut eligible = Vec::new();

        for (key, value, value_lightness) in &authored {
            let on_side = if lower_side {
                *value_lightness < canvas_lightness
            } else {
                *value_lightness > canvas_lightness
            };

            if used.contains(*value) {
                continue;
            }

            if (*value_lightness - target).abs() > 0.015 + 1e-12 {
                continue;
            }

            if !on_side || *value_lightness <= previous + 1e-6 {
                continue;
            }

            eligible.push((
                (*value_lightness - target).abs(),
                *key,
                *value,
                *value_lightness,
            ));
        }

        eligible.sort_by(|left, right| left.0.total_cmp(&right.0).then(left.1.cmp(right.1)));

        let output = if let Some((_, _, value, _)) = eligible.first() {
            (*value).to_owned()
        } else {
            let (_, source, _) = authored
                .iter()
                .min_by(|left, right| {
                    (left.2 - target)
                        .abs()
                        .total_cmp(&(right.2 - target).abs())
                        .then(left.0.cmp(right.0))
                })
                .expect("validated palettes always provide authored surface colors");
            let mut output = tone(source, target, 1.0)?;
            if lightness(&output)? <= previous + 1e-6 {
                output = tone(source, (previous + 0.004).min(1.0), 1.0)?;
            }
            output
        };

        used.insert(output.clone());
        previous = lightness(&output)?;

        surfaces.insert(role.into(), output);
    }

    Ok(surfaces)
}

fn minimum_contrast(foreground: &str, backgrounds: &[String]) -> Result<f64> {
    backgrounds
        .iter()
        .try_fold(f64::INFINITY, |minimum, background| {
            Ok(minimum.min(contrast_ratio(foreground, background)?))
        })
}

fn contrasts_satisfy(
    foreground: &str,
    backgrounds: &[String],
    references: &[f64],
    predicate: impl Fn(f64, f64) -> bool,
) -> Result<bool> {
    assert_eq!(backgrounds.len(), references.len());
    backgrounds
        .iter()
        .zip(references)
        .try_fold(true, |passes, (background, reference)| {
            Ok(passes && predicate(contrast_ratio(foreground, background)?, *reference))
        })
}

fn minimum_pairwise(
    first: &[String],
    second: &[String],
    metric: impl Fn(&str, &str) -> Result<f64>,
) -> Result<f64> {
    assert_eq!(
        first.len(),
        second.len(),
        "paired generated color contexts must have equal lengths"
    );
    first
        .iter()
        .zip(second)
        .try_fold(f64::INFINITY, |minimum, (left, right)| {
            Ok(minimum.min(metric(left, right)?))
        })
}

fn fit_player_cursors(
    search: &mut Search,
    seeds: &[String],
    backgrounds: &[String],
    mode: &str,
) -> Result<Vec<String>> {
    search
        .fit_distinct_colors_with_separation(
            seeds,
            backgrounds,
            CONTROL_CONTRAST,
            PLAYER_CURSOR_NORMAL_DELTA_E,
            PLAYER_CURSOR_CVD_DELTA_E,
        )
        .map_err(|error| error.context(format!("players.cursor ({mode})")))
}

struct TerminalRequest<'a> {
    seeds: [&'a str; 3],
    backgrounds: &'a [String],
    mode: &'a str,
    policy: TerminalPolicy,
}

fn terminal_triplet(search: &mut Search, request: TerminalRequest<'_>) -> Result<[String; 3]> {
    let [dim_seed, normal_seed, bright_seed] = request.seeds;
    let backgrounds = request.backgrounds;
    let authored_normal = minimum_contrast(normal_seed, backgrounds)?.max(TEXT_CONTRAST);
    let normal_maximum = (authored_normal * 1.10).min(21.0);
    let normal = search.fit_color_bounded(
        normal_seed,
        backgrounds,
        &[],
        FitBounds::new(MetricBand::bounded(
            TEXT_CONTRAST,
            authored_normal.min(21.0),
            normal_maximum,
        )),
    )?;
    let normal_l = lightness(&normal)?;
    let normal_contrasts = backgrounds
        .iter()
        .map(|background| contrast_ratio(&normal, background))
        .collect::<Result<Vec<_>>>()?;
    let normal_contrast = normal_contrasts
        .iter()
        .copied()
        .fold(f64::INFINITY, f64::min);
    let (dim_lower, dim_upper, bright_lower, bright_upper) = if is_dark_mode(request.mode) {
        (0.0, normal_l, normal_l, 1.0)
    } else {
        (normal_l, 1.0, 0.0, normal_l)
    };
    let dim_preferred = (normal_contrast.ln() * request.policy.dim_saliency)
        .exp()
        .max(HARD_TEXT_CONTRAST);
    let dim_maximum = (normal_contrast.ln() * (request.policy.dim_saliency + 0.08).min(0.90))
        .exp()
        .max(dim_preferred);
    let dim_bounds = FitBounds {
        lower_lightness: dim_lower,
        upper_lightness: dim_upper,
        ..FitBounds::new(MetricBand::bounded(
            HARD_TEXT_CONTRAST,
            dim_preferred,
            dim_maximum,
        ))
    };
    let fit_dim = |search: &mut Search, seed: &str| {
        search.fit_color_bounded_with_contrast_ceilings(
            seed,
            backgrounds,
            backgrounds,
            &normal_contrasts,
            &[],
            dim_bounds,
        )
    };
    let mut dim = fit_dim(search, dim_seed)?;
    let dim_respects_normal = |color: &str| {
        contrasts_satisfy(color, backgrounds, &normal_contrasts, |actual, normal| {
            actual <= normal + 1e-12
        })
    };
    if !dim_respects_normal(&dim)? {
        dim = fit_dim(search, &normal)?;
        if !dim_respects_normal(&dim)? {
            dim = normal.clone();
        }
    }
    let bright_preferred = (normal_contrast.ln() * request.policy.bright_saliency)
        .exp()
        .max(normal_contrast)
        .min(21.0);
    let bright_maximum = (normal_contrast.ln() * (request.policy.bright_saliency + 0.08).min(1.24))
        .exp()
        .max(bright_preferred)
        .min(21.0);
    let bright_bounds = FitBounds {
        lower_lightness: bright_lower,
        upper_lightness: bright_upper,
        ..FitBounds::new(MetricBand::bounded(
            normal_contrast,
            bright_preferred,
            bright_maximum,
        ))
    };
    let fit_bright = |search: &mut Search, seed: &str| {
        search.fit_color_bounded_with_contrast_floors(
            seed,
            backgrounds,
            backgrounds,
            &normal_contrasts,
            &[],
            bright_bounds,
        )
    };
    let mut bright = fit_bright(search, bright_seed)?;
    let bright_respects_normal = |color: &str| {
        contrasts_satisfy(color, backgrounds, &normal_contrasts, |actual, normal| {
            actual >= normal - 1e-12
        })
    };
    if !bright_respects_normal(&bright)? {
        bright = fit_bright(search, &normal)?;
        if !bright_respects_normal(&bright)? {
            bright = normal.clone();
        }
    }
    Ok([dim, normal, bright])
}

struct ContentColors {
    primary: String,
    secondary: String,
    placeholder: String,
    disabled: String,
    icon_muted: String,
    icon_placeholder: String,
    icon_disabled: String,
}

struct SemanticColors {
    primary: String,
    secondary: String,
    placeholder: String,
    disabled: String,
    icon_muted: String,
    icon_placeholder: String,
    icon_disabled: String,
    accent: String,
    structural: String,
    passive: String,
    red: String,
    green: String,
    blue: String,
    yellow: String,
    orange: String,
    cyan: String,
    magenta: String,
}

struct ChangeIdentity {
    added: String,
    deleted: String,
}

impl ChangeIdentity {
    const ADDED_HUE_DEGREES: f64 = 145.0;
    const DELETED_HUE_DEGREES: f64 = 25.0;

    fn from_palette(palette: &ResolvedPalette) -> Result<Self> {
        Ok(Self {
            added: conventional_semantic_seed(palette, "green", Self::ADDED_HUE_DEGREES)?,
            deleted: conventional_semantic_seed(palette, "red", Self::DELETED_HUE_DEGREES)?,
        })
    }

    fn editor_overlay_seeds(&self, mode: &str) -> Result<[String; 2]> {
        Ok([
            diff_overlay_seed(&self.added, mode, Self::ADDED_HUE_DEGREES)?,
            diff_overlay_seed(&self.deleted, mode, Self::DELETED_HUE_DEGREES)?,
        ])
    }
}

#[derive(Clone, Copy)]
struct DiffPresentationProfile {
    line_target_contrast: f64,
    line_minimum_delta_e: f64,
    line_opacity: f64,
    hollow_opacity: f64,
    border_opacity: f64,
    word_opacity: f64,
}

impl DiffPresentationProfile {
    fn for_mode(mode: &str) -> Self {
        if !is_dark_mode(mode) {
            Self {
                line_target_contrast: LIGHT_DIFF_LINE_TARGET_CONTRAST,
                line_minimum_delta_e: LIGHT_DIFF_LINE_MINIMUM_DELTA_E,
                line_opacity: LIGHT_DIFF_LINE_OPACITY,
                hollow_opacity: LIGHT_DIFF_HOLLOW_OPACITY,
                border_opacity: LIGHT_DIFF_BORDER_OPACITY,
                word_opacity: LIGHT_DIFF_WORD_OPACITY,
            }
        } else {
            Self {
                line_target_contrast: DARK_DIFF_LINE_TARGET_CONTRAST,
                line_minimum_delta_e: DARK_DIFF_LINE_MINIMUM_DELTA_E,
                line_opacity: DARK_DIFF_LINE_OPACITY,
                hollow_opacity: DARK_DIFF_HOLLOW_OPACITY,
                border_opacity: DARK_DIFF_BORDER_OPACITY,
                word_opacity: DARK_DIFF_WORD_OPACITY,
            }
        }
    }
}

struct DiffLayers {
    added_line: String,
    deleted_line: String,
    added_hollow: String,
    deleted_hollow: String,
    added_border: String,
    deleted_border: String,
}

impl DiffLayers {
    fn from_pigments(added: &str, deleted: &str, profile: DiffPresentationProfile) -> Result<Self> {
        Ok(Self {
            added_line: apply_opacity(added, profile.line_opacity)?,
            deleted_line: apply_opacity(deleted, profile.line_opacity)?,
            added_hollow: apply_opacity(added, profile.hollow_opacity)?,
            deleted_hollow: apply_opacity(deleted, profile.hollow_opacity)?,
            added_border: apply_opacity(added, profile.border_opacity)?,
            deleted_border: apply_opacity(deleted, profile.border_opacity)?,
        })
    }

    fn rendered_semantic_penalty(
        &self,
        editor_bases: &[String],
        editor_foreground: &str,
        profile: DiffPresentationProfile,
    ) -> Result<f64> {
        let added_line = render_on_bases(editor_bases, &[&self.added_line])?;
        let deleted_line = render_on_bases(editor_bases, &[&self.deleted_line])?;
        let added_hollow = render_on_bases(editor_bases, &[&self.added_hollow])?;
        let deleted_hollow = render_on_bases(editor_bases, &[&self.deleted_hollow])?;
        let added_border = render_on_bases(&added_hollow, &[&self.added_border])?;
        let deleted_border = render_on_bases(&deleted_hollow, &[&self.deleted_border])?;
        let line_visibility = minimum_pairwise(editor_bases, &added_line, contrast_ratio)?.min(
            minimum_pairwise(editor_bases, &deleted_line, contrast_ratio)?,
        );
        let text_backgrounds = unique(
            added_line
                .iter()
                .chain(&deleted_line)
                .chain(&added_hollow)
                .chain(&deleted_hollow)
                .cloned(),
        );
        let mut penalty = quality_shortfall(line_visibility, profile.line_target_contrast)
            + quality_shortfall(
                minimum_contrast(editor_foreground, &text_backgrounds)?,
                HARD_TEXT_CONTRAST,
            );
        for (first, second, normal_floor, cvd_floor) in [
            (
                &added_line,
                &deleted_line,
                DIFF_NORMAL_FLOOR_DELTA_E,
                DIFF_CVD_FLOOR_DELTA_E,
            ),
            (&added_hollow, &deleted_hollow, 0.003, 0.002),
            (
                &added_border,
                &deleted_border,
                DIFF_NORMAL_FLOOR_DELTA_E,
                0.008,
            ),
        ] {
            penalty += quality_shortfall(minimum_pairwise(first, second, delta_e)?, normal_floor)
                + quality_shortfall(minimum_pairwise(first, second, cvd_distance)?, cvd_floor);
        }
        Ok(penalty)
    }
}

fn conventional_semantic_seed(
    palette: &ResolvedPalette,
    key: &str,
    target_hue_degrees: f64,
) -> Result<String> {
    let [source_lightness, source_chroma, source_hue] = oklab_to_oklch(lab(color(palette, key))?);
    // Exact gamut endpoints cannot carry chroma. Re-enter the usable gamut before
    // imposing the conventional hue; downstream fitting still chooses final tone.
    let lightness = source_lightness.clamp(0.35, 0.80);
    let target_hue = target_hue_degrees.to_radians();
    let difference = (source_hue - target_hue).abs();
    let hue_distance = difference.min(std::f64::consts::TAU - difference);
    let hue = if source_chroma >= 0.035 && hue_distance <= 40.0_f64.to_radians() {
        source_hue
    } else {
        target_hue
    };
    Ok(gamut_map_oklch_unchecked(lightness, source_chroma.clamp(0.080, 0.180), hue).opaque_hex())
}

fn diff_overlay_seed(identity: &str, mode: &str, target_hue_degrees: f64) -> Result<String> {
    let [lightness, chroma, _] = oklab_to_oklch(lab(identity)?);
    // A low-opacity paint projection needs conventional hue and enough chroma
    // to retain its semantic edge after composition, especially in muted themes.
    let (minimum_lightness, maximum_lightness) = if !is_dark_mode(mode) {
        (0.52, 0.65)
    } else {
        (0.48, 0.65)
    };
    let lightness = lightness.clamp(minimum_lightness, maximum_lightness);
    Ok(gamut_map_oklch_unchecked(
        lightness,
        chroma.max(DIFF_OVERLAY_PIGMENT_CHROMA_FLOOR),
        target_hue_degrees.to_radians(),
    )
    .opaque_hex())
}

fn derive_content(
    search: &mut Search,
    palette: &ResolvedPalette,
    policy: &UiPolicy,
    backgrounds: &[String],
    primary: String,
) -> Result<ContentColors> {
    let secondary = fit_relative(
        search,
        color(palette, "muted"),
        &primary,
        SaliencyRequest::new(
            backgrounds,
            HARD_TEXT_CONTRAST,
            policy.content.muted_saliency,
        )
        .with_maximum_saliency((policy.content.muted_saliency + 0.08).min(0.86))
        .with_contrast_ceiling(&primary),
    )?
    .output;
    let placeholder = fit_relative(
        search,
        color(palette, "dark_foreground"),
        &primary,
        SaliencyRequest::new(
            backgrounds,
            HARD_TEXT_CONTRAST,
            policy.content.placeholder_saliency,
        )
        .with_maximum_saliency(policy.content.muted_saliency)
        .with_contrast_ceiling(&secondary),
    )?
    .output;
    let disabled = fit_relative(
        search,
        color(palette, "dark_foreground"),
        &primary,
        SaliencyRequest::new(
            backgrounds,
            CONTROL_CONTRAST,
            policy.content.disabled_saliency,
        )
        .with_maximum_saliency(policy.content.placeholder_saliency)
        .with_contrast_ceiling(&placeholder),
    )?
    .output;
    let icon_muted_saliency = (policy.content.muted_saliency * 0.84).clamp(0.36, 0.64);
    let icon_muted = fit_relative(
        search,
        color(palette, "muted"),
        &primary,
        SaliencyRequest::new(backgrounds, CONTROL_CONTRAST, icon_muted_saliency)
            .with_maximum_saliency(0.70)
            .with_contrast_ceiling(&primary),
    )?
    .output;
    let icon_placeholder = fit_relative(
        search,
        color(palette, "dark_foreground"),
        &primary,
        SaliencyRequest::new(backgrounds, 2.25, icon_muted_saliency * 0.82)
            .with_maximum_saliency(icon_muted_saliency)
            .with_contrast_ceiling(&icon_muted),
    )?
    .output;
    let icon_disabled = fit_relative(
        search,
        color(palette, "dark_foreground"),
        &primary,
        SaliencyRequest::new(
            backgrounds,
            PASSIVE_CONTRAST,
            icon_muted_saliency * 0.82 * 0.78,
        )
        .with_maximum_saliency(icon_muted_saliency * 0.82)
        .with_contrast_ceiling(&icon_placeholder),
    )?
    .output;

    Ok(ContentColors {
        primary,
        secondary,
        placeholder,
        disabled,
        icon_muted,
        icon_placeholder,
        icon_disabled,
    })
}

fn derive_semantics(
    search: &mut Search,
    palette: &ResolvedPalette,
    policy: &UiPolicy,
    content: ContentColors,
    ui_backgrounds: &[String],
    structure_backgrounds: &[String],
    semantic_backgrounds: &[String],
) -> Result<SemanticColors> {
    let ContentColors {
        primary,
        secondary,
        placeholder,
        disabled,
        icon_muted,
        icon_placeholder,
        icon_disabled,
    } = content;
    let accent = search.fit_color(color(palette, "accent"), ui_backgrounds, CONTROL_CONTRAST)?;
    let passive = fit_bounded_color(
        search,
        color(palette, "muted"),
        structure_backgrounds,
        policy.structure.passive,
    )?;
    let normal_maximum = policy
        .structure
        .normal
        .maximum()
        .expect("UI structure bands are bounded");
    let normal_preferred = policy
        .structure
        .normal
        .preferred()
        .expect("UI structure bands are bounded")
        .max(geometric_contrast(&passive, structure_backgrounds)? + 0.08)
        .min(normal_maximum);
    // Distinct preferred ratios can still quantize to the same byte color, so
    // make the structural hierarchy a per-surface constraint as well.
    let normal_contrast_floors = structure_backgrounds
        .iter()
        .map(|background| {
            contrast_ratio(&passive, background).map(|contrast| {
                (contrast + policy.structure.minimum_hierarchy_step)
                    .max(policy.structure.normal.minimum())
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let structural = search.fit_color_bounded_with_contrast_floors(
        color(palette, "muted"),
        structure_backgrounds,
        structure_backgrounds,
        &normal_contrast_floors,
        &[],
        FitBounds::new(MetricBand::bounded(
            policy.structure.normal.minimum(),
            normal_preferred,
            normal_maximum,
        )),
    )?;
    let [green, red] = search
        .fit_pair(
            color(palette, "green"),
            color(palette, "red"),
            semantic_backgrounds,
            PairConstraints::from_contract(TEXT_CONTRAST, SEMANTIC_PAIR_CONTRACT),
        )
        .map_err(|error| error.context("semantic add/delete foregrounds"))?;
    let blue = search.fit_color(color(palette, "blue"), semantic_backgrounds, TEXT_CONTRAST)?;
    let yellow = search.fit_color_avoiding(
        color(palette, "yellow"),
        semantic_backgrounds,
        TEXT_CONTRAST,
        std::slice::from_ref(&blue),
    )?;
    Ok(SemanticColors {
        primary,
        secondary,
        placeholder,
        disabled,
        icon_muted,
        icon_placeholder,
        icon_disabled,
        accent,
        structural,
        passive,
        red,
        green,
        blue,
        yellow,
        orange: search.fit_color(
            color(palette, "orange"),
            semantic_backgrounds,
            TEXT_CONTRAST,
        )?,
        cyan: search.fit_color(color(palette, "cyan"), semantic_backgrounds, TEXT_CONTRAST)?,
        magenta: search.fit_color(
            color(palette, "magenta"),
            semantic_backgrounds,
            TEXT_CONTRAST,
        )?,
    })
}

pub fn build_theme(palette: &ResolvedPalette) -> Result<Value> {
    palette.validate()?;
    build_theme_from_validated_palette(palette)
}

fn build_theme_from_validated_palette(palette: &ResolvedPalette) -> Result<Value> {
    let mut search = Search::default();
    search.prewarm(CANONICAL_COLOR_KEYS.iter().map(|key| color(palette, key)))?;
    let ui_policy = UiPolicy::derive(palette)?;
    let surfaces = derive_surfaces(palette, &ui_policy)?;
    let canvas = surfaces["canvas"].clone();
    let surface = surfaces["surface"].clone();
    let elevated = surfaces["elevated"].clone();
    let chrome = surfaces["chrome"].clone();
    let provisional_editor_text = search.fit_color(
        color(palette, "foreground"),
        &[canvas.clone(), chrome.clone()],
        EDITOR_CANVAS_TEXT_CONTRAST,
    )?;

    let base_ui_backgrounds = unique([
        canvas.clone(),
        surface.clone(),
        elevated.clone(),
        chrome.clone(),
    ]);
    let provisional_primary = search.fit_color(
        color(palette, "foreground"),
        &base_ui_backgrounds,
        TEXT_CONTRAST * 1.25,
    )?;
    let readable_ui_state = [(
        provisional_primary.clone(),
        TEXT_CONTRAST,
        STATE_CONSECUTIVE_DELTA_E,
    )];

    let chrome_separates_active_tab = contrast_ratio(&chrome, &canvas)? >= 1.08 - 1e-12
        && delta_e(&chrome, &canvas)? >= 0.025 - 1e-12;
    let tab_inactive = if chrome_separates_active_tab {
        chrome.clone()
    } else {
        let tab_backgrounds = [canvas.clone()];
        search
            .fit_state_request(
                &chrome,
                StateFitRequest::new(
                    &tab_backgrounds,
                    MetricBand::bounded(
                        1.08,
                        ui_policy
                            .structure
                            .passive
                            .preferred()
                            .expect("UI structure bands are bounded")
                            .max(1.10),
                        ui_policy
                            .structure
                            .normal
                            .maximum()
                            .expect("UI structure bands are bounded"),
                    ),
                    MetricBand::bounded(0.025, 0.040, 0.120),
                )
                .with_references(&readable_ui_state),
            )
            .map_err(|error| error.context("inactive tab"))?
    };

    let panel_overlay_backgrounds = [surface.clone()];
    let panel_overlay = search
        .fit_state_request(
            &elevated,
            StateFitRequest::new(
                &panel_overlay_backgrounds,
                ui_policy.structure.passive,
                MetricBand::bounded(0.020, 0.035, 0.100),
            )
            .with_references(&readable_ui_state),
        )
        .map_err(|error| error.context("panel overlay"))?;
    let panel_hover_references = std::iter::once((
        panel_overlay.clone(),
        ui_policy.interactions.adjacent_contrast,
        ui_policy.interactions.adjacent_delta_e,
    ))
    .chain(readable_ui_state.iter().cloned())
    .collect::<Vec<_>>();
    let panel_overlay_hover = search
        .fit_state_request(
            &panel_overlay,
            StateFitRequest::new(
                &panel_overlay_backgrounds,
                ui_policy.interactions.hover.contrast,
                ui_policy.interactions.hover.delta_e,
            )
            .with_references(&panel_hover_references),
        )
        .map_err(|error| error.context("panel overlay hover"))?;

    let interaction_bases = unique(
        base_ui_backgrounds
            .iter()
            .cloned()
            .chain([panel_overlay.clone(), canvas.clone()]),
    );
    let readable_interaction_foreground = [(provisional_primary.clone(), TEXT_CONTRAST)];
    let element_hover = search
        .fit_readable_overlay(
            &surface,
            bounded_overlay_request(&interaction_bases, ui_policy.interactions.hover)
                .with_runtime_state((0.6, 1.10, 0.025))
                .with_readable_foregrounds(&readable_interaction_foreground),
        )
        .map_err(|error| error.context("element hover"))?;
    let element_active_references = [(
        element_hover.clone(),
        ui_policy.interactions.adjacent_contrast,
        ui_policy.interactions.adjacent_delta_e,
    )];
    let element_runtime_hover = apply_opacity(&element_hover, 0.6)?;
    let element_runtime_references = [(
        element_runtime_hover,
        ui_policy.interactions.adjacent_contrast,
        ui_policy.interactions.adjacent_delta_e,
        0.01,
    )];
    let element_active = search
        .fit_readable_overlay(
            &surface,
            bounded_overlay_request(&interaction_bases, ui_policy.interactions.active)
                .with_runtime_state((0.5, 1.14, 0.035))
                .with_readable_foregrounds(&readable_interaction_foreground)
                .with_rendered_references(&element_active_references)
                .with_runtime_rendered_references(&element_runtime_references),
        )
        .map_err(|error| error.context("element active"))?;
    let ghost_hover = search
        .fit_readable_overlay(
            &canvas,
            bounded_overlay_request(&interaction_bases, ui_policy.interactions.hover)
                .with_readable_foregrounds(&readable_interaction_foreground),
        )
        .map_err(|error| error.context("ghost hover"))?;
    let ghost_active_references = [(
        ghost_hover.clone(),
        ui_policy.interactions.adjacent_contrast,
        ui_policy.interactions.adjacent_delta_e,
    )];
    let ghost_active = search
        .fit_readable_overlay(
            &canvas,
            bounded_overlay_request(&interaction_bases, ui_policy.interactions.active)
                .with_readable_foregrounds(&readable_interaction_foreground)
                .with_rendered_references(&ghost_active_references),
        )
        .map_err(|error| error.context("ghost active"))?;
    let selected_references = [
        (
            element_active.clone(),
            ui_policy.interactions.adjacent_contrast,
            ui_policy.interactions.adjacent_delta_e,
        ),
        (
            ghost_active.clone(),
            ui_policy.interactions.adjacent_contrast,
            ui_policy.interactions.adjacent_delta_e,
        ),
    ];
    let element_selected = search
        .fit_readable_overlay(
            color(palette, "selection"),
            bounded_overlay_request(&interaction_bases, ui_policy.interactions.selected)
                .with_runtime_state((0.5, 1.08, 0.020))
                .with_readable_foregrounds(&readable_interaction_foreground)
                .with_rendered_references(&selected_references),
        )
        .map_err(|error| error.context("selected controls"))?;
    let ghost_selected = element_selected.clone();

    let panel_guide_backgrounds = [surface.clone()];
    let panel_guide_passive = search
        .fit_state_request(
            color(palette, "muted"),
            StateFitRequest::new(
                &panel_guide_backgrounds,
                ui_policy.structure.passive,
                MetricBand::bounded(0.020, 0.035, 0.090),
            ),
        )
        .map_err(|error| error.context("passive panel guide"))?;
    let panel_guide_hover_references = [(
        panel_guide_passive.clone(),
        ui_policy.interactions.adjacent_contrast,
        ui_policy.interactions.adjacent_delta_e,
    )];
    let panel_guide_hover = search
        .fit_state_request(
            color(palette, "accent"),
            StateFitRequest::new(
                &panel_guide_backgrounds,
                ui_policy.structure.normal,
                MetricBand::bounded(0.035, 0.055, 0.130),
            )
            .with_references(&panel_guide_hover_references),
        )
        .map_err(|error| error.context("hovered panel guide"))?;
    let panel_guide_active_references = [(
        panel_guide_hover.clone(),
        ui_policy.interactions.adjacent_contrast,
        ui_policy.interactions.adjacent_delta_e,
    )];
    let panel_guide_active = search
        .fit_state_request(
            color(palette, "accent"),
            StateFitRequest::new(
                &panel_guide_backgrounds,
                ui_policy.structure.active_guide,
                MetricBand::bounded(0.050, 0.080, 0.180),
            )
            .with_references(&panel_guide_active_references),
        )
        .map_err(|error| error.context("active panel guide"))?;
    let panel_guide_ladder = [panel_guide_passive, panel_guide_hover, panel_guide_active];

    let rendered_ui_bases = unique(
        interaction_bases
            .iter()
            .cloned()
            .chain(std::iter::once(tab_inactive.clone())),
    );
    let mut rendered_ui_state_backgrounds = Vec::new();
    for base in &rendered_ui_bases {
        for layer in [
            &element_hover,
            &element_active,
            &element_selected,
            &ghost_hover,
            &ghost_active,
            &ghost_selected,
        ] {
            rendered_ui_state_backgrounds.push(gpui_blend(base, layer)?.opaque_hex());
        }
        rendered_ui_state_backgrounds
            .push(gpui_blend(base, &apply_opacity(&element_hover, 0.6)?)?.opaque_hex());
        rendered_ui_state_backgrounds
            .push(gpui_blend(base, &apply_opacity(&element_active, 0.5)?)?.opaque_hex());
    }
    let ui_backgrounds = unique(
        rendered_ui_bases
            .iter()
            .cloned()
            .chain([panel_overlay_hover.clone()])
            .chain(rendered_ui_state_backgrounds),
    );

    // Interaction paints depend on the provisional text color. Validate the
    // composed scenes once they exist, then repair only palettes that need it.
    let primary =
        if minimum_contrast(&provisional_primary, &ui_backgrounds)? >= TEXT_CONTRAST - 1e-12 {
            provisional_primary
        } else {
            search.fit_color(color(palette, "foreground"), &ui_backgrounds, TEXT_CONTRAST)?
        };
    let content = derive_content(
        &mut search,
        palette,
        &ui_policy,
        &base_ui_backgrounds,
        primary,
    )?;

    // A canvas-fitted fallback tab is an isolated state surface, not a base for
    // semantic fills.
    let semantic_backgrounds = unique(
        interaction_bases
            .iter()
            .cloned()
            .chain(chrome_separates_active_tab.then(|| tab_inactive.clone())),
    );

    let semantic = derive_semantics(
        &mut search,
        palette,
        &ui_policy,
        content,
        &ui_backgrounds,
        &base_ui_backgrounds,
        &semantic_backgrounds,
    )?;

    let content_accent = search.fit_color(&semantic.accent, &ui_backgrounds, TEXT_CONTRAST)?;
    let focus_backgrounds = unique([
        canvas.clone(),
        surface.clone(),
        elevated.clone(),
        chrome.clone(),
    ]);
    let focus_references = [(
        semantic.structural.clone(),
        ui_policy.interactions.adjacent_contrast,
        ui_policy.interactions.adjacent_delta_e,
    )];
    let focus_border = search.fit_state_request(
        &semantic.accent,
        StateFitRequest::new(
            &focus_backgrounds,
            ui_policy.structure.focus,
            MetricBand::bounded(0.060, 0.120, 0.400),
        )
        .with_references(&focus_references),
    )?;
    let element_selection = element_selected.clone();

    let editor_active_line = search
        .fit_readable_overlay(
            &canvas,
            bounded_overlay_request(std::slice::from_ref(&canvas), ui_policy.interactions.hover)
                .with_readable_foregrounds(&[(
                    provisional_editor_text.clone(),
                    EDITOR_BASE_TEXT_CONTRAST,
                )]),
        )
        .map_err(|error| error.context("active editor line"))?;

    let rendered_editor_active_line = gpui_blend(&canvas, &editor_active_line)?.opaque_hex();
    let editor_highlighted_line = search
        .fit_readable_overlay(
            &surface,
            bounded_overlay_request(std::slice::from_ref(&canvas), ui_policy.interactions.active)
                .with_readable_foregrounds(&[(
                    provisional_editor_text.clone(),
                    EDITOR_BASE_TEXT_CONTRAST,
                )])
                .with_rendered_references(&[(
                    rendered_editor_active_line.clone(),
                    STATE_CONSECUTIVE_CONTRAST,
                    STATE_CONSECUTIVE_DELTA_E,
                )]),
        )
        .map_err(|error| error.context("highlighted editor line"))?;
    let rendered_editor_highlighted_line =
        gpui_blend(&canvas, &editor_highlighted_line)?.opaque_hex();
    let debugger_active = search
        .fit_readable_overlay(
            &semantic.red,
            bounded_overlay_request(
                std::slice::from_ref(&canvas),
                ui_policy.interactions.selected,
            )
            .with_readable_foregrounds(&[(
                provisional_editor_text.clone(),
                EDITOR_BASE_TEXT_CONTRAST,
            )])
            .with_rendered_references(&[(
                rendered_editor_highlighted_line.clone(),
                STATE_CONSECUTIVE_CONTRAST,
                STATE_CONSECUTIVE_DELTA_E,
            )]),
        )
        .map_err(|error| error.context("debugger active line"))?;
    let rendered_debugger_active = gpui_blend(&canvas, &debugger_active)?.opaque_hex();
    let editor_bases = unique([
        canvas.clone(),
        chrome.clone(),
        rendered_editor_active_line,
        rendered_editor_highlighted_line,
        rendered_debugger_active,
    ]);

    let editor_primary = search.fit_color(
        color(palette, "foreground"),
        &editor_bases,
        EDITOR_BASE_TEXT_CONTRAST,
    )?;

    let editor_line_number = fit_relative(
        &mut search,
        color(palette, "muted"),
        &editor_primary,
        SaliencyRequest::new(
            &editor_bases,
            PASSIVE_CONTRAST,
            INACTIVE_LINE_NUMBER_SALIENCY,
        )
        .with_maximum_saliency(0.52),
    )?;
    let editor_hover_line_number = fit_relative(
        &mut search,
        color(palette, "muted"),
        &editor_primary,
        SaliencyRequest::new(&editor_bases, CONTROL_CONTRAST, HOVER_LINE_NUMBER_SALIENCY)
            .with_maximum_saliency(0.80),
    )?;
    let editor_active_line_number = fit_relative(
        &mut search,
        &editor_primary,
        &editor_primary,
        SaliencyRequest::new(&editor_bases, TEXT_CONTRAST, PRIMARY_SALIENCY),
    )?;

    // Every consumer projects these two palette-native semantic identities into
    // its own rendering domain. Editor paint is solved before generic highlights
    // so later layers can use the complete emitted diff scenes as their bases.
    let change_identity = ChangeIdentity::from_palette(palette)?;
    let diff_yellow_seed = conventional_semantic_seed(palette, "yellow", 85.0)?;
    let [version_control_added, version_control_deleted] = search
        .fit_pair(
            &change_identity.added,
            &change_identity.deleted,
            &interaction_bases,
            PairConstraints::from_contract(TEXT_CONTRAST, SEMANTIC_PAIR_CONTRACT)
                .with_minimum_chroma(0.025),
        )
        .map_err(|error| error.context("version-control add/delete foregrounds"))?;
    let version_control_modified = search.fit_color_bounded(
        &diff_yellow_seed,
        &interaction_bases,
        &[],
        FitBounds {
            lower_chroma: 0.025,
            ..FitBounds::new(MetricBand::floor(TEXT_CONTRAST))
        },
    )?;

    let presentation = DiffPresentationProfile::for_mode(&palette.mode);
    let [diff_added_seed, diff_deleted_seed] =
        change_identity.editor_overlay_seeds(&palette.mode)?;
    let readable_diff_text = [(editor_primary.clone(), EDITOR_OVERLAY_TEXT_CONTRAST)];
    let diff_line_request = |backgrounds| {
        OverlayFitRequest::new(
            backgrounds,
            MetricBand::floor(presentation.line_target_contrast),
            MetricBand::floor(presentation.line_minimum_delta_e),
        )
        .with_readable_foregrounds(&readable_diff_text)
        .prefer_source_fidelity()
    };
    let line_alpha = opacity_byte(presentation.line_opacity);
    let [line_added, line_deleted] = search.fit_overlay_pair(
        &diff_added_seed,
        &diff_deleted_seed,
        OverlayPairRequest::new(
            diff_line_request(&editor_bases),
            diff_line_request(&editor_bases),
            PairConstraints::new(
                1.0,
                DIFF_PAIR_CONTRAST,
                DIFF_NORMAL_FLOOR_DELTA_E,
                DIFF_CVD_TARGET_DELTA_E,
            )
            .with_minimum_chroma(DIFF_OVERLAY_MINIMUM_CHROMA)
            .balance_rendered_salience(),
        )
        .with_alpha_range(line_alpha, line_alpha, 512),
    )?;
    let mut pigment_candidates = vec![[
        parse_hex(&line_added)?.opaque_hex(),
        parse_hex(&line_deleted)?.opaque_hex(),
    ]];
    let hollow_alpha = opacity_byte(presentation.hollow_opacity);
    let hollow_pair_request = |backgrounds| {
        OverlayFitRequest::new(
            backgrounds,
            MetricBand::floor(1.01),
            MetricBand::floor(0.003),
        )
        .with_readable_foregrounds(&readable_diff_text)
    };
    let [hollow_added, hollow_deleted] = search.fit_overlay_pair(
        &diff_added_seed,
        &diff_deleted_seed,
        OverlayPairRequest::new(
            hollow_pair_request(&editor_bases),
            hollow_pair_request(&editor_bases),
            PairConstraints::new(1.0, 1.001, 0.003, 0.002)
                .with_minimum_chroma(DIFF_OVERLAY_MINIMUM_CHROMA)
                .balance_rendered_salience(),
        )
        .with_alpha_range(hollow_alpha, hollow_alpha, 512),
    )?;
    pigment_candidates.push([
        parse_hex(&hollow_added)?.opaque_hex(),
        parse_hex(&hollow_deleted)?.opaque_hex(),
    ]);
    pigment_candidates.push([diff_added_seed.clone(), diff_deleted_seed.clone()]);
    let mut diff_layers: Option<(DiffLayers, f64)> = None;
    for [added, deleted] in &pigment_candidates {
        let candidate = DiffLayers::from_pigments(added, deleted, presentation)?;
        let penalty =
            candidate.rendered_semantic_penalty(&editor_bases, &editor_primary, presentation)?;
        if diff_layers
            .as_ref()
            .is_none_or(|(_, best_penalty)| penalty.total_cmp(best_penalty).is_lt())
        {
            diff_layers = Some((candidate, penalty));
        }
    }
    let diff_layers = diff_layers
        .expect("editor diff fitting always evaluates authored pigments")
        .0;
    let DiffLayers {
        added_line: diff_added,
        deleted_line: diff_deleted,
        added_hollow: diff_added_hollow,
        deleted_hollow: diff_deleted_hollow,
        added_border: diff_added_hollow_border,
        deleted_border: diff_deleted_hollow_border,
    } = diff_layers;

    let added_hunk_scenes = render_on_bases(&editor_bases, &[&diff_added])?;
    let deleted_hunk_scenes = render_on_bases(&editor_bases, &[&diff_deleted])?;
    let added_hollow_scenes = render_on_bases(&editor_bases, &[&diff_added_hollow])?;
    let deleted_hollow_scenes = render_on_bases(&editor_bases, &[&diff_deleted_hollow])?;
    let readable_editor_overlay_text = [(editor_primary.clone(), EDITOR_OVERLAY_TEXT_CONTRAST)];
    let search_match_request = OverlayFitRequest::new(
        &editor_bases,
        MetricBand::floor(SEARCH_MATCH_CONTRAST),
        MetricBand::floor(STATE_HOVER_DELTA_E),
    )
    .with_readable_foregrounds(&readable_editor_overlay_text);
    let search_active_request = OverlayFitRequest::new(
        &editor_bases,
        MetricBand::floor(SEARCH_ACTIVE_CONTRAST),
        MetricBand::floor(STATE_SELECTED_DELTA_E),
    )
    .with_readable_foregrounds(&readable_editor_overlay_text);
    let sequential_search = match fit_highlight_strict(
        &mut search,
        "search.match_background",
        &semantic.yellow,
        search_match_request,
    )? {
        Some(search_match) => {
            let references = [(
                search_match.clone(),
                STATE_CONSECUTIVE_CONTRAST,
                STATE_CONSECUTIVE_DELTA_E,
            )];
            fit_highlight_strict(
                &mut search,
                "search.active_match_background",
                &semantic.accent,
                search_active_request.with_rendered_references(&references),
            )?
            .map(|search_active| [search_match, search_active])
        }
        None => None,
    };
    let [search_match, search_active] = match sequential_search {
        Some(search) => search,
        None => search.fit_overlay_pair(
            &semantic.yellow,
            &semantic.accent,
            OverlayPairRequest::new(
                search_match_request,
                search_active_request,
                PairConstraints::new(
                    SEARCH_MATCH_CONTRAST,
                    STATE_CONSECUTIVE_CONTRAST,
                    STATE_CONSECUTIVE_DELTA_E,
                    0.0,
                ),
            )
            .with_limits(OVERLAY_MAX_ALPHA, 512),
        )?,
    };

    let document_read = fit_highlight(
        &mut search,
        "editor.document_highlight.read_background",
        &semantic.accent,
        OverlayFitRequest::new(
            &editor_bases,
            MetricBand::floor(STATE_SELECTED_CONTRAST),
            MetricBand::floor(STATE_SELECTED_DELTA_E),
        )
        .with_readable_foregrounds(&readable_editor_overlay_text),
    )?;

    let document_write = fit_highlight(
        &mut search,
        "editor.document_highlight.write_background",
        &semantic.orange,
        OverlayFitRequest::new(
            &editor_bases,
            MetricBand::floor(STATE_SELECTED_CONTRAST),
            MetricBand::floor(STATE_SELECTED_DELTA_E),
        )
        .with_readable_foregrounds(&readable_editor_overlay_text),
    )?;
    let document_bracket = fit_highlight(
        &mut search,
        "editor.document_highlight.bracket_background",
        &semantic.cyan,
        OverlayFitRequest::new(
            &editor_bases,
            MetricBand::floor(STATE_SELECTED_CONTRAST),
            MetricBand::floor(STATE_SELECTED_DELTA_E),
        )
        .with_readable_foregrounds(&readable_editor_overlay_text),
    )?;

    let conflict_constraints = PairConstraints::new(CONFLICT_FILL_CONTRAST, 1.01, 0.030, 0.030)
        .with_separation_alternative(Some((1.12, 0.075, 0.035)))
        .prefer_background();
    let conflict_fill_request = |backgrounds| {
        OverlayFitRequest::new(
            backgrounds,
            MetricBand::floor(CONFLICT_FILL_CONTRAST),
            MetricBand::floor(0.030),
        )
        .with_readable_foregrounds(&readable_diff_text)
    };
    let [conflict_ours, conflict_theirs] = search
        .fit_overlay_pair(
            &change_identity.added,
            color(palette, "blue"),
            OverlayPairRequest::new(
                conflict_fill_request(&editor_bases),
                conflict_fill_request(&editor_bases),
                conflict_constraints,
            ),
        )
        .map_err(|error| error.context("version-control conflict markers"))?;

    let yank = fit_highlight(
        &mut search,
        "vim.yank.background",
        &semantic.yellow,
        OverlayFitRequest::new(
            &editor_bases,
            MetricBand::floor(STATE_SELECTED_CONTRAST),
            MetricBand::floor(STATE_SELECTED_DELTA_E),
        )
        .with_readable_foregrounds(&readable_editor_overlay_text),
    )?;

    let generic_highlights = [
        search_match.as_str(),
        search_active.as_str(),
        document_read.as_str(),
        document_write.as_str(),
        document_bracket.as_str(),
        yank.as_str(),
    ];
    let word_added_bases = added_hunk_scenes
        .iter()
        .cloned()
        .chain(added_hollow_scenes.iter().cloned())
        .collect::<Vec<_>>();
    let word_deleted_bases = deleted_hunk_scenes
        .iter()
        .cloned()
        .chain(deleted_hollow_scenes.iter().cloned())
        .collect::<Vec<_>>();
    let word_added_underlays =
        render_with_bounded_generic_highlights(&word_added_bases, &generic_highlights)?;
    let word_deleted_underlays =
        render_with_bounded_generic_highlights(&word_deleted_bases, &generic_highlights)?;
    let word_request = |backgrounds| {
        OverlayFitRequest::new(backgrounds, MetricBand::floor(1.0), MetricBand::floor(0.0))
            .prefer_source_fidelity()
    };
    let word_alpha = opacity_byte(presentation.word_opacity);
    let [word_added, word_deleted] = search
        .fit_overlay_pair(
            &change_identity.added,
            &change_identity.deleted,
            OverlayPairRequest::new(
                word_request(&word_added_underlays),
                word_request(&word_deleted_underlays),
                PairConstraints::new(1.0, 1.0, 0.0, 0.0)
                    .with_minimum_chroma(DIFF_OVERLAY_MINIMUM_CHROMA),
            )
            .with_alpha_range(word_alpha, word_alpha, 512),
        )
        .map_err(|error| error.context("version-control word diff overlays"))?;
    let rendered_editor_overlays = [
        &search_match,
        &search_active,
        &document_read,
        &document_write,
        &document_bracket,
        &yank,
        &conflict_ours,
        &conflict_theirs,
    ]
    .into_iter()
    .map(|overlay| render_on_bases(&editor_bases, &[overlay]))
    .collect::<Result<Vec<_>>>()?;
    let rendered_editor_overlay_contexts = rendered_editor_overlays
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();

    let word_added_scenes = render_on_bases(&word_added_underlays, &[&word_added])?;
    let word_deleted_scenes = render_on_bases(&word_deleted_underlays, &[&word_deleted])?;
    let selection_visibility_backgrounds = unique(
        editor_bases
            .iter()
            .cloned()
            .chain(added_hunk_scenes.iter().cloned())
            .chain(deleted_hunk_scenes.iter().cloned())
            .chain(added_hollow_scenes.iter().cloned())
            .chain(deleted_hollow_scenes.iter().cloned())
            .chain(rendered_editor_overlay_contexts.iter().cloned()),
    );
    let editor_text_backgrounds = unique(
        selection_visibility_backgrounds
            .iter()
            .cloned()
            .chain(word_added_scenes.iter().cloned())
            .chain(word_deleted_scenes.iter().cloned()),
    );
    let fit_editor_primary = |search: &mut Search, target| {
        search.fit_color_bounded_with_preference_backgrounds(
            color(palette, "foreground"),
            &editor_text_backgrounds,
            &editor_bases,
            &[],
            FitBounds::new(MetricBand::floor(target)),
        )
    };
    let editor_primary = fit_editor_primary(&mut search, EDITOR_OVERLAY_TEXT_CONTRAST)?;

    let selection_readable = [(editor_primary.clone(), TEXT_CONTRAST)];
    let selection_request = || {
        bounded_overlay_request(
            &selection_visibility_backgrounds,
            ui_policy.interactions.selected,
        )
        .with_runtime_state((0.5, 1.08, 0.020))
        .with_readability_backgrounds(&editor_text_backgrounds)
        .with_readable_foregrounds(&selection_readable)
    };
    let selection = search.fit_readable_overlay_alpha_range(
        color(palette, "selection"),
        selection_request(),
        u8::MAX,
        u8::MAX,
    )?;

    let local_unfocused_overlay = apply_opacity(&selection, 0.5)?;
    let local_selection = gpui_blend(&canvas, &selection)?.opaque_hex();
    let local_unfocused_selection = gpui_blend(&canvas, &local_unfocused_overlay)?.opaque_hex();
    let terminal_backgrounds = unique([canvas.clone(), local_selection, local_unfocused_selection]);
    let foreground_triplet = terminal_triplet(
        &mut search,
        TerminalRequest {
            seeds: [
                color(palette, "dark_foreground"),
                color(palette, "foreground"),
                color(palette, "bright_foreground"),
            ],
            backgrounds: &terminal_backgrounds,
            mode: &palette.mode,
            policy: ui_policy.terminal,
        },
    )?;
    let mut terminal = BTreeMap::from([
        ("terminal.background".into(), canvas.clone()),
        ("terminal.ansi.background".into(), canvas.clone()),
        (
            "terminal.dim_foreground".into(),
            foreground_triplet[0].clone(),
        ),
        ("terminal.foreground".into(), foreground_triplet[1].clone()),
        (
            "terminal.bright_foreground".into(),
            foreground_triplet[2].clone(),
        ),
    ]);
    for (index, name) in [
        "black", "red", "green", "yellow", "blue", "magenta", "cyan", "white",
    ]
    .iter()
    .enumerate()
    {
        let dim_key = format!("color{index}");
        let bright_key = format!("color{}", index + 8);
        let triplet = terminal_triplet(
            &mut search,
            TerminalRequest {
                seeds: [
                    color(palette, &dim_key),
                    color(palette, &dim_key),
                    color(palette, &bright_key),
                ],
                backgrounds: &terminal_backgrounds,
                mode: &palette.mode,
                policy: ui_policy.terminal,
            },
        )?;
        terminal.insert(format!("terminal.ansi.dim_{name}"), triplet[0].clone());
        terminal.insert(format!("terminal.ansi.{name}"), triplet[1].clone());
        terminal.insert(format!("terminal.ansi.bright_{name}"), triplet[2].clone());
    }

    let overlay_contexts = editor_text_backgrounds.clone();
    let local_focused = editor_text_backgrounds
        .iter()
        .map(|base| Ok(gpui_blend(base, &selection)?.opaque_hex()))
        .collect::<Result<Vec<_>>>()?;
    let local_unfocused = editor_text_backgrounds
        .iter()
        .map(|base| Ok(gpui_blend(base, &local_unfocused_overlay)?.opaque_hex()))
        .collect::<Result<Vec<_>>>()?;
    let syntax_contexts = unique(
        editor_bases
            .iter()
            .cloned()
            .chain(overlay_contexts.iter().cloned())
            .chain(local_focused)
            .chain(local_unfocused),
    );

    // Zed's tinted default buttons consume the info channel, so they should
    // carry the palette's accent identity rather than its generic blue role.
    let status_seeds: BTreeMap<&str, &String> = BTreeMap::from([
        ("created", &change_identity.added),
        ("deleted", &change_identity.deleted),
        ("hidden", &semantic.disabled),
        ("hint", &semantic.cyan),
        ("ignored", &semantic.secondary),
        ("info", &semantic.accent),
        ("predictive", &semantic.secondary),
        ("unreachable", &semantic.secondary),
        ("warning", &diff_yellow_seed),
    ]);
    let mut status_backgrounds = BTreeMap::new();
    for name in status_seeds.keys() {
        let minimum_chroma = retained_tint_chroma(status_seeds[name])?;
        let status_background_contexts = [surface.clone()];
        let status_references = [
            (
                semantic.primary.clone(),
                TEXT_CONTRAST,
                STATE_CONSECUTIVE_DELTA_E,
            ),
            (
                semantic.structural.clone(),
                ui_policy.interactions.adjacent_contrast,
                ui_policy.interactions.adjacent_delta_e,
            ),
        ];
        status_backgrounds.insert(
            *name,
            search.fit_state_request(
                status_seeds[name],
                StateFitRequest::new(
                    &status_background_contexts,
                    ui_policy.interactions.selected.contrast,
                    ui_policy.interactions.selected.delta_e,
                )
                .with_minimum_chroma(minimum_chroma)
                .with_references(&status_references),
            )?,
        );
    }

    let mut status_foregrounds = BTreeMap::new();
    let created_backgrounds = unique(
        interaction_bases
            .iter()
            .chain(editor_text_backgrounds.iter())
            .cloned()
            .chain(std::iter::once(status_backgrounds["created"].clone())),
    );
    let deleted_backgrounds = unique(
        interaction_bases
            .iter()
            .chain(editor_text_backgrounds.iter())
            .cloned()
            .chain(std::iter::once(status_backgrounds["deleted"].clone())),
    );
    let status_pair_constraints =
        PairConstraints::from_contract(TEXT_CONTRAST, SEMANTIC_PAIR_CONTRACT)
            .with_minimum_chroma(0.025);
    let [created, deleted] = search.fit_pair_on_backgrounds(
        &change_identity.added,
        &created_backgrounds,
        &change_identity.deleted,
        &deleted_backgrounds,
        status_pair_constraints,
    )?;
    status_foregrounds.insert("created", created);
    status_foregrounds.insert("deleted", deleted);
    for (name, seed) in &status_seeds {
        if *name == "ignored" || status_foregrounds.contains_key(name) {
            continue;
        }
        let syntax_backgrounds = if *name == "predictive" {
            syntax_contexts.as_slice()
        } else {
            &[]
        };
        let status_foreground_backgrounds = unique(
            interaction_bases
                .iter()
                .chain(editor_text_backgrounds.iter())
                .cloned()
                .chain(std::iter::once(status_backgrounds[name].clone()))
                .chain(syntax_backgrounds.iter().cloned()),
        );
        let output = if matches!(*name, "created" | "deleted" | "warning") {
            search.fit_color_bounded(
                seed,
                &status_foreground_backgrounds,
                &[],
                FitBounds {
                    lower_chroma: 0.025,
                    ..FitBounds::new(MetricBand::floor(TEXT_CONTRAST))
                },
            )?
        } else {
            search.fit_color(seed, &status_foreground_backgrounds, TEXT_CONTRAST)?
        };
        status_foregrounds.insert(*name, output);
    }
    status_foregrounds.insert("ignored", status_foregrounds["hidden"].clone());

    let mut statuses = BTreeMap::new();
    for name in status_seeds.keys() {
        let background = status_backgrounds[name].clone();
        let border = search.fit_color_bounded(
            status_seeds[name],
            &[surface.clone(), background.clone()],
            &[],
            FitBounds {
                lower_chroma: retained_tint_chroma(status_seeds[name])?,
                ..FitBounds::new(ui_policy.structure.status_border)
            },
        )?;
        statuses.insert((*name).to_owned(), status_foregrounds[name].clone());
        statuses.insert(format!("{name}.background"), background);
        statuses.insert(format!("{name}.border"), border);
    }
    let mode_seeds: [(&str, &String); 8] = [
        ("normal", &semantic.accent),
        ("insert", &semantic.green),
        ("replace", &semantic.red),
        ("visual", &semantic.magenta),
        ("visual_line", &semantic.blue),
        ("visual_block", &semantic.yellow),
        ("helix_normal", &semantic.cyan),
        ("helix_select", &semantic.accent),
    ];
    let mut vim = BTreeMap::from([
        ("vim.yank.background".into(), yank.clone()),
        (
            "vim.helix_jump_label.foreground".into(),
            search.fit_color(&semantic.red, &editor_bases, TEXT_CONTRAST)?,
        ),
    ]);
    for (name, seed) in mode_seeds {
        let background_contexts = [chrome.clone()];
        let background = search.fit_state_request(
            seed,
            StateFitRequest::new(
                &background_contexts,
                ui_policy.interactions.selected.contrast,
                ui_policy.interactions.selected.delta_e,
            ),
        )?;
        vim.insert(
            format!("vim.{name}.foreground"),
            search.fit_color(seed, std::slice::from_ref(&background), TEXT_CONTRAST)?,
        );
        vim.insert(format!("vim.{name}.background"), background);
    }

    let syntax = build_syntax(
        &mut search,
        palette,
        SyntaxContexts {
            ordinary: std::slice::from_ref(&canvas),
            rendered: &syntax_contexts,
        },
        &editor_primary,
        &statuses["predictive"],
        [
            &change_identity.added,
            &diff_yellow_seed,
            &change_identity.deleted,
        ],
    )?;

    let accent_seeds = cvd_greedy_order(&[
        semantic.accent.clone(),
        semantic.orange.clone(),
        semantic.magenta.clone(),
        semantic.green.clone(),
        semantic.blue.clone(),
        semantic.yellow.clone(),
        semantic.cyan.clone(),
        semantic.red.clone(),
        color(palette, "brown").into(),
        color(palette, "bright_blue").into(),
        color(palette, "bright_magenta").into(),
        color(palette, "bright_green").into(),
    ])?;
    let accents = search.fit_distinct_colors(
        &accent_seeds,
        std::slice::from_ref(&canvas),
        CONTROL_CONTRAST,
    )?;

    let drop_target = search.fit_readable_overlay_bounded(
        &semantic.accent,
        bounded_overlay_request(
            std::slice::from_ref(&surface),
            ui_policy.interactions.selected,
        ),
        OVERLAY_MAX_ALPHA,
    )?;
    let rendered_drop_target = render_layers(&surface, &[&drop_target])?;
    let drop_target_border = search.fit_color(
        &semantic.accent,
        &[surface.clone(), rendered_drop_target],
        CONTROL_CONTRAST,
    )?;

    let thumb_contexts = unique([chrome.clone(), surface.clone(), canvas.clone()]);
    let thumb_base = search.fit_readable_overlay_bounded(
        &semantic.primary,
        bounded_overlay_request(&thumb_contexts, ui_policy.scroll.idle),
        OVERLAY_MAX_ALPHA,
    )?;
    let thumb_hover_references = [(
        thumb_base.clone(),
        ui_policy.interactions.adjacent_contrast,
        ui_policy.interactions.adjacent_delta_e,
    )];
    let thumb_hover = search.fit_readable_overlay_bounded(
        &semantic.primary,
        bounded_overlay_request(&thumb_contexts, ui_policy.scroll.hover)
            .with_rendered_references(&thumb_hover_references),
        OVERLAY_MAX_ALPHA,
    )?;
    let thumb_active_references = [(
        thumb_hover.clone(),
        ui_policy.interactions.adjacent_contrast,
        ui_policy.interactions.adjacent_delta_e,
    )];
    let thumb_active = search.fit_readable_overlay_bounded(
        &semantic.primary,
        bounded_overlay_request(&thumb_contexts, ui_policy.scroll.active)
            .with_rendered_references(&thumb_active_references),
        OVERLAY_MAX_ALPHA,
    )?;
    let thumb_ladder = [thumb_base, thumb_hover, thumb_active];
    let thumb_border = semantic.structural.clone();
    let track_border = fit_bounded_color(
        &mut search,
        &semantic.passive,
        &thumb_contexts,
        ui_policy.structure.passive,
    )?;

    let wrap_guide = with_alpha(&semantic.structural, 0x0d as f64 / 255.0)?;
    let active_wrap_guide = with_alpha(&semantic.structural, 0x1a as f64 / 255.0)?;
    let editor_invisible = fit_bounded_color(
        &mut search,
        color(palette, "muted"),
        std::slice::from_ref(&canvas),
        ui_policy.structure.normal,
    )?;
    let editor_indent_guide = fit_bounded_color(
        &mut search,
        &semantic.passive,
        std::slice::from_ref(&canvas),
        ui_policy.structure.passive,
    )?;
    let editor_indent_guide_active = fit_bounded_color(
        &mut search,
        &semantic.structural,
        std::slice::from_ref(&canvas),
        ui_policy.structure.active_guide,
    )?;

    // Multiplayer is the final color-allocation stage so lower-priority player
    // differentiation cannot shape syntax, diff, or interface choices.
    let player_seed_values = [
        semantic.accent.clone(),
        semantic.orange.clone(),
        semantic.magenta.clone(),
        semantic.green.clone(),
        semantic.blue.clone(),
        semantic.yellow.clone(),
        semantic.cyan.clone(),
        semantic.red.clone(),
    ];
    let mut player_seeds = vec![player_seed_values[0].clone()];
    player_seeds.extend(cvd_greedy_order(&player_seed_values[1..])?);
    let mut player_cursor_backgrounds = editor_text_backgrounds.clone();
    for background in &editor_text_backgrounds {
        player_cursor_backgrounds.push(gpui_blend(background, &selection)?.opaque_hex());
        player_cursor_backgrounds
            .push(gpui_blend(background, &local_unfocused_overlay)?.opaque_hex());
    }
    let player_cursors = fit_player_cursors(
        &mut search,
        &player_seeds,
        &unique(player_cursor_backgrounds),
        &palette.mode,
    )?;
    let readable = [(editor_primary.clone(), TEXT_CONTRAST)];
    let mut player_selections = vec![selection.clone()];
    for cursor in player_cursors.iter().skip(1) {
        let references = player_selections
            .iter()
            .map(|selection| (selection.clone(), 1.0, PLAYER_SELECTION_DELTA_E))
            .collect::<Vec<_>>();
        // Simultaneous player identities outrank the ordinary selection ceiling:
        // a crowded session may need more salience to keep every owner distinct.
        let request = OverlayFitRequest::new(
            &selection_visibility_backgrounds,
            MetricBand::floor(ui_policy.interactions.selected.contrast.minimum()),
            MetricBand::floor(ui_policy.interactions.selected.delta_e.minimum()),
        )
        .with_runtime_state((0.5, 1.08, 0.020))
        .with_readability_backgrounds(&editor_text_backgrounds)
        .with_readable_foregrounds(&readable)
        .with_rendered_references(&references);
        player_selections.push(search.fit_readable_overlay_alpha_range(
            cursor,
            request,
            u8::MAX,
            u8::MAX,
        )?);
    }
    let mut players = Vec::with_capacity(player_seeds.len());
    for ((seed, cursor), selection) in player_seeds
        .iter()
        .zip(&player_cursors)
        .zip(player_selections)
    {
        let background_contexts = [canvas.clone()];
        let background_references = [(cursor.clone(), CONTROL_CONTRAST, STATE_CONSECUTIVE_DELTA_E)];
        let background = search.fit_state_request(
            seed,
            StateFitRequest::new(
                &background_contexts,
                MetricBand::floor(ui_policy.interactions.hover.contrast.minimum()),
                MetricBand::floor(ui_policy.interactions.hover.delta_e.minimum()),
            )
            .with_references(&background_references),
        )?;
        players.push(BTreeMap::from([
            ("cursor".into(), cursor.clone()),
            ("background".into(), background),
            ("selection".into(), selection),
        ]));
    }

    let status_channel = |name: &str| -> Result<StatusChannel> {
        Ok(StatusChannel {
            foreground: OpaqueColor::new(statuses[name].clone())?,
            background: OpaqueColor::new(statuses[&format!("{name}.background")].clone())?,
            border: OpaqueColor::new(statuses[&format!("{name}.border")].clone())?,
        })
    };

    let theme_tokens = ThemeTokens {
        surfaces: SurfaceTokens {
            editor_canvas: OpaqueColor::new(canvas.clone())?,
            app_frame: OpaqueColor::new(chrome.clone())?,
            elevated: OpaqueColor::new(elevated.clone())?,
            secondary: OpaqueColor::new(surface.clone())?,
            inactive_control: OpaqueColor::new(chrome.clone())?,
            editor_highlighted_line: OverlayColor::new(editor_highlighted_line.clone())?,
        },
        content: ContentTokens {
            primary: OpaqueColor::new(semantic.primary.clone())?,
            muted: OpaqueColor::new(semantic.secondary.clone())?,
            placeholder: OpaqueColor::new(semantic.placeholder.clone())?,
            disabled: OpaqueColor::new(semantic.disabled.clone())?,
            icon_muted: OpaqueColor::new(semantic.icon_muted.clone())?,
            icon_placeholder: OpaqueColor::new(semantic.icon_placeholder.clone())?,
            icon_disabled: OpaqueColor::new(semantic.icon_disabled.clone())?,
            accent: OpaqueColor::new(content_accent)?,
            editor_primary: OpaqueColor::new(editor_primary.clone())?,
        },
        interactions: InteractionTokens {
            element_hover: OverlayColor::new(element_hover.clone())?,
            element_active: OverlayColor::new(element_active.clone())?,
            element_selected: OverlayColor::new(element_selected.clone())?,
            ghost_hover: OverlayColor::new(ghost_hover.clone())?,
            ghost_active: OverlayColor::new(ghost_active.clone())?,
            ghost_selected: OverlayColor::new(ghost_selected.clone())?,
        },
        statuses: StatusTokens {
            positive: status_channel("created")?,
            negative: status_channel("deleted")?,
            warning: status_channel("warning")?,
            informational: status_channel("info")?,
            predictive: status_channel("predictive")?,
            hint: status_channel("hint")?,
            hidden: status_channel("hidden")?,
            ignored: status_channel("ignored")?,
            unreachable: status_channel("unreachable")?,
        },
        derived: DerivedTokens {
            editor_active_line: OverlayColor::new(editor_active_line.clone())?,
            wrap_guide: OverlayColor::new(wrap_guide.clone())?,
            active_wrap_guide: OverlayColor::new(active_wrap_guide.clone())?,
            document_read: OverlayColor::new(document_read.clone())?,
        },
    };

    let mut fixed = StyleBuilder::default();
    macro_rules! put {
        ($name:expr, $value:expr) => {
            fixed.insert_opaque($name, $value)?;
        };
    }
    macro_rules! put_overlay {
        ($name:expr, $value:expr) => {
            fixed.insert_overlay($name, $value)?;
        };
    }
    put!("border", semantic.structural.clone());
    put!("border.variant", semantic.passive.clone());
    put!("border.focused", focus_border.clone());
    put!("border.selected", focus_border.clone());
    put!("border.disabled", semantic.passive.clone());
    put!("element.background", surface.clone());
    put_overlay!("element.selection_background", element_selection);
    put_overlay!("drop_target.background", drop_target);
    put!("drop_target.border", drop_target_border);
    put!("debugger.accent", semantic.red.clone());
    put!("tab_bar.background", chrome.clone());
    put!("tab.inactive_background", tab_inactive.clone());
    put_overlay!("search.match_background", search_match);
    put_overlay!("search.active_match_background", search_active);
    put!("panel.background", surface.clone());
    put!("panel.focused_border", focus_border.clone());
    put!("panel.indent_guide", panel_guide_ladder[0].clone());
    put!("panel.indent_guide_hover", panel_guide_ladder[1].clone());
    put!("panel.indent_guide_active", panel_guide_ladder[2].clone());
    put!("panel.overlay_background", panel_overlay);
    put!("panel.overlay_hover", panel_overlay_hover);
    put!("pane.focused_border", focus_border);
    put!("pane_group.border", semantic.structural.clone());
    put_overlay!("scrollbar.thumb.background", thumb_ladder[0].clone());
    put_overlay!("scrollbar.thumb.hover_background", thumb_ladder[1].clone());
    put_overlay!("scrollbar.thumb.active_background", thumb_ladder[2].clone());
    put!("scrollbar.thumb.border", thumb_border.clone());
    put!("scrollbar.track.border", track_border);
    put_overlay!("minimap.thumb.background", thumb_ladder[0].clone());
    put_overlay!("minimap.thumb.hover_background", thumb_ladder[1].clone());
    put_overlay!("minimap.thumb.active_background", thumb_ladder[2].clone());
    put!("minimap.thumb.border", thumb_border);
    put!("editor.subheader.background", chrome);
    put_overlay!("editor.debugger_active_line.background", debugger_active);
    put!("editor.line_number", editor_line_number.output);
    put!(
        "editor.active_line_number",
        editor_active_line_number.output
    );
    put!("editor.hover_line_number", editor_hover_line_number.output);
    put!("editor.invisible", editor_invisible);
    put!("editor.indent_guide", editor_indent_guide);
    put!("editor.indent_guide_active", editor_indent_guide_active);
    put_overlay!("editor.document_highlight.write_background", document_write);
    put_overlay!(
        "editor.document_highlight.bracket_background",
        document_bracket
    );
    put_overlay!("editor.diff_hunk.added.background", diff_added);
    put_overlay!(
        "editor.diff_hunk.added.hollow_background",
        diff_added_hollow.clone()
    );
    put_overlay!(
        "editor.diff_hunk.added.hollow_border",
        diff_added_hollow_border
    );
    put_overlay!("editor.diff_hunk.deleted.background", diff_deleted);
    put_overlay!(
        "editor.diff_hunk.deleted.hollow_background",
        diff_deleted_hollow.clone()
    );
    put_overlay!(
        "editor.diff_hunk.deleted.hollow_border",
        diff_deleted_hollow_border
    );
    fixed.extend_opaque(terminal)?;

    put!("version_control.added", version_control_added);
    put!("version_control.deleted", version_control_deleted);
    put!("version_control.modified", version_control_modified);
    put!("version_control.renamed", semantic.blue.clone());
    put!("version_control.conflict", semantic.orange.clone());
    put!("version_control.ignored", semantic.secondary.clone());
    put_overlay!("version_control.word_added", word_added);
    put_overlay!("version_control.word_deleted", word_deleted);
    put_overlay!("version_control.conflict_marker.ours", conflict_ours);
    put_overlay!("version_control.conflict_marker.theirs", conflict_theirs);

    let vim_yank = vim
        .remove("vim.yank.background")
        .expect("generated Vim roles must include the yank highlight");
    fixed.insert_overlay("vim.yank.background", vim_yank)?;

    fixed.extend_opaque(vim)?;
    fixed.extend(theme_tokens.zed_roles());
    let mut status_roles = StyleBuilder::default();
    status_roles.extend(theme_tokens.statuses.zed_roles());

    let mut style = Map::new();
    style.insert("background.appearance".into(), "opaque".into());
    status_roles.append_to(&mut style);
    fixed.append_to(&mut style);

    style.insert(
        "accents".into(),
        Value::Array(accents.into_iter().map(Into::into).collect()),
    );

    style.insert(
        "players".into(),
        Value::Array(
            players
                .into_iter()
                .map(|player| {
                    Value::Object(
                        player
                            .into_iter()
                            .map(|(key, value)| (key, value.into()))
                            .collect(),
                    )
                })
                .collect(),
        ),
    );

    style.insert("syntax".into(), Value::Object(syntax));

    let document = json!({
        "$schema": SCHEMA_URL, "name": THEME_NAME, "author": "APS",
        "themes": [{"name": THEME_NAME, "appearance": palette.mode, "style": style}],
    });

    if let Err(error) = validate_theme_structure(&document) {
        panic!("generated theme has invalid structure: {error}");
    }

    Ok(document)
}

fn validate_theme_structure(document: &Value) -> Result<()> {
    let root = document
        .as_object()
        .ok_or_else(|| Error::invalid("generated theme document must be an object"))?;
    let expected_root = ["$schema", "author", "name", "themes"]
        .into_iter()
        .collect::<BTreeSet<_>>();
    if root.keys().map(String::as_str).collect::<BTreeSet<_>>() != expected_root {
        return Err(Error::invalid(
            "generated theme document manifest does not match its schema",
        ));
    }
    if root.get("$schema").and_then(Value::as_str) != Some(SCHEMA_URL)
        || root.get("name").and_then(Value::as_str) != Some(THEME_NAME)
        || root.get("author").and_then(Value::as_str) != Some("APS")
    {
        return Err(Error::invalid(
            "generated theme document metadata does not match its schema",
        ));
    }
    let themes = root
        .get("themes")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::invalid("generated themes must be an array"))?;
    if themes.len() != 1 {
        return Err(Error::invalid(
            "generated theme document must contain exactly one theme",
        ));
    }
    let theme = themes[0]
        .as_object()
        .ok_or_else(|| Error::invalid("generated theme entry must be an object"))?;
    let expected_theme = ["appearance", "name", "style"]
        .into_iter()
        .collect::<BTreeSet<_>>();
    if theme.keys().map(String::as_str).collect::<BTreeSet<_>>() != expected_theme
        || theme.get("name").and_then(Value::as_str) != Some(THEME_NAME)
    {
        return Err(Error::invalid(
            "generated theme entry manifest does not match its schema",
        ));
    }
    let appearance = theme
        .get("appearance")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::invalid("generated theme appearance must be a string"))?;
    if !matches!(appearance, "dark" | "light") {
        return Err(Error::invalid(format!(
            "generated theme appearance must be dark or light, got {appearance:?}"
        )));
    }
    let style = theme
        .get("style")
        .and_then(Value::as_object)
        .ok_or_else(|| Error::invalid("generated theme style must be an object"))?;
    let fixed_expected: BTreeSet<String> = FOUNDATION_FIELDS
        .iter()
        .chain(CHROME_FIELDS)
        .chain(EDITOR_FIELDS)
        .chain(TERMINAL_FIELDS)
        .chain(LINK_VC_FIELDS)
        .chain(VIM_FIELDS)
        .map(|name| (*name).to_owned())
        .collect();
    let status_expected: BTreeSet<String> = STATUS_NAMES
        .iter()
        .flat_map(|name| {
            [
                (*name).to_owned(),
                format!("{name}.background"),
                format!("{name}.border"),
            ]
        })
        .collect();
    let allowed: BTreeSet<String> = fixed_expected
        .iter()
        .cloned()
        .chain(status_expected.iter().cloned())
        .chain(["background.appearance", "accents", "players", "syntax"].map(str::to_owned))
        .collect();
    let actual = style.keys().cloned().collect::<BTreeSet<_>>();
    if actual != allowed {
        return Err(Error::invalid(
            "generated style manifest does not match its schema",
        ));
    }
    for name in fixed_expected.iter().chain(&status_expected) {
        let color = style
            .get(name)
            .and_then(Value::as_str)
            .ok_or_else(|| Error::invalid(format!("generated theme role {name} is not a color")))?;
        parse_hex(color)?;
    }
    if style.get("background.appearance").and_then(Value::as_str) != Some("opaque") {
        return Err(Error::invalid(
            "generated background appearance must be opaque",
        ));
    }
    let accents = style
        .get("accents")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::invalid("generated accents must be an array"))?;
    if accents.len() != 12 {
        return Err(Error::invalid("generated theme must contain 12 accents"));
    }
    for (index, color) in accents.iter().enumerate() {
        parse_hex(
            color.as_str().ok_or_else(|| {
                Error::invalid(format!("generated accent {index} is not a color"))
            })?,
        )?;
    }
    let players = style
        .get("players")
        .and_then(Value::as_array)
        .ok_or_else(|| Error::invalid("generated players must be an array"))?;
    if players.len() != 8 {
        return Err(Error::invalid("generated theme must contain 8 players"));
    }
    for (index, player) in players.iter().enumerate() {
        let player = player
            .as_object()
            .ok_or_else(|| Error::invalid(format!("generated player {index} is not an object")))?;
        if player.len() != 3 {
            return Err(Error::invalid(format!(
                "generated player {index} must contain cursor, background, and selection"
            )));
        }
        for role in ["cursor", "background", "selection"] {
            parse_hex(player.get(role).and_then(Value::as_str).ok_or_else(|| {
                Error::invalid(format!("generated player {index}.{role} is not a color"))
            })?)?;
        }
    }
    let syntax = style
        .get("syntax")
        .and_then(Value::as_object)
        .ok_or_else(|| Error::invalid("generated syntax must be an object"))?;
    let expected_syntax = crate::syntax::policy::CAPTURE_POLICIES
        .iter()
        .map(|policy| policy.capture)
        .collect::<BTreeSet<_>>();
    if syntax.keys().map(String::as_str).collect::<BTreeSet<_>>() != expected_syntax {
        return Err(Error::invalid(
            "generated syntax manifest does not match capture policy",
        ));
    }
    for (name, value) in syntax {
        parse_hex(
            value
                .get("color")
                .and_then(Value::as_str)
                .ok_or_else(|| Error::invalid(format!("generated syntax.{name} has no color")))?,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[should_panic(expected = "theme role text was generated twice")]
    fn style_builder_rejects_duplicate_roles() {
        let mut style = StyleBuilder::default();
        style.insert_opaque("text", "#112233".into()).unwrap();
        style.insert_opaque("text", "#445566".into()).unwrap();
    }

    #[test]
    fn structural_validation_rejects_incomplete_document_envelopes() {
        for document in [
            json!({}),
            json!({
                "$schema": SCHEMA_URL,
                "name": THEME_NAME,
                "author": "APS",
                "themes": [],
            }),
            json!({
                "$schema": SCHEMA_URL,
                "name": THEME_NAME,
                "author": "someone else",
                "themes": [{"name": THEME_NAME, "appearance": "dark", "style": {}}],
            }),
        ] {
            assert_eq!(
                validate_theme_structure(&document).unwrap_err().kind(),
                crate::ErrorKind::InvalidInput
            );
        }
    }

    #[test]
    fn conventional_semantic_seed_recovers_chroma_from_gamut_endpoints() {
        let palette = ResolvedPalette {
            mode: "dark".into(),
            colors: BTreeMap::from([
                ("green".into(), "#000000".into()),
                ("red".into(), "#ffffff".into()),
                ("yellow".into(), "#000000".into()),
            ]),
            provenance: BTreeMap::new(),
        };
        for (key, target) in [("green", 145.0), ("red", 25.0), ("yellow", 85.0)] {
            let output = conventional_semantic_seed(&palette, key, target).unwrap();
            let [_, chroma, hue] = oklab_to_oklch(lab(&output).unwrap());
            let target = target.to_radians();
            let difference = (hue - target).abs();
            let hue_distance = difference.min(std::f64::consts::TAU - difference);
            assert!(chroma >= 0.025, "{key} stayed achromatic: {output}");
            assert!(hue_distance <= 5.0_f64.to_radians(), "{key}: {output}");
        }
    }
}
