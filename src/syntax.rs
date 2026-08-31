//! Builds deterministic syntax colors from Omarchy theme character.
//!
//! A fixed semantic hierarchy decides which syntax distinctions matter. The
//! authored palette only determines how many perceptually distinct families can
//! be expressed and which colors carry them. Contrast, diff separation, gamut,
//! and validation remain hard constraints.

pub mod plan;
pub mod policy;
pub mod profile;

use crate::Result;
use crate::color::{contrast_ratio, delta_e, gamut_map_oklch, lab, oklab_to_oklch};
use crate::constants::SYNTAX_DIFF_CONTRACT;
use crate::palette::ResolvedPalette;
use crate::saliency::SaliencyFit;
use crate::search::{FitBounds, PairConstraints, Search, cvd_distance, round6};
use plan::{MergePlan, SemanticRole, ToneBand};
use policy::{
    CAPTURE_POLICIES, SYNTAX_ADAPTIVE_OVERLAY_FLOOR, SYNTAX_PRIMARY_FLOOR, SYNTAX_SEMANTIC_FLOOR,
    SYNTAX_SUBDUED_FLOOR, SYNTAX_SUBDUED_OVERLAY_FLOOR,
};
use profile::{EvidenceColor, HueStrategy, SyntaxProfile};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};

pub use policy::{capture_policy, contrast_floor, overlay_contrast_floor};

const ORDINARY_NORMAL_SEPARATION: f64 = 0.025;
const ORDINARY_CVD_SEPARATION: f64 = 0.020;
const MINIMUM_TONE_SALIENCY_GAP: f64 = 0.08;
const MINIMUM_SEMANTIC_SALIENCY_GAP: f64 = 0.03;
const MINIMUM_AUTHORED_CHROMA_RETENTION: f64 = 0.60;
const MINIMUM_TRUNK_SALIENCY: f64 = 0.62;
const MAXIMUM_TRUNK_SALIENCY: f64 = 0.95;
const MAX_FAMILY_SOURCE_CANDIDATES: usize = 4;
const SOURCE_KEY_ORDER: [&str; 15] = [
    "green",
    "blue",
    "magenta",
    "yellow",
    "red",
    "cyan",
    "orange",
    "accent",
    "brown",
    "bright_green",
    "bright_blue",
    "bright_magenta",
    "bright_yellow",
    "bright_red",
    "bright_cyan",
];

#[derive(Clone, Debug)]
struct FamilyAllocation {
    family: usize,
    roles: Vec<SemanticRole>,
    source: Option<usize>,
    output: String,
    measured_saliency: f64,
}

struct SemanticPlan {
    plan: MergePlan,
    allocations: Vec<FamilyAllocation>,
}

#[derive(Clone, Debug)]
struct ToneAllocation {
    band: ToneBand,
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

struct FamilyFitRequest<'a> {
    preference_contexts: &'a [String],
    required_contexts: &'a [String],
    reference: &'a str,
    profile: &'a SyntaxProfile,
    family: usize,
    source: Option<usize>,
    preferred_saliency: f64,
    inherited: bool,
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
        SemanticRole::Callable => &["blue", "accent", "cyan", "magenta"],
        SemanticRole::Type => &["yellow", "cyan", "blue", "accent", "magenta"],
        SemanticRole::Member => &["cyan", "yellow", "magenta", "accent"],
        SemanticRole::Control => &["magenta", "red", "orange", "accent"],
        SemanticRole::Value => &["orange", "magenta", "yellow", "red", "brown"],
        SemanticRole::String => &["green", "yellow", "cyan", "red", "magenta"],
        SemanticRole::Metadata => &["yellow", "blue", "cyan", "accent"],
        SemanticRole::Link => &["accent", "blue", "cyan", "magenta"],
        _ => &[],
    }
}

fn key_family(key: &str) -> &str {
    key.strip_prefix("bright_").unwrap_or(key)
}

fn source_affinity(role: SemanticRole, source: &EvidenceColor) -> Option<usize> {
    role_source_preferences(role)
        .iter()
        .enumerate()
        .find_map(|(rank, preferred)| {
            source
                .keys
                .iter()
                .any(|key| key_family(key) == *preferred)
                .then_some(rank)
        })
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
        actual_saliency,
    })
}

fn ranked_sources(profile: &SyntaxProfile, role: SemanticRole) -> Vec<usize> {
    let mut sources = (0..profile.evidence.len()).collect::<Vec<_>>();
    sources.sort_by(|left, right| {
        let left = &profile.evidence[*left];
        let right = &profile.evidence[*right];
        match (source_affinity(role, left), source_affinity(role, right)) {
            (Some(left), Some(right)) => left.cmp(&right),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => right.chroma.total_cmp(&left.chroma),
        }
        .then_with(|| source_priority(left).cmp(&source_priority(right)))
        .then_with(|| right.chroma.total_cmp(&left.chroma))
        .then_with(|| left.keys.cmp(&right.keys))
        .then_with(|| left.value.cmp(&right.value))
    });
    sources
}

fn source_cluster(profile: &SyntaxProfile, source: usize) -> Option<usize> {
    profile
        .clusters
        .iter()
        .position(|cluster| cluster.members.contains(&source))
}

fn allocate_family(search: &mut Search, request: FamilyFitRequest<'_>) -> Result<FamilyAllocation> {
    let saliency = if let Some(source) = request.source.filter(|_| !request.inherited) {
        let source_contrast = geometric_contrast(
            &request.profile.evidence[source].value,
            request.preference_contexts,
        )?;
        let reference_contrast =
            geometric_contrast(request.reference, request.preference_contexts)?;
        (source_contrast.ln() / reference_contrast.ln().max(1e-12))
            .clamp(MINIMUM_TRUNK_SALIENCY, MAXIMUM_TRUNK_SALIENCY)
    } else {
        request.preferred_saliency
    };
    let authored_preference = request.profile.chroma_envelope.target_median
        + (request.profile.chroma_envelope.ordinary_maximum
            - request.profile.chroma_envelope.target_median)
            * saliency.powi(2);
    let (seed, chroma_cap, chroma_floor) = if let Some(source) = request.source {
        let evidence = &request.profile.evidence[source];
        let chroma_cap = request.profile.chroma_envelope.ordinary_maximum;
        let preferred_chroma = evidence.chroma.min(authored_preference).min(chroma_cap);
        (
            gamut_map_oklch(evidence.lightness, preferred_chroma, evidence.hue).opaque_hex(),
            chroma_cap,
            preferred_chroma * MINIMUM_AUTHORED_CHROMA_RETENTION,
        )
    } else {
        let reference_chroma = oklab_to_oklch(lab(request.reference)?)[1];
        (
            request.reference.to_owned(),
            reference_chroma.max(0.005),
            0.0,
        )
    };
    let fit = fit_tone(
        search,
        ToneFitRequest {
            seed: &seed,
            reference: request.reference,
            preference_contexts: request.preference_contexts,
            required_contexts: request.required_contexts,
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
    let output_chroma = oklab_to_oklch(lab(&fit.output)?)[1];
    if output_chroma > chroma_cap + 1e-12 {
        return Err(crate::Error(format!(
            "syntax family {} escaped its chroma envelope: {output_chroma:.6} > {chroma_cap:.6}",
            request.family
        )));
    }
    Ok(FamilyAllocation {
        family: request.family,
        roles: Vec::new(),
        source: request.source,
        output: fit.output,
        measured_saliency: fit.actual_saliency,
    })
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

fn separated_from(
    candidate: &str,
    existing: impl IntoIterator<Item = impl AsRef<str>>,
) -> Result<bool> {
    for color in existing {
        let color = color.as_ref();
        if delta_e(candidate, color)? < ORDINARY_NORMAL_SEPARATION - 1e-12
            || cvd_distance(candidate, color)? < ORDINARY_CVD_SEPARATION - 1e-12
        {
            return Ok(false);
        }
    }
    Ok(true)
}

fn semantic_above_subdued(allocations: &[FamilyAllocation], subdued_saliency: f64) -> bool {
    allocations.iter().all(|allocation| {
        allocation.measured_saliency >= subdued_saliency + MINIMUM_SEMANTIC_SALIENCY_GAP - 1e-12
    })
}

struct SemanticSearchRequest<'a> {
    preference_contexts: &'a [String],
    required_contexts: &'a [String],
    reference: &'a str,
    profile: &'a SyntaxProfile,
    fixed_outputs: [&'a str; 2],
    hierarchy: &'a MergePlan,
    subdued_saliency: f64,
}

fn semantic_score(hierarchy: &MergePlan, active: &BTreeSet<usize>) -> (usize, usize) {
    let mut trunk_priority = 0;
    let mut branch_count = 0;
    for (family, semantic) in hierarchy.families.iter().enumerate() {
        if semantic.parent.is_none() {
            trunk_priority = (trunk_priority << 1) | usize::from(active.contains(&family));
        } else if active.contains(&family) {
            branch_count += 1;
        }
    }
    (trunk_priority, branch_count)
}

fn search_semantic_forest(
    search: &mut Search,
    request: &SemanticSearchRequest<'_>,
    family: usize,
    allocations: &mut Vec<FamilyAllocation>,
    best: &mut Vec<FamilyAllocation>,
) -> Result<()> {
    if family == request.hierarchy.families.len() {
        let active = allocations
            .iter()
            .map(|allocation| allocation.family)
            .collect::<BTreeSet<_>>();
        let best_active = best
            .iter()
            .map(|allocation| allocation.family)
            .collect::<BTreeSet<_>>();
        if semantic_score(request.hierarchy, &active)
            > semantic_score(request.hierarchy, &best_active)
        {
            *best = allocations.clone();
        }
        return Ok(());
    }

    let mut potential = allocations
        .iter()
        .map(|allocation| allocation.family)
        .collect::<BTreeSet<_>>();
    potential.extend(family..request.hierarchy.families.len());
    let best_active = best
        .iter()
        .map(|allocation| allocation.family)
        .collect::<BTreeSet<_>>();
    if semantic_score(request.hierarchy, &potential)
        <= semantic_score(request.hierarchy, &best_active)
    {
        return Ok(());
    }

    let semantic = &request.hierarchy.families[family];
    let sources = if let Some(parent) = semantic.parent {
        let Some(parent) = allocations
            .iter()
            .find(|allocation| allocation.family == parent)
        else {
            return search_semantic_forest(search, request, family + 1, allocations, best);
        };
        vec![parent.source]
    } else if request.profile.evidence.is_empty() {
        vec![None]
    } else {
        let mut sources = ranked_sources(request.profile, semantic.source_preference);
        let used_trunk_clusters = allocations
            .iter()
            .filter(|allocation| {
                request.hierarchy.families[allocation.family]
                    .parent
                    .is_none()
            })
            .filter_map(|allocation| allocation.source)
            .filter_map(|source| source_cluster(request.profile, source))
            .collect::<BTreeSet<_>>();
        if sources.iter().any(|source| {
            source_cluster(request.profile, *source)
                .is_some_and(|cluster| !used_trunk_clusters.contains(&cluster))
        }) {
            sources.sort_by_key(|source| {
                source_cluster(request.profile, *source)
                    .is_none_or(|cluster| used_trunk_clusters.contains(&cluster))
            });
        }
        sources.truncate(MAX_FAMILY_SOURCE_CANDIDATES);
        sources.into_iter().map(Some).collect()
    };
    for source in sources {
        let preferred_saliency = semantic
            .parent
            .map(|parent| {
                let parent = allocations
                    .iter()
                    .find(|allocation| allocation.family == parent)
                    .unwrap();
                (parent.measured_saliency + semantic.parent_saliency_delta)
                    .clamp(request.subdued_saliency + 0.06, 0.96)
            })
            .unwrap_or(semantic.fallback_saliency);
        let Ok(candidate) = allocate_family(
            search,
            FamilyFitRequest {
                preference_contexts: request.preference_contexts,
                required_contexts: request.required_contexts,
                reference: request.reference,
                profile: request.profile,
                family,
                source,
                preferred_saliency,
                inherited: semantic.parent.is_some(),
            },
        ) else {
            continue;
        };
        allocations.push(candidate);
        let readable_hierarchy = semantic_above_subdued(allocations, request.subdued_saliency);
        let separated = if readable_hierarchy {
            let candidate = allocations.last().unwrap();
            let existing = request.fixed_outputs.iter().copied().chain(
                allocations[..allocations.len() - 1]
                    .iter()
                    .map(|allocation| allocation.output.as_str()),
            );
            separated_from(&candidate.output, existing)?
        } else {
            false
        };
        if separated {
            search_semantic_forest(search, request, family + 1, allocations, best)?;
        }
        allocations.pop();
    }
    search_semantic_forest(search, request, family + 1, allocations, best)?;
    Ok(())
}

fn select_semantic_plan(
    search: &mut Search,
    preference_contexts: &[String],
    required_contexts: &[String],
    reference: &str,
    profile: &SyntaxProfile,
    fixed_outputs: [&str; 2],
) -> Result<SemanticPlan> {
    let subdued_saliency = geometric_contrast(fixed_outputs[1], preference_contexts)?.ln()
        / geometric_contrast(reference, preference_contexts)?
            .ln()
            .max(1e-12);
    let hierarchy = MergePlan::hierarchy(profile.requested_hue_family_count.clamp(1, 3));
    let request = SemanticSearchRequest {
        preference_contexts,
        required_contexts,
        reference,
        profile,
        fixed_outputs,
        hierarchy: &hierarchy,
        subdued_saliency,
    };
    let mut current = Vec::with_capacity(hierarchy.families.len());
    let mut allocations = Vec::with_capacity(hierarchy.families.len());
    search_semantic_forest(search, &request, 0, &mut current, &mut allocations)?;

    let active = allocations
        .iter()
        .map(|allocation| allocation.family)
        .collect::<BTreeSet<_>>();
    let plan = hierarchy.activate(&active);
    for (family, allocation) in allocations.iter_mut().enumerate() {
        allocation.family = family;
        let family = &plan.families[family];
        allocation.roles = family.roles.clone();
    }
    Ok(SemanticPlan { plan, allocations })
}

fn build_tone_allocations(
    search: &mut Search,
    preference_contexts: &[String],
    required_contexts: &[String],
    reference: &str,
    profile: &SyntaxProfile,
) -> Result<Vec<ToneAllocation>> {
    let reference_chroma = oklab_to_oklch(lab(reference)?)[1];
    let (seed, preferred_chroma, chroma_cap) = match profile.hue_strategy {
        HueStrategy::Neutral => (
            reference.to_owned(),
            reference_chroma,
            // Byte-quantized neutral colors carry a small numerical Oklab chroma.
            // Leave enough headroom for the grayscale lightness ladder without
            // turning that quantization residue into an authored hue.
            reference_chroma.max(0.005),
        ),
        HueStrategy::AccentLed => {
            let evidence = profile
                .evidence
                .iter()
                .max_by(|left, right| {
                    left.chroma
                        .total_cmp(&right.chroma)
                        .then_with(|| right.value.cmp(&left.value))
                })
                .ok_or_else(|| crate::Error("accent-led syntax has no evidenced hue".into()))?;
            let preferred_chroma = evidence
                .chroma
                .min(profile.chroma_envelope.ordinary_maximum);
            (
                gamut_map_oklch(evidence.lightness, preferred_chroma, evidence.hue).opaque_hex(),
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
        allocations.push(ToneAllocation { band, fit });
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

pub fn build_syntax(
    search: &mut Search,
    palette: &ResolvedPalette,
    contexts: SyntaxContexts<'_>,
    saliency_reference: &str,
    predictive: &str,
    diff_sources: [&str; 3],
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

    let mut role_colors = if profile.hue_strategy == HueStrategy::Neutral {
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
        let secondary = tone_color(ToneBand::Secondary);
        let mut semantic_budget =
            usize::from(separated_from(&secondary, [&base, &subdued_fit.output])?);
        if semantic_budget == 1 {
            let primary = tone_color(ToneBand::Primary);
            if separated_from(&primary, [&base, &subdued_fit.output, &secondary])? {
                semantic_budget = 2;
            }
        }
        let semantic_plan = MergePlan::with_budget(semantic_budget);
        let mut role_colors = BTreeMap::from([
            (SemanticRole::Base, base.clone()),
            (SemanticRole::Subdued, subdued_fit.output.clone()),
            (SemanticRole::Predictive, predictive.to_owned()),
        ]);
        for role in plan::ORDINARY_ROLES {
            let color = semantic_plan
                .family_for(role)
                .map(|family| tone_color(semantic_plan.families[family].anchor.tone_band()))
                .unwrap_or_else(|| role_colors[&semantic_plan.fallback_for(role)].clone());
            role_colors.insert(role, color);
        }
        role_colors
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
        let SemanticPlan {
            plan: semantic_plan,
            allocations,
        } = select_semantic_plan(
            search,
            preference_contexts,
            required_contexts,
            saliency_reference,
            &profile,
            [&base, &subdued_fit.output],
        )?;
        let mut role_colors = BTreeMap::from([
            (SemanticRole::Base, base.clone()),
            (SemanticRole::Subdued, subdued_fit.output.clone()),
            (SemanticRole::Predictive, predictive.to_owned()),
        ]);
        for allocation in &allocations {
            for role in &allocation.roles {
                role_colors.insert(*role, allocation.output.clone());
            }
        }
        for role in plan::ORDINARY_ROLES {
            if semantic_plan.family_for(role).is_none() {
                let fallback = semantic_plan.fallback_for(role);
                role_colors.insert(role, role_colors[&fallback].clone());
            }
        }
        role_colors
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
    let syntax_change =
        search.fit_color(diff_sources[1], required_contexts, SYNTAX_SEMANTIC_FLOOR)?;
    role_colors.extend([
        (SemanticRole::DiffChange, syntax_change.clone()),
        (SemanticRole::DiffAdd, syntax_add.clone()),
        (SemanticRole::DiffDelete, syntax_delete.clone()),
    ]);

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semantic_score_preserves_trunk_priority_before_branch_count() {
        let hierarchy = MergePlan::hierarchy(3);
        let data_and_symbol = BTreeSet::from([0, 1]);
        let data_control_and_all_available_branches = BTreeSet::from([0, 2, 3]);
        assert!(
            semantic_score(&hierarchy, &data_and_symbol)
                > semantic_score(&hierarchy, &data_control_and_all_available_branches)
        );

        let data_symbol_and_value = BTreeSet::from([0, 1, 3]);
        assert!(
            semantic_score(&hierarchy, &data_symbol_and_value)
                > semantic_score(&hierarchy, &data_and_symbol)
        );
    }
}
