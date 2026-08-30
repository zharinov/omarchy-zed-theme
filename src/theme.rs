//! Builds a complete Zed theme and rejects output that violates its color constraints.
//!
//! Omarchy supplies every UI color. Built-in colors may repair syntax roles only.
//! Search can record an unmet preference, but it cannot downgrade a failed validation.

use self::tokens::{
    ContentTokens, DerivedTokens, InteractionTokens, OpaqueColor, OverlayColor, PaintColor,
    RoleColor, StatusChannel, StatusTokens, SurfaceTokens, ThemeTokens,
};
use crate::color::{
    apply_opacity, contrast_ratio, delta_e, gamut_map_oklch, gpui_blend, lab, lightness,
    oklab_to_oklch, parse_hex, render_layers, tone, with_alpha,
};
use crate::constants::*;
use crate::palette::ResolvedPalette;
use crate::saliency::{
    HOVER_LINE_NUMBER_SALIENCY, INACTIVE_LINE_NUMBER_SALIENCY, PRIMARY_SALIENCY, SaliencyRequest,
    fit_relative,
};
use crate::search::{
    FitBounds, OverlayFitRequest, OverlayPairRequest, PairConstraints, Search, cvd_greedy_order,
    round6,
};
use crate::syntax::{build_syntax, contrast_floor};
use crate::{Error, Result};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet, btree_map::Entry};

mod tokens;

#[derive(Clone, Debug)]
pub struct Audit {
    pub mode: String,
    pub extras: Vec<String>,
    pub surface_changes: Vec<Value>,
    pub repairs: Vec<Value>,
    pub degradations: Vec<Value>,
    pub minimums: BTreeMap<String, f64>,
    pub warnings: Vec<String>,
    pub saliency: Vec<Value>,
    pub syntax_analysis: Value,
    pub syntax_roles: Vec<Value>,
    pub syntax_collapses: Vec<Value>,
    pub diff_metrics: Vec<Value>,
    pub interaction_ladders: Vec<Value>,
    pub fidelity_deviations: Vec<Value>,
}

impl Audit {
    fn new(palette: &ResolvedPalette) -> Self {
        Self {
            mode: palette.mode.clone(),
            extras: palette.extras.keys().cloned().collect(),
            surface_changes: Vec::new(),
            repairs: Vec::new(),
            degradations: Vec::new(),
            minimums: BTreeMap::new(),
            warnings: (!palette.resolver_stderr.is_empty())
                .then(|| palette.resolver_stderr.clone())
                .into_iter()
                .collect(),
            saliency: Vec::new(),
            syntax_analysis: Value::Null,
            syntax_roles: Vec::new(),
            syntax_collapses: Vec::new(),
            diff_metrics: Vec::new(),
            interaction_ladders: Vec::new(),
            fidelity_deviations: Vec::new(),
        }
    }

    pub fn degradation(&mut self, role: String, invariant: &str, detail: Value) {
        let mut record = Map::new();
        record.insert("role".into(), role.into());
        record.insert("invariant".into(), invariant.into());
        if let Value::Object(detail) = detail {
            record.extend(detail);
        }
        self.degradations.push(Value::Object(record));
    }

    fn repair(&mut self, role: &str, source: &str, output: &str) -> Result<()> {
        if source != output {
            self.repairs.push(json!({"role": role, "source": source, "output": output, "delta_e": round6(delta_e(source, output)?)}));
        }
        Ok(())
    }

    pub fn detail(&self) -> Value {
        json!({
            "mode": self.mode, "extras": self.extras, "surface_changes": self.surface_changes,
            "repairs": self.repairs, "degradations": self.degradations,
            "minimums": self.minimums, "warnings": self.warnings,
            "saliency": self.saliency,
            "syntax_analysis": self.syntax_analysis, "syntax_roles": self.syntax_roles,
            "syntax_collapses": self.syntax_collapses, "diff_metrics": self.diff_metrics,
            "interaction_ladders": self.interaction_ladders, "fidelity_deviations": self.fidelity_deviations,
        })
    }
}

fn color<'a>(palette: &'a ResolvedPalette, key: &str) -> &'a str {
    &palette.colors[key]
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

fn fit_highlight_with_alpha_fallback(
    search: &mut Search,
    audit: &mut Audit,
    role: &str,
    seed: &str,
    request: OverlayFitRequest<'_>,
) -> Result<String> {
    let preferred_cap_error =
        match search.fit_readable_overlay_bounded(seed, request, PREFERRED_HIGHLIGHT_MAX_ALPHA) {
            Ok(output) => return Ok(output),
            Err(error) => error,
        };
    let output = search
        .fit_readable_overlay_bounded(seed, request, OVERLAY_MAX_ALPHA)
        .map_err(|fallback_error| {
            Error(format!(
                "{role}: preferred alpha cap failed: {preferred_cap_error}; relaxed alpha cap failed: {fallback_error}"
            ))
        })?;

    audit.degradation(
        role.into(),
        "highlight_alpha_cap_relaxed",
        json!({
            "preferred_maximum_alpha": PREFERRED_HIGHLIGHT_MAX_ALPHA,
            "fallback_maximum_alpha": OVERLAY_MAX_ALPHA,
            "actual_alpha": round6(parse_hex(&output)?.a),
            "reason": preferred_cap_error.to_string(),
        }),
    );
    Ok(output)
}

#[derive(Default)]
struct StyleBuilder(BTreeMap<String, String>);

impl StyleBuilder {
    fn insert(&mut self, role: RoleColor) -> Result<()> {
        let (name, value) = role.into_parts();
        match self.0.entry(name) {
            Entry::Vacant(entry) => {
                entry.insert(value);
                Ok(())
            }
            Entry::Occupied(entry) => Err(Error(format!(
                "theme role {} was generated twice",
                entry.key()
            ))),
        }
    }

    fn insert_opaque(&mut self, role: impl Into<String>, color: String) -> Result<()> {
        self.insert(RoleColor::opaque_value(role, color)?)
    }

    fn insert_overlay(&mut self, role: impl Into<String>, color: String) -> Result<()> {
        self.insert(RoleColor::overlay_value(role, color)?)
    }

    fn extend(&mut self, roles: impl IntoIterator<Item = RoleColor>) -> Result<()> {
        for role in roles {
            self.insert(role)?;
        }
        Ok(())
    }

    fn extend_opaque(&mut self, roles: impl IntoIterator<Item = (String, String)>) -> Result<()> {
        for (role, color) in roles {
            self.insert_opaque(role, color)?;
        }
        Ok(())
    }

    fn append_to(self, style: &mut Map<String, Value>) -> Result<()> {
        for (role, color) in self.0 {
            if style.contains_key(&role) {
                return Err(Error(format!("theme role {role} was generated twice")));
            }
            style.insert(role, color.into());
        }
        Ok(())
    }
}

fn derive_surfaces(
    palette: &ResolvedPalette,
    audit: &mut Audit,
) -> Result<BTreeMap<String, String>> {
    let canvas = color(palette, "background");
    let canvas_lightness = lightness(canvas)?;
    let offsets = if palette.mode == "dark" {
        [
            ("sunken", -0.045),
            ("chrome", -0.025),
            ("surface", 0.018),
            ("raised", 0.035),
            ("elevated", 0.055),
        ]
    } else {
        [
            ("sunken", -0.070),
            ("chrome", -0.045),
            ("surface", -0.025),
            ("raised", 0.010),
            ("elevated", 0.020),
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

        let (source_key, source, source_lightness, output) =
            if let Some((_, key, value, value_lightness)) = eligible.first() {
                (*key, *value, *value_lightness, (*value).to_owned())
            } else {
                let Some((key, source, source_lightness)) =
                    authored.iter().min_by(|left, right| {
                        (left.2 - target)
                            .abs()
                            .total_cmp(&(right.2 - target).abs())
                            .then(left.0.cmp(right.0))
                    })
                else {
                    return Err(Error("no authored surface colors are available".into()));
                };
                let mut output = tone(source, target, 1.0)?;
                if lightness(&output)? <= previous + 1e-6 {
                    output = tone(source, (previous + 0.004).min(1.0), 1.0)?;
                }
                (*key, *source, *source_lightness, output)
            };

        used.insert(output.clone());
        previous = lightness(&output)?;

        audit.surface_changes.push(json!({
            "role": role, "source_key": source_key, "source": source, "output": output,
            "delta_l": round6(lightness(&output)? - source_lightness),
            "delta_e": round6(delta_e(&output, source)?),
        }));

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

fn minimum_pairwise(
    first: &[String],
    second: &[String],
    metric: impl Fn(&str, &str) -> Result<f64>,
) -> Result<f64> {
    if first.len() != second.len() {
        return Err(Error(format!(
            "paired color contexts have different lengths: {} and {}",
            first.len(),
            second.len()
        )));
    }
    first
        .iter()
        .zip(second)
        .try_fold(f64::INFINITY, |minimum, (left, right)| {
            Ok(minimum.min(metric(left, right)?))
        })
}

fn fit_player_cursors(
    search: &mut Search,
    audit: &mut Audit,
    seeds: &[String],
    backgrounds: &[String],
) -> Result<Vec<String>> {
    match search.fit_distinct_colors(
        seeds,
        backgrounds,
        TERMINAL_BRIGHT_PREFERRED,
        audit,
        "players.cursor",
    ) {
        Ok(values) => Ok(values),
        Err(_) => {
            audit.degradation(
                "players.cursor".into(),
                "preferred_cursor_contrast",
                json!({"preferred": TERMINAL_BRIGHT_PREFERRED, "hard_floor": TEXT_CONTRAST}),
            );
            search.fit_distinct_colors(seeds, backgrounds, TEXT_CONTRAST, audit, "players.cursor")
        }
    }
}

struct TerminalRequest<'a> {
    seeds: [&'a str; 3],
    backgrounds: &'a [String],
    mode: &'a str,
    role: &'a str,
}

fn terminal_triplet(
    search: &mut Search,
    audit: &mut Audit,
    request: TerminalRequest<'_>,
) -> Result<[String; 3]> {
    let [dim_seed, normal_seed, bright_seed] = request.seeds;
    let backgrounds = request.backgrounds;
    let role = request.role;
    let endpoint = if request.mode == "dark" {
        "#ffffff"
    } else {
        "#000000"
    };
    let preferred = |search: &mut Search,
                     seed: &str,
                     target: f64,
                     variant: &str,
                     lower: f64,
                     upper: f64,
                     prefer_background: bool,
                     audit: &mut Audit|
     -> Result<String> {
        if lightness(endpoint)? >= lower - 1e-12
            && lightness(endpoint)? <= upper + 1e-12
            && minimum_contrast(endpoint, backgrounds)? >= target - 1e-12
        {
            search.fit_color_bounded(
                seed,
                backgrounds,
                target,
                &[],
                FitBounds {
                    lower_lightness: lower,
                    upper_lightness: upper,
                    prefer_background,
                    ..FitBounds::default()
                },
            )
        } else {
            let candidate = search.fit_color_bounded(
                seed,
                backgrounds,
                TEXT_CONTRAST,
                &[],
                FitBounds {
                    lower_lightness: lower,
                    upper_lightness: upper,
                    prefer_background,
                    ..FitBounds::default()
                },
            )?;
            audit.degradation(format!("{role}.{variant}"), "terminal_preferred_contrast", json!({
                "preferred": target, "hard_floor": TEXT_CONTRAST, "actual": round6(minimum_contrast(&candidate, backgrounds)?),
            }));
            Ok(candidate)
        }
    };
    let normal = preferred(
        search,
        normal_seed,
        TERMINAL_NORMAL_PREFERRED,
        "normal",
        0.0,
        1.0,
        false,
        audit,
    )?;
    let normal_l = lightness(&normal)?;
    let (dim_lower, dim_upper, bright_lower, bright_upper) = if request.mode == "dark" {
        (0.0, normal_l, normal_l, 1.0)
    } else {
        (normal_l, 1.0, 0.0, normal_l)
    };
    let dim = match search.fit_color_bounded(
        dim_seed,
        backgrounds,
        TEXT_CONTRAST,
        &[],
        FitBounds {
            lower_lightness: dim_lower,
            upper_lightness: dim_upper,
            prefer_background: true,
            ..FitBounds::default()
        },
    ) {
        Ok(value) => value,
        Err(_) => {
            audit.degradation(format!("{role}.dim"), "terminal_dim_hue_preserving_space", json!({
                "hard_floor": TEXT_CONTRAST, "actual": round6(minimum_contrast(&normal, backgrounds)?),
            }));
            normal.clone()
        }
    };
    let bright = preferred(
        search,
        bright_seed,
        TERMINAL_BRIGHT_PREFERRED,
        "bright",
        bright_lower,
        bright_upper,
        false,
        audit,
    )?;
    for (variant, candidate) in [("dim", &dim), ("bright", &bright)] {
        if candidate == &normal {
            audit.degradation(
                format!("{role}.{variant}"),
                "terminal_variant_distinctness",
                json!({"actual": candidate}),
            );
        }
    }
    Ok([dim, normal, bright])
}

struct SemanticColors {
    primary: String,
    secondary: String,
    disabled: String,
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
    Ok(gamut_map_oklch(lightness, source_chroma.clamp(0.080, 0.180), hue).opaque_hex())
}

fn derive_semantics(
    search: &mut Search,
    palette: &ResolvedPalette,
    text_backgrounds: &[String],
    semantic_backgrounds: &[String],
    audit: &mut Audit,
) -> Result<SemanticColors> {
    let primary = search.fit_color(
        color(palette, "foreground"),
        text_backgrounds,
        TEXT_CONTRAST,
    )?;
    let secondary = search.fit_color(color(palette, "muted"), text_backgrounds, TEXT_CONTRAST)?;
    let disabled = search.fit_color(
        color(palette, "dark_foreground"),
        text_backgrounds,
        TEXT_CONTRAST,
    )?;
    let accent = search.fit_color(color(palette, "accent"), text_backgrounds, CONTROL_CONTRAST)?;
    let structural =
        search.fit_color(color(palette, "muted"), text_backgrounds, CONTROL_CONTRAST)?;
    let passive = search.fit_color(color(palette, "muted"), text_backgrounds, PASSIVE_CONTRAST)?;
    for (role, source, output) in [
        ("text", color(palette, "foreground"), &primary),
        ("text.muted", color(palette, "muted"), &secondary),
        (
            "text.disabled",
            color(palette, "dark_foreground"),
            &disabled,
        ),
        ("accent", color(palette, "accent"), &accent),
        ("structural", color(palette, "muted"), &structural),
    ] {
        audit.repair(role, source, output)?;
    }
    let [green, red] = search
        .fit_pair(
            color(palette, "green"),
            color(palette, "red"),
            semantic_backgrounds,
            PairConstraints::from_contract(TEXT_CONTRAST, SEMANTIC_PAIR_CONTRACT),
        )
        .map_err(|error| Error(format!("semantic add/delete foregrounds: {error}")))?;
    let blue = search.fit_color(color(palette, "blue"), semantic_backgrounds, TEXT_CONTRAST)?;
    let yellow = match search.fit_color_avoiding(
        color(palette, "yellow"),
        semantic_backgrounds,
        TEXT_CONTRAST,
        std::slice::from_ref(&blue),
    ) {
        Ok(yellow) => yellow,
        Err(error) => {
            let yellow = search.fit_color(
                color(palette, "yellow"),
                semantic_backgrounds,
                TEXT_CONTRAST,
            )?;
            audit.degradation(
                "semantic.yellow".into(),
                "blue_yellow_separation",
                json!({"actual": yellow, "reason": error.to_string()}),
            );
            yellow
        }
    };
    Ok(SemanticColors {
        primary,
        secondary,
        disabled,
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

pub fn build_theme(palette: &ResolvedPalette) -> Result<(Value, Audit)> {
    let mut search = Search::default();
    search.prewarm(palette.colors.values().map(String::as_str))?;
    let mut audit = Audit::new(palette);
    let surfaces = derive_surfaces(palette, &mut audit)?;
    let canvas = surfaces["canvas"].clone();
    let surface = surfaces["surface"].clone();
    let elevated = surfaces["elevated"].clone();
    let chrome = surfaces["chrome"].clone();
    let sunken = surfaces["sunken"].clone();
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
        sunken.clone(),
    ]);
    let provisional_ui_text = search
        .fit_color(
            color(palette, "foreground"),
            &base_ui_backgrounds,
            UI_STATE_TEXT_CONTRAST,
        )
        .map_err(|error| Error(format!("provisional UI text: {error}")))?;
    let readable_ui_state = [(
        provisional_ui_text,
        TEXT_CONTRAST,
        STATE_CONSECUTIVE_DELTA_E,
    )];

    let fitted_tab_inactive = search
        .fit_state(
            color(palette, "background"),
            std::slice::from_ref(&chrome),
            TAB_STATE_CONTRAST,
            STATE_CONSECUTIVE_DELTA_E,
            &readable_ui_state,
        )
        .map_err(|error| Error(format!("inactive tab: {error}")))?;
    let tab_inactive_reuses_chrome_context = contrast_ratio(&fitted_tab_inactive, &canvas)?
        >= STATE_HOVER_CONTRAST - 1e-12
        && delta_e(&fitted_tab_inactive, &canvas)? >= STATE_HOVER_DELTA_E - 1e-12;
    let tab_inactive = if tab_inactive_reuses_chrome_context {
        fitted_tab_inactive
    } else {
        search.fit_state(
            &canvas,
            std::slice::from_ref(&canvas),
            STATE_HOVER_CONTRAST,
            STATE_HOVER_DELTA_E,
            &readable_ui_state,
        )?
    };

    let panel_overlay = search
        .fit_state(
            &elevated,
            std::slice::from_ref(&surface),
            TAB_STATE_CONTRAST,
            STATE_CONSECUTIVE_DELTA_E,
            &readable_ui_state,
        )
        .map_err(|error| Error(format!("panel overlay: {error}")))?;
    let panel_overlay_hover = search
        .fit_state(
            &panel_overlay,
            std::slice::from_ref(&panel_overlay),
            STATE_HOVER_CONTRAST,
            STATE_HOVER_DELTA_E,
            &readable_ui_state,
        )
        .map_err(|error| Error(format!("panel overlay hover: {error}")))?;

    let interaction_bases = unique(
        base_ui_backgrounds
            .iter()
            .cloned()
            .chain([panel_overlay.clone(), canvas.clone()]),
    );
    let interaction_ui_text = search.fit_color(
        color(palette, "foreground"),
        &interaction_bases,
        UI_STATE_TEXT_CONTRAST,
    )?;
    let readable_interaction_foreground = [(interaction_ui_text, TEXT_CONTRAST)];
    let element_hover = search
        .fit_readable_overlay(
            &surface,
            OverlayFitRequest::new(
                &interaction_bases,
                LAYER_HOVER_CONTRAST,
                STATE_HOVER_DELTA_E,
            )
            .with_runtime_state((0.6, STATE_HOVER_CONTRAST, STATE_HOVER_DELTA_E))
            .with_readable_foregrounds(&readable_interaction_foreground),
        )
        .map_err(|error| Error(format!("element hover: {error}")))?;
    let element_active = search
        .fit_readable_overlay(
            &surface,
            OverlayFitRequest::new(
                &interaction_bases,
                LAYER_ACTIVE_CONTRAST,
                STATE_ACTIVE_DELTA_E,
            )
            .with_runtime_state((0.5, STATE_HOVER_CONTRAST, STATE_HOVER_DELTA_E))
            .with_readable_foregrounds(&readable_interaction_foreground)
            .with_rendered_references(&[(
                element_hover.clone(),
                STATE_CONSECUTIVE_CONTRAST,
                STATE_CONSECUTIVE_DELTA_E,
            )])
            .with_runtime_rendered_references(&[(
                apply_opacity(&element_hover, 0.6)?,
                RUNTIME_STATE_CONSECUTIVE_CONTRAST,
                RUNTIME_STATE_CONSECUTIVE_DELTA_E,
                RUNTIME_STATE_BASE_CONTRAST_STEP,
            )]),
        )
        .map_err(|error| Error(format!("element active: {error}")))?;
    let ghost_hover = search
        .fit_readable_overlay(
            &canvas,
            OverlayFitRequest::new(
                &interaction_bases,
                LAYER_HOVER_CONTRAST,
                STATE_HOVER_DELTA_E,
            )
            .with_readable_foregrounds(&readable_interaction_foreground),
        )
        .map_err(|error| Error(format!("ghost hover: {error}")))?;
    let ghost_active = search
        .fit_readable_overlay(
            &canvas,
            OverlayFitRequest::new(
                &interaction_bases,
                LAYER_ACTIVE_CONTRAST,
                STATE_ACTIVE_DELTA_E,
            )
            .with_readable_foregrounds(&readable_interaction_foreground)
            .with_rendered_references(&[(
                ghost_hover.clone(),
                STATE_CONSECUTIVE_CONTRAST,
                STATE_CONSECUTIVE_DELTA_E,
            )]),
        )
        .map_err(|error| Error(format!("ghost active: {error}")))?;

    let panel_guide_ladder = search
        .fit_state_ladder(
            color(palette, "accent"),
            std::slice::from_ref(&surface),
            &[
                (PASSIVE_CONTRAST, 0.025),
                (CONTROL_CONTRAST, 0.040),
                (THUMB_HOVER_CONTRAST, 0.065),
            ],
            &[],
        )
        .map_err(|error| Error(format!("panel guide ladder: {error}")))?;

    let mut rendered_ui_state_backgrounds = Vec::new();
    for base in &interaction_bases {
        for layer in [&element_hover, &element_active, &ghost_hover, &ghost_active] {
            rendered_ui_state_backgrounds.push(gpui_blend(base, layer)?.opaque_hex());
        }
        rendered_ui_state_backgrounds
            .push(gpui_blend(base, &apply_opacity(&element_hover, 0.6)?)?.opaque_hex());
        rendered_ui_state_backgrounds
            .push(gpui_blend(base, &apply_opacity(&element_active, 0.5)?)?.opaque_hex());
    }
    let ui_backgrounds = unique(
        interaction_bases
            .iter()
            .cloned()
            .chain([panel_overlay_hover.clone()])
            .chain(rendered_ui_state_backgrounds),
    );

    // A canvas-fitted fallback tab is an isolated state surface, not a base for
    // semantic fills. Its text readability is still validated after emission.
    let semantic_backgrounds = unique(
        interaction_bases
            .iter()
            .cloned()
            .chain(tab_inactive_reuses_chrome_context.then(|| tab_inactive.clone())),
    );

    let semantic = derive_semantics(
        &mut search,
        palette,
        &ui_backgrounds,
        &semantic_backgrounds,
        &mut audit,
    )?;

    let content_accent = search.fit_color(&semantic.accent, &ui_backgrounds, TEXT_CONTRAST)?;
    let element_selection = search
        .fit_readable_overlay(
            color(palette, "selection"),
            OverlayFitRequest::new(
                &interaction_bases,
                FOCUSED_SELECTION_CONTRAST,
                FOCUSED_SELECTION_DELTA_E,
            )
            .with_runtime_state((0.5, 1.08, 0.020))
            .with_readable_foregrounds(&[(semantic.primary.clone(), TEXT_CONTRAST)]),
        )
        .map_err(|error| Error(format!("UI selection: {error}")))?;

    let editor_active_line = search
        .fit_readable_overlay(
            &canvas,
            OverlayFitRequest::new(
                std::slice::from_ref(&canvas),
                STATE_HOVER_CONTRAST,
                STATE_HOVER_DELTA_E,
            )
            .with_readable_foregrounds(&[(
                provisional_editor_text.clone(),
                EDITOR_BASE_TEXT_CONTRAST,
            )]),
        )
        .map_err(|error| Error(format!("active editor line: {error}")))?;

    if parse_hex(&editor_active_line)?.opaque_hex() != surface {
        audit.fidelity_deviations.push(json!({
            "role": "editor.active_line.background",
            "requested_relation": "surface and active line share RGB",
            "source": surface,
            "output": parse_hex(&editor_active_line)?.opaque_hex(),
            "reason": "shared RGB is subordinate to readability and state visibility",
        }));
    }

    let rendered_editor_active_line = gpui_blend(&canvas, &editor_active_line)?.opaque_hex();
    let editor_highlighted_line = search
        .fit_readable_overlay(
            &surface,
            OverlayFitRequest::new(
                std::slice::from_ref(&canvas),
                STATE_ACTIVE_CONTRAST,
                STATE_ACTIVE_DELTA_E,
            )
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
        .map_err(|error| Error(format!("highlighted editor line: {error}")))?;
    let rendered_editor_highlighted_line =
        gpui_blend(&canvas, &editor_highlighted_line)?.opaque_hex();
    let debugger_active = search
        .fit_readable_overlay(
            &semantic.red,
            OverlayFitRequest::new(
                std::slice::from_ref(&canvas),
                STATE_SELECTED_CONTRAST,
                STATE_SELECTED_DELTA_E,
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
        .map_err(|error| Error(format!("debugger active line: {error}")))?;
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
        ),
    )?;
    let editor_hover_line_number = fit_relative(
        &mut search,
        color(palette, "muted"),
        &editor_primary,
        SaliencyRequest::new(&editor_bases, CONTROL_CONTRAST, HOVER_LINE_NUMBER_SALIENCY),
    )?;
    let editor_active_line_number = fit_relative(
        &mut search,
        &editor_primary,
        &editor_primary,
        SaliencyRequest::new(&editor_bases, TEXT_CONTRAST, PRIMARY_SALIENCY),
    )?;
    audit.saliency.extend([
        editor_line_number.audit(
            "editor.line_number",
            PASSIVE_CONTRAST,
            "911-theme median from tmp/zed-saliency-policy-evaluation.json",
        ),
        editor_hover_line_number.audit(
            "editor.hover_line_number",
            CONTROL_CONTRAST,
            "deterministic midpoint between inactive and primary",
        ),
        editor_active_line_number.audit(
            "editor.active_line_number",
            TEXT_CONTRAST,
            "911-theme active median",
        ),
    ]);

    let readable_editor_overlay_text = [(editor_primary.clone(), EDITOR_OVERLAY_TEXT_CONTRAST)];
    let search_match_request =
        OverlayFitRequest::new(&editor_bases, SEARCH_MATCH_CONTRAST, STATE_HOVER_DELTA_E)
            .with_readable_foregrounds(&readable_editor_overlay_text);
    let initial_search_match = fit_highlight_with_alpha_fallback(
        &mut search,
        &mut audit,
        "search.match_background",
        &semantic.yellow,
        search_match_request,
    )?;
    let search_active_request = OverlayFitRequest::new(
        &editor_bases,
        SEARCH_ACTIVE_CONTRAST,
        STATE_SELECTED_DELTA_E,
    )
    .with_readable_foregrounds(&readable_editor_overlay_text);
    let (search_match, search_active) = match fit_highlight_with_alpha_fallback(
        &mut search,
        &mut audit,
        "search.active_match_background",
        &semantic.accent,
        search_active_request.with_rendered_references(&[(
            initial_search_match.clone(),
            STATE_CONSECUTIVE_CONTRAST,
            STATE_CONSECUTIVE_DELTA_E,
        )]),
    ) {
        Ok(search_active) => (initial_search_match, search_active),
        Err(sequential_error) => {
            let [joint_search_match, joint_search_active] = search
                .fit_overlay_pair(
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
                    .with_limits(PREFERRED_HIGHLIGHT_MAX_ALPHA, 512),
                )
                .map_err(|joint_error| {
                    Error(format!(
                        "search highlights failed sequentially ({sequential_error}) and jointly ({joint_error})"
                    ))
                })?;
            audit.degradation(
                "search.active_match_background".into(),
                "joint_state_fit",
                json!({
                    "initial_match": initial_search_match,
                    "joint_match": joint_search_match,
                    "reason": sequential_error.to_string(),
                }),
            );
            (joint_search_match, joint_search_active)
        }
    };

    let document_read = fit_highlight_with_alpha_fallback(
        &mut search,
        &mut audit,
        "editor.document_highlight.read_background",
        &semantic.accent,
        OverlayFitRequest::new(
            &editor_bases,
            STATE_SELECTED_CONTRAST,
            STATE_SELECTED_DELTA_E,
        )
        .with_readable_foregrounds(&readable_editor_overlay_text),
    )?;

    if parse_hex(&document_read)?.opaque_hex() != content_accent {
        audit.fidelity_deviations.push(json!({
            "role": "editor.document_highlight.read_background",
            "requested_relation": "content accent and document read share RGB",
            "source": content_accent,
            "output": parse_hex(&document_read)?.opaque_hex(),
            "reason": "shared RGB is subordinate to readability and state visibility",
        }));
    }

    let document_write = fit_highlight_with_alpha_fallback(
        &mut search,
        &mut audit,
        "editor.document_highlight.write_background",
        &semantic.orange,
        OverlayFitRequest::new(
            &editor_bases,
            STATE_SELECTED_CONTRAST,
            STATE_SELECTED_DELTA_E,
        )
        .with_readable_foregrounds(&readable_editor_overlay_text),
    )?;
    let document_bracket = fit_highlight_with_alpha_fallback(
        &mut search,
        &mut audit,
        "editor.document_highlight.bracket_background",
        &semantic.cyan,
        OverlayFitRequest::new(
            &editor_bases,
            STATE_SELECTED_CONTRAST,
            STATE_SELECTED_DELTA_E,
        )
        .with_readable_foregrounds(&readable_editor_overlay_text),
    )?;

    // Diff colors are derived as a dedicated semantic subsystem because diff viewers
    // combine text, fills, hollow borders, selections, and conflict overlays.
    let diff_green_seed = conventional_semantic_seed(palette, "green", 145.0)?;
    let diff_red_seed = conventional_semantic_seed(palette, "red", 25.0)?;
    let diff_yellow_seed = conventional_semantic_seed(palette, "yellow", 85.0)?;
    let [version_control_added, version_control_deleted] = search
        .fit_pair(
            &diff_green_seed,
            &diff_red_seed,
            &interaction_bases,
            PairConstraints::from_contract(TEXT_CONTRAST, SEMANTIC_PAIR_CONTRACT)
                .with_minimum_chroma(0.025),
        )
        .map_err(|error| Error(format!("version-control add/delete foregrounds: {error}")))?;
    let version_control_modified = search.fit_color_bounded(
        &diff_yellow_seed,
        &interaction_bases,
        TEXT_CONTRAST,
        &[],
        FitBounds {
            lower_chroma: 0.025,
            ..FitBounds::default()
        },
    )?;

    let diff_constraints = PairConstraints::new(
        DIFF_FILL_CONTRAST,
        DIFF_PAIR_CONTRAST,
        DIFF_NORMAL_FLOOR_DELTA_E,
        DIFF_CVD_FLOOR_DELTA_E,
    )
    .with_minimum_chroma(0.025)
    .with_separation_alternative(Some((
        DIFF_LUMINANCE_SEPARATION_CONTRAST,
        DIFF_NORMAL_DELTA_E,
        DIFF_CVD_DELTA_E,
    )))
    .prefer_background();

    let readable_diff_text = [(editor_primary.clone(), EDITOR_OVERLAY_TEXT_CONTRAST)];
    let diff_fill_request = |backgrounds| {
        OverlayFitRequest::new(backgrounds, DIFF_FILL_CONTRAST, DIFF_NORMAL_FLOOR_DELTA_E)
            .with_readable_foregrounds(&readable_diff_text)
    };
    let [constraint_diff_added, constraint_diff_deleted] = search
        .fit_overlay_pair(
            &diff_green_seed,
            &diff_red_seed,
            OverlayPairRequest::new(
                diff_fill_request(&editor_bases),
                diff_fill_request(&editor_bases),
                diff_constraints,
            ),
        )
        .map_err(|error| Error(format!("solid diff hunks: {error}")))?;
    let [constraint_diff_added_hollow, constraint_diff_deleted_hollow] = search
        .fit_overlay_pair(
            &diff_green_seed,
            &diff_red_seed,
            OverlayPairRequest::new(
                diff_fill_request(&editor_bases).with_target(DIFF_HOLLOW_CONTRAST),
                diff_fill_request(&editor_bases).with_target(DIFF_HOLLOW_CONTRAST),
                diff_constraints.with_foreground_contrast(DIFF_HOLLOW_CONTRAST),
            ),
        )
        .map_err(|error| Error(format!("hollow diff hunks: {error}")))?;

    let constraint_added_hunk_scenes = render_on_bases(&editor_bases, &[&constraint_diff_added])?;
    let constraint_deleted_hunk_scenes =
        render_on_bases(&editor_bases, &[&constraint_diff_deleted])?;
    let constraint_added_hollow_scenes =
        render_on_bases(&editor_bases, &[&constraint_diff_added_hollow])?;
    let constraint_deleted_hollow_scenes =
        render_on_bases(&editor_bases, &[&constraint_diff_deleted_hollow])?;
    let [conflict_ours, conflict_theirs] = search
        .fit_overlay_pair(
            &diff_green_seed,
            color(palette, "blue"),
            OverlayPairRequest::new(
                diff_fill_request(&editor_bases),
                diff_fill_request(&editor_bases),
                diff_constraints
                    .with_foreground_contrast(DIFF_FILL_CONTRAST)
                    .with_minimum_chroma(0.0),
            ),
        )
        .map_err(|error| Error(format!("conflict backgrounds: {error}")))?;

    let (hunk_filled_opacity, hunk_hollow_opacity, hunk_border_opacity) = if palette.mode == "light"
    {
        (
            LIGHT_DIFF_HUNK_FILLED_OPACITY,
            LIGHT_DIFF_HUNK_HOLLOW_BACKGROUND_OPACITY,
            LIGHT_DIFF_HUNK_HOLLOW_BORDER_OPACITY,
        )
    } else {
        (
            DARK_DIFF_HUNK_FILLED_OPACITY,
            DARK_DIFF_HUNK_HOLLOW_BACKGROUND_OPACITY,
            DARK_DIFF_HUNK_HOLLOW_BORDER_OPACITY,
        )
    };
    let diff_added = apply_opacity(&version_control_added, hunk_filled_opacity)?;
    let diff_deleted = apply_opacity(&version_control_deleted, hunk_filled_opacity)?;
    let diff_added_hollow = apply_opacity(&version_control_added, hunk_hollow_opacity)?;
    let diff_deleted_hollow = apply_opacity(&version_control_deleted, hunk_hollow_opacity)?;
    let diff_added_hollow_border = apply_opacity(&version_control_added, hunk_border_opacity)?;
    let diff_deleted_hollow_border = apply_opacity(&version_control_deleted, hunk_border_opacity)?;

    let yank = fit_highlight_with_alpha_fallback(
        &mut search,
        &mut audit,
        "vim.yank.background",
        &semantic.yellow,
        OverlayFitRequest::new(
            &editor_bases,
            STATE_SELECTED_CONTRAST,
            STATE_SELECTED_DELTA_E,
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
    audit.degradation(
        "editor.generic_highlight_stack".into(),
        "bounded_overlap_depth",
        json!({
            "validated_generic_highlight_depth": 1,
            "reason": "Zed permits multiple unordered highlights, but deeper stacks cannot preserve every palette's text contract"
        }),
    );

    let constraint_word_added_bases = constraint_added_hunk_scenes
        .iter()
        .cloned()
        .chain(constraint_added_hollow_scenes.iter().cloned())
        .collect::<Vec<_>>();
    let constraint_word_deleted_bases = constraint_deleted_hunk_scenes
        .iter()
        .cloned()
        .chain(constraint_deleted_hollow_scenes.iter().cloned())
        .collect::<Vec<_>>();
    let constraint_word_added_underlays =
        render_with_bounded_generic_highlights(&constraint_word_added_bases, &generic_highlights)?;
    let constraint_word_deleted_underlays = render_with_bounded_generic_highlights(
        &constraint_word_deleted_bases,
        &generic_highlights,
    )?;
    let constraint_readable_word_text = [(editor_primary.clone(), WORD_TEXT_CONTRAST)];
    let constraint_word_request = |backgrounds| {
        OverlayFitRequest::new(backgrounds, WORD_DIFF_CONTRAST, STATE_HOVER_DELTA_E)
            .with_readable_foregrounds(&constraint_readable_word_text)
    };
    let constraint_word_pair = search
        .fit_overlay_pair_with_fallback(
            &diff_green_seed,
            &diff_red_seed,
            OverlayPairRequest::new(
                constraint_word_request(&constraint_word_added_underlays),
                constraint_word_request(&constraint_word_deleted_underlays),
                diff_constraints
                    .with_foreground_contrast(WORD_DIFF_CONTRAST)
                    .with_minimum_chroma(0.0),
            )
            .with_limits(WORD_OVERLAY_MAX_ALPHA, 128),
            diff_constraints
                .with_foreground_contrast(WORD_DIFF_CONTRAST)
                .with_minimum_chroma(0.0)
                .with_separation_alternative(None),
            512,
        )
        .map_err(|error| Error(format!("word diff backgrounds: {error}")))?;

    let [constraint_word_added, constraint_word_deleted] = constraint_word_pair.colors;
    let word_added = apply_opacity(&version_control_added, hunk_filled_opacity)?;
    let word_deleted = apply_opacity(&version_control_deleted, hunk_filled_opacity)?;
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

    let constraint_word_added_scenes =
        render_on_bases(&constraint_word_added_underlays, &[&constraint_word_added])?;
    let constraint_word_deleted_scenes = render_on_bases(
        &constraint_word_deleted_underlays,
        &[&constraint_word_deleted],
    )?;
    // Downstream fitting keeps the former conservative diff scenes so the quieter
    // emitted treatment cannot retune unrelated syntax, status, or player roles.
    let base_overlay_backgrounds = unique(
        editor_bases
            .iter()
            .cloned()
            .chain(constraint_added_hunk_scenes.iter().cloned())
            .chain(constraint_deleted_hunk_scenes.iter().cloned())
            .chain(constraint_added_hollow_scenes.iter().cloned())
            .chain(constraint_deleted_hollow_scenes.iter().cloned())
            .chain(rendered_editor_overlay_contexts.iter().cloned()),
    );
    let editor_text_backgrounds = unique(
        base_overlay_backgrounds
            .iter()
            .cloned()
            .chain(constraint_word_added_scenes.iter().cloned())
            .chain(constraint_word_deleted_scenes.iter().cloned()),
    );

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

    let selection_readable = [(editor_primary.clone(), TEXT_CONTRAST)];
    let selection_request = || {
        OverlayFitRequest::new(
            &base_overlay_backgrounds,
            FOCUSED_SELECTION_CONTRAST,
            FOCUSED_SELECTION_DELTA_E,
        )
        .with_runtime_state((0.5, 1.08, 0.020))
        .with_readable_foregrounds(&selection_readable)
    };
    let selection = match search.fit_readable_overlay_alpha_range(
        color(palette, "selection"),
        selection_request(),
        u8::MAX,
        u8::MAX,
    ) {
        Ok(selection) => selection,
        Err(source_error) => {
            match search.fit_readable_overlay_alpha_range(
                &semantic.accent,
                selection_request(),
                u8::MAX,
                u8::MAX,
            ) {
                Ok(selection) => {
                    audit.degradation(
                        "players[0].selection".into(),
                        "selection_seed_substitution",
                        json!({
                            "requested": color(palette, "selection"),
                            "fallback_seed": semantic.accent,
                            "reason": source_error.to_string(),
                        }),
                    );
                    selection
                }
                Err(accent_error) => {
                    let mut fallback_errors = Vec::new();
                    let mut fallback_candidate = None;
                    for seed in player_seeds.iter().skip(1) {
                        match search.fit_readable_overlay_alpha_range(
                            seed,
                            selection_request(),
                            u8::MAX,
                            u8::MAX,
                        ) {
                            Ok(selection) => {
                                fallback_candidate = Some((seed, selection));
                                break;
                            }
                            Err(error) => fallback_errors.push(format!("{seed}: {error}")),
                        }
                    }
                    let (fallback_seed, selection) = fallback_candidate.ok_or_else(|| {
                        Error(format!(
                            "focused selection failed for source ({source_error}), accent ({accent_error}), and player seeds ({})",
                            fallback_errors.join("; ")
                        ))
                    })?;
                    audit.degradation(
                        "players[0].selection".into(),
                        "selection_seed_substitution",
                        json!({
                            "requested": color(palette, "selection"),
                            "fallback_seed": fallback_seed,
                            "reason": source_error.to_string(),
                        }),
                    );
                    selection
                }
            }
        }
    };

    let mut player_cursor_backgrounds = editor_text_backgrounds.clone();
    for background in &editor_text_backgrounds {
        player_cursor_backgrounds.push(gpui_blend(background, &selection)?.opaque_hex());
        player_cursor_backgrounds
            .push(gpui_blend(background, &apply_opacity(&selection, 0.5)?)?.opaque_hex());
    }
    let player_cursor_backgrounds = unique(player_cursor_backgrounds);
    let provisional_player_cursors = fit_player_cursors(
        &mut search,
        &mut audit,
        &player_seeds,
        &player_cursor_backgrounds,
    )?;

    let readable = [(editor_primary.clone(), TEXT_CONTRAST)];
    let mut player_selections = vec![selection];
    let mut shared_selection_fallback = false;
    for (index, cursor) in provisional_player_cursors.iter().enumerate().skip(1) {
        let references = player_selections
            .iter()
            .map(|selection| (selection.clone(), 1.0, PLAYER_SELECTION_DELTA_E))
            .collect::<Vec<_>>();
        let request = || {
            OverlayFitRequest::new(
                &base_overlay_backgrounds,
                FOCUSED_SELECTION_CONTRAST,
                FOCUSED_SELECTION_DELTA_E,
            )
            .with_runtime_state((0.5, 1.08, 0.020))
            .with_readable_foregrounds(&readable)
            .with_rendered_references(&references)
        };
        let fitted = if let Ok(fitted) =
            search.fit_readable_overlay_alpha_range(cursor, request(), u8::MAX, u8::MAX)
        {
            fitted
        } else {
            audit.degradation(
                format!("players[{index}].selection"),
                "shared_selection_fallback",
                json!({"reason": "palette cannot distinguish another readable selection"}),
            );
            shared_selection_fallback = true;
            player_selections[0].clone()
        };
        player_selections.push(fitted);
    }

    if shared_selection_fallback {
        let shared = player_selections[0].clone();
        player_selections.fill(shared);
    }

    let mut final_cursor_backgrounds = editor_text_backgrounds.clone();
    for background in &editor_text_backgrounds {
        for selection in &player_selections {
            final_cursor_backgrounds.push(gpui_blend(background, selection)?.opaque_hex());
            final_cursor_backgrounds
                .push(gpui_blend(background, &apply_opacity(selection, 0.5)?)?.opaque_hex());
        }
    }
    let final_cursor_backgrounds = unique(final_cursor_backgrounds);

    let player_cursors = if shared_selection_fallback {
        provisional_player_cursors.clone()
    } else {
        match fit_player_cursors(
            &mut search,
            &mut audit,
            &player_seeds,
            &final_cursor_backgrounds,
        ) {
            Ok(values) => values,
            Err(_) => {
                audit.degradation(
                    "players.selection".into(),
                    "shared_selection_fallback",
                    json!({"reason": "palette cannot keep every distinct selection readable beneath its cursor"}),
                );
                let shared = player_selections[0].clone();
                player_selections.fill(shared);
                provisional_player_cursors.clone()
            }
        }
    };

    let mut players = Vec::new();
    for (index, ((seed, cursor), selection)) in player_seeds
        .iter()
        .zip(&player_cursors)
        .zip(player_selections)
        .enumerate()
    {
        let background = search
            .fit_state(
                seed,
                std::slice::from_ref(&canvas),
                STATE_HOVER_CONTRAST,
                STATE_HOVER_DELTA_E,
                &[(cursor.clone(), TEXT_CONTRAST, STATE_CONSECUTIVE_DELTA_E)],
            )
            .unwrap_or_else(|_| canvas.clone());
        if background == canvas {
            audit.degradation(
                format!("players[{index}].background"),
                "colored_mermaid_background",
                json!({"actual": canvas}),
            );
        }

        let selection_rgb = parse_hex(&selection)?.opaque_hex();
        if selection_rgb != *cursor {
            audit.fidelity_deviations.push(json!({
                "role": format!("players[{index}].selection"),
                "requested_relation": "cursor and selection share RGB",
                "source": cursor,
                "output": selection_rgb,
                "reason": "shared RGB is subordinate to selection visibility and cursor readability",
            }));
        }

        players.push(BTreeMap::from([
            ("cursor".into(), cursor.clone()),
            ("background".into(), background),
            ("selection".into(), selection),
        ]));
    }

    let local_selection_overlay = players
        .first()
        .and_then(|player| player.get("selection"))
        .ok_or_else(|| Error("local player has no selection color".into()))?;
    let local_unfocused_overlay = apply_opacity(local_selection_overlay, 0.5)?;
    let local_selection = gpui_blend(&canvas, local_selection_overlay)?.opaque_hex();
    let local_unfocused_selection = gpui_blend(&canvas, &local_unfocused_overlay)?.opaque_hex();
    let terminal_backgrounds = unique([canvas.clone(), local_selection, local_unfocused_selection]);
    let foreground_triplet = terminal_triplet(
        &mut search,
        &mut audit,
        TerminalRequest {
            seeds: [
                color(palette, "dark_foreground"),
                color(palette, "foreground"),
                color(palette, "bright_foreground"),
            ],
            backgrounds: &terminal_backgrounds,
            mode: &palette.mode,
            role: "terminal.foreground",
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
        let role = format!("terminal.ansi.{name}");
        let triplet = terminal_triplet(
            &mut search,
            &mut audit,
            TerminalRequest {
                seeds: [
                    color(palette, &dim_key),
                    color(palette, &dim_key),
                    color(palette, &bright_key),
                ],
                backgrounds: &terminal_backgrounds,
                mode: &palette.mode,
                role: &role,
            },
        )?;
        terminal.insert(format!("terminal.ansi.dim_{name}"), triplet[0].clone());
        terminal.insert(format!("terminal.ansi.{name}"), triplet[1].clone());
        terminal.insert(format!("terminal.ansi.bright_{name}"), triplet[2].clone());
    }

    let overlay_contexts = editor_text_backgrounds.clone();
    let mut focused_selections = Vec::with_capacity(editor_text_backgrounds.len() * players.len());
    for base in &editor_text_backgrounds {
        for player in &players {
            let selection = player
                .get("selection")
                .ok_or_else(|| Error("player has no selection color".into()))?;
            focused_selections.push(gpui_blend(base, selection)?.opaque_hex());
        }
    }
    let local_unfocused = editor_text_backgrounds
        .iter()
        .map(|base| Ok(gpui_blend(base, &local_unfocused_overlay)?.opaque_hex()))
        .collect::<Result<Vec<_>>>()?;
    let syntax_contexts = unique(
        editor_bases
            .iter()
            .cloned()
            .chain(overlay_contexts.iter().cloned())
            .chain(focused_selections)
            .chain(local_unfocused),
    );

    let status_seeds: BTreeMap<&str, &String> = BTreeMap::from([
        ("created", &diff_green_seed),
        ("deleted", &diff_red_seed),
        ("hidden", &semantic.disabled),
        ("hint", &semantic.cyan),
        ("ignored", &semantic.secondary),
        ("info", &semantic.blue),
        ("predictive", &semantic.secondary),
        ("unreachable", &semantic.secondary),
        ("warning", &diff_yellow_seed),
    ]);
    let mut status_backgrounds = BTreeMap::new();
    for name in status_seeds.keys() {
        status_backgrounds.insert(
            *name,
            search.fit_state(
                status_seeds[name],
                std::slice::from_ref(&surface),
                STATE_SELECTED_CONTRAST,
                STATE_SELECTED_DELTA_E,
                &[
                    (
                        semantic.primary.clone(),
                        TEXT_CONTRAST,
                        STATE_CONSECUTIVE_DELTA_E,
                    ),
                    (
                        semantic.structural.clone(),
                        CONTROL_CONTRAST,
                        STATE_CONSECUTIVE_DELTA_E,
                    ),
                ],
            )?,
        );
    }

    let mut status_foregrounds = BTreeMap::new();
    for (name, seed) in &status_seeds {
        if *name == "ignored" {
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
                TEXT_CONTRAST,
                &[],
                FitBounds {
                    lower_chroma: 0.025,
                    ..FitBounds::default()
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
        let border = search.fit_color(
            status_seeds[name],
            &[surface.clone(), background.clone()],
            CONTROL_CONTRAST,
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
        let background = search.fit_state(
            seed,
            std::slice::from_ref(&chrome),
            STATE_SELECTED_CONTRAST,
            STATE_SELECTED_DELTA_E,
            &[],
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
        &syntax_contexts,
        &editor_primary,
        &statuses["predictive"],
        [&diff_green_seed, &diff_yellow_seed, &diff_red_seed],
        &mut audit,
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
        &mut audit,
        "accents",
    )?;

    let drop_target = search.fit_readable_overlay_bounded(
        &semantic.accent,
        OverlayFitRequest::new(
            std::slice::from_ref(&surface),
            STATE_SELECTED_CONTRAST,
            STATE_SELECTED_DELTA_E,
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
        OverlayFitRequest::new(&thumb_contexts, CONTROL_CONTRAST, STATE_SELECTED_DELTA_E),
        OVERLAY_MAX_ALPHA,
    )?;
    let thumb_hover = search.fit_readable_overlay_bounded(
        &semantic.primary,
        OverlayFitRequest::new(
            &thumb_contexts,
            THUMB_HOVER_CONTRAST,
            STATE_SELECTED_DELTA_E,
        )
        .with_rendered_references(&[(
            thumb_base.clone(),
            STATE_CONSECUTIVE_CONTRAST,
            STATE_CONSECUTIVE_DELTA_E,
        )]),
        OVERLAY_MAX_ALPHA,
    )?;
    let thumb_active = search.fit_readable_overlay_bounded(
        &semantic.primary,
        OverlayFitRequest::new(
            &thumb_contexts,
            THUMB_ACTIVE_CONTRAST,
            STATE_SELECTED_DELTA_E,
        )
        .with_rendered_references(&[(
            thumb_hover.clone(),
            STATE_CONSECUTIVE_CONTRAST,
            STATE_CONSECUTIVE_DELTA_E,
        )]),
        OVERLAY_MAX_ALPHA,
    )?;
    let thumb_ladder = [thumb_base, thumb_hover, thumb_active];
    let thumb_border = semantic.structural.clone();
    let track_border = search.fit_color(&semantic.passive, &thumb_contexts, PASSIVE_CONTRAST)?;

    let wrap_guide = with_alpha(&semantic.structural, 0x0d as f64 / 255.0)?;
    let active_wrap_guide = with_alpha(&semantic.structural, 0x1a as f64 / 255.0)?;

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
            editor_highlighted_line: PaintColor::new(editor_highlighted_line.clone())?,
        },
        content: ContentTokens {
            primary: OpaqueColor::new(semantic.primary.clone())?,
            accent: OpaqueColor::new(content_accent)?,
            editor_primary: OpaqueColor::new(editor_primary.clone())?,
        },
        interactions: InteractionTokens {
            element_hover: OverlayColor::new(element_hover.clone())?,
            element_engaged: OverlayColor::new(element_active.clone())?,
            ghost_hover: OverlayColor::new(ghost_hover.clone())?,
            ghost_engaged: OverlayColor::new(ghost_active.clone())?,
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
    put!("border.variant", semantic.structural.clone());
    put!("border.focused", semantic.accent.clone());
    put!("border.selected", semantic.accent.clone());
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
    put!("panel.focused_border", semantic.accent.clone());
    put!("panel.indent_guide", panel_guide_ladder[0].clone());
    put!("panel.indent_guide_hover", panel_guide_ladder[1].clone());
    put!("panel.indent_guide_active", panel_guide_ladder[2].clone());
    put!("panel.overlay_background", panel_overlay);
    put!("panel.overlay_hover", panel_overlay_hover);
    put!("pane.focused_border", semantic.accent.clone());
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
    put!(
        "editor.invisible",
        search.fit_color(
            color(palette, "muted"),
            std::slice::from_ref(&canvas),
            CONTROL_CONTRAST
        )?
    );
    put!(
        "editor.indent_guide",
        search.fit_color(
            &semantic.passive,
            std::slice::from_ref(&canvas),
            PASSIVE_CONTRAST
        )?
    );
    put!(
        "editor.indent_guide_active",
        search.fit_color(
            &semantic.structural,
            std::slice::from_ref(&canvas),
            CONTROL_CONTRAST
        )?
    );
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
        .ok_or_else(|| Error("missing generated Vim yank highlight".into()))?;
    fixed.insert_overlay("vim.yank.background", vim_yank)?;

    fixed.extend_opaque(vim)?;
    fixed.extend(theme_tokens.zed_roles())?;
    let mut status_roles = StyleBuilder::default();
    status_roles.extend(theme_tokens.statuses.zed_roles())?;

    let mut style = Map::new();
    style.insert("background.appearance".into(), "opaque".into());
    status_roles.append_to(&mut style)?;
    fixed.append_to(&mut style)?;

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

    let validation_ui_backgrounds = unique(
        ui_backgrounds
            .iter()
            .cloned()
            .chain(std::iter::once(tab_inactive)),
    );

    validate_theme(
        &document,
        ValidationContexts {
            ui_backgrounds: &validation_ui_backgrounds,
            interaction_bases: &interaction_bases,
            syntax_contexts: &syntax_contexts,
            editor_bases: &editor_bases,
            selection_visibility_backgrounds: &base_overlay_backgrounds,
            editor_text_backgrounds: &editor_text_backgrounds,
            terminal_backgrounds: &terminal_backgrounds,
        },
        &mut audit,
    )?;

    Ok((document, audit))
}

fn style(document: &Value) -> Result<&Map<String, Value>> {
    document
        .get("themes")
        .and_then(Value::as_array)
        .and_then(|themes| (themes.len() == 1).then(|| &themes[0]))
        .and_then(|theme| theme.get("style"))
        .and_then(Value::as_object)
        .ok_or_else(|| {
            Error("theme document must contain exactly one theme with a style object".into())
        })
}

fn style_color<'a>(style: &'a Map<String, Value>, name: &str) -> Result<&'a str> {
    style
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| Error(format!("theme role {name} is not a color string")))
}

struct ValidationContexts<'a> {
    ui_backgrounds: &'a [String],
    interaction_bases: &'a [String],
    syntax_contexts: &'a [String],
    editor_bases: &'a [String],
    selection_visibility_backgrounds: &'a [String],
    editor_text_backgrounds: &'a [String],
    terminal_backgrounds: &'a [String],
}

fn validate_theme(
    document: &Value,
    contexts: ValidationContexts<'_>,
    audit: &mut Audit,
) -> Result<()> {
    let ValidationContexts {
        ui_backgrounds,
        interaction_bases,
        syntax_contexts,
        editor_bases,
        selection_visibility_backgrounds,
        editor_text_backgrounds,
        terminal_backgrounds,
    } = contexts;
    let style = style(document)?;
    let appearance = document["themes"][0]["appearance"]
        .as_str()
        .ok_or_else(|| Error("theme appearance is not a string".into()))?;
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
    let fixed_actual: BTreeSet<String> = style
        .keys()
        .filter(|key| fixed_expected.contains(*key))
        .cloned()
        .collect();
    let status_actual: BTreeSet<String> = style
        .keys()
        .filter(|key| status_expected.contains(*key))
        .cloned()
        .collect();
    let mut errors = Vec::new();
    for group in [
        &[
            "editor.background",
            "editor.gutter.background",
            "tab.active_background",
            "toolbar.background",
        ][..],
        &[
            "background",
            "status_bar.background",
            "title_bar.background",
        ],
        &["text", "icon"],
        &[
            "text.muted",
            "icon.muted",
            "icon.placeholder",
            "unreachable",
        ],
        &[
            "text.placeholder",
            "text.disabled",
            "icon.disabled",
            "hidden",
            "ignored",
        ],
        &["text.accent", "icon.accent", "link_text.hover"],
        &["element.active", "element.selected"],
        &["ghost_element.active", "ghost_element.selected"],
        &[
            "element.disabled",
            "ghost_element.disabled",
            "title_bar.inactive_background",
        ],
    ] {
        let expected = style_color(style, group[0])?;
        for name in &group[1..] {
            if style_color(style, name)? != expected {
                errors.push(format!("{name} does not share the {} token", group[0]));
            }
        }
    }
    for name in [
        "border.transparent",
        "ghost_element.background",
        "scrollbar.track.background",
    ] {
        if style_color(style, name)? != "#00000000" {
            errors.push(format!("{name} is not canonical transparent black"));
        }
    }
    for (source, aliases) in [
        ("created", &["success"][..]),
        ("deleted", &["error"]),
        ("warning", &["conflict", "modified"]),
        ("info", &["renamed"]),
    ] {
        for suffix in ["", ".background", ".border"] {
            let expected = style_color(style, &format!("{source}{suffix}"))?;
            for alias in aliases {
                let name = format!("{alias}{suffix}");
                if style_color(style, &name)? != expected {
                    errors.push(format!("{name} does not share the {source}{suffix} token"));
                }
            }
        }
    }
    let structural_rgb = parse_hex(style_color(style, "border")?)?.opaque_hex();
    for name in ["editor.wrap_guide", "editor.active_wrap_guide"] {
        if parse_hex(style_color(style, name)?)?.opaque_hex() != structural_rgb {
            errors.push(format!("{name} does not preserve the structural RGB"));
        }
    }
    let editor_canvas = style_color(style, "editor.background")?;
    let rendered_wrap =
        gpui_blend(editor_canvas, style_color(style, "editor.wrap_guide")?)?.opaque_hex();
    let rendered_active_wrap = gpui_blend(
        editor_canvas,
        style_color(style, "editor.active_wrap_guide")?,
    )?
    .opaque_hex();
    if contrast_ratio(&rendered_active_wrap, editor_canvas)?
        <= contrast_ratio(&rendered_wrap, editor_canvas)? + 1e-12
        || delta_e(&rendered_active_wrap, editor_canvas)?
            <= delta_e(&rendered_wrap, editor_canvas)? + 1e-12
    {
        errors.push("active wrap guide does not render more visibly than wrap guide".into());
    }
    if fixed_actual != fixed_expected {
        errors.push(format!(
            "fixed manifest mismatch: expected {}, got {}",
            fixed_expected.len(),
            fixed_actual.len()
        ));
    }
    if status_actual != status_expected {
        errors.push(format!(
            "status manifest mismatch: expected {}, got {}",
            status_expected.len(),
            status_actual.len()
        ));
    }
    let allowed: BTreeSet<String> = fixed_expected
        .iter()
        .cloned()
        .chain(status_expected.iter().cloned())
        .chain(["background.appearance", "accents", "players", "syntax"].map(str::to_owned))
        .collect();
    let unexpected: Vec<_> = style
        .keys()
        .filter(|key| !allowed.contains(key.as_str()))
        .collect();
    if !unexpected.is_empty() {
        errors.push(format!("unexpected style fields: {unexpected:?}"));
    }

    let ui_text_fields = [
        "text",
        "text.muted",
        "text.placeholder",
        "text.disabled",
        "text.accent",
    ];
    let semantic_text_fields = [
        "link_text.hover",
        "version_control.added",
        "version_control.deleted",
        "version_control.modified",
        "version_control.renamed",
        "version_control.conflict",
        "version_control.ignored",
    ];
    let mut text_minimum = f64::INFINITY;
    for (names, backgrounds) in [
        (&ui_text_fields[..], ui_backgrounds),
        (&semantic_text_fields[..], interaction_bases),
    ] {
        for name in names {
            let value = style_color(style, name)?;
            let actual = minimum_contrast(value, backgrounds)?;
            text_minimum = text_minimum.min(actual);
            if actual < HARD_TEXT_CONTRAST - 1e-9 {
                errors.push(format!("{name} reaches only {actual:.3}:1"));
            }
        }
    }
    audit.minimums.insert(
        "ui_text".into(),
        (text_minimum * 10_000.0).round() / 10_000.0,
    );
    let editor_foreground = style_color(style, "editor.foreground")?;
    let mut editor_label_minimum = f64::INFINITY;
    for (name, floor) in [
        ("editor.line_number", HARD_PASSIVE_CONTRAST),
        ("editor.hover_line_number", HARD_CONTROL_CONTRAST),
        ("editor.active_line_number", HARD_TEXT_CONTRAST),
    ] {
        let actual = minimum_contrast(style_color(style, name)?, editor_bases)?;
        editor_label_minimum = editor_label_minimum.min(actual);
        if actual < floor - 1e-9 {
            errors.push(format!(
                "{name} reaches only {actual:.3}:1; floor is {floor:.2}:1"
            ));
        }
    }
    audit.minimums.insert(
        "editor_labels".into(),
        (editor_label_minimum * 10_000.0).round() / 10_000.0,
    );
    let inactive_saliency = crate::saliency::relative_saliency(
        style_color(style, "editor.line_number")?,
        editor_foreground,
        editor_bases,
    )?;
    let hover_saliency = crate::saliency::relative_saliency(
        style_color(style, "editor.hover_line_number")?,
        editor_foreground,
        editor_bases,
    )?;
    let active_saliency = crate::saliency::relative_saliency(
        style_color(style, "editor.active_line_number")?,
        editor_foreground,
        editor_bases,
    )?;
    if inactive_saliency + 0.10 > hover_saliency
        || inactive_saliency + 0.20 > active_saliency
        || hover_saliency > active_saliency + 0.03
    {
        errors.push(format!(
            "editor line-number saliency hierarchy is invalid: inactive {inactive_saliency:.3}, hover {hover_saliency:.3}, active {active_saliency:.3}"
        ));
    }
    let editor_foreground_minimum = minimum_contrast(editor_foreground, editor_text_backgrounds)?;
    let editor_base_minimum = minimum_contrast(editor_foreground, editor_bases)?;
    if editor_base_minimum < EDITOR_BASE_TEXT_CONTRAST - 1e-9 {
        errors.push(format!(
            "editor.foreground reserve on base surfaces is only {editor_base_minimum:.3}:1"
        ));
    }
    if editor_foreground_minimum < TEXT_CONTRAST - 1e-9 {
        errors.push(format!(
            "editor.foreground on rendered editor overlays is only {editor_foreground_minimum:.3}:1"
        ));
    }

    let syntax = style
        .get("syntax")
        .and_then(Value::as_object)
        .ok_or_else(|| Error("style.syntax is not an object".into()))?;
    let expected_syntax: BTreeSet<_> = BASE_SYNTAX_FIELDS
        .iter()
        .chain(ADDITIONAL_SYNTAX_FIELDS)
        .copied()
        .collect();
    let actual_syntax: BTreeSet<_> = syntax.keys().map(String::as_str).collect();
    if actual_syntax != expected_syntax {
        errors.push(format!(
            "syntax manifest mismatch: expected {}, got {}",
            expected_syntax.len(),
            actual_syntax.len()
        ));
    }
    let mut syntax_minimum = f64::INFINITY;
    for (name, spec) in syntax {
        let value = spec
            .get("color")
            .and_then(Value::as_str)
            .ok_or_else(|| Error(format!("syntax role {name} has no color")))?;
        let actual = minimum_contrast(value, syntax_contexts)?;
        syntax_minimum = syntax_minimum.min(actual);
        let target = contrast_floor(name)
            .ok_or_else(|| Error(format!("syntax role {name} has no capture policy")))?
            - 0.02;
        if actual < target - 1e-9 {
            errors.push(format!(
                "syntax.{name} reaches only {actual:.3}:1; floor is {target:.2}:1"
            ));
        }
    }
    let syntax_primary_saliency = crate::saliency::relative_saliency(
        syntax
            .get("primary")
            .and_then(|spec| spec.get("color"))
            .and_then(Value::as_str)
            .ok_or_else(|| Error("syntax.primary has no color".into()))?,
        editor_foreground,
        syntax_contexts,
    )?;
    let syntax_subdued_saliency = crate::saliency::relative_saliency(
        syntax
            .get("comment")
            .and_then(|spec| spec.get("color"))
            .and_then(Value::as_str)
            .ok_or_else(|| Error("syntax.comment has no color".into()))?,
        editor_foreground,
        syntax_contexts,
    )?;
    if syntax_subdued_saliency + 0.03 > syntax_primary_saliency {
        errors.push(format!(
            "subdued syntax does not remain below primary saliency: subdued {syntax_subdued_saliency:.3}, primary {syntax_primary_saliency:.3}"
        ));
    }
    let syntax_color = |name: &str| -> Result<&str> {
        syntax
            .get(name)
            .and_then(|spec| spec.get("color"))
            .and_then(Value::as_str)
            .ok_or_else(|| Error(format!("syntax role {name} has no color")))
    };
    for (name, expected) in [
        ("primary", editor_foreground),
        ("variable", editor_foreground),
        ("predictive", style_color(style, "predictive")?),
    ] {
        if syntax_color(name)? != expected {
            errors.push(format!(
                "syntax.{name} does not share its semantic content token"
            ));
        }
    }
    let syntax_added = syntax_color("diff.plus")?;
    let syntax_deleted = syntax_color("diff.minus")?;
    let syntax_diff_contrast = contrast_ratio(syntax_added, syntax_deleted)?;
    let syntax_diff_delta = delta_e(syntax_added, syntax_deleted)?;
    let syntax_diff_cvd = crate::search::cvd_distance(syntax_added, syntax_deleted)?;
    if syntax_diff_contrast < SYNTAX_DIFF_CONTRACT.contrast - 1e-9
        || syntax_diff_delta < SYNTAX_DIFF_CONTRACT.normal_delta_e - 1e-9
        || syntax_diff_cvd < SYNTAX_DIFF_CONTRACT.cvd_delta_e - 1e-9
    {
        errors.push(format!(
            "syntax diff pair is ambiguous: contrast {syntax_diff_contrast:.3}, delta E {syntax_diff_delta:.3}, CVD {syntax_diff_cvd:.3}"
        ));
    }
    audit.minimums.insert(
        "syntax".into(),
        (syntax_minimum * 10_000.0).round() / 10_000.0,
    );

    let terminal_names: Vec<_> = TERMINAL_FIELDS
        .iter()
        .copied()
        .filter(|name| !matches!(*name, "terminal.background" | "terminal.ansi.background"))
        .collect();
    let mut terminal_minimum = f64::INFINITY;
    for name in terminal_names {
        let actual = minimum_contrast(style_color(style, name)?, terminal_backgrounds)?;
        terminal_minimum = terminal_minimum.min(actual);
        if actual < HARD_TEXT_CONTRAST - 1e-9 {
            errors.push(format!("{name} reaches only {actual:.3}:1"));
        }
    }
    audit.minimums.insert(
        "terminal".into(),
        (terminal_minimum * 10_000.0).round() / 10_000.0,
    );

    let structural = [
        ("border", "surface.background", HARD_CONTROL_CONTRAST),
        (
            "border.variant",
            "surface.background",
            HARD_PASSIVE_CONTRAST,
        ),
        (
            "border.focused",
            "surface.background",
            HARD_CONTROL_CONTRAST,
        ),
        (
            "border.selected",
            "surface.background",
            HARD_CONTROL_CONTRAST,
        ),
        (
            "drop_target.border",
            "surface.background",
            HARD_CONTROL_CONTRAST,
        ),
        (
            "panel.focused_border",
            "panel.background",
            HARD_CONTROL_CONTRAST,
        ),
        (
            "panel.indent_guide",
            "panel.background",
            HARD_PASSIVE_CONTRAST,
        ),
        (
            "panel.indent_guide_hover",
            "panel.background",
            HARD_CONTROL_CONTRAST,
        ),
        (
            "panel.indent_guide_active",
            "panel.background",
            HARD_CONTROL_CONTRAST,
        ),
        (
            "pane.focused_border",
            "panel.background",
            HARD_CONTROL_CONTRAST,
        ),
        (
            "pane_group.border",
            "panel.background",
            HARD_CONTROL_CONTRAST,
        ),
        (
            "editor.indent_guide",
            "editor.background",
            HARD_PASSIVE_CONTRAST,
        ),
        (
            "editor.indent_guide_active",
            "editor.background",
            HARD_CONTROL_CONTRAST,
        ),
        (
            "editor.invisible",
            "editor.background",
            HARD_CONTROL_CONTRAST,
        ),
        (
            "scrollbar.track.border",
            "surface.background",
            HARD_PASSIVE_CONTRAST,
        ),
    ];
    for (foreground, background, target) in structural {
        let actual = contrast_ratio(
            style_color(style, foreground)?,
            style_color(style, background)?,
        )?;
        if actual < target - 1e-9 {
            errors.push(format!(
                "{foreground} against {background} is {actual:.3}:1"
            ));
        }
    }
    let generic_scrollbar_border = gpui_blend(
        style_color(style, "surface.background")?,
        &apply_opacity(style_color(style, "border.variant")?, 0.6)?,
    )?
    .opaque_hex();
    let generic_scrollbar_border_contrast = contrast_ratio(
        &generic_scrollbar_border,
        style_color(style, "surface.background")?,
    )?;
    if generic_scrollbar_border_contrast < PASSIVE_CONTRAST - 1e-9 {
        errors.push(format!(
            "generic scrollbar border is {generic_scrollbar_border_contrast:.3}:1 after runtime opacity"
        ));
    }

    let states = [
        (
            "element.hover",
            "element.background",
            STATE_HOVER_CONTRAST,
            STATE_HOVER_DELTA_E,
        ),
        (
            "element.active",
            "element.background",
            STATE_ACTIVE_CONTRAST,
            STATE_ACTIVE_DELTA_E,
        ),
        (
            "element.selected",
            "element.background",
            STATE_ACTIVE_CONTRAST,
            STATE_ACTIVE_DELTA_E,
        ),
        (
            "ghost_element.hover",
            "background",
            STATE_HOVER_CONTRAST,
            STATE_HOVER_DELTA_E,
        ),
        (
            "ghost_element.active",
            "background",
            STATE_ACTIVE_CONTRAST,
            STATE_ACTIVE_DELTA_E,
        ),
        (
            "ghost_element.selected",
            "background",
            STATE_ACTIVE_CONTRAST,
            STATE_ACTIVE_DELTA_E,
        ),
        (
            "panel.overlay_background",
            "panel.background",
            TAB_STATE_CONTRAST,
            STATE_CONSECUTIVE_DELTA_E,
        ),
        (
            "panel.overlay_hover",
            "panel.overlay_background",
            STATE_HOVER_CONTRAST,
            STATE_HOVER_DELTA_E,
        ),
        (
            "editor.active_line.background",
            "editor.background",
            STATE_HOVER_CONTRAST,
            STATE_HOVER_DELTA_E,
        ),
        (
            "editor.highlighted_line.background",
            "editor.background",
            STATE_ACTIVE_CONTRAST,
            STATE_ACTIVE_DELTA_E,
        ),
        (
            "editor.debugger_active_line.background",
            "editor.background",
            STATE_SELECTED_CONTRAST,
            STATE_SELECTED_DELTA_E,
        ),
        (
            "search.match_background",
            "editor.background",
            SEARCH_MATCH_CONTRAST,
            STATE_HOVER_DELTA_E,
        ),
        (
            "search.active_match_background",
            "editor.background",
            SEARCH_ACTIVE_CONTRAST,
            STATE_SELECTED_DELTA_E,
        ),
    ];
    for (foreground, background, target, target_delta) in states {
        let foreground = style_color(style, foreground)?;
        let background = style_color(style, background)?;
        let rendered = gpui_blend(background, foreground)?.opaque_hex();
        let ratio = contrast_ratio(&rendered, background)?;
        let distance = delta_e(&rendered, background)?;
        if ratio < target - 1e-9 {
            errors.push(format!("interaction state contrast is {ratio:.3}:1"));
        }
        if distance < target_delta - 1e-9 {
            errors.push(format!("interaction state delta E is {distance:.3}"));
        }
    }
    for (name, raw_target, raw_delta, runtime) in [
        (
            "element.hover",
            STATE_HOVER_CONTRAST,
            STATE_HOVER_DELTA_E,
            Some((0.6, STATE_HOVER_CONTRAST, STATE_HOVER_DELTA_E)),
        ),
        (
            "element.active",
            STATE_ACTIVE_CONTRAST,
            STATE_ACTIVE_DELTA_E,
            Some((0.5, STATE_HOVER_CONTRAST, STATE_HOVER_DELTA_E)),
        ),
        (
            "element.selected",
            STATE_ACTIVE_CONTRAST,
            STATE_ACTIVE_DELTA_E,
            None,
        ),
        (
            "ghost_element.hover",
            STATE_HOVER_CONTRAST,
            STATE_HOVER_DELTA_E,
            None,
        ),
        (
            "ghost_element.active",
            STATE_ACTIVE_CONTRAST,
            STATE_ACTIVE_DELTA_E,
            None,
        ),
        (
            "ghost_element.selected",
            STATE_ACTIVE_CONTRAST,
            STATE_ACTIVE_DELTA_E,
            None,
        ),
    ] {
        let value = style_color(style, name)?;
        for base in interaction_bases {
            let raw_rendered = gpui_blend(base, value)?.opaque_hex();
            let raw_ratio = contrast_ratio(&raw_rendered, base)?;
            let raw_distance = delta_e(&raw_rendered, base)?;
            if raw_ratio < raw_target - 1e-9 || raw_distance < raw_delta - 1e-9 {
                errors.push(format!(
                    "{name} collapses on {base}: contrast {raw_ratio:.3}, delta E {raw_distance:.3}"
                ));
            }
            if let Some((opacity, target, target_delta)) = runtime {
                let rendered = gpui_blend(base, &apply_opacity(value, opacity)?)?.opaque_hex();
                let ratio = contrast_ratio(&rendered, base)?;
                let distance = delta_e(&rendered, base)?;
                if ratio < target - 1e-9 || distance < target_delta - 1e-9 {
                    errors.push(format!(
                        "{name}@{opacity:.1} collapses on {base}: contrast {ratio:.3}, delta E {distance:.3}"
                    ));
                }
            }
        }
    }
    let runtime_hover = apply_opacity(style_color(style, "element.hover")?, 0.6)?;
    let runtime_active = apply_opacity(style_color(style, "element.active")?, 0.5)?;
    for base in interaction_bases {
        let hover = gpui_blend(base, &runtime_hover)?.opaque_hex();
        let active = gpui_blend(base, &runtime_active)?.opaque_hex();
        let hover_base_ratio = contrast_ratio(&hover, base)?;
        let active_base_ratio = contrast_ratio(&active, base)?;
        let consecutive_ratio = contrast_ratio(&active, &hover)?;
        let consecutive_delta = delta_e(&active, &hover)?;
        if active_base_ratio < hover_base_ratio + RUNTIME_STATE_BASE_CONTRAST_STEP - 1e-9
            || consecutive_ratio < RUNTIME_STATE_CONSECUTIVE_CONTRAST - 1e-9
            || consecutive_delta < RUNTIME_STATE_CONSECUTIVE_DELTA_E - 1e-9
        {
            errors.push(format!(
                "element.active@0.5 does not advance element.hover@0.6 on {base}: base contrast {hover_base_ratio:.3}->{active_base_ratio:.3}, consecutive contrast {consecutive_ratio:.3}, delta E {consecutive_delta:.3}"
            ));
        }
    }
    for (family, names) in [
        ("element", &["element.hover", "element.active"][..]),
        (
            "ghost_element",
            &["ghost_element.hover", "ghost_element.active"][..],
        ),
    ] {
        for base in interaction_bases {
            let mut previous_ratio = 1.0;
            for name in names {
                let rendered = gpui_blend(base, style_color(style, name)?)?.opaque_hex();
                let ratio = contrast_ratio(&rendered, base)?;
                if ratio < previous_ratio + STATE_BASE_CONTRAST_STEP - 1e-9 {
                    errors.push(format!(
                        "{family} rung {name} does not advance base contrast on {base}: {previous_ratio:.3}->{ratio:.3}"
                    ));
                }
                previous_ratio = ratio;
            }
        }
    }
    for (active, selected) in [
        ("element.active", "element.selected"),
        ("ghost_element.active", "ghost_element.selected"),
    ] {
        if style_color(style, active)? != style_color(style, selected)? {
            errors.push(format!(
                "{active} and {selected} must share the engaged token"
            ));
        }
    }
    let ui_selection = style_color(style, "element.selection_background")?;
    let ui_text = style_color(style, "text")?;
    for base in interaction_bases {
        for (opacity, target, target_delta, label) in [
            (
                1.0,
                FOCUSED_SELECTION_CONTRAST,
                FOCUSED_SELECTION_DELTA_E,
                "focused",
            ),
            (0.5, 1.08, 0.020, "unfocused"),
        ] {
            let rendered = gpui_blend(base, &apply_opacity(ui_selection, opacity)?)?.opaque_hex();
            let ratio = contrast_ratio(&rendered, base)?;
            let distance = delta_e(&rendered, base)?;
            let text_contrast = contrast_ratio(ui_text, &rendered)?;
            if ratio < target - 1e-9
                || distance < target_delta - 1e-9
                || text_contrast < TEXT_CONTRAST - 1e-9
            {
                errors.push(format!(
                    "{label} UI selection fails on {base}: contrast {ratio:.3}, delta E {distance:.3}, text {text_contrast:.3}:1"
                ));
            }
        }
    }
    let editor_base = style_color(style, "editor.background")?;
    let mut previous_editor_state: Option<(String, &str)> = None;
    for name in [
        "editor.active_line.background",
        "editor.highlighted_line.background",
        "editor.debugger_active_line.background",
    ] {
        let rendered = gpui_blend(editor_base, style_color(style, name)?)?.opaque_hex();
        if let Some((previous, previous_name)) = &previous_editor_state {
            let ratio = contrast_ratio(&rendered, previous)?;
            let distance = delta_e(&rendered, previous)?;
            if ratio < STATE_CONSECUTIVE_CONTRAST - 1e-9
                || distance < STATE_CONSECUTIVE_DELTA_E - 1e-9
            {
                errors.push(format!(
                    "{name} collapses into {previous_name}: contrast {ratio:.3}, delta E {distance:.3}"
                ));
            }
        }
        previous_editor_state = Some((rendered, name));
    }
    for (family, background, names, require_base_step) in [
        (
            "element",
            "element.background",
            &["element.hover", "element.active"][..],
            false,
        ),
        (
            "ghost_element",
            "background",
            &["ghost_element.hover", "ghost_element.active"][..],
            false,
        ),
        (
            "panel.indent_guide",
            "panel.background",
            &[
                "panel.indent_guide",
                "panel.indent_guide_hover",
                "panel.indent_guide_active",
            ][..],
            true,
        ),
    ] {
        let base = style_color(style, background)?;
        let mut previous_ratio = 1.0;
        let mut previous_color = base.to_owned();
        let mut rungs = Vec::new();
        for name in names {
            let value = gpui_blend(base, style_color(style, name)?)?.opaque_hex();
            let ratio = contrast_ratio(&value, base)?;
            let consecutive_ratio = contrast_ratio(&value, &previous_color)?;
            let consecutive_delta = delta_e(&value, &previous_color)?;
            if require_base_step && ratio < previous_ratio + STATE_BASE_CONTRAST_STEP - 1e-9 {
                errors.push(format!(
                    "{name} does not increase base contrast by {STATE_BASE_CONTRAST_STEP:.2}"
                ));
            }
            if previous_color != base
                && (consecutive_ratio < STATE_CONSECUTIVE_CONTRAST - 1e-9
                    || consecutive_delta < STATE_CONSECUTIVE_DELTA_E - 1e-9)
            {
                errors.push(format!(
                    "{name} is not distinct from the preceding interaction rung"
                ));
            }
            rungs.push(json!({"role": name, "color": value, "base_contrast": round6(ratio), "previous_contrast": round6(consecutive_ratio), "previous_delta_e": round6(consecutive_delta)}));
            previous_ratio = ratio;
            previous_color = value;
        }
        audit
            .interaction_ladders
            .push(json!({"family": family, "background": base, "rungs": rungs}));
    }
    {
        let (first, second, family) = ("tab.inactive_background", "tab.active_background", "tab");
        let contrast = contrast_ratio(style_color(style, first)?, style_color(style, second)?)?;
        let distance = delta_e(style_color(style, first)?, style_color(style, second)?)?;
        if contrast < TAB_STATE_CONTRAST - 1e-9 || distance < STATE_CONSECUTIVE_DELTA_E - 1e-9 {
            errors.push(format!(
                "{family} states collapse: contrast {contrast:.3}, delta E {distance:.3}"
            ));
        }
    }

    // Diff roles are paint layers. Validate the pixels Zed renders rather than the
    // uncomposited RGBA source channels.
    let added_solid = render_on_bases(
        editor_bases,
        &[style_color(style, "editor.diff_hunk.added.background")?],
    )?;
    let deleted_solid = render_on_bases(
        editor_bases,
        &[style_color(style, "editor.diff_hunk.deleted.background")?],
    )?;
    let added_hollow = render_on_bases(
        editor_bases,
        &[style_color(
            style,
            "editor.diff_hunk.added.hollow_background",
        )?],
    )?;
    let deleted_hollow = render_on_bases(
        editor_bases,
        &[style_color(
            style,
            "editor.diff_hunk.deleted.hollow_background",
        )?],
    )?;
    let added_border = render_on_bases(
        &added_hollow,
        &[style_color(style, "editor.diff_hunk.added.hollow_border")?],
    )?;
    let deleted_border = render_on_bases(
        &deleted_hollow,
        &[style_color(
            style,
            "editor.diff_hunk.deleted.hollow_border",
        )?],
    )?;
    let conflict_ours = render_on_bases(
        editor_bases,
        &[style_color(style, "version_control.conflict_marker.ours")?],
    )?;
    let conflict_theirs = render_on_bases(
        editor_bases,
        &[style_color(
            style,
            "version_control.conflict_marker.theirs",
        )?],
    )?;
    let generic_highlights = [
        style_color(style, "search.match_background")?,
        style_color(style, "search.active_match_background")?,
        style_color(style, "editor.document_highlight.read_background")?,
        style_color(style, "editor.document_highlight.write_background")?,
        style_color(style, "editor.document_highlight.bracket_background")?,
        style_color(style, "vim.yank.background")?,
    ];
    let added_word_bases = added_solid
        .iter()
        .cloned()
        .chain(added_hollow.iter().cloned())
        .collect::<Vec<_>>();
    let deleted_word_bases = deleted_solid
        .iter()
        .cloned()
        .chain(deleted_hollow.iter().cloned())
        .collect::<Vec<_>>();
    let added_word_underlays =
        render_with_bounded_generic_highlights(&added_word_bases, &generic_highlights)?;
    let deleted_word_underlays =
        render_with_bounded_generic_highlights(&deleted_word_bases, &generic_highlights)?;
    let word_added = render_on_bases(
        &added_word_underlays,
        &[style_color(style, "version_control.word_added")?],
    )?;
    let word_deleted = render_on_bases(
        &deleted_word_underlays,
        &[style_color(style, "version_control.word_deleted")?],
    )?;

    let (expected_filled_opacity, expected_hollow_opacity, expected_border_opacity) =
        if appearance == "light" {
            (
                LIGHT_DIFF_HUNK_FILLED_OPACITY,
                LIGHT_DIFF_HUNK_HOLLOW_BACKGROUND_OPACITY,
                LIGHT_DIFF_HUNK_HOLLOW_BORDER_OPACITY,
            )
        } else {
            (
                DARK_DIFF_HUNK_FILLED_OPACITY,
                DARK_DIFF_HUNK_HOLLOW_BACKGROUND_OPACITY,
                DARK_DIFF_HUNK_HOLLOW_BORDER_OPACITY,
            )
        };
    for (family, marker, filled, hollow, border, word) in [
        (
            "added",
            "version_control.added",
            "editor.diff_hunk.added.background",
            "editor.diff_hunk.added.hollow_background",
            "editor.diff_hunk.added.hollow_border",
            "version_control.word_added",
        ),
        (
            "deleted",
            "version_control.deleted",
            "editor.diff_hunk.deleted.background",
            "editor.diff_hunk.deleted.hollow_background",
            "editor.diff_hunk.deleted.hollow_border",
            "version_control.word_deleted",
        ),
    ] {
        let marker_rgb = parse_hex(style_color(style, marker)?)?.opaque_hex();
        for (role, expected_opacity) in [
            (filled, expected_filled_opacity),
            (hollow, expected_hollow_opacity),
            (border, expected_border_opacity),
        ] {
            let value = parse_hex(style_color(style, role)?)?;
            let expected_alpha = (expected_opacity * 255.0).round();
            let actual_alpha = (value.a * 255.0).round();
            if value.opaque_hex() != marker_rgb || actual_alpha != expected_alpha {
                errors.push(format!(
                    "{role} must reuse {marker} RGB at {:.0}% opacity",
                    expected_opacity * 100.0
                ));
            }
        }
        let word_value = parse_hex(style_color(style, word)?)?;
        let expected_word_alpha = (expected_filled_opacity * 255.0).round();
        let actual_word_alpha = (word_value.a * 255.0).round();
        if word_value.opaque_hex() != marker_rgb || actual_word_alpha != expected_word_alpha {
            errors.push(format!(
                "{word} must reuse the {family} version-control RGB at {:.0}% opacity",
                expected_filled_opacity * 100.0
            ));
        }
    }

    let mut diff_fill_minimum = f64::INFINITY;
    for rendered in [
        &added_solid,
        &deleted_solid,
        &added_hollow,
        &deleted_hollow,
        &conflict_ours,
        &conflict_theirs,
    ] {
        let actual = minimum_pairwise(editor_bases, rendered, contrast_ratio)?;
        diff_fill_minimum = diff_fill_minimum.min(actual);
    }
    for (name, rendered) in [
        ("version_control.conflict_marker.ours", &conflict_ours),
        ("version_control.conflict_marker.theirs", &conflict_theirs),
    ] {
        let actual = minimum_pairwise(editor_bases, rendered, contrast_ratio)?;
        if actual < DIFF_FILL_CONTRAST - 1e-9 {
            errors.push(format!("diff fill {name} reaches only {actual:.3}:1"));
        }
    }

    for (name, bases, rendered) in [
        (
            "version_control.word_added",
            &added_word_underlays,
            &word_added,
        ),
        (
            "version_control.word_deleted",
            &deleted_word_underlays,
            &word_deleted,
        ),
    ] {
        let actual = minimum_pairwise(bases, rendered, contrast_ratio)?;
        if actual < PRESENTATION_WORD_DIFF_CONTRAST - 1e-9 {
            errors.push(format!("word diff {name} reaches only {actual:.3}:1"));
        }
    }

    let added = style_color(style, "version_control.added")?;
    let deleted = style_color(style, "version_control.deleted")?;
    let pair_delta = delta_e(added, deleted)?;
    let pair_lightness = (lightness(added)? - lightness(deleted)?).abs();
    let pair_cvd = crate::search::cvd_distance(added, deleted)?;
    let pair_contrast = contrast_ratio(added, deleted)?;
    let semantic_pair_is_strong =
        SEMANTIC_PAIR_CONTRACT
            .separation_alternative
            .is_none_or(|(contrast, normal, cvd)| {
                pair_contrast >= contrast - 1e-9
                    || (pair_delta >= normal - 1e-9 && pair_cvd >= cvd - 1e-9)
            });
    if pair_delta < SEMANTIC_PAIR_CONTRACT.normal_delta_e - 1e-9
        || pair_cvd < SEMANTIC_PAIR_CONTRACT.cvd_delta_e - 1e-9
        || pair_contrast < SEMANTIC_PAIR_CONTRACT.contrast - 1e-9
        || !semantic_pair_is_strong
    {
        errors.push(format!("diff added/deleted pair is ambiguous: contrast {pair_contrast:.3}, delta E {pair_delta:.3}, delta L {pair_lightness:.3}, CVD {pair_cvd:.3}"));
    }
    for (family, first_role, second_role, first_scenes, second_scenes) in [
        (
            "hunk.solid",
            "editor.diff_hunk.added.background",
            "editor.diff_hunk.deleted.background",
            &added_solid,
            &deleted_solid,
        ),
        (
            "hunk.hollow",
            "editor.diff_hunk.added.hollow_background",
            "editor.diff_hunk.deleted.hollow_background",
            &added_hollow,
            &deleted_hollow,
        ),
        (
            "hunk.border",
            "editor.diff_hunk.added.hollow_border",
            "editor.diff_hunk.deleted.hollow_border",
            &added_border,
            &deleted_border,
        ),
        (
            "word",
            "version_control.word_added",
            "version_control.word_deleted",
            &word_added,
            &word_deleted,
        ),
        (
            "conflict",
            "version_control.conflict_marker.ours",
            "version_control.conflict_marker.theirs",
            &conflict_ours,
            &conflict_theirs,
        ),
    ] {
        let contrast = minimum_pairwise(first_scenes, second_scenes, contrast_ratio)?;
        let normal = minimum_pairwise(first_scenes, second_scenes, delta_e)?;
        let cvd = minimum_pairwise(first_scenes, second_scenes, crate::search::cvd_distance)?;
        let first_value = style_color(style, first_role)?;
        let second_value = style_color(style, second_role)?;
        audit.diff_metrics.push(json!({
            "family": family,
            "first": first_value,
            "second": second_value,
            "first_alpha": round6(crate::color::parse_hex(first_value)?.a),
            "second_alpha": round6(crate::color::parse_hex(second_value)?.a),
            "normal_delta_e": round6(normal),
            "cvd_delta_e": round6(cvd),
            "pair_contrast": round6(contrast),
        }));
        let strong_separation = contrast >= DIFF_LUMINANCE_SEPARATION_CONTRAST - 1e-9
            || (normal >= DIFF_NORMAL_DELTA_E - 1e-9 && cvd >= DIFF_CVD_DELTA_E - 1e-9);
        if family == "conflict"
            && (normal < DIFF_NORMAL_FLOOR_DELTA_E - 1e-9
                || cvd < DIFF_CVD_FLOOR_DELTA_E - 1e-9
                || contrast < DIFF_PAIR_CONTRAST - 1e-9
                || !strong_separation)
        {
            errors.push(format!("diff {family} pair is ambiguous: contrast {contrast:.3}, delta E {normal:.3}, CVD {cvd:.3}"));
        }
    }
    for (family, hollow, border, word) in [
        (
            "added",
            &added_hollow,
            &added_border,
            style_color(style, "version_control.word_added")?,
        ),
        (
            "deleted",
            &deleted_hollow,
            &deleted_border,
            style_color(style, "version_control.word_deleted")?,
        ),
    ] {
        let highlighted_hollow =
            render_with_bounded_generic_highlights(hollow, &generic_highlights)?;
        let highlighted_border =
            render_with_bounded_generic_highlights(border, &generic_highlights)?;
        let word_on_hollow = render_on_bases(&highlighted_hollow, &[word])?;
        let word_on_border = render_on_bases(&highlighted_border, &[word])?;
        let retained = minimum_pairwise(&word_on_hollow, &word_on_border, delta_e)?;
        if highlighted_hollow.len() != word_on_hollow.len()
            || highlighted_border.len() != word_on_border.len()
            || highlighted_hollow.len() != highlighted_border.len()
        {
            return Err(Error(
                "diff border retention contexts have different lengths".into(),
            ));
        }
        let retained_ratio = highlighted_hollow
            .iter()
            .zip(&highlighted_border)
            .zip(word_on_hollow.iter().zip(&word_on_border))
            .try_fold(
                f64::INFINITY,
                |minimum, ((fill, border), (word_fill, word_border))| {
                    let ratio =
                        delta_e(word_fill, word_border)? / delta_e(fill, border)?.max(1e-12);
                    Ok::<_, Error>(minimum.min(ratio))
                },
            )?;
        if retained < DIFF_BORDER_RETENTION_DELTA_E - 1e-9
            || retained_ratio < DIFF_BORDER_RETENTION_RATIO - 1e-9
        {
            errors.push(format!(
                "{family} word overlay erases its hunk border: delta E {retained:.3}, retained {:.1}%",
                retained_ratio * 100.0,
            ));
        }
    }
    audit.minimums.insert(
        "diff_fill".into(),
        (diff_fill_minimum * 10_000.0).round() / 10_000.0,
    );

    let status_surface = style_color(style, "surface.background")?;
    let global_text = style_color(style, "text")?;
    let global_icon = style_color(style, "icon")?;
    for name in STATUS_NAMES {
        let foreground = style_color(style, name)?;
        let background_name = format!("{name}.background");
        let border_name = format!("{name}.border");
        let background = style_color(style, &background_name)?;
        let border = style_color(style, &border_name)?;
        let state_contrast = contrast_ratio(background, status_surface)?;
        let state_distance = delta_e(background, status_surface)?;
        let status_foreground_backgrounds = unique(
            interaction_bases
                .iter()
                .chain(editor_text_backgrounds.iter())
                .cloned()
                .chain(std::iter::once(background.to_owned())),
        );
        let foreground_minimum = minimum_contrast(foreground, &status_foreground_backgrounds)?;
        if foreground_minimum < HARD_TEXT_CONTRAST - 1e-9 {
            errors.push(format!(
                "{name} reaches only {foreground_minimum:.3}:1 on a runtime surface"
            ));
        }
        for (role, value, target) in [
            (*name, foreground, TEXT_CONTRAST),
            ("text", global_text, TEXT_CONTRAST),
            ("icon", global_icon, CONTROL_CONTRAST),
        ] {
            let actual = contrast_ratio(value, background)?;
            if actual < target - 1e-9 {
                errors.push(format!("{role} on {background_name} is {actual:.3}:1"));
            }
        }
        if state_contrast < STATE_SELECTED_CONTRAST - 1e-9
            || state_distance < STATE_SELECTED_DELTA_E - 1e-9
        {
            errors.push(format!(
                "{background_name} collapses into surface: contrast {state_contrast:.3}, delta E {state_distance:.3}"
            ));
        }
        for adjacent in [status_surface, background] {
            let actual = contrast_ratio(border, adjacent)?;
            if actual < CONTROL_CONTRAST - 1e-9 {
                errors.push(format!("{border_name} is {actual:.3}:1 on {adjacent}"));
            }
        }
        for opacity in [0.08, 0.10, 0.15, 0.20, 0.40, 0.50] {
            let rendered =
                gpui_blend(status_surface, &apply_opacity(background, opacity)?)?.opaque_hex();
            let text_contrast = contrast_ratio(global_text, &rendered)?;
            let icon_contrast = contrast_ratio(global_icon, &rendered)?;
            let status_contrast = contrast_ratio(foreground, &rendered)?;
            if text_contrast < TEXT_CONTRAST - 1e-9
                || icon_contrast < CONTROL_CONTRAST - 1e-9
                || status_contrast < TEXT_CONTRAST - 1e-9
            {
                errors.push(format!(
                    "{background_name}@{opacity:.2} loses content contrast: status {status_contrast:.3}, text {text_contrast:.3}, icon {icon_contrast:.3}"
                ));
            }
        }
    }

    let status_bar = style_color(style, "status_bar.background")?;
    for name in [
        "normal",
        "insert",
        "replace",
        "visual",
        "visual_line",
        "visual_block",
        "helix_normal",
        "helix_select",
    ] {
        let foreground_name = format!("vim.{name}.foreground");
        let background_name = format!("vim.{name}.background");
        let foreground = style_color(style, &foreground_name)?;
        let background = style_color(style, &background_name)?;
        let content_contrast = contrast_ratio(foreground, background)?;
        let state_contrast = contrast_ratio(background, status_bar)?;
        let state_distance = delta_e(background, status_bar)?;
        if content_contrast < TEXT_CONTRAST - 1e-9 {
            errors.push(format!("{foreground_name} is {content_contrast:.3}:1"));
        }
        if state_contrast < STATE_SELECTED_CONTRAST - 1e-9
            || state_distance < STATE_SELECTED_DELTA_E - 1e-9
        {
            errors.push(format!(
                "{background_name} collapses into status bar: contrast {state_contrast:.3}, delta E {state_distance:.3}"
            ));
        }
    }

    let players = style
        .get("players")
        .and_then(Value::as_array)
        .ok_or_else(|| Error("players is not an array".into()))?;
    if players.len() != 8 {
        errors.push(format!("expected 8 players, got {}", players.len()));
    }
    let mut player_values = Vec::new();
    for (index, player) in players.iter().enumerate() {
        let player = player
            .as_object()
            .ok_or_else(|| Error(format!("players[{index}] is not an object")))?;
        let read = |role| {
            player
                .get(role)
                .and_then(Value::as_str)
                .ok_or_else(|| Error(format!("players[{index}].{role} is not a color")))
        };
        player_values.push((read("cursor")?, read("background")?, read("selection")?));
    }
    let mut cursor_backgrounds = editor_text_backgrounds.to_vec();
    let mut prior_selections: Vec<Vec<String>> = Vec::new();
    let mut raw_selections = BTreeSet::new();
    let selection_fallbacks = audit
        .degradations
        .iter()
        .filter(|degradation| {
            degradation.get("invariant").and_then(Value::as_str)
                == Some("shared_selection_fallback")
        })
        .filter_map(|degradation| {
            degradation
                .get("role")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .collect::<BTreeSet<_>>();
    let shared_selection_mode = !selection_fallbacks.is_empty();
    for (index, (_, _, selection)) in player_values.iter().enumerate() {
        let selection_role = format!("players[{index}].selection");
        let is_fallback =
            shared_selection_mode || selection_fallbacks.contains(selection_role.as_str());
        if !raw_selections.insert(*selection) && !is_fallback {
            errors.push(format!(
                "players[{index}].selection duplicates another player"
            ));
        }
        let mut rendered_selection = Vec::new();
        for background in selection_visibility_backgrounds {
            let rendered = gpui_blend(background, selection)?.opaque_hex();
            let contrast = contrast_ratio(&rendered, background)?;
            let distance = delta_e(&rendered, background)?;
            let text_contrast = contrast_ratio(editor_foreground, &rendered)?;
            if contrast < FOCUSED_SELECTION_CONTRAST - 1e-9
                || distance < FOCUSED_SELECTION_DELTA_E - 1e-9
            {
                errors.push(format!(
                    "focused selection is not visible on {background}: contrast {contrast:.3}, delta E {distance:.3}"
                ));
            }
            if text_contrast < TEXT_CONTRAST - 1e-9 {
                errors.push(format!(
                    "editor text is unreadable on players[{index}].selection over {background}: {text_contrast:.3}:1"
                ));
            }
            let unfocused = gpui_blend(background, &apply_opacity(selection, 0.5)?)?.opaque_hex();
            let unfocused_contrast = contrast_ratio(&unfocused, background)?;
            let unfocused_distance = delta_e(&unfocused, background)?;
            let unfocused_text_contrast = contrast_ratio(editor_foreground, &unfocused)?;
            if unfocused_contrast < 1.08 - 1e-9 || unfocused_distance < 0.020 - 1e-9 {
                errors.push(format!(
                    "unfocused selection is not visible on {background}: contrast {unfocused_contrast:.3}, delta E {unfocused_distance:.3}"
                ));
            }
            if unfocused_text_contrast < TEXT_CONTRAST - 1e-9 {
                errors.push(format!(
                    "editor text is unreadable on unfocused players[{index}].selection over {background}: {unfocused_text_contrast:.3}:1"
                ));
            }
            cursor_backgrounds.extend([rendered.clone(), unfocused.clone()]);
            rendered_selection.extend([rendered, unfocused]);
        }
        let mut word_visibility = [f64::INFINITY; 4];
        for background in editor_text_backgrounds {
            let rendered = gpui_blend(background, selection)?.opaque_hex();
            let text_contrast = contrast_ratio(editor_foreground, &rendered)?;
            word_visibility[0] = word_visibility[0].min(contrast_ratio(&rendered, background)?);
            word_visibility[1] = word_visibility[1].min(delta_e(&rendered, background)?);
            if text_contrast < HARD_TEXT_CONTRAST - 1e-9 {
                errors.push(format!(
                    "editor text is unreadable on players[{index}].selection over a reachable word scene: {text_contrast:.3}:1"
                ));
            }
            let unfocused = gpui_blend(background, &apply_opacity(selection, 0.5)?)?.opaque_hex();
            let unfocused_text_contrast = contrast_ratio(editor_foreground, &unfocused)?;
            word_visibility[2] = word_visibility[2].min(contrast_ratio(&unfocused, background)?);
            word_visibility[3] = word_visibility[3].min(delta_e(&unfocused, background)?);
            if unfocused_text_contrast < HARD_TEXT_CONTRAST - 1e-9 {
                errors.push(format!(
                    "editor text is unreadable on unfocused players[{index}].selection over a reachable word scene: {unfocused_text_contrast:.3}:1"
                ));
            }
            cursor_backgrounds.extend([rendered, unfocused]);
        }
        if word_visibility[0] < FOCUSED_SELECTION_CONTRAST - 1e-9
            || word_visibility[1] < FOCUSED_SELECTION_DELTA_E - 1e-9
            || word_visibility[2] < 1.08 - 1e-9
            || word_visibility[3] < 0.020 - 1e-9
        {
            audit.degradation(
                selection_role.clone(),
                "word_scene_visibility",
                json!({
                    "focused_contrast": round6(word_visibility[0]),
                    "focused_delta_e": round6(word_visibility[1]),
                    "unfocused_contrast": round6(word_visibility[2]),
                    "unfocused_delta_e": round6(word_visibility[3]),
                    "reason": "selection readability is preserved, but no hue-preserving selection satisfies the full visibility contract on every word scene",
                }),
            );
        }
        for (prior_index, prior) in prior_selections.iter().enumerate() {
            if is_fallback {
                continue;
            }
            for (position, (current, prior)) in rendered_selection.iter().zip(prior).enumerate() {
                if position % 2 == 1 {
                    continue;
                }
                let distance = delta_e(current, prior)?;
                if distance < PLAYER_SELECTION_DELTA_E - 1e-9 {
                    errors.push(format!(
                        "players[{index}].selection is ambiguous with players[{prior_index}].selection: delta E {distance:.3}"
                    ));
                }
            }
        }
        prior_selections.push(rendered_selection);
    }
    let cursor_backgrounds = unique(cursor_backgrounds);
    for (index, (cursor, background, _)) in player_values.iter().enumerate() {
        let cursor_minimum = minimum_contrast(cursor, &cursor_backgrounds)?;
        if cursor_minimum < TEXT_CONTRAST - 1e-9 {
            errors.push(format!(
                "players[{index}].cursor falls below {TEXT_CONTRAST:.2}:1 on an editor composite: {cursor_minimum:.3}:1"
            ));
        }
        let mermaid_contrast = contrast_ratio(cursor, background)?;
        if mermaid_contrast < TEXT_CONTRAST - 1e-9 {
            errors.push(format!(
                "players[{index}] Mermaid foreground/background is {mermaid_contrast:.3}:1"
            ));
        }
    }

    let accents = style
        .get("accents")
        .and_then(Value::as_array)
        .ok_or_else(|| Error("accents is not an array".into()))?;
    if accents.len() != 12 {
        errors.push(format!("expected 12 accents, got {}", accents.len()));
    }

    if !errors.is_empty() {
        return Err(Error(format!(
            "theme validation failed:\n  - {}",
            errors.join("\n  - ")
        )));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn style_builder_rejects_duplicate_roles() {
        let mut style = StyleBuilder::default();
        style.insert_opaque("text", "#112233".into()).unwrap();
        assert!(style.insert_opaque("text", "#445566".into()).is_err());
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
            extras: BTreeMap::new(),
            resolver_stderr: String::new(),
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
