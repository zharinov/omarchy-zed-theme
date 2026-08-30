//! Adapts syntax color variety to the distinguishable colors in the Omarchy palette.
//!
//! Neutral palettes use the built-in light or dark syntax baseline. Authored colors
//! replace those fallbacks only when they remain distinct and readable. This module
//! never supplies colors to the Zed interface.

use crate::Result;
use crate::color::{contrast_ratio, delta_e, lab, oklab_to_oklch};
use crate::constants::SYNTAX_DIFF_CONTRACT;
use crate::palette::{Provenance, ResolvedPalette};
use crate::search::{PairConstraints, Search, cvd_distance, round6};
use crate::theme::Audit;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};

const SOURCE_KEYS: [&str; 8] = [
    "green", "blue", "magenta", "yellow", "red", "cyan", "orange", "accent",
];
const SLOT_KEYS: [&str; 8] = [
    "declaration",
    "value",
    "string",
    "metadata",
    "link",
    "special",
    "type",
    "danger",
];
const SLOT_SOURCE: [&str; 8] = [
    "blue", "magenta", "green", "yellow", "accent", "orange", "cyan", "red",
];
pub const SYNTAX_PRIMARY_FLOOR: f64 = 4.52;
pub const SYNTAX_SEMANTIC_FLOOR: f64 = 3.52;
pub const SYNTAX_SUBDUED_FLOOR: f64 = 3.02;

const LIGHT_FALLBACKS: [&str; 10] = [
    "#325cc0", "#7a3e9d", "#448c27", "#702c00", "#1f6ae2", "#7a3e9d", "#325cc0", "#db0a37",
    "#7c7c7c", "#454c54",
];
const DARK_FALLBACKS: [&str; 10] = [
    "#7aa7e6", "#b49bd8", "#82b56d", "#c9a172", "#5ba3f5", "#b49bd8", "#7aa7e6", "#e0757e",
    "#71808d", "#95a3b0",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Tier {
    Baseline,
    Restrained,
    Broad,
    Rich,
}

impl Tier {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Baseline => "baseline",
            Self::Restrained => "restrained",
            Self::Broad => "broad",
            Self::Rich => "rich",
        }
    }
}

struct Richness {
    tier: Tier,
    clique: BTreeSet<String>,
    audit: Value,
}

fn authored_richness(palette: &ResolvedPalette) -> Result<Richness> {
    let mut candidates = Vec::new();

    for key in SOURCE_KEYS {
        let value = &palette.colors[key];
        let [lightness, chroma, hue] = oklab_to_oklch(lab(value)?);
        let provenance = palette
            .provenance
            .get(key)
            .copied()
            .unwrap_or(Provenance::Derived);
        if provenance != Provenance::Derived && chroma >= 0.025 - 1e-12 {
            candidates.push((key, value.clone(), lightness, chroma, hue, provenance));
        }
    }

    let mut normal = vec![vec![0.0; candidates.len()]; candidates.len()];
    let mut cvd = normal.clone();

    for left in 0..candidates.len() {
        for right in left + 1..candidates.len() {
            normal[left][right] = delta_e(&candidates[left].1, &candidates[right].1)?;
            normal[right][left] = normal[left][right];
            cvd[left][right] = cvd_distance(&candidates[left].1, &candidates[right].1)?;
            cvd[right][left] = cvd[left][right];
        }
    }

    let mut best_mask = 0usize;
    let mut best_size = 0u32;
    let mut best_priority = 0u16;
    let mut best_distance = f64::NEG_INFINITY;
    'masks: for mask in 0usize..(1usize << candidates.len()) {
        let mut distance = 0.0;

        for left in 0..candidates.len() {
            if mask & (1 << left) == 0 {
                continue;
            }

            for right in left + 1..candidates.len() {
                if mask & (1 << right) == 0 {
                    continue;
                }

                if normal[left][right] < 0.060 - 1e-12 || cvd[left][right] < 0.030 - 1e-12 {
                    continue 'masks;
                }

                distance += cvd[left][right];
            }
        }

        let size = mask.count_ones();
        let priority = (0..candidates.len()).fold(0u16, |score, index| {
            let key_priority = SOURCE_KEYS
                .iter()
                .position(|key| *key == candidates[index].0)
                .unwrap();
            score | (((mask >> index) & 1) as u16) << (7 - key_priority)
        });

        if (size, priority) > (best_size, best_priority)
            || ((size, priority) == (best_size, best_priority) && distance > best_distance)
        {
            best_mask = mask;
            best_size = size;
            best_priority = priority;
            best_distance = distance;
        }
    }

    let clique: BTreeSet<_> = candidates
        .iter()
        .enumerate()
        .filter(|(index, _)| best_mask & (1 << index) != 0)
        .map(|(_, candidate)| candidate.0.to_owned())
        .collect();

    let tier = match best_size {
        0 | 1 => Tier::Baseline,
        2..=4 => Tier::Restrained,
        5 => Tier::Broad,
        _ => Tier::Rich,
    };
    let candidate_audit: Vec<_> = candidates.iter().map(|(key, value, lightness, chroma, hue, provenance)| json!({
        "key": key, "value": value, "provenance": format!("{provenance:?}").to_ascii_lowercase(),
        "oklch": [round6(*lightness), round6(*chroma), round6(*hue)],
    })).collect();

    Ok(Richness {
        tier,
        clique: clique.clone(),
        audit: json!({
            "thresholds": {"chroma": 0.025, "normal_delta_e": 0.060, "cvd_delta_e": 0.030},
            "candidates": candidate_audit, "winning_clique": clique, "R": best_size, "tier": tier.as_str(),
            "normal_matrix": normal, "cvd_matrix": cvd,
        }),
    })
}

pub fn build_syntax(
    search: &mut Search,
    palette: &ResolvedPalette,
    contexts: &[String],
    audit: &mut Audit,
) -> Result<Map<String, Value>> {
    let richness = authored_richness(palette)?;
    audit.syntax_richness = richness.audit;

    let fallbacks = if palette.mode == "dark" {
        DARK_FALLBACKS
    } else {
        LIGHT_FALLBACKS
    };

    let mut fallback_outputs = Vec::new();
    let mut source_outputs: Vec<Option<String>> = Vec::new();

    for index in 0..SLOT_KEYS.len() {
        fallback_outputs.push(search.fit_color(
            fallbacks[index],
            contexts,
            SYNTAX_SEMANTIC_FLOOR,
        )?);
        let source_key = SLOT_SOURCE[index];
        let source = &palette.colors[source_key];
        let source_lch = oklab_to_oklch(lab(source)?);

        let output = if richness.clique.contains(source_key) {
            let fitted = search.fit_color(source, contexts, SYNTAX_SEMANTIC_FLOOR)?;
            let fitted_lch = oklab_to_oklch(lab(&fitted)?);
            (fitted_lch[1] >= 0.025 - 1e-12
                && fitted_lch[1] / source_lch[1].max(1e-12) >= 0.35 - 1e-12)
                .then_some(fitted)
        } else {
            None
        };
        source_outputs.push(output);
    }

    let metric_colors: Vec<String> = fallback_outputs
        .iter()
        .chain(source_outputs.iter().flatten())
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let metric_indices: BTreeMap<&str, usize> = metric_colors
        .iter()
        .enumerate()
        .map(|(index, color)| (color.as_str(), index))
        .collect();

    let mut normal_matrix = vec![vec![0.0; metric_colors.len()]; metric_colors.len()];
    let mut cvd_matrix = normal_matrix.clone();

    for left in 0..metric_colors.len() {
        for right in left + 1..metric_colors.len() {
            normal_matrix[left][right] = delta_e(&metric_colors[left], &metric_colors[right])?;
            normal_matrix[right][left] = normal_matrix[left][right];
            cvd_matrix[left][right] = cvd_distance(&metric_colors[left], &metric_colors[right])?;
            cvd_matrix[right][left] = cvd_matrix[left][right];
        }
    }

    let mut best_mask = 0usize;
    let mut best_count = 0u32;
    let mut best_minimum = f64::NEG_INFINITY;
    let mut best_transform = f64::INFINITY;
    'masks: for mask in 0usize..256 {
        if (0..8).any(|index| mask & (1 << index) != 0 && source_outputs[index].is_none()) {
            continue;
        }

        let outputs: Vec<&String> = (0..8)
            .map(|index| {
                if mask & (1 << index) != 0 {
                    source_outputs[index].as_ref().unwrap()
                } else {
                    &fallback_outputs[index]
                }
            })
            .collect();

        let mut minimum = f64::INFINITY;

        for left in 0..8 {
            for right in left + 1..8 {
                if outputs[left] == outputs[right] {
                    if mask & (1 << left) != 0 || mask & (1 << right) != 0 {
                        continue 'masks;
                    }

                    continue;
                }

                let left_metric = metric_indices[outputs[left].as_str()];
                let right_metric = metric_indices[outputs[right].as_str()];
                let normal = normal_matrix[left_metric][right_metric];
                let cvd = cvd_matrix[left_metric][right_metric];

                if (mask & (1 << left) != 0 || mask & (1 << right) != 0)
                    && (normal < 0.035 - 1e-12 || cvd < 0.020 - 1e-12)
                {
                    continue 'masks;
                }

                minimum = minimum.min(normal.min(cvd));
            }
        }

        let count = mask.count_ones();
        let transform = (0..8)
            .filter(|index| mask & (1 << index) != 0)
            .map(|index| {
                delta_e(
                    &palette.colors[SLOT_SOURCE[index]],
                    source_outputs[index].as_ref().unwrap(),
                )
                .unwrap()
            })
            .sum::<f64>();

        if count > best_count
            || (count == best_count
                && (minimum > best_minimum
                    || (minimum == best_minimum && transform < best_transform)))
        {
            best_mask = mask;
            best_count = count;
            best_minimum = minimum;
            best_transform = transform;
        }
    }

    let mut slots = BTreeMap::new();
    let chosen_outputs: Vec<_> = (0..8)
        .map(|index| {
            let source_selected = best_mask & (1 << index) != 0;
            let output = if source_selected {
                source_outputs[index].clone().unwrap()
            } else {
                fallback_outputs[index].clone()
            };
            (source_selected, output)
        })
        .collect();

    for index in 0..8 {
        let (source_selected, output) = &chosen_outputs[index];
        let source_key = SLOT_SOURCE[index];
        let source = &palette.colors[source_key];
        let output_lch = oklab_to_oklch(lab(output)?);
        let source_lch = oklab_to_oklch(lab(source)?);

        let disposition = if *source_selected && output == source {
            "source"
        } else if *source_selected {
            "source_repaired"
        } else if source_outputs[index].is_some() {
            "collision_fallback"
        } else {
            "baseline_fallback"
        };

        let hue_drift = if source_lch[1] >= 0.005 && output_lch[1] >= 0.005 {
            let difference = (source_lch[2] - output_lch[2]).abs();
            Some(difference.min(std::f64::consts::TAU - difference))
        } else {
            None
        };
        let minimum_normal_separation = chosen_outputs
            .iter()
            .enumerate()
            .filter(|(other, _)| *other != index)
            .map(|(_, (_, other))| delta_e(output, other).unwrap())
            .fold(f64::INFINITY, f64::min);
        let minimum_cvd_separation = chosen_outputs
            .iter()
            .enumerate()
            .filter(|(other, _)| *other != index)
            .map(|(_, (_, other))| cvd_distance(output, other).unwrap())
            .fold(f64::INFINITY, f64::min);

        audit.syntax_roles.push(json!({
            "slot": SLOT_KEYS[index], "fallback_seed": fallbacks[index], "source_key": source_key,
            "source": source, "output": output, "disposition": disposition,
            "source_output_delta_e": round6(delta_e(source, output)?),
            "hue_drift": hue_drift.map(round6),
            "chroma_retention": round6(output_lch[1] / source_lch[1].max(1e-12)),
            "minimum_contrast": round6(contexts.iter().map(|context| contrast_ratio(output, context).unwrap()).fold(f64::INFINITY, f64::min)),
            "minimum_normal_separation": round6(minimum_normal_separation),
            "minimum_cvd_separation": round6(minimum_cvd_separation),
        }));

        if disposition != "source" {
            audit.fidelity_deviations.push(json!({
                "role": format!("syntax.{}", SLOT_KEYS[index]), "source": source, "output": output,
                "reason": disposition, "delta_e": round6(delta_e(source, output)?),
            }));
        }

        slots.insert(SLOT_KEYS[index], output.clone());
    }

    let base = search.fit_color(
        &palette.colors["foreground"],
        contexts,
        SYNTAX_PRIMARY_FLOOR,
    )?;
    let authored_subdued =
        search.fit_color(&palette.colors["muted"], contexts, SYNTAX_SUBDUED_FLOOR)?;

    let subdued = if delta_e(&base, &authored_subdued)? >= 0.035
        && cvd_distance(&base, &authored_subdued)? >= 0.020
    {
        authored_subdued
    } else {
        search.fit_color(fallbacks[8], contexts, SYNTAX_SUBDUED_FLOOR)?
    };

    let hint = search.fit_color(fallbacks[9], contexts, SYNTAX_SEMANTIC_FLOOR)?;

    let add_seed = if richness.clique.contains("green") {
        &palette.colors["green"]
    } else {
        fallbacks[2]
    };

    let delete_seed = if richness.clique.contains("red") {
        &palette.colors["red"]
    } else {
        fallbacks[7]
    };

    let pair_constraints = PairConstraints {
        foreground_contrast: SYNTAX_SEMANTIC_FLOOR,
        pair_contrast: SYNTAX_DIFF_CONTRACT.contrast,
        normal_delta: SYNTAX_DIFF_CONTRACT.normal_delta_e,
        cvd_delta: SYNTAX_DIFF_CONTRACT.cvd_delta_e,
        lightness_delta: 0.0,
        separation_alternative: SYNTAX_DIFF_CONTRACT.separation_alternative,
        prefer_background: false,
    };

    let [syntax_add, syntax_delete] = match search.fit_pair(
        add_seed,
        delete_seed,
        contexts,
        pair_constraints,
    ) {
        Ok(pair) => pair,
        Err(_) => {
            audit.syntax_collapses.push(json!({
                "roles": ["syntax-add", "syntax-delete"], "reason": "authored_pair_failed_hard_invariants",
                "fallback": "baseline",
            }));
            search.fit_pair(fallbacks[2], fallbacks[7], contexts, pair_constraints)?
        }
    };

    slots.insert("base", base);
    slots.insert("subdued", subdued);
    slots.insert("hint", hint);
    slots.insert("syntax-add", syntax_add);
    slots.insert("syntax-delete", syntax_delete);

    let mut output = Map::new();
    for capture in crate::constants::BASE_SYNTAX_FIELDS
        .iter()
        .chain(crate::constants::ADDITIONAL_SYNTAX_FIELDS)
    {
        let group = capture_group(capture, richness.tier);
        let (style, weight) = capture_style(capture);
        let mut spec = Map::from_iter([("color".into(), slots[group].clone().into())]);
        if let Some(style) = style {
            spec.insert("font_style".into(), style.into());
        }

        if let Some(weight) = weight {
            spec.insert("font_weight".into(), weight.into());
        }

        output.insert((*capture).into(), Value::Object(spec));
    }

    Ok(output)
}

fn capture_group(capture: &str, tier: Tier) -> &'static str {
    if capture == "diff.plus" {
        return "syntax-add";
    }

    if capture == "diff.minus" {
        return "syntax-delete";
    }

    if matches!(
        capture,
        "comment"
            | "comment.doc"
            | "predictive"
            | "strikethrough"
            | "punctuation"
            | "punctuation.delimiter"
            | "punctuation.list_marker"
            | "punctuation.markup"
    ) {
        return "subdued";
    }

    if capture == "hint" {
        return "hint";
    }

    if matches!(capture, "link_text" | "link_uri") {
        return "link";
    }

    if matches!(capture, "constant" | "text.literal" | "variant") {
        return "value";
    }

    if capture == "string" {
        return "string";
    }

    if matches!(
        capture,
        "constructor" | "function" | "function.builtin" | "label"
    ) {
        return "declaration";
    }

    if matches!(capture, "warning" | "diff") {
        return "metadata";
    }

    match tier {
        Tier::Baseline | Tier::Restrained => match capture {
            "boolean" | "number" | "string.escape" | "lifetime" => "value",
            "string.regex" => "string",
            "enum" => "metadata",
            "type" | "concept" | "namespace" | "module" => "declaration",
            "punctuation.special" => "subdued",
            _ => "base",
        },
        Tier::Broad => match capture {
            "boolean" | "number" => "value",
            "string.regex" => "string",
            "string.escape"
            | "string.special"
            | "string.special.symbol"
            | "variable.special"
            | "selector.pseudo"
            | "keyword"
            | "preproc"
            | "storageclass"
            | "punctuation.special" => "special",
            "lifetime" => "special",
            "enum" => "metadata",
            "type" | "concept" | "namespace" | "module" => "type",
            "attribute" | "property" | "selector" | "tag" => "link",
            "title" => "declaration",
            _ => "base",
        },
        Tier::Rich => match capture {
            "boolean"
            | "number"
            | "string.escape"
            | "string.special"
            | "string.special.symbol"
            | "punctuation.special" => "special",
            "string.regex" | "variable.special" | "lifetime" | "property" => "danger",
            "enum" | "type" | "concept" | "namespace" | "module" => "type",
            "attribute" | "keyword" | "preproc" | "storageclass" => "value",
            "selector" => "metadata",
            "tag" | "title" => "declaration",
            "selector.pseudo" => "special",
            _ => "base",
        },
    }
}

fn capture_style(capture: &str) -> (Option<&'static str>, Option<u16>) {
    let italic = matches!(
        capture,
        "comment"
            | "comment.doc"
            | "predictive"
            | "variable.parameter"
            | "lifetime"
            | "emphasis"
            | "link_uri"
    )
    .then_some("italic");
    let weight = if capture == "emphasis.strong" || capture == "title" {
        Some(700)
    } else if matches!(
        capture,
        "function.builtin"
            | "keyword"
            | "preproc"
            | "storageclass"
            | "hint"
            | "warning"
            | "diff"
            | "diff.plus"
            | "diff.minus"
    ) {
        Some(600)
    } else {
        None
    };

    (italic, weight)
}

pub fn contrast_floor(capture: &str, tier: Tier) -> f64 {
    match capture_group(capture, tier) {
        "base" => SYNTAX_PRIMARY_FLOOR,
        "subdued" => SYNTAX_SUBDUED_FLOOR,
        _ => SYNTAX_SEMANTIC_FLOOR,
    }
}
