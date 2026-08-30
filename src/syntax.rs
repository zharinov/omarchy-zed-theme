//! Builds deterministic syntax colors from Omarchy theme character.
//!
//! The profile selects a supported authored-hue budget, while tone bands control
//! ordinary reading hierarchy independently of semantic merging. Contrast, diff
//! separation, gamut, and validation remain hard constraints.

pub mod plan;
pub mod policy;
pub mod profile;

use crate::Result;
use crate::color::{contrast_ratio, delta_e, gamut_map_oklch, lab, oklab_to_oklch};
use crate::constants::SYNTAX_DIFF_CONTRACT;
use crate::palette::ResolvedPalette;
use crate::saliency::{PRIMARY_SALIENCY, SaliencyFit};
use crate::search::{FitBounds, PairConstraints, Search, cvd_distance, round6};
use crate::theme::Audit;
use plan::{MergePlan, SemanticRole, ToneBand};
use policy::{
    CAPTURE_POLICIES, SYNTAX_ADAPTIVE_OVERLAY_FLOOR, SYNTAX_PRIMARY_FLOOR, SYNTAX_SEMANTIC_FLOOR,
    SYNTAX_SUBDUED_FLOOR, SYNTAX_SUBDUED_OVERLAY_FLOOR,
};
use profile::{EvidenceColor, HueStrategy, SyntaxProfile};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

pub use policy::{capture_policy, contrast_floor, overlay_contrast_floor};

const ORDINARY_NORMAL_SEPARATION: f64 = 0.025;
const ORDINARY_CVD_SEPARATION: f64 = 0.005;
const MINIMUM_TONE_SALIENCY_GAP: f64 = 0.08;
const MINIMUM_PALETTE_NATIVE_SALIENCY_GAP: f64 = 0.03;
const SOURCE_KEY_ORDER: [&str; 8] = [
    "green", "blue", "magenta", "yellow", "red", "cyan", "orange", "accent",
];

#[derive(Clone, Debug)]
struct FamilyAllocation {
    family: usize,
    seed: String,
    seed_kind: &'static str,
    source: usize,
    preferred_chroma: f64,
    chroma_cap: f64,
    output: String,
    reference_contrast: f64,
    preferred_contrast: f64,
    actual_contrast: f64,
    measured_saliency: f64,
}

#[derive(Clone, Debug)]
struct ToneAllocation {
    band: ToneBand,
    seed: String,
    seed_kind: &'static str,
    source: Option<usize>,
    preferred_chroma: f64,
    chroma_cap: f64,
    fit: SaliencyFit,
}

struct ToneFitRequest<'a> {
    seed: &'a str,
    reference: &'a str,
    preference_contexts: &'a [String],
    required_contexts: &'a [String],
    preference_floor: f64,
    required_floor: f64,
    preferred_saliency: f64,
    bounds: FitBounds,
}

pub struct SyntaxContexts<'a> {
    pub ordinary: &'a [String],
    pub rendered: &'a [String],
}

type DistanceMatrix = Vec<Vec<f64>>;

fn minimum_contrast(color: &str, contexts: &[String]) -> Result<f64> {
    contexts.iter().try_fold(f64::INFINITY, |minimum, context| {
        Ok(minimum.min(contrast_ratio(color, context)?))
    })
}

fn geometric_contrast(color: &str, contexts: &[String]) -> Result<f64> {
    if contexts.is_empty() {
        return Err(crate::Error(
            "syntax tone fitting requires at least one preference context".into(),
        ));
    }
    let mean_log = contexts
        .iter()
        .map(|context| contrast_ratio(color, context).map(f64::ln))
        .sum::<Result<f64>>()?
        / contexts.len() as f64;
    Ok(mean_log.exp())
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

fn fit_tone(search: &mut Search, request: ToneFitRequest<'_>) -> Result<SaliencyFit> {
    let reference_contrast = geometric_contrast(request.reference, request.preference_contexts)?;
    let preferred_saliency = request.preferred_saliency.clamp(0.0, 1.0);
    let preferred_contrast = (reference_contrast.ln() * preferred_saliency)
        .exp()
        .max(request.preference_floor);
    let mut bounds = request.bounds;
    bounds.preferred_contrast = Some(preferred_contrast);
    let output = search.fit_color_bounded_with_preference_backgrounds(
        request.seed,
        request.required_contexts,
        request.preference_contexts,
        request.required_floor,
        &[],
        bounds,
    )?;
    let actual_contrast = geometric_contrast(&output, request.preference_contexts)?;
    let actual_saliency = actual_contrast.ln() / reference_contrast.ln().max(1e-12);
    Ok(SaliencyFit {
        output,
        reference_contrast,
        preferred_contrast,
        actual_contrast,
        preferred_saliency,
        actual_saliency,
    })
}

fn allocate_families(
    search: &mut Search,
    preference_contexts: &[String],
    required_contexts: &[String],
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

    let mut allocations = Vec::with_capacity(plan.family_count);
    for (family, assigned_source) in assigned.iter().copied().enumerate() {
        let saliency = family_saliency(plan, family);
        let authored_preference = profile.chroma_envelope.target_median
            + (profile.chroma_envelope.ordinary_maximum - profile.chroma_envelope.target_median)
                * saliency.powi(2);
        let source = assigned_source.ok_or_else(|| {
            crate::Error(format!(
                "palette-native plan requested {} hue families from {} evidenced clusters",
                plan.family_count,
                profile.clusters.len()
            ))
        })?;
        let evidence = &profile.authored_colors[source];
        let seed = gamut_map_oklch(
            evidence.lightness,
            evidence.chroma.min(authored_preference),
            evidence.hue,
        )
        .opaque_hex();
        let seed_kind = "authored_hue";
        let preferred_chroma = authored_preference;
        let chroma_cap = profile.chroma_envelope.ordinary_maximum;
        let chroma_floor = (preferred_chroma.min(chroma_cap) * 0.45).min(0.025);
        let saliency_fit = fit_tone(
            search,
            ToneFitRequest {
                seed: &seed,
                reference,
                preference_contexts,
                required_contexts,
                preference_floor: SYNTAX_SEMANTIC_FLOOR,
                required_floor: SYNTAX_ADAPTIVE_OVERLAY_FLOOR,
                preferred_saliency: saliency,
                bounds: FitBounds {
                    lower_chroma: chroma_floor,
                    upper_chroma: chroma_cap,
                    ..FitBounds::default()
                },
            },
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
            seed,
            seed_kind,
            source,
            preferred_chroma,
            chroma_cap,
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

fn separated(normal: &[Vec<f64>], cvd: &[Vec<f64>]) -> bool {
    (0..normal.len()).all(|left| {
        (left + 1..normal.len()).all(|right| {
            normal[left][right] >= ORDINARY_NORMAL_SEPARATION - 1e-12
                && cvd[left][right] >= ORDINARY_CVD_SEPARATION - 1e-12
        })
    })
}

fn select_palette_native_plan(
    search: &mut Search,
    preference_contexts: &[String],
    required_contexts: &[String],
    reference: &str,
    profile: &SyntaxProfile,
    excluded_outputs: &[String],
) -> Result<(
    MergePlan,
    Vec<FamilyAllocation>,
    DistanceMatrix,
    DistanceMatrix,
)> {
    let mut failures = Vec::new();
    for family_count in (3..=profile.requested_hue_family_count).rev() {
        let plan = MergePlan::with_family_count(family_count);
        let allocations = match allocate_families(
            search,
            preference_contexts,
            required_contexts,
            reference,
            profile,
            &plan,
        ) {
            Ok(allocations) => allocations,
            Err(error) => {
                failures.push(format!("{family_count}: {error}"));
                continue;
            }
        };
        if allocations
            .iter()
            .any(|allocation| excluded_outputs.contains(&allocation.output))
        {
            failures.push(format!("{family_count}: collided with a fixed syntax tone"));
            continue;
        }
        let colors = allocations
            .iter()
            .map(|allocation| allocation.output.clone())
            .collect::<Vec<_>>();
        let (normal, cvd) = pair_matrices(&colors)?;
        if !separated(&normal, &cvd) {
            failures.push(format!("{family_count}: pair separation failed"));
            continue;
        }
        if !saliency_ordered(&plan, &allocations) {
            failures.push(format!("{family_count}: tone saliency order failed"));
            continue;
        }
        return Ok((plan, allocations, normal, cvd));
    }

    Err(crate::Error(format!(
        "no palette-native syntax plan satisfies the hue and tone contracts (normal {:.3}, CVD {:.3}; {})",
        ORDINARY_NORMAL_SEPARATION,
        ORDINARY_CVD_SEPARATION,
        failures.join("; ")
    )))
}

fn build_tone_allocations(
    search: &mut Search,
    preference_contexts: &[String],
    required_contexts: &[String],
    reference: &str,
    profile: &SyntaxProfile,
) -> Result<Vec<ToneAllocation>> {
    let reference_chroma = oklab_to_oklch(lab(reference)?)[1];
    let (seed, seed_kind, source, preferred_chroma, chroma_cap) = match profile.hue_strategy {
        HueStrategy::Neutral => (
            reference.to_owned(),
            "shared_editor_hue",
            None,
            reference_chroma,
            // Byte-quantized neutral colors carry a small numerical Oklab chroma.
            // Leave enough headroom for the grayscale lightness ladder without
            // turning that quantization residue into an authored hue.
            reference_chroma.max(0.005),
        ),
        HueStrategy::AccentLed => {
            let (source, evidence) = profile
                .evidence
                .iter()
                .enumerate()
                .max_by(|left, right| {
                    left.1
                        .chroma
                        .total_cmp(&right.1.chroma)
                        .then_with(|| right.1.value.cmp(&left.1.value))
                })
                .ok_or_else(|| crate::Error("accent-led syntax has no evidenced hue".into()))?;
            let preferred_chroma = evidence
                .chroma
                .min(profile.chroma_envelope.ordinary_maximum);
            (
                gamut_map_oklch(evidence.lightness, preferred_chroma, evidence.hue).opaque_hex(),
                "authored_hue",
                Some(source),
                preferred_chroma,
                profile.chroma_envelope.ordinary_maximum,
            )
        }
        HueStrategy::PaletteNative => {
            return Err(crate::Error(
                "palette-native syntax cannot use the single-hue tone allocator".into(),
            ));
        }
    };
    let chroma_floor = if profile.hue_strategy == HueStrategy::Neutral {
        0.0
    } else {
        (preferred_chroma * 0.45).min(0.025)
    };

    let reference_contrast = geometric_contrast(reference, preference_contexts)?;
    let floor_saliency = SYNTAX_SEMANTIC_FLOOR.ln() / reference_contrast.ln().max(1e-12);
    let subdued_saliency = ToneBand::Subdued.single_hue_saliency().max(floor_saliency);
    let secondary_saliency = ToneBand::Secondary
        .single_hue_saliency()
        .max(subdued_saliency + MINIMUM_TONE_SALIENCY_GAP);
    let primary_saliency = ToneBand::Primary
        .single_hue_saliency()
        .max(secondary_saliency + MINIMUM_TONE_SALIENCY_GAP);
    if primary_saliency > 1.0 + 1e-12 {
        return Err(crate::Error(format!(
            "editor foreground leaves no room for three readable syntax tones: floor saliency {floor_saliency:.3}"
        )));
    }

    let mut allocations = Vec::new();
    for (band, preferred_saliency) in [
        (ToneBand::Primary, primary_saliency),
        (ToneBand::Secondary, secondary_saliency),
        (ToneBand::Subdued, subdued_saliency),
    ] {
        let required_floor = match band {
            ToneBand::Subdued => SYNTAX_SUBDUED_OVERLAY_FLOOR,
            ToneBand::Primary | ToneBand::Secondary => SYNTAX_ADAPTIVE_OVERLAY_FLOOR,
        };
        let fit = fit_tone(
            search,
            ToneFitRequest {
                seed: &seed,
                reference,
                preference_contexts,
                required_contexts,
                preference_floor: SYNTAX_SEMANTIC_FLOOR,
                required_floor,
                preferred_saliency,
                bounds: FitBounds {
                    lower_chroma: chroma_floor,
                    upper_chroma: chroma_cap,
                    ..FitBounds::default()
                },
            },
        )?;
        allocations.push(ToneAllocation {
            band,
            seed: seed.clone(),
            seed_kind,
            source,
            preferred_chroma,
            chroma_cap,
            fit,
        });
    }

    let colors = allocations
        .iter()
        .map(|allocation| allocation.fit.output.clone())
        .collect::<Vec<_>>();
    let (normal, cvd) = pair_matrices(&colors)?;
    if !separated(&normal, &cvd) {
        return Err(crate::Error(format!(
            "three-tone syntax ladder is not perceptually separated (normal {:.3}, CVD {:.3}; colors {}; normal {:?}; CVD {:?})",
            ORDINARY_NORMAL_SEPARATION,
            ORDINARY_CVD_SEPARATION,
            colors.join(", "),
            rounded_matrix(&normal),
            rounded_matrix(&cvd),
        )));
    }
    if allocations.windows(2).any(|pair| {
        pair[0].fit.actual_saliency
            < pair[1].fit.actual_saliency + MINIMUM_TONE_SALIENCY_GAP - 1e-12
    }) {
        return Err(crate::Error(format!(
            "three-tone syntax ladder has less than {:.2} relative-saliency separation: {}",
            MINIMUM_TONE_SALIENCY_GAP,
            allocations
                .iter()
                .map(|allocation| format!(
                    "{}={:.3}",
                    allocation.band.as_str(),
                    allocation.fit.actual_saliency
                ))
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }

    Ok(allocations)
}

fn saliency_ordered(plan: &MergePlan, allocations: &[FamilyAllocation]) -> bool {
    allocations.iter().all(|left| {
        allocations.iter().all(|right| {
            family_saliency(plan, left.family) <= family_saliency(plan, right.family) + 1e-12
                || left.measured_saliency
                    >= right.measured_saliency + MINIMUM_PALETTE_NATIVE_SALIENCY_GAP - 1e-12
        })
    })
}

pub fn build_syntax(
    search: &mut Search,
    palette: &ResolvedPalette,
    contexts: SyntaxContexts<'_>,
    saliency_reference: &str,
    predictive: &str,
    diff_sources: [&str; 3],
    audit: &mut Audit,
) -> Result<Map<String, Value>> {
    let preference_contexts = contexts.ordinary;
    let required_contexts = contexts.rendered;
    let profile = profile::measure(palette)?;
    let base = saliency_reference.to_owned();
    if minimum_contrast(&base, required_contexts)? < SYNTAX_PRIMARY_FLOOR - 1e-12 {
        return Err(crate::Error(
            "editor primary does not meet the syntax-primary floor".into(),
        ));
    }

    let (
        mut role_colors,
        subdued_fit,
        hue_plan_audit,
        allocation_audit,
        distinct_ordinary_colors,
        normal_matrix,
        cvd_matrix,
        measured_order_verified,
        role_families,
        role_merges,
    ) = if profile.hue_strategy != HueStrategy::PaletteNative {
        let tones = build_tone_allocations(
            search,
            preference_contexts,
            required_contexts,
            saliency_reference,
            &profile,
        )?;
        let tone_color = |band| {
            tones
                .iter()
                .find(|allocation| allocation.band == band)
                .map(|allocation| allocation.fit.output.clone())
                .unwrap()
        };
        let subdued_fit = tones
            .iter()
            .find(|allocation| allocation.band == ToneBand::Subdued)
            .unwrap()
            .fit
            .clone();
        let mut role_colors = BTreeMap::from([
            (SemanticRole::Base, base.clone()),
            (SemanticRole::Subdued, subdued_fit.output.clone()),
            (SemanticRole::Predictive, predictive.to_owned()),
        ]);
        for role in plan::ORDINARY_ROLES {
            role_colors.insert(role, tone_color(role.tone_band()));
        }
        let distinct_colors = tones
            .iter()
            .map(|allocation| allocation.fit.output.clone())
            .collect::<Vec<_>>();
        let (normal, cvd) = pair_matrices(&distinct_colors)?;
        let allocation_audit = tones
            .iter()
            .map(|allocation| -> Result<Value> {
                let roles = plan::ORDINARY_ROLES
                    .iter()
                    .copied()
                    .filter(|role| role.tone_band() == allocation.band)
                    .map(SemanticRole::as_str)
                    .chain(
                        (allocation.band == ToneBand::Subdued)
                            .then_some(SemanticRole::Subdued.as_str()),
                    )
                    .collect::<Vec<_>>();
                let output_lch = oklab_to_oklch(lab(&allocation.fit.output)?);
                let source = allocation.source.map(|index| &profile.evidence[index]);
                let source_cluster = allocation.source.and_then(|index| {
                    profile
                        .clusters
                        .iter()
                        .position(|cluster| cluster.members.contains(&index))
                });
                Ok(json!({
                    "tone_band": allocation.band.as_str(),
                    "roles": roles,
                    "default_saliency_preference": round6(allocation.band.single_hue_saliency()),
                    "saliency_preference": round6(allocation.fit.preferred_saliency),
                    "measured_saliency": round6(allocation.fit.actual_saliency),
                    "reference_contrast": round6(allocation.fit.reference_contrast),
                    "preferred_contrast": round6(allocation.fit.preferred_contrast),
                    "actual_contrast": round6(allocation.fit.actual_contrast),
                    "preferred_chroma": round6(allocation.preferred_chroma),
                    "chroma_cap": round6(allocation.chroma_cap),
                    "seed_kind": allocation.seed_kind,
                    "seed": allocation.seed,
                    "source_value": source.map(|value| value.value.clone()),
                    "source_keys": source.map(|value| value.keys.clone()).unwrap_or_default(),
                    "source_provenance": source.map(|value| format!("{:?}", value.provenance).to_ascii_lowercase()),
                    "source_cluster": source_cluster,
                    "output": allocation.fit.output,
                    "output_chroma": round6(output_lch[1]),
                    "rendered_minimum_contrast": round6(minimum_contrast(&allocation.fit.output, required_contexts)?),
                }))
            })
            .collect::<Result<Vec<_>>>()?;
        let role_families = plan::ORDINARY_ROLES
            .iter()
            .copied()
            .map(|role| (role, None))
            .collect::<BTreeMap<_, _>>();
        let role_merges = plan::ORDINARY_ROLES
            .iter()
            .copied()
            .map(|role| {
                (
                    role,
                    plan::ORDINARY_ROLES
                        .iter()
                        .copied()
                        .filter(|other| *other != role && other.tone_band() == role.tone_band())
                        .collect::<Vec<_>>(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        (
            role_colors,
            subdued_fit,
            json!({
                "strategy": profile.hue_strategy.as_str(),
                "requested_hue_family_count": profile.requested_hue_family_count,
                "effective_hue_family_count": profile.requested_hue_family_count,
                "tone_count": 3,
                "semantic_merge_plan": Value::Null,
            }),
            allocation_audit,
            distinct_colors,
            normal,
            cvd,
            true,
            role_families,
            role_merges,
        )
    } else {
        let mut subdued_fit = fit_tone(
            search,
            ToneFitRequest {
                seed: &palette.colors["muted"],
                reference: saliency_reference,
                preference_contexts,
                required_contexts,
                preference_floor: SYNTAX_SUBDUED_FLOOR,
                required_floor: SYNTAX_SUBDUED_OVERLAY_FLOOR,
                preferred_saliency: ToneBand::Subdued.saliency(),
                bounds: FitBounds {
                    upper_chroma: profile.chroma_envelope.ordinary_maximum,
                    ..FitBounds::default()
                },
            },
        )?;
        if subdued_fit.output == base {
            let reference_chroma = oklab_to_oklch(lab(saliency_reference)?)[1];
            subdued_fit = fit_tone(
                search,
                ToneFitRequest {
                    seed: saliency_reference,
                    reference: saliency_reference,
                    preference_contexts,
                    required_contexts,
                    preference_floor: SYNTAX_SUBDUED_FLOOR,
                    required_floor: SYNTAX_SUBDUED_OVERLAY_FLOOR,
                    preferred_saliency: ToneBand::Subdued.saliency(),
                    bounds: FitBounds {
                        upper_chroma: reference_chroma.max(1e-9),
                        ..FitBounds::default()
                    },
                },
            )?;
        }
        if subdued_fit.output == base {
            return Err(crate::Error(
                "base and subdued syntax roles collided exactly".into(),
            ));
        }
        let (plan, allocations, normal, cvd) = select_palette_native_plan(
            search,
            preference_contexts,
            required_contexts,
            saliency_reference,
            &profile,
            &[base.clone(), subdued_fit.output.clone()],
        )?;
        let mut role_colors = BTreeMap::from([
            (SemanticRole::Base, base.clone()),
            (SemanticRole::Subdued, subdued_fit.output.clone()),
            (SemanticRole::Predictive, predictive.to_owned()),
        ]);
        for allocation in &allocations {
            for role in &plan.families[allocation.family].roles {
                role_colors.insert(*role, allocation.output.clone());
            }
        }
        let distinct_colors = allocations
            .iter()
            .map(|allocation| allocation.output.clone())
            .collect::<Vec<_>>();
        let allocation_audit = allocations
            .iter()
            .map(|allocation| -> Result<Value> {
                let source = &profile.authored_colors[allocation.source];
                let source_evidence = profile
                    .evidence
                    .iter()
                    .position(|evidence| evidence.value == source.value);
                let source_cluster = source_evidence.and_then(|index| {
                    profile
                        .clusters
                        .iter()
                        .position(|cluster| cluster.members.contains(&index))
                });
                let output_lch = oklab_to_oklch(lab(&allocation.output)?);
                Ok(json!({
                    "family": allocation.family,
                    "roles": plan.families[allocation.family].roles.iter().map(|role| role.as_str()).collect::<Vec<_>>(),
                    "tone_band": plan.families[allocation.family].anchor.tone_band().as_str(),
                    "saliency_preference": round6(family_saliency(&plan, allocation.family)),
                    "measured_saliency": round6(allocation.measured_saliency),
                    "reference_contrast": round6(allocation.reference_contrast),
                    "preferred_contrast": round6(allocation.preferred_contrast),
                    "actual_contrast": round6(allocation.actual_contrast),
                    "preferred_chroma": round6(allocation.preferred_chroma),
                    "chroma_cap": round6(allocation.chroma_cap),
                    "seed_kind": allocation.seed_kind,
                    "seed": allocation.seed,
                    "source_value": source.value.clone(),
                    "source_keys": source.keys.clone(),
                    "source_provenance": format!("{:?}", source.provenance).to_ascii_lowercase(),
                    "source_cluster": source_cluster,
                    "output": allocation.output,
                    "output_chroma": round6(output_lch[1]),
                    "rendered_minimum_contrast": round6(minimum_contrast(&allocation.output, required_contexts)?),
                }))
            })
            .collect::<Result<Vec<_>>>()?;
        let role_families = plan::ORDINARY_ROLES
            .iter()
            .copied()
            .map(|role| (role, plan.family_for(role)))
            .collect::<BTreeMap<_, _>>();
        let role_merges = plan::ORDINARY_ROLES
            .iter()
            .copied()
            .map(|role| {
                let family = plan.family_for(role).unwrap();
                (
                    role,
                    plan.families[family]
                        .roles
                        .iter()
                        .copied()
                        .filter(|other| *other != role)
                        .collect::<Vec<_>>(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let mut plan_audit = plan.audit();
        if let Some(object) = plan_audit.as_object_mut() {
            object.insert("strategy".into(), profile.hue_strategy.as_str().into());
            object.insert(
                "requested_hue_family_count".into(),
                profile.requested_hue_family_count.into(),
            );
            object.insert(
                "effective_hue_family_count".into(),
                plan.family_count.into(),
            );
            object.insert("tone_count".into(), 3.into());
            object.insert(
                "separation_fallback".into(),
                (plan.family_count < profile.requested_hue_family_count).into(),
            );
        }
        (
            role_colors,
            subdued_fit,
            plan_audit,
            allocation_audit,
            distinct_colors,
            normal,
            cvd,
            saliency_ordered(&plan, &allocations),
            role_families,
            role_merges,
        )
    };

    let pair_constraints =
        PairConstraints::from_contract(SYNTAX_SEMANTIC_FLOOR, SYNTAX_DIFF_CONTRACT)
            .with_minimum_chroma(0.025);
    let [syntax_add, syntax_delete] = search
        .fit_pair(
            diff_sources[0],
            diff_sources[2],
            required_contexts,
            pair_constraints,
        )
        .map_err(|error| crate::Error(format!("syntax diff semantic pair: {error}")))?;
    let diff_disposition = "conventional_semantic_anchors";
    let syntax_change =
        search.fit_color(diff_sources[1], required_contexts, SYNTAX_SEMANTIC_FLOOR)?;
    role_colors.extend([
        (SemanticRole::DiffChange, syntax_change.clone()),
        (SemanticRole::DiffAdd, syntax_add.clone()),
        (SemanticRole::DiffDelete, syntax_delete.clone()),
    ]);

    let capture_audit = CAPTURE_POLICIES
        .iter()
        .map(|capture| {
            json!({
                "capture": capture.capture,
                "role": capture.role.as_str(),
                "family": role_families.get(&capture.role).copied().flatten(),
                "tone_band": capture.role.tone_band().as_str(),
            })
        })
        .collect::<Vec<_>>();

    audit.syntax_analysis = json!({
        "version": 2,
        "profile": profile.audit(),
        "hue_plan": hue_plan_audit,
        "contrast_contracts": {
            "ordinary_editor": {
                "base": SYNTAX_PRIMARY_FLOOR,
                "ordinary": SYNTAX_SEMANTIC_FLOOR,
                "subdued": SYNTAX_SUBDUED_FLOOR,
            },
            "rendered_overlays": {
                "base_and_predictive": SYNTAX_PRIMARY_FLOOR,
                "diff": SYNTAX_SEMANTIC_FLOOR,
                "adaptive_ordinary": SYNTAX_ADAPTIVE_OVERLAY_FLOOR,
                "subdued": SYNTAX_SUBDUED_OVERLAY_FLOOR,
            },
        },
        "saliency": {
            "allocation_order": "fit three tone bands on ordinary editor contexts, then validate every emitted overlay context",
            "bands_are_preferences": true,
            "metric": "log(role geometric-mean contrast) / log(editor foreground geometric-mean contrast)",
            "measured_order_verified": measured_order_verified,
            "allocations": allocation_audit,
            "ordinary_pair_metrics": {
                "colors": distinct_ordinary_colors,
                "normal_delta_e": rounded_matrix(&normal_matrix),
                "cvd_delta_e": rounded_matrix(&cvd_matrix),
                "minimum_normal_delta_e": ORDINARY_NORMAL_SEPARATION,
                "minimum_cvd_delta_e": ORDINARY_CVD_SEPARATION,
                "separation_verified": separated(&normal_matrix, &cvd_matrix),
                "exact_collision_policy": "only roles intentionally sharing a tone or hue family may collide",
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
        "ordinary_contrast": round6(geometric_contrast(&base, preference_contexts)?),
        "rendered_minimum_contrast": round6(minimum_contrast(&base, required_contexts)?),
        "preferred_saliency": PRIMARY_SALIENCY,
        "measured_saliency": PRIMARY_SALIENCY,
        "disposition": "shared_editor_primary",
    }));
    audit.syntax_roles.push(json!({
        "role": SemanticRole::Subdued.as_str(),
        "family": Value::Null,
        "tone_band": ToneBand::Subdued.as_str(),
        "output": subdued_fit.output,
        "ordinary_contrast": round6(geometric_contrast(&subdued_fit.output, preference_contexts)?),
        "rendered_minimum_contrast": round6(minimum_contrast(&subdued_fit.output, required_contexts)?),
        "preferred_saliency": round6(subdued_fit.preferred_saliency),
        "measured_saliency": round6(subdued_fit.actual_saliency),
    }));
    audit.syntax_roles.push(json!({
        "role": SemanticRole::Predictive.as_str(),
        "family": Value::Null,
        "output": predictive,
        "ordinary_contrast": round6(geometric_contrast(predictive, preference_contexts)?),
        "rendered_minimum_contrast": round6(minimum_contrast(predictive, required_contexts)?),
        "disposition": "shared_predictive_content",
    }));
    for role in plan::ORDINARY_ROLES {
        let output = &role_colors[&role];
        audit.syntax_roles.push(json!({
            "role": role.as_str(),
            "family": role_families[&role],
            "tone_band": role.tone_band().as_str(),
            "merged_with": role_merges[&role].iter().map(|other| other.as_str()).collect::<Vec<_>>(),
            "output": output,
            "ordinary_contrast": round6(geometric_contrast(output, preference_contexts)?),
            "rendered_minimum_contrast": round6(minimum_contrast(output, required_contexts)?),
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
            "ordinary_contrast": round6(geometric_contrast(output, preference_contexts)?),
            "rendered_minimum_contrast": round6(minimum_contrast(output, required_contexts)?),
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
