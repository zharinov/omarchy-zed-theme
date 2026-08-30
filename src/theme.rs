//! Builds a complete Zed theme and rejects output that violates its color constraints.
//!
//! Omarchy supplies every UI color. Built-in colors may repair syntax roles only.
//! Search can record an unmet preference, but it cannot downgrade a failed validation.

use crate::color::{apply_opacity, contrast_ratio, delta_e, gpui_blend, lightness, tone};
use crate::constants::*;
use crate::palette::ResolvedPalette;
use crate::search::{FillRequest, FitBounds, PairConstraints, Search, cvd_greedy_order, round6};
use crate::syntax::{Tier, build_syntax, contrast_floor};
use crate::{Error, Result};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug)]
pub struct Audit {
    pub mode: String,
    pub extras: Vec<String>,
    pub surface_changes: Vec<Value>,
    pub repairs: Vec<Value>,
    pub degradations: Vec<Value>,
    pub minimums: BTreeMap<String, f64>,
    pub warnings: Vec<String>,
    pub syntax_richness: Value,
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
            syntax_richness: Value::Null,
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
            "syntax_richness": self.syntax_richness, "syntax_roles": self.syntax_roles,
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
    let authored: Vec<(&str, &str)> = [
        "darker_background",
        "dark_background",
        "lighter_background",
        "background",
    ]
    .into_iter()
    .map(|key| (key, color(palette, key)))
    .collect();

    let mut surfaces = BTreeMap::from([("canvas".into(), canvas.to_owned())]);
    let mut used = BTreeSet::from([canvas.to_owned()]);
    let mut previous = f64::NEG_INFINITY;

    for (role, offset) in offsets {
        let target = (canvas_lightness + offset).clamp(0.0, 1.0);
        let lower_side = target < canvas_lightness;
        let mut eligible = Vec::new();

        for (key, value) in &authored {
            let value_lightness = lightness(value)?;
            let on_side = if lower_side {
                value_lightness < canvas_lightness
            } else {
                value_lightness > canvas_lightness
            };

            if used.contains(*value) {
                continue;
            }

            if (value_lightness - target).abs() > 0.015 + 1e-12 {
                continue;
            }

            if !on_side || value_lightness <= previous + 1e-6 {
                continue;
            }

            eligible.push(((value_lightness - target).abs(), *key, *value));
        }

        eligible.sort_by(|left, right| left.0.total_cmp(&right.0).then(left.1.cmp(right.1)));

        let (source_key, output) = if let Some((_, key, value)) = eligible.first() {
            (*key, (*value).to_owned())
        } else {
            let (key, source) = authored
                .iter()
                .min_by(|left, right| {
                    (lightness(left.1).unwrap() - target)
                        .abs()
                        .total_cmp(&(lightness(right.1).unwrap() - target).abs())
                        .then(left.0.cmp(right.0))
                })
                .unwrap();
            let mut output = tone(source, target, 1.0)?;
            if lightness(&output)? <= previous + 1e-6 {
                output = tone(source, (previous + 0.004).min(1.0), 1.0)?;
            }
            (*key, output)
        };

        used.insert(output.clone());
        previous = lightness(&output)?;

        audit.surface_changes.push(json!({
            "role": role, "source_key": source_key, "source": color(palette, source_key), "output": output,
            "delta_l": round6(lightness(&output)? - lightness(color(palette, source_key))?),
            "delta_e": round6(delta_e(&output, color(palette, source_key))?),
        }));

        surfaces.insert(role.into(), output);
    }

    Ok(surfaces)
}

fn minimum_contrast(foreground: &str, backgrounds: &[String]) -> Result<f64> {
    backgrounds
        .iter()
        .map(|background| contrast_ratio(foreground, background))
        .collect::<Result<Vec<_>>>()
        .map(|values| values.into_iter().fold(f64::INFINITY, f64::min))
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

struct SemanticPalette {
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

fn derive_semantics(
    search: &mut Search,
    palette: &ResolvedPalette,
    text_backgrounds: &[String],
    semantic_backgrounds: &[String],
    audit: &mut Audit,
) -> Result<SemanticPalette> {
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
            PairConstraints {
                foreground_contrast: TEXT_CONTRAST,
                pair_contrast: SEMANTIC_PAIR_CONTRACT.contrast,
                normal_delta: SEMANTIC_PAIR_CONTRACT.normal_delta_e,
                cvd_delta: SEMANTIC_PAIR_CONTRACT.cvd_delta_e,
                lightness_delta: 0.0,
                separation_alternative: SEMANTIC_PAIR_CONTRACT.separation_alternative,
                prefer_background: false,
            },
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
    Ok(SemanticPalette {
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

    let tab_active = search
        .fit_state(
            color(palette, "background"),
            std::slice::from_ref(&chrome),
            TAB_STATE_CONTRAST,
            STATE_CONSECUTIVE_DELTA_E,
            &readable_ui_state,
        )
        .map_err(|error| Error(format!("active tab: {error}")))?;
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
            .chain([panel_overlay.clone(), tab_active.clone()]),
    );
    let interaction_ui_text = search.fit_color(
        color(palette, "foreground"),
        &interaction_bases,
        UI_STATE_TEXT_CONTRAST,
    )?;
    let readable_interaction_foreground = [(interaction_ui_text, TEXT_CONTRAST)];
    let element_hover = search
        .fit_fill_readable(
            &surface,
            FillRequest {
                backgrounds: &interaction_bases,
                target: LAYER_HOVER_CONTRAST,
                minimum_delta_e: STATE_HOVER_DELTA_E,
                runtime_state: Some((0.6, STATE_HOVER_CONTRAST, STATE_HOVER_DELTA_E)),
                readable_foregrounds: &readable_interaction_foreground,
                rendered_references: &[],
                runtime_rendered_references: &[],
            },
        )
        .map_err(|error| Error(format!("element hover: {error}")))?;
    let element_active = search
        .fit_fill_readable(
            &surface,
            FillRequest {
                backgrounds: &interaction_bases,
                target: LAYER_ACTIVE_CONTRAST,
                minimum_delta_e: STATE_ACTIVE_DELTA_E,
                runtime_state: Some((0.5, STATE_HOVER_CONTRAST, STATE_HOVER_DELTA_E)),
                readable_foregrounds: &readable_interaction_foreground,
                rendered_references: &[(
                    element_hover.clone(),
                    STATE_CONSECUTIVE_CONTRAST,
                    STATE_CONSECUTIVE_DELTA_E,
                )],
                runtime_rendered_references: &[(
                    apply_opacity(&element_hover, 0.6)?,
                    RUNTIME_STATE_CONSECUTIVE_CONTRAST,
                    RUNTIME_STATE_CONSECUTIVE_DELTA_E,
                    RUNTIME_STATE_BASE_CONTRAST_STEP,
                )],
            },
        )
        .map_err(|error| Error(format!("element active: {error}")))?;
    let element_selected = search
        .fit_fill_readable(
            &surface,
            FillRequest {
                backgrounds: &interaction_bases,
                target: LAYER_SELECTED_CONTRAST,
                minimum_delta_e: STATE_SELECTED_DELTA_E,
                runtime_state: None,
                readable_foregrounds: &readable_interaction_foreground,
                rendered_references: &[(
                    element_active.clone(),
                    STATE_CONSECUTIVE_CONTRAST,
                    STATE_CONSECUTIVE_DELTA_E,
                )],
                runtime_rendered_references: &[],
            },
        )
        .map_err(|error| Error(format!("element selected: {error}")))?;
    let ghost_hover = search
        .fit_fill_readable(
            &canvas,
            FillRequest {
                backgrounds: &interaction_bases,
                target: LAYER_HOVER_CONTRAST,
                minimum_delta_e: STATE_HOVER_DELTA_E,
                runtime_state: None,
                readable_foregrounds: &readable_interaction_foreground,
                rendered_references: &[],
                runtime_rendered_references: &[],
            },
        )
        .map_err(|error| Error(format!("ghost hover: {error}")))?;
    let ghost_active = search
        .fit_fill_readable(
            &canvas,
            FillRequest {
                backgrounds: &interaction_bases,
                target: LAYER_ACTIVE_CONTRAST,
                minimum_delta_e: STATE_ACTIVE_DELTA_E,
                runtime_state: None,
                readable_foregrounds: &readable_interaction_foreground,
                rendered_references: &[(
                    ghost_hover.clone(),
                    STATE_CONSECUTIVE_CONTRAST,
                    STATE_CONSECUTIVE_DELTA_E,
                )],
                runtime_rendered_references: &[],
            },
        )
        .map_err(|error| Error(format!("ghost active: {error}")))?;
    let ghost_selected = search
        .fit_fill_readable(
            &canvas,
            FillRequest {
                backgrounds: &interaction_bases,
                target: LAYER_SELECTED_CONTRAST,
                minimum_delta_e: STATE_SELECTED_DELTA_E,
                runtime_state: None,
                readable_foregrounds: &readable_interaction_foreground,
                rendered_references: &[(
                    ghost_active.clone(),
                    STATE_CONSECUTIVE_CONTRAST,
                    STATE_CONSECUTIVE_DELTA_E,
                )],
                runtime_rendered_references: &[],
            },
        )
        .map_err(|error| Error(format!("ghost selected: {error}")))?;
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
        interaction_bases
            .iter()
            .cloned()
            .chain([panel_overlay_hover.clone()])
            .chain(rendered_ui_state_backgrounds),
    );
    let semantic = derive_semantics(
        &mut search,
        palette,
        &ui_backgrounds,
        &interaction_bases,
        &mut audit,
    )?;
    let element_selection = search
        .fit_fill_readable(
            color(palette, "selection"),
            FillRequest {
                backgrounds: &interaction_bases,
                target: FOCUSED_SELECTION_CONTRAST,
                minimum_delta_e: FOCUSED_SELECTION_DELTA_E,
                runtime_state: Some((0.5, 1.08, 0.020)),
                readable_foregrounds: &[(semantic.primary.clone(), TEXT_CONTRAST)],
                rendered_references: &[],
                runtime_rendered_references: &[],
            },
        )
        .map_err(|error| Error(format!("UI selection: {error}")))?;

    let provisional_editor_text = search.fit_color(
        color(palette, "foreground"),
        &[canvas.clone(), chrome.clone()],
        EDITOR_CANVAS_TEXT_CONTRAST,
    )?;
    let editor_active_line = search
        .fit_fill_readable(
            &canvas,
            FillRequest {
                backgrounds: std::slice::from_ref(&canvas),
                target: STATE_HOVER_CONTRAST,
                minimum_delta_e: STATE_HOVER_DELTA_E,
                runtime_state: None,
                readable_foregrounds: &[(
                    provisional_editor_text.clone(),
                    EDITOR_BASE_TEXT_CONTRAST,
                )],
                rendered_references: &[],
                runtime_rendered_references: &[],
            },
        )
        .map_err(|error| Error(format!("active editor line: {error}")))?;
    let rendered_editor_active_line = gpui_blend(&canvas, &editor_active_line)?.opaque_hex();
    let editor_highlighted_line = search
        .fit_fill_readable(
            &surface,
            FillRequest {
                backgrounds: std::slice::from_ref(&canvas),
                target: STATE_ACTIVE_CONTRAST,
                minimum_delta_e: STATE_ACTIVE_DELTA_E,
                runtime_state: None,
                readable_foregrounds: &[(
                    provisional_editor_text.clone(),
                    EDITOR_BASE_TEXT_CONTRAST,
                )],
                rendered_references: &[(
                    rendered_editor_active_line.clone(),
                    STATE_CONSECUTIVE_CONTRAST,
                    STATE_CONSECUTIVE_DELTA_E,
                )],
                runtime_rendered_references: &[],
            },
        )
        .map_err(|error| Error(format!("highlighted editor line: {error}")))?;
    let rendered_editor_highlighted_line =
        gpui_blend(&canvas, &editor_highlighted_line)?.opaque_hex();
    let debugger_active = search
        .fit_fill_readable(
            &semantic.red,
            FillRequest {
                backgrounds: std::slice::from_ref(&canvas),
                target: STATE_SELECTED_CONTRAST,
                minimum_delta_e: STATE_SELECTED_DELTA_E,
                runtime_state: None,
                readable_foregrounds: &[(
                    provisional_editor_text.clone(),
                    EDITOR_BASE_TEXT_CONTRAST,
                )],
                rendered_references: &[(
                    rendered_editor_highlighted_line.clone(),
                    STATE_CONSECUTIVE_CONTRAST,
                    STATE_CONSECUTIVE_DELTA_E,
                )],
                runtime_rendered_references: &[],
            },
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
    let search_match = search
        .fit_state(
            &semantic.yellow,
            std::slice::from_ref(&canvas),
            SEARCH_MATCH_CONTRAST,
            STATE_HOVER_DELTA_E,
            &[(
                editor_primary.clone(),
                EDITOR_OVERLAY_TEXT_CONTRAST,
                STATE_CONSECUTIVE_DELTA_E,
            )],
        )
        .map_err(|error| Error(format!("search match: {error}")))?;
    let search_active = search
        .fit_state(
            &semantic.accent,
            std::slice::from_ref(&canvas),
            SEARCH_ACTIVE_CONTRAST,
            STATE_SELECTED_DELTA_E,
            &[(
                editor_primary.clone(),
                EDITOR_OVERLAY_TEXT_CONTRAST,
                STATE_CONSECUTIVE_DELTA_E,
            )],
        )
        .map_err(|error| Error(format!("active search match: {error}")))?;
    let document_read = search
        .fit_state(
            &semantic.accent,
            std::slice::from_ref(&canvas),
            STATE_SELECTED_CONTRAST,
            STATE_SELECTED_DELTA_E,
            &[(
                editor_primary.clone(),
                EDITOR_OVERLAY_TEXT_CONTRAST,
                STATE_CONSECUTIVE_DELTA_E,
            )],
        )
        .map_err(|error| Error(format!("document read highlight: {error}")))?;
    let document_write = search
        .fit_state(
            &semantic.orange,
            std::slice::from_ref(&canvas),
            STATE_SELECTED_CONTRAST,
            STATE_SELECTED_DELTA_E,
            &[(
                editor_primary.clone(),
                EDITOR_OVERLAY_TEXT_CONTRAST,
                STATE_CONSECUTIVE_DELTA_E,
            )],
        )
        .map_err(|error| Error(format!("document write highlight: {error}")))?;
    let document_bracket = search
        .fit_state(
            &semantic.cyan,
            std::slice::from_ref(&canvas),
            STATE_SELECTED_CONTRAST,
            STATE_SELECTED_DELTA_E,
            &[(
                editor_primary.clone(),
                EDITOR_OVERLAY_TEXT_CONTRAST,
                STATE_CONSECUTIVE_DELTA_E,
            )],
        )
        .map_err(|error| Error(format!("document bracket highlight: {error}")))?;

    // Diff colors are derived as a dedicated semantic subsystem because diff viewers
    // combine text, fills, hollow borders, selections, and conflict overlays.
    let diff_constraints = PairConstraints {
        foreground_contrast: DIFF_FILL_CONTRAST,
        pair_contrast: DIFF_PAIR_CONTRAST,
        normal_delta: DIFF_NORMAL_FLOOR_DELTA_E,
        cvd_delta: DIFF_CVD_FLOOR_DELTA_E,
        lightness_delta: 0.0,
        separation_alternative: Some((
            DIFF_LUMINANCE_SEPARATION_CONTRAST,
            DIFF_NORMAL_DELTA_E,
            DIFF_CVD_DELTA_E,
        )),
        prefer_background: true,
    };
    let readable_diff_text = [(editor_primary.clone(), EDITOR_OVERLAY_TEXT_CONTRAST)];
    let [diff_added, diff_deleted] = search
        .fit_pair_on_backgrounds_readable(
            color(palette, "green"),
            &editor_bases,
            color(palette, "red"),
            &editor_bases,
            diff_constraints,
            &readable_diff_text,
        )
        .map_err(|error| Error(format!("solid diff hunks: {error}")))?;
    let [diff_added_hollow, diff_deleted_hollow] = search
        .fit_pair_on_backgrounds_readable(
            color(palette, "green"),
            &editor_bases,
            color(palette, "red"),
            &editor_bases,
            PairConstraints {
                foreground_contrast: DIFF_HOLLOW_CONTRAST,
                ..diff_constraints
            },
            &readable_diff_text,
        )
        .map_err(|error| Error(format!("hollow diff hunks: {error}")))?;
    let [word_added, word_deleted] = search
        .fit_pair_on_backgrounds_readable(
            color(palette, "green"),
            std::slice::from_ref(&diff_added),
            color(palette, "red"),
            std::slice::from_ref(&diff_deleted),
            PairConstraints {
                foreground_contrast: DIFF_FILL_CONTRAST,
                ..diff_constraints
            },
            &readable_diff_text,
        )
        .map_err(|error| Error(format!("word diff backgrounds: {error}")))?;
    let [conflict_ours, conflict_theirs] = search
        .fit_pair_on_backgrounds_readable(
            color(palette, "blue"),
            &editor_bases,
            color(palette, "magenta"),
            &editor_bases,
            PairConstraints {
                foreground_contrast: DIFF_FILL_CONTRAST,
                ..diff_constraints
            },
            &readable_diff_text,
        )
        .map_err(|error| Error(format!("conflict backgrounds: {error}")))?;
    let yank = search
        .fit_state(
            &semantic.yellow,
            std::slice::from_ref(&canvas),
            STATE_SELECTED_CONTRAST,
            STATE_SELECTED_DELTA_E,
            &[(
                editor_primary.clone(),
                EDITOR_OVERLAY_TEXT_CONTRAST,
                STATE_CONSECUTIVE_DELTA_E,
            )],
        )
        .map_err(|error| Error(format!("Vim yank highlight: {error}")))?;
    let editor_text_backgrounds = unique(editor_bases.iter().cloned().chain([
        search_match.clone(),
        search_active.clone(),
        document_read.clone(),
        document_write.clone(),
        document_bracket.clone(),
        yank.clone(),
        diff_added.clone(),
        diff_added_hollow.clone(),
        diff_deleted.clone(),
        diff_deleted_hollow.clone(),
        word_added.clone(),
        word_deleted.clone(),
        conflict_ours.clone(),
        conflict_theirs.clone(),
    ]));
    let selection = search
        .fit_fill_readable(
            color(palette, "selection"),
            FillRequest {
                backgrounds: &editor_text_backgrounds,
                target: FOCUSED_SELECTION_CONTRAST,
                minimum_delta_e: FOCUSED_SELECTION_DELTA_E,
                runtime_state: Some((0.5, 1.08, 0.020)),
                readable_foregrounds: &[(editor_primary.clone(), TEXT_CONTRAST)],
                rendered_references: &[],
                runtime_rendered_references: &[],
            },
        )
        .map_err(|error| Error(format!("focused selection: {error}")))?;
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
        let request = || FillRequest {
            backgrounds: &editor_text_backgrounds,
            target: FOCUSED_SELECTION_CONTRAST,
            minimum_delta_e: FOCUSED_SELECTION_DELTA_E,
            runtime_state: Some((0.5, 1.08, 0.020)),
            readable_foregrounds: &readable,
            rendered_references: &references,
            runtime_rendered_references: &[],
        };
        let fitted = if let Some(exact) = search.fit_exact_fill_readable(cursor, request())? {
            exact
        } else if let Ok(fitted) = search.fit_fill_readable(cursor, request()) {
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
        players.push(BTreeMap::from([
            ("cursor".into(), cursor.clone()),
            ("background".into(), background),
            ("selection".into(), selection),
        ]));
    }

    let local_selection = gpui_blend(&canvas, &players[0]["selection"])?.opaque_hex();
    let local_unfocused_selection =
        gpui_blend(&canvas, &apply_opacity(&players[0]["selection"], 0.5)?)?.opaque_hex();
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

    let status_seeds: BTreeMap<&str, &String> = BTreeMap::from([
        ("conflict", &semantic.orange),
        ("created", &semantic.green),
        ("deleted", &semantic.red),
        ("error", &semantic.red),
        ("hidden", &semantic.disabled),
        ("hint", &semantic.cyan),
        ("ignored", &semantic.secondary),
        ("info", &semantic.blue),
        ("modified", &semantic.yellow),
        ("predictive", &semantic.secondary),
        ("renamed", &semantic.blue),
        ("success", &semantic.green),
        ("unreachable", &semantic.secondary),
        ("warning", &semantic.yellow),
    ]);
    let mut status_backgrounds = BTreeMap::new();
    for name in STATUS_NAMES {
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
        let status_foreground_backgrounds = unique(
            interaction_bases
                .iter()
                .chain(editor_text_backgrounds.iter())
                .cloned()
                .chain(std::iter::once(status_backgrounds[name].clone())),
        );
        status_foregrounds.insert(
            *name,
            search.fit_color(seed, &status_foreground_backgrounds, TEXT_CONTRAST)?,
        );
    }
    let mut statuses = BTreeMap::new();
    for name in STATUS_NAMES {
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

    let overlay_contexts = unique([
        search_match.clone(),
        search_active.clone(),
        document_read.clone(),
        document_write.clone(),
        document_bracket.clone(),
        diff_added.clone(),
        diff_added_hollow.clone(),
        diff_deleted.clone(),
        diff_deleted_hollow.clone(),
        word_added.clone(),
        word_deleted.clone(),
        conflict_ours.clone(),
        conflict_theirs.clone(),
        yank.clone(),
    ]);
    let focused_selections: Vec<_> = editor_bases
        .iter()
        .flat_map(|base| {
            players
                .iter()
                .map(|player| gpui_blend(base, &player["selection"]).unwrap().opaque_hex())
        })
        .collect();
    let local_unfocused: Vec<_> = editor_bases
        .iter()
        .chain(&overlay_contexts)
        .map(|base| {
            gpui_blend(base, &apply_opacity(&players[0]["selection"], 0.5).unwrap())
                .unwrap()
                .opaque_hex()
        })
        .collect();
    let chained: Vec<_> = overlay_contexts
        .iter()
        .flat_map(|overlay| {
            players.iter().map(|player| {
                gpui_blend(overlay, &player["selection"])
                    .unwrap()
                    .opaque_hex()
            })
        })
        .collect();
    let syntax_contexts = unique(
        editor_bases
            .iter()
            .cloned()
            .chain(overlay_contexts.iter().cloned())
            .chain(focused_selections)
            .chain(local_unfocused)
            .chain(chained),
    );

    let syntax = build_syntax(&mut search, palette, &syntax_contexts, &mut audit)?;

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

    let drop_target = search.fit_state(
        &semantic.accent,
        std::slice::from_ref(&surface),
        STATE_SELECTED_CONTRAST,
        STATE_SELECTED_DELTA_E,
        &[],
    )?;
    let drop_target_border = search.fit_color(
        &semantic.accent,
        &[surface.clone(), drop_target.clone()],
        CONTROL_CONTRAST,
    )?;
    let thumb_contexts = unique([chrome.clone(), surface.clone(), canvas.clone()]);
    let thumb_ladder = search.fit_state_ladder(
        &semantic.primary,
        &thumb_contexts,
        &[
            (CONTROL_CONTRAST, STATE_SELECTED_DELTA_E),
            (THUMB_HOVER_CONTRAST, STATE_SELECTED_DELTA_E),
            (THUMB_ACTIVE_CONTRAST, STATE_SELECTED_DELTA_E),
        ],
        &[],
    )?;
    let thumb_border = semantic.structural.clone();
    let track_border = search.fit_color(&semantic.passive, &thumb_contexts, PASSIVE_CONTRAST)?;

    let mut fixed = BTreeMap::<String, String>::new();
    macro_rules! put {
        ($name:expr, $value:expr) => {
            fixed.insert($name.into(), $value);
        };
    }
    put!("border", semantic.structural.clone());
    put!("border.variant", semantic.structural.clone());
    put!("border.focused", semantic.accent.clone());
    put!("border.selected", semantic.accent.clone());
    put!(
        "border.transparent",
        apply_opacity(&semantic.structural, 0.0)?
    );
    put!("border.disabled", semantic.passive.clone());
    put!("elevated_surface.background", elevated.clone());
    put!("surface.background", surface.clone());
    put!("background", canvas.clone());
    put!("element.background", surface.clone());
    put!("element.hover", element_hover);
    put!("element.active", element_active);
    put!("element.selected", element_selected);
    put!("element.disabled", chrome.clone());
    put!("element.selection_background", element_selection);
    put!("drop_target.background", drop_target);
    put!("drop_target.border", drop_target_border);
    put!("ghost_element.background", apply_opacity(&canvas, 0.0)?);
    put!("ghost_element.hover", ghost_hover);
    put!("ghost_element.active", ghost_active);
    put!("ghost_element.selected", ghost_selected);
    put!("ghost_element.disabled", chrome.clone());
    put!("text", semantic.primary.clone());
    put!("text.muted", semantic.secondary.clone());
    put!("text.placeholder", semantic.secondary.clone());
    put!("text.disabled", semantic.disabled.clone());
    put!(
        "text.accent",
        search.fit_color(&semantic.accent, &ui_backgrounds, TEXT_CONTRAST)?
    );
    put!("icon", semantic.primary.clone());
    put!(
        "icon.muted",
        search.fit_color(color(palette, "muted"), &ui_backgrounds, CONTROL_CONTRAST)?
    );
    put!(
        "icon.disabled",
        search.fit_color(
            color(palette, "dark_foreground"),
            &ui_backgrounds,
            CONTROL_CONTRAST
        )?
    );
    put!(
        "icon.placeholder",
        search.fit_color(color(palette, "muted"), &ui_backgrounds, CONTROL_CONTRAST)?
    );
    put!("icon.accent", semantic.accent.clone());
    put!("debugger.accent", semantic.red.clone());
    put!("status_bar.background", chrome.clone());
    put!("title_bar.background", chrome.clone());
    put!("title_bar.inactive_background", sunken);
    put!("toolbar.background", surface.clone());
    put!("tab_bar.background", chrome.clone());
    put!("tab.inactive_background", chrome.clone());
    put!("tab.active_background", tab_active);
    put!("search.match_background", search_match);
    put!("search.active_match_background", search_active);
    put!("panel.background", surface.clone());
    put!("panel.focused_border", semantic.accent.clone());
    put!("panel.indent_guide", panel_guide_ladder[0].clone());
    put!("panel.indent_guide_hover", panel_guide_ladder[1].clone());
    put!("panel.indent_guide_active", panel_guide_ladder[2].clone());
    put!("panel.overlay_background", panel_overlay);
    put!("panel.overlay_hover", panel_overlay_hover);
    put!("pane.focused_border", semantic.accent.clone());
    put!("pane_group.border", semantic.structural.clone());
    put!("scrollbar.thumb.background", thumb_ladder[0].clone());
    put!("scrollbar.thumb.hover_background", thumb_ladder[1].clone());
    put!("scrollbar.thumb.active_background", thumb_ladder[2].clone());
    put!("scrollbar.thumb.border", thumb_border.clone());
    put!("scrollbar.track.background", apply_opacity(&chrome, 0.0)?);
    put!("scrollbar.track.border", track_border);
    put!("minimap.thumb.background", thumb_ladder[0].clone());
    put!("minimap.thumb.hover_background", thumb_ladder[1].clone());
    put!("minimap.thumb.active_background", thumb_ladder[2].clone());
    put!("minimap.thumb.border", thumb_border);
    put!("editor.foreground", editor_primary);
    put!("editor.background", canvas.clone());
    put!("editor.gutter.background", canvas.clone());
    put!("editor.subheader.background", chrome);
    put!("editor.active_line.background", editor_active_line);
    put!(
        "editor.highlighted_line.background",
        editor_highlighted_line
    );
    put!("editor.debugger_active_line.background", debugger_active);
    put!("editor.line_number", semantic.secondary.clone());
    put!("editor.active_line_number", semantic.primary.clone());
    put!("editor.hover_line_number", semantic.primary.clone());
    put!(
        "editor.invisible",
        search.fit_color(
            color(palette, "muted"),
            std::slice::from_ref(&canvas),
            CONTROL_CONTRAST
        )?
    );
    put!(
        "editor.wrap_guide",
        search.fit_color(
            &semantic.passive,
            std::slice::from_ref(&canvas),
            PASSIVE_CONTRAST
        )?
    );
    put!(
        "editor.active_wrap_guide",
        search.fit_color(
            &semantic.structural,
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
    put!("editor.document_highlight.read_background", document_read);
    put!("editor.document_highlight.write_background", document_write);
    put!(
        "editor.document_highlight.bracket_background",
        document_bracket
    );
    put!("editor.diff_hunk.added.background", diff_added);
    put!(
        "editor.diff_hunk.added.hollow_background",
        diff_added_hollow.clone()
    );
    put!(
        "editor.diff_hunk.added.hollow_border",
        search.fit_color(
            &semantic.green,
            &[canvas.clone(), diff_added_hollow.clone()],
            CONTROL_CONTRAST
        )?
    );
    put!("editor.diff_hunk.deleted.background", diff_deleted);
    put!(
        "editor.diff_hunk.deleted.hollow_background",
        diff_deleted_hollow.clone()
    );
    put!(
        "editor.diff_hunk.deleted.hollow_border",
        search.fit_color(
            &semantic.red,
            &[canvas.clone(), diff_deleted_hollow.clone()],
            CONTROL_CONTRAST
        )?
    );
    fixed.extend(terminal);
    put!(
        "link_text.hover",
        search.fit_color(&semantic.accent, &ui_backgrounds, TEXT_CONTRAST)?
    );
    put!("version_control.added", semantic.green.clone());
    put!("version_control.deleted", semantic.red.clone());
    put!("version_control.modified", semantic.yellow.clone());
    put!("version_control.renamed", semantic.blue.clone());
    put!("version_control.conflict", semantic.orange.clone());
    put!("version_control.ignored", semantic.secondary.clone());
    put!("version_control.word_added", word_added);
    put!("version_control.word_deleted", word_deleted);
    put!("version_control.conflict_marker.ours", conflict_ours);
    put!("version_control.conflict_marker.theirs", conflict_theirs);
    fixed.extend(vim);

    let mut style = Map::new();
    style.insert("background.appearance".into(), "opaque".into());
    style.extend(fixed.into_iter().map(|(key, value)| (key, value.into())));
    style.extend(statuses.into_iter().map(|(key, value)| (key, value.into())));
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
    validate_theme(
        &document,
        ValidationContexts {
            ui_backgrounds: &ui_backgrounds,
            interaction_bases: &interaction_bases,
            syntax_contexts: &syntax_contexts,
            editor_bases: &editor_bases,
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
        editor_text_backgrounds,
        terminal_backgrounds,
    } = contexts;
    let style = style(document)?;
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
    let editor_label_fields = [
        "editor.line_number",
        "editor.active_line_number",
        "editor.hover_line_number",
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
        (&editor_label_fields[..], editor_bases),
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
    let editor_foreground_minimum = minimum_contrast(editor_foreground, editor_text_backgrounds)?;
    let editor_base_minimum = minimum_contrast(editor_foreground, editor_bases)?;
    if editor_base_minimum < EDITOR_BASE_TEXT_CONTRAST - 1e-9 {
        errors.push(format!(
            "editor.foreground reserve on base surfaces is only {editor_base_minimum:.3}:1"
        ));
    }
    if editor_foreground_minimum < EDITOR_OVERLAY_TEXT_CONTRAST - 1e-9 {
        errors.push(format!(
            "editor.foreground reserve on editor overlays is only {editor_foreground_minimum:.3}:1"
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
    let syntax_tier = match audit.syntax_richness.get("tier").and_then(Value::as_str) {
        Some("baseline") => Tier::Baseline,
        Some("restrained") => Tier::Restrained,
        Some("broad") => Tier::Broad,
        Some("rich") => Tier::Rich,
        _ => return Err(Error("syntax richness tier is missing".into())),
    };
    for (name, spec) in syntax {
        let value = spec
            .get("color")
            .and_then(Value::as_str)
            .ok_or_else(|| Error(format!("syntax role {name} has no color")))?;
        let actual = minimum_contrast(value, syntax_contexts)?;
        syntax_minimum = syntax_minimum.min(actual);
        let target = contrast_floor(name, syntax_tier) - 0.02;
        if actual < target - 1e-9 {
            errors.push(format!(
                "syntax.{name} reaches only {actual:.3}:1; floor is {target:.2}:1"
            ));
        }
    }
    let syntax_color = |name: &str| -> Result<&str> {
        syntax
            .get(name)
            .and_then(|spec| spec.get("color"))
            .and_then(Value::as_str)
            .ok_or_else(|| Error(format!("syntax role {name} has no color")))
    };
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
            "editor.wrap_guide",
            "editor.background",
            HARD_PASSIVE_CONTRAST,
        ),
        (
            "editor.active_wrap_guide",
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
        (
            "editor.diff_hunk.added.hollow_border",
            "editor.background",
            HARD_CONTROL_CONTRAST,
        ),
        (
            "editor.diff_hunk.deleted.hollow_border",
            "editor.background",
            HARD_CONTROL_CONTRAST,
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
            STATE_SELECTED_CONTRAST,
            STATE_SELECTED_DELTA_E,
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
            STATE_SELECTED_CONTRAST,
            STATE_SELECTED_DELTA_E,
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
            STATE_SELECTED_CONTRAST,
            STATE_SELECTED_DELTA_E,
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
            STATE_SELECTED_CONTRAST,
            STATE_SELECTED_DELTA_E,
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
        (
            "element",
            ["element.hover", "element.active", "element.selected"],
        ),
        (
            "ghost_element",
            [
                "ghost_element.hover",
                "ghost_element.active",
                "ghost_element.selected",
            ],
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
            ["element.hover", "element.active", "element.selected"],
            false,
        ),
        (
            "ghost_element",
            "background",
            [
                "ghost_element.hover",
                "ghost_element.active",
                "ghost_element.selected",
            ],
            false,
        ),
        (
            "panel.indent_guide",
            "panel.background",
            [
                "panel.indent_guide",
                "panel.indent_guide_hover",
                "panel.indent_guide_active",
            ],
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

    // Diff validation is intentionally redundant with general validation: these roles
    // are too important to be weakened accidentally by future projection changes.
    let diff_fills = [
        ("editor.diff_hunk.added.background", DIFF_FILL_CONTRAST),
        (
            "editor.diff_hunk.added.hollow_background",
            DIFF_HOLLOW_CONTRAST,
        ),
        ("editor.diff_hunk.deleted.background", DIFF_FILL_CONTRAST),
        (
            "editor.diff_hunk.deleted.hollow_background",
            DIFF_HOLLOW_CONTRAST,
        ),
        ("version_control.conflict_marker.ours", DIFF_FILL_CONTRAST),
        ("version_control.conflict_marker.theirs", DIFF_FILL_CONTRAST),
    ];
    let mut diff_fill_minimum = f64::INFINITY;
    for (name, target) in diff_fills {
        let actual = editor_bases
            .iter()
            .map(|base| contrast_ratio(style_color(style, name).unwrap(), base).unwrap())
            .fold(f64::INFINITY, f64::min);
        diff_fill_minimum = diff_fill_minimum.min(actual);
        if actual < target - 1e-9 {
            errors.push(format!("diff fill {name} reaches only {actual:.3}:1"));
        }
    }
    for (word, hunk) in [
        (
            "version_control.word_added",
            "editor.diff_hunk.added.background",
        ),
        (
            "version_control.word_deleted",
            "editor.diff_hunk.deleted.background",
        ),
    ] {
        let actual = contrast_ratio(style_color(style, word)?, style_color(style, hunk)?)?;
        if actual < DIFF_FILL_CONTRAST - 1e-9 {
            errors.push(format!(
                "word diff {word} reaches only {actual:.3}:1 against its hunk"
            ));
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
    for (family, first, second) in [
        (
            "hunk.solid",
            "editor.diff_hunk.added.background",
            "editor.diff_hunk.deleted.background",
        ),
        (
            "hunk.hollow",
            "editor.diff_hunk.added.hollow_background",
            "editor.diff_hunk.deleted.hollow_background",
        ),
        (
            "word",
            "version_control.word_added",
            "version_control.word_deleted",
        ),
        (
            "conflict",
            "version_control.conflict_marker.ours",
            "version_control.conflict_marker.theirs",
        ),
    ] {
        let first_value = style_color(style, first)?;
        let second_value = style_color(style, second)?;
        let normal = delta_e(first_value, second_value)?;
        let cvd = crate::search::cvd_distance(first_value, second_value)?;
        let contrast = contrast_ratio(first_value, second_value)?;
        audit.diff_metrics.push(json!({"family": family, "first": first_value, "second": second_value, "normal_delta_e": round6(normal), "cvd_delta_e": round6(cvd), "pair_contrast": round6(contrast)}));
        let strong_separation = contrast >= DIFF_LUMINANCE_SEPARATION_CONTRAST - 1e-9
            || (normal >= DIFF_NORMAL_DELTA_E - 1e-9 && cvd >= DIFF_CVD_DELTA_E - 1e-9);
        if normal < DIFF_NORMAL_FLOOR_DELTA_E - 1e-9
            || cvd < DIFF_CVD_FLOOR_DELTA_E - 1e-9
            || contrast < DIFF_PAIR_CONTRAST - 1e-9
            || !strong_separation
        {
            errors.push(format!("diff {family} pair is ambiguous: contrast {contrast:.3}, delta E {normal:.3}, CVD {cvd:.3}"));
        }
    }
    for (border, fill) in [
        (
            "editor.diff_hunk.added.hollow_border",
            "editor.diff_hunk.added.hollow_background",
        ),
        (
            "editor.diff_hunk.deleted.hollow_border",
            "editor.diff_hunk.deleted.hollow_background",
        ),
    ] {
        let actual = contrast_ratio(style_color(style, border)?, style_color(style, fill)?)?;
        if actual < HARD_CONTROL_CONTRAST - 1e-9 {
            errors.push(format!("{border} against {fill} is only {actual:.3}:1"));
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
        .filter_map(|degradation| degradation.get("role").and_then(Value::as_str))
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
        for background in editor_text_backgrounds {
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
    if errors.is_empty() {
        Ok(())
    } else {
        Err(Error(format!(
            "theme validation failed:\n  - {}",
            errors.join("\n  - ")
        )))
    }
}
