//! Builds deterministic syntax colors from Omarchy theme character.
//!
//! The profile measures authored breadth and intensity, the merge plan fixes
//! semantic identity, and saliency ranks how the profile's color budget is spent.
//! Contrast, diff separation, gamut, and validation remain hard constraints.

pub mod plan;
pub mod policy;
pub mod profile;

use crate::Result;
use crate::color::{contrast_ratio, delta_e, gamut_map_oklch, lab, oklab_to_oklch};
use crate::constants::SYNTAX_DIFF_CONTRACT;
use crate::palette::ResolvedPalette;
use crate::saliency::{PRIMARY_SALIENCY, SaliencyRequest, fit_relative};
use crate::search::{FitBounds, PairConstraints, Search, cvd_distance, round6};
use crate::theme::Audit;
use plan::{MergePlan, SemanticRole};
use policy::{CAPTURE_POLICIES, SYNTAX_PRIMARY_FLOOR, SYNTAX_SEMANTIC_FLOOR, SYNTAX_SUBDUED_FLOOR};
use profile::{EvidenceColor, SCAFFOLD_MAXIMUM_CHROMA, SyntaxProfile};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::f64::consts::{PI, TAU};

pub use policy::{capture_policy, contrast_floor};

const ORDINARY_NORMAL_SEPARATION: f64 = 0.025;
const ORDINARY_CVD_SEPARATION: f64 = 0.005;
const SOURCE_KEY_ORDER: [&str; 8] = [
    "green", "blue", "magenta", "yellow", "red", "cyan", "orange", "accent",
];

#[derive(Clone, Debug)]
struct FamilyAllocation {
    family: usize,
    origin_family: usize,
    seed: String,
    seed_kind: &'static str,
    source: Option<usize>,
    preferred_chroma: f64,
    chroma_cap: f64,
    output: String,
    preferred_saliency: f64,
    reference_contrast: f64,
    preferred_contrast: f64,
    actual_contrast: f64,
    measured_saliency: f64,
}

type DistanceMatrix = Vec<Vec<f64>>;

fn minimum_contrast(color: &str, contexts: &[String]) -> Result<f64> {
    contexts
        .iter()
        .map(|context| contrast_ratio(color, context))
        .collect::<Result<Vec<_>>>()
        .map(|values| values.into_iter().fold(f64::INFINITY, f64::min))
}

fn role_source_preferences(role: SemanticRole) -> &'static [&'static str] {
    match role {
        SemanticRole::Declaration => &["blue", "accent", "cyan", "magenta"],
        SemanticRole::Type => &["cyan", "blue", "accent", "magenta"],
        SemanticRole::Member => &["yellow", "cyan", "magenta", "accent"],
        SemanticRole::Control => &["magenta", "red", "orange", "accent"],
        SemanticRole::Value => &["magenta", "orange", "yellow", "red"],
        SemanticRole::String => &["green", "yellow", "cyan"],
        SemanticRole::Special => &["orange", "magenta", "red", "accent"],
        SemanticRole::Metadata => &["yellow", "blue", "cyan", "accent"],
        SemanticRole::Link => &["accent", "blue", "cyan", "magenta"],
        _ => &[],
    }
}

fn source_affinity(roles: &[SemanticRole], source: &EvidenceColor) -> usize {
    roles
        .iter()
        .flat_map(|role| {
            role_source_preferences(*role)
                .iter()
                .enumerate()
                .filter_map(|(rank, key)| source.keys.contains(key).then_some(rank))
        })
        .min()
        .unwrap_or(usize::MAX / 2)
}

fn source_priority(source: &EvidenceColor) -> usize {
    source
        .keys
        .iter()
        .filter_map(|key| {
            SOURCE_KEY_ORDER
                .iter()
                .position(|candidate| candidate == key)
        })
        .min()
        .unwrap_or(SOURCE_KEY_ORDER.len())
}

fn family_saliency(plan: &MergePlan, family: usize) -> f64 {
    plan.families[family]
        .roles
        .iter()
        .map(|role| role.saliency())
        .fold(0.0, f64::max)
}

fn scaffold_hue(profile: &SyntaxProfile, family: usize) -> f64 {
    const CHANNEL_STEP: f64 = PI / 2.0;
    const SECONDARY_OFFSET: f64 = 35.0 * PI / 180.0;
    (profile.scaffold_phase
        + (family % 4) as f64 * CHANNEL_STEP
        + (family / 4) as f64 * SECONDARY_OFFSET)
        .rem_euclid(TAU)
}

fn scaffold_lightness(palette: &ResolvedPalette) -> Result<f64> {
    let background = oklab_to_oklch(lab(&palette.colors["background"])?)[0];
    let foreground = oklab_to_oklch(lab(&palette.colors["foreground"])?)[0];
    Ok((background + 0.72 * (foreground - background)).clamp(0.18, 0.82))
}

fn allocate_families(
    search: &mut Search,
    palette: &ResolvedPalette,
    contexts: &[String],
    reference: &str,
    profile: &SyntaxProfile,
    plan: &MergePlan,
) -> Result<Vec<FamilyAllocation>> {
    let mut source_candidates = profile
        .clusters
        .iter()
        .filter_map(|cluster| {
            let representative = &profile.evidence[cluster.representative];
            profile
                .authored_colors
                .iter()
                .position(|color| color.value == representative.value)
        })
        .collect::<Vec<_>>();

    let mut family_order = (0..plan.families.len()).collect::<Vec<_>>();
    family_order.sort_by(|left, right| {
        family_saliency(plan, *right)
            .total_cmp(&family_saliency(plan, *left))
            .then_with(|| left.cmp(right))
    });
    let mut assigned = vec![None; plan.families.len()];
    for family in family_order {
        if source_candidates.is_empty() {
            break;
        }
        let best = source_candidates
            .iter()
            .enumerate()
            .min_by(|(_, left), (_, right)| {
                let left = &profile.authored_colors[**left];
                let right = &profile.authored_colors[**right];
                source_affinity(&plan.families[family].roles, left)
                    .cmp(&source_affinity(&plan.families[family].roles, right))
                    .then_with(|| source_priority(left).cmp(&source_priority(right)))
                    .then_with(|| left.keys.cmp(&right.keys))
                    .then_with(|| left.value.cmp(&right.value))
            })
            .map(|(position, source)| (position, *source))
            .unwrap();
        assigned[family] = Some(best.1);
        source_candidates.remove(best.0);
    }

    let scaffold_lightness = scaffold_lightness(palette)?;
    let mut allocations = Vec::with_capacity(plan.family_count);
    for (family, assigned_source) in assigned.iter().copied().enumerate() {
        let saliency = family_saliency(plan, family);
        let authored_preference = profile.chroma_envelope.target_median
            + (profile.chroma_envelope.ordinary_maximum - profile.chroma_envelope.target_median)
                * saliency.powi(2);
        let (seed, seed_kind, preferred_chroma) = if let Some(source) = assigned_source {
            let evidence = &profile.authored_colors[source];
            (
                gamut_map_oklch(
                    evidence.lightness,
                    evidence.chroma.min(authored_preference),
                    evidence.hue,
                )
                .opaque_hex(),
                "authored_hue",
                authored_preference,
            )
        } else {
            let scaffold_preference = (profile.chroma_envelope.target_median
                * (0.80 + 0.40 * saliency))
                .min(SCAFFOLD_MAXIMUM_CHROMA)
                .min(profile.chroma_envelope.ordinary_maximum);
            (
                gamut_map_oklch(
                    scaffold_lightness,
                    scaffold_preference,
                    scaffold_hue(profile, family),
                )
                .opaque_hex(),
                "dynamic_scaffold",
                scaffold_preference,
            )
        };
        let chroma_cap = if seed_kind == "dynamic_scaffold" {
            profile
                .chroma_envelope
                .ordinary_maximum
                .min(SCAFFOLD_MAXIMUM_CHROMA)
        } else {
            profile.chroma_envelope.ordinary_maximum
        };
        let chroma_floor = (preferred_chroma.min(chroma_cap) * 0.45).min(0.025);
        let saliency_fit = fit_relative(
            search,
            &seed,
            reference,
            SaliencyRequest::new(contexts, SYNTAX_SEMANTIC_FLOOR, saliency).with_bounds(
                FitBounds {
                    lower_chroma: chroma_floor,
                    upper_chroma: chroma_cap,
                    ..FitBounds::default()
                },
            ),
        )?;
        let output = saliency_fit.output;
        let output_chroma = oklab_to_oklch(lab(&output)?)[1];
        if output_chroma > chroma_cap + 1e-12 {
            return Err(crate::Error(format!(
                "syntax family {family} escaped its chroma envelope: {output_chroma:.6} > {:.6}",
                chroma_cap
            )));
        }
        allocations.push(FamilyAllocation {
            family,
            origin_family: family,
            seed,
            seed_kind,
            source: assigned_source,
            preferred_chroma,
            chroma_cap,
            preferred_saliency: saliency_fit.preferred_saliency,
            reference_contrast: saliency_fit.reference_contrast,
            preferred_contrast: saliency_fit.preferred_contrast,
            actual_contrast: saliency_fit.actual_contrast,
            measured_saliency: saliency_fit.actual_saliency,
            output,
        });
    }

    Ok(allocations)
}

fn pair_matrices(colors: &[String]) -> Result<(DistanceMatrix, DistanceMatrix)> {
    let mut normal = vec![vec![0.0; colors.len()]; colors.len()];
    let mut cvd = normal.clone();
    for left in 0..colors.len() {
        for right in left + 1..colors.len() {
            normal[left][right] = delta_e(&colors[left], &colors[right])?;
            normal[right][left] = normal[left][right];
            cvd[left][right] = cvd_distance(&colors[left], &colors[right])?;
            cvd[right][left] = cvd[left][right];
        }
    }
    Ok((normal, cvd))
}

fn rounded_matrix(matrix: &[Vec<f64>]) -> DistanceMatrix {
    matrix
        .iter()
        .map(|row| row.iter().map(|value| round6(*value)).collect())
        .collect()
}

fn project_matrix(matrix: &[Vec<f64>], indices: &[usize]) -> DistanceMatrix {
    indices
        .iter()
        .map(|left| indices.iter().map(|right| matrix[*left][*right]).collect())
        .collect()
}

fn minimum_pair(matrix: &[Vec<f64>]) -> f64 {
    (0..matrix.len())
        .flat_map(|left| (left + 1..matrix.len()).map(move |right| matrix[left][right]))
        .fold(f64::INFINITY, f64::min)
}

fn separated(normal: &[Vec<f64>], cvd: &[Vec<f64>]) -> bool {
    (0..normal.len()).all(|left| {
        (left + 1..normal.len()).all(|right| {
            normal[left][right] >= ORDINARY_NORMAL_SEPARATION - 1e-12
                && cvd[left][right] >= ORDINARY_CVD_SEPARATION - 1e-12
        })
    })
}

fn select_separated_plan(
    requested_families: usize,
    full_allocations: &[FamilyAllocation],
    full_normal: &[Vec<f64>],
    full_cvd: &[Vec<f64>],
    excluded_outputs: &[String],
) -> Result<(
    MergePlan,
    Vec<FamilyAllocation>,
    DistanceMatrix,
    DistanceMatrix,
)> {
    for family_count in (4..=requested_families).rev() {
        let plan = MergePlan::with_family_count(family_count);
        let mut best: Option<(Vec<usize>, [f64; 3])> = None;
        for mask in 0_u16..(1_u16 << full_allocations.len()) {
            if mask.count_ones() as usize != family_count {
                continue;
            }
            let indices = (0..full_allocations.len())
                .filter(|index| {
                    mask & (1 << index) != 0
                        && !excluded_outputs.contains(&full_allocations[*index].output)
                })
                .collect::<Vec<_>>();
            if indices.len() != family_count {
                continue;
            }
            let normal = project_matrix(full_normal, &indices);
            let cvd = project_matrix(full_cvd, &indices);
            if !separated(&normal, &cvd) {
                continue;
            }
            let score = [
                minimum_pair(&cvd),
                minimum_pair(&normal),
                indices
                    .iter()
                    .map(|index| full_allocations[*index].measured_saliency)
                    .sum(),
            ];
            if best.as_ref().is_none_or(|(best_indices, best_score)| {
                score
                    .iter()
                    .zip(best_score)
                    .find_map(|(left, right)| {
                        let ordering = left.total_cmp(right);
                        (!ordering.is_eq()).then_some(ordering.is_gt())
                    })
                    .unwrap_or_else(|| indices < *best_indices)
            }) {
                best = Some((indices, score));
            }
        }
        if let Some((indices, _)) = best {
            let mut allocations = indices
                .iter()
                .map(|index| full_allocations[*index].clone())
                .collect::<Vec<_>>();
            allocations.sort_by(|left, right| {
                right
                    .measured_saliency
                    .total_cmp(&left.measured_saliency)
                    .then_with(|| left.output.cmp(&right.output))
            });
            let mut desired_families = (0..plan.family_count).collect::<Vec<_>>();
            desired_families.sort_by(|left, right| {
                family_saliency(&plan, *right)
                    .total_cmp(&family_saliency(&plan, *left))
                    .then_with(|| left.cmp(right))
            });
            for (allocation, family) in allocations.iter_mut().zip(desired_families) {
                allocation.family = family;
            }
            allocations.sort_by_key(|allocation| allocation.family);

            let ordered_indices = allocations
                .iter()
                .map(|allocation| allocation.origin_family)
                .collect::<Vec<_>>();
            let normal = project_matrix(full_normal, &ordered_indices);
            let cvd = project_matrix(full_cvd, &ordered_indices);
            return Ok((plan, allocations, normal, cvd));
        }
    }

    let candidates = full_allocations
        .iter()
        .map(|allocation| {
            format!(
                "{}:{}:{:.3}",
                allocation.seed_kind, allocation.output, allocation.measured_saliency
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    Err(crate::Error(format!(
        "no subset of the bounded syntax candidate pool satisfies a four-family merge plan (normal {:.3}, CVD {:.3}; candidates {candidates})",
        ORDINARY_NORMAL_SEPARATION, ORDINARY_CVD_SEPARATION
    )))
}

fn saliency_ordered(plan: &MergePlan, allocations: &[FamilyAllocation]) -> bool {
    allocations.iter().all(|left| {
        allocations.iter().all(|right| {
            family_saliency(plan, left.family) <= family_saliency(plan, right.family) + 1e-12
                || left.measured_saliency >= right.measured_saliency - 1e-12
        })
    })
}

pub fn build_syntax(
    search: &mut Search,
    palette: &ResolvedPalette,
    contexts: &[String],
    saliency_reference: &str,
    predictive: &str,
    diff_sources: [&str; 3],
    audit: &mut Audit,
) -> Result<Map<String, Value>> {
    let profile = profile::measure(palette)?;
    let base = saliency_reference.to_owned();
    if minimum_contrast(&base, contexts)? < SYNTAX_PRIMARY_FLOOR - 1e-12 {
        return Err(crate::Error(
            "editor primary does not meet the syntax-primary floor".into(),
        ));
    }

    let mut subdued_fit = fit_relative(
        search,
        &palette.colors["muted"],
        saliency_reference,
        SaliencyRequest::new(
            contexts,
            SYNTAX_SUBDUED_FLOOR,
            SemanticRole::Subdued.saliency(),
        ),
    )?;
    let mut subdued = subdued_fit.output.clone();
    if subdued == base {
        let seed = gamut_map_oklch(
            scaffold_lightness(palette)?,
            profile
                .chroma_envelope
                .target_median
                .min(SCAFFOLD_MAXIMUM_CHROMA)
                * 0.75,
            (profile.scaffold_phase + PI).rem_euclid(TAU),
        )
        .opaque_hex();
        subdued_fit = fit_relative(
            search,
            &seed,
            saliency_reference,
            SaliencyRequest::new(
                contexts,
                SYNTAX_SUBDUED_FLOOR,
                SemanticRole::Subdued.saliency(),
            )
            .with_bounds(FitBounds {
                upper_chroma: profile.chroma_envelope.ordinary_maximum,
                ..FitBounds::default()
            }),
        )?;
        subdued = subdued_fit.output.clone();
    }
    if subdued == base {
        return Err(crate::Error(
            "base and subdued syntax roles collided exactly".into(),
        ));
    }

    let full_plan = MergePlan::with_family_count(8);
    let full_allocations = allocate_families(
        search,
        palette,
        contexts,
        saliency_reference,
        &profile,
        &full_plan,
    )?;
    let full_colors = full_allocations
        .iter()
        .map(|allocation| allocation.output.clone())
        .collect::<Vec<_>>();
    let (full_normal, full_cvd) = pair_matrices(&full_colors)?;
    let (plan, allocations, normal_matrix, cvd_matrix) = select_separated_plan(
        profile.requested_family_count,
        &full_allocations,
        &full_normal,
        &full_cvd,
        &[base.clone(), subdued.clone()],
    )?;
    for allocation in &allocations {
        if allocation.output == base || allocation.output == subdued {
            return Err(crate::Error(format!(
                "syntax family {} collided exactly with an unmerged base role",
                allocation.family
            )));
        }
    }

    let pair_constraints =
        PairConstraints::from_contract(SYNTAX_SEMANTIC_FLOOR, SYNTAX_DIFF_CONTRACT)
            .with_minimum_chroma(0.025);
    let [syntax_add, syntax_delete] = search
        .fit_pair(diff_sources[0], diff_sources[2], contexts, pair_constraints)
        .map_err(|error| crate::Error(format!("syntax diff semantic pair: {error}")))?;
    let diff_disposition = "conventional_semantic_anchors";
    let syntax_change = search.fit_color(diff_sources[1], contexts, SYNTAX_SEMANTIC_FLOOR)?;

    let mut role_colors = BTreeMap::from([
        (SemanticRole::Base, base.clone()),
        (SemanticRole::Subdued, subdued.clone()),
        (SemanticRole::Predictive, predictive.to_owned()),
        (SemanticRole::DiffChange, syntax_change.clone()),
        (SemanticRole::DiffAdd, syntax_add.clone()),
        (SemanticRole::DiffDelete, syntax_delete.clone()),
    ]);
    for allocation in &allocations {
        for role in &plan.families[allocation.family].roles {
            role_colors.insert(*role, allocation.output.clone());
        }
    }

    let distinct_family_colors = allocations
        .iter()
        .map(|allocation| allocation.output.clone())
        .collect::<Vec<_>>();
    let allocation_audit = allocations
        .iter()
        .map(|allocation| -> Result<Value> {
            let source = allocation
                .source
                .map(|index| &profile.authored_colors[index]);
            let output_lch = oklab_to_oklch(lab(&allocation.output)?);
            Ok(json!({
                "family": allocation.family,
                "candidate_origin_family": allocation.origin_family,
                "roles": plan.families[allocation.family].roles.iter().map(|role| role.as_str()).collect::<Vec<_>>(),
                "saliency_preference": round6(family_saliency(&plan, allocation.family)),
                "candidate_target_saliency": round6(allocation.preferred_saliency),
                "measured_saliency": round6(allocation.measured_saliency),
                "reference_contrast": round6(allocation.reference_contrast),
                "preferred_contrast": round6(allocation.preferred_contrast),
                "actual_contrast": round6(allocation.actual_contrast),
                "preferred_chroma": round6(allocation.preferred_chroma),
                "chroma_cap": round6(allocation.chroma_cap),
                "seed_kind": allocation.seed_kind,
                "seed": allocation.seed,
                "source_value": source.map(|value| value.value.clone()),
                "source_keys": source.map(|value| value.keys.clone()).unwrap_or_default(),
                "output": allocation.output,
                "output_chroma": round6(output_lch[1]),
                "minimum_contrast": round6(minimum_contrast(&allocation.output, contexts)?),
            }))
        })
        .collect::<Result<Vec<_>>>()?;
    let capture_audit = CAPTURE_POLICIES
        .iter()
        .map(|capture| {
            json!({
                "capture": capture.capture,
                "role": capture.role.as_str(),
                "family": plan.family_for(capture.role),
            })
        })
        .collect::<Vec<_>>();

    let mut merge_plan_audit = plan.audit();
    if let Some(object) = merge_plan_audit.as_object_mut() {
        object.insert(
            "requested_family_count".into(),
            profile.requested_family_count.into(),
        );
        object.insert("effective_family_count".into(), plan.family_count.into());
        object.insert(
            "separation_fallback".into(),
            (plan.family_count < profile.requested_family_count).into(),
        );
    }

    audit.syntax_analysis = json!({
        "version": 1,
        "profile": profile.audit(),
        "merge_plan": merge_plan_audit,
        "saliency": {
            "allocation_order": "fit stable candidates to relative contrast preferences, then rank valid outputs against family saliency",
            "bands_are_preferences": true,
            "metric": "log(role geometric-mean contrast) / log(editor foreground geometric-mean contrast)",
            "measured_order_verified": saliency_ordered(&plan, &allocations),
            "fitted_family_seed_count": full_allocations.len(),
            "maximum_fitted_family_seeds": 8,
            "allocations": allocation_audit,
            "ordinary_pair_metrics": {
                "colors": distinct_family_colors,
                "normal_delta_e": rounded_matrix(&normal_matrix),
                "cvd_delta_e": rounded_matrix(&cvd_matrix),
                "minimum_normal_delta_e": ORDINARY_NORMAL_SEPARATION,
                "minimum_cvd_delta_e": ORDINARY_CVD_SEPARATION,
                "separation_verified": separated(&normal_matrix, &cvd_matrix),
                "exact_collision_policy": "forbidden_between_effective_families",
            },
        },
        "captures": capture_audit,
        "diff": {
            "disposition": diff_disposition,
            "profile_budgeted": false,
            "change_source_key": "yellow",
            "change": syntax_change,
            "added_source_key": "green",
            "added": syntax_add,
            "deleted_source_key": "red",
            "deleted": syntax_delete,
            "ordinary_chroma_envelope_exempt": true,
            "ui_presentation": {
                "adaptive": false,
                "scope": "existing editor diff-hunk, version-control, status, fill, hollow-fill, and border behavior is preserved",
            },
        },
    });

    audit.syntax_roles.push(json!({
        "role": SemanticRole::Base.as_str(),
        "family": Value::Null,
        "output": base,
        "minimum_contrast": round6(minimum_contrast(&base, contexts)?),
        "preferred_saliency": PRIMARY_SALIENCY,
        "measured_saliency": PRIMARY_SALIENCY,
        "disposition": "shared_editor_primary",
    }));
    audit.syntax_roles.push(json!({
        "role": SemanticRole::Subdued.as_str(),
        "family": Value::Null,
        "output": subdued,
        "minimum_contrast": round6(minimum_contrast(&subdued, contexts)?),
        "preferred_saliency": round6(subdued_fit.preferred_saliency),
        "measured_saliency": round6(subdued_fit.actual_saliency),
    }));
    audit.syntax_roles.push(json!({
        "role": SemanticRole::Predictive.as_str(),
        "family": Value::Null,
        "output": predictive,
        "minimum_contrast": round6(minimum_contrast(predictive, contexts)?),
        "disposition": "shared_predictive_content",
    }));
    for role in plan.families.iter().flat_map(|family| family.roles.iter()) {
        let family = plan.family_for(*role).unwrap();
        let output = &role_colors[role];
        audit.syntax_roles.push(json!({
            "role": role.as_str(),
            "family": family,
            "merged_with": plan.families[family].roles.iter().filter(|other| *other != role).map(|other| other.as_str()).collect::<Vec<_>>(),
            "output": output,
            "minimum_contrast": round6(minimum_contrast(output, contexts)?),
            "chroma": round6(oklab_to_oklch(lab(output)?)[1]),
        }));
    }
    for (role, output) in [
        (SemanticRole::DiffChange, &syntax_change),
        (SemanticRole::DiffAdd, &syntax_add),
        (SemanticRole::DiffDelete, &syntax_delete),
    ] {
        audit.syntax_roles.push(json!({
            "role": role.as_str(),
            "family": Value::Null,
            "output": output,
            "minimum_contrast": round6(minimum_contrast(output, contexts)?),
            "disposition": diff_disposition,
        }));
    }

    let mut output = Map::new();
    for capture in CAPTURE_POLICIES {
        let color = role_colors.get(&capture.role).ok_or_else(|| {
            crate::Error(format!(
                "no syntax color allocated for {}",
                capture.role.as_str()
            ))
        })?;
        let (style, weight) = capture_style(capture.capture);
        let mut spec = Map::from_iter([("color".into(), color.clone().into())]);
        if let Some(style) = style {
            spec.insert("font_style".into(), style.into());
        }
        if let Some(weight) = weight {
            spec.insert("font_weight".into(), weight.into());
        }
        output.insert(capture.capture.into(), Value::Object(spec));
    }

    Ok(output)
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
