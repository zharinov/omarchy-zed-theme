//! Builds deterministic syntax colors from Omarchy theme character.
//!
//! A fixed semantic hierarchy decides which syntax distinctions matter. Authored
//! hues and neutral tones decide how those domains are rendered, while missing
//! chromatic lineages fall back to the foreground's tone family. Contrast, diff
//! separation, gamut, and validation remain hard constraints.

pub mod plan;
pub mod policy;
pub mod profile;

use crate::Result;
use crate::color::{contrast_ratio, delta_e, gamut_map_oklch_unchecked, lab, oklab_to_oklch};
use crate::constants::SYNTAX_DIFF_CONTRACT;
use crate::palette::ResolvedPalette;
use crate::saliency::SaliencyFit;
use crate::search::{FitBounds, PairConstraints, Search, cvd_distance};
use plan::{MergePlan, SemanticRole};
use policy::{
    CAPTURE_POLICIES, SYNTAX_ADAPTIVE_OVERLAY_FLOOR, SYNTAX_PRIMARY_FLOOR, SYNTAX_SEMANTIC_FLOOR,
    SYNTAX_SUBDUED_FLOOR, SYNTAX_SUBDUED_OVERLAY_FLOOR,
};
use profile::{CHROMA_EVIDENCE, EVIDENCE_KEYS, EvidenceColor, SyntaxProfile};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};

pub use policy::{capture_policy, contrast_floor, overlay_contrast_floor};

const ORDINARY_NORMAL_SEPARATION: f64 = 0.025;
const ORDINARY_CVD_SEPARATION: f64 = 0.020;
const MINIMUM_SEMANTIC_SALIENCY_GAP: f64 = 0.03;
const MINIMUM_AUTHORED_CHROMA_RETENTION: f64 = 0.60;
const SUBDUED_SALIENCY: f64 = 0.55;
const SOURCE_KEY_ORDER: [&str; 15] = EVIDENCE_KEYS;

#[derive(Clone, Debug, PartialEq)]
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
}

pub struct SyntaxContexts<'a> {
    pub ordinary: &'a [String],
    pub rendered: &'a [String],
}

fn minimum_contrast(color: &str, contexts: &[String]) -> Result<f64> {
    contexts.iter().try_fold(f64::INFINITY, |minimum, context| {
        Ok(minimum.min(contrast_ratio(color, context)?))
    })
}

fn geometric_contrast(color: &str, contexts: &[String]) -> Result<f64> {
    if contexts.is_empty() {
        return Err(crate::Error::invalid(
            "syntax tone fitting requires at least one preference context",
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
        .map(|key| {
            SOURCE_KEY_ORDER
                .iter()
                .position(|candidate| candidate == key)
                .expect("syntax evidence keys must belong to the source registry")
        })
        .min()
        .expect("syntax evidence must retain at least one source key")
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

fn root_sources(profile: &SyntaxProfile, role: SemanticRole) -> Vec<Option<usize>> {
    ranked_sources(profile, role)
        .into_iter()
        .map(Some)
        .chain(std::iter::once(None))
        .collect()
}

fn source_cluster(profile: &SyntaxProfile, source: usize) -> Option<usize> {
    let evidence = profile
        .evidence
        .get(source)
        .expect("semantic source index must reference syntax evidence");
    let mut memberships = profile
        .clusters
        .iter()
        .enumerate()
        .filter_map(|(index, cluster)| cluster.members.contains(&source).then_some(index));
    let cluster = memberships.next();
    assert!(
        memberships.next().is_none(),
        "syntax evidence cannot belong to more than one hue cluster"
    );
    assert_eq!(
        cluster.is_some(),
        evidence.chroma >= CHROMA_EVIDENCE - 1e-12,
        "chromatic syntax evidence must belong to exactly one hue cluster"
    );
    cluster
}

fn allocate_family(search: &mut Search, request: FamilyFitRequest<'_>) -> Result<FamilyAllocation> {
    let saliency = request.preferred_saliency;
    let authored_preference = request.profile.chroma_envelope.target_median
        + (request.profile.chroma_envelope.ordinary_maximum
            - request.profile.chroma_envelope.target_median)
            * saliency.powi(2);
    let (seed, chroma_cap, chroma_floor) = if let Some(source) = request.source {
        let evidence = &request.profile.evidence[source];
        let chroma_cap = if evidence.chroma < CHROMA_EVIDENCE - 1e-12 {
            evidence.chroma.max(0.005)
        } else {
            request.profile.chroma_envelope.ordinary_maximum
        };
        let preferred_chroma = evidence.chroma.min(authored_preference).min(chroma_cap);
        (
            gamut_map_oklch_unchecked(evidence.lightness, preferred_chroma, evidence.hue)
                .opaque_hex(),
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
    assert!(
        output_chroma <= chroma_cap + 1e-12,
        "bounded syntax family {} escaped its chroma envelope: {output_chroma:.6} > {chroma_cap:.6}",
        request.family
    );
    Ok(FamilyAllocation {
        family: request.family,
        roles: Vec::new(),
        source: request.source,
        output: fit.output,
        measured_saliency: fit.actual_saliency,
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

// Derived ordering makes this field order the search objective, from strongest
// feasibility requirement to weakest deterministic tie-break.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
struct SemanticScore {
    trunk_count: usize,
    hue_cluster_count: usize,
    branch_count: usize,
    authored_trunk_count: usize,
    source_preference: usize,
    active_priority: usize,
}

fn source_preference_value(role: SemanticRole, evidence: &EvidenceColor) -> usize {
    let affinity = source_affinity(role, evidence)
        .map(|rank| role_source_preferences(role).len() - rank)
        .unwrap_or(0);
    let priority = SOURCE_KEY_ORDER
        .len()
        .saturating_sub(source_priority(evidence));
    affinity * (SOURCE_KEY_ORDER.len() + 1) + priority
}

fn semantic_score(
    hierarchy: &MergePlan,
    profile: &SyntaxProfile,
    allocations: &[FamilyAllocation],
) -> SemanticScore {
    let active = allocations
        .iter()
        .map(|allocation| allocation.family)
        .collect::<BTreeSet<_>>();

    let mut score = SemanticScore::default();
    let mut clusters = BTreeSet::new();

    for (family, semantic) in hierarchy.families.iter().enumerate() {
        score.active_priority =
            (score.active_priority << 1) | usize::from(active.contains(&family));
        if !active.contains(&family) {
            continue;
        }

        if semantic.parent.is_none() {
            score.trunk_count += 1;
            let allocation = allocations
                .iter()
                .find(|allocation| allocation.family == family)
                .expect("active syntax trunk must have an allocation");
            if let Some(source) = allocation.source {
                score.authored_trunk_count += 1;
                if let Some(cluster) = source_cluster(profile, source) {
                    clusters.insert(cluster);
                }
                let evidence = &profile.evidence[source];
                score.source_preference +=
                    source_preference_value(semantic.source_preference, evidence);
            }
        } else {
            score.branch_count += 1;
        }
    }

    score.hue_cluster_count = clusters.len();
    score
}

fn optimistic_semantic_score(
    hierarchy: &MergePlan,
    profile: &SyntaxProfile,
    next_family: usize,
    allocations: &[FamilyAllocation],
) -> SemanticScore {
    let mut score = semantic_score(hierarchy, profile, allocations);
    let remaining = &hierarchy.families[next_family..];
    let remaining_trunks = remaining
        .iter()
        .filter(|semantic| semantic.parent.is_none())
        .count();

    let used_sources = allocations
        .iter()
        .filter(|allocation| hierarchy.families[allocation.family].parent.is_none())
        .filter_map(|allocation| allocation.source)
        .collect::<BTreeSet<_>>();
    let used_clusters = used_sources
        .iter()
        .filter_map(|source| source_cluster(profile, *source))
        .collect::<BTreeSet<_>>();

    let available_sources = (0..profile.evidence.len())
        .filter(|source| {
            !used_sources.contains(source)
                && source_cluster(profile, *source)
                    .is_none_or(|cluster| !used_clusters.contains(&cluster))
        })
        .collect::<Vec<_>>();
    let available_clusters = available_sources
        .iter()
        .filter_map(|source| source_cluster(profile, *source))
        .collect::<BTreeSet<_>>();
    let available_neutral_sources = available_sources
        .iter()
        .filter(|source| source_cluster(profile, **source).is_none())
        .count();

    score.trunk_count += remaining_trunks;
    score.hue_cluster_count += remaining_trunks.min(available_clusters.len());

    score.branch_count += remaining
        .iter()
        .filter(|semantic| semantic.parent.is_some())
        .count();

    score.authored_trunk_count +=
        remaining_trunks.min(available_clusters.len() + available_neutral_sources);
    score.source_preference += remaining
        .iter()
        .filter(|semantic| semantic.parent.is_none())
        .map(|semantic| {
            available_sources
                .iter()
                .map(|source| {
                    source_preference_value(semantic.source_preference, &profile.evidence[*source])
                })
                .max()
                .unwrap_or(0)
        })
        .sum::<usize>();

    score.active_priority = (1 << hierarchy.families.len()) - 1;
    score
}

fn root_source_is_available(
    hierarchy: &MergePlan,
    profile: &SyntaxProfile,
    allocations: &[FamilyAllocation],
    source: Option<usize>,
) -> bool {
    let Some(source) = source else {
        // Generated foreground tones form the palette's shared neutral lineage.
        return true;
    };
    allocations
        .iter()
        .filter(|allocation| hierarchy.families[allocation.family].parent.is_none())
        .all(|allocation| {
            allocation.source != Some(source)
                && source_cluster(profile, source).is_none_or(|cluster| {
                    allocation
                        .source
                        .and_then(|source| source_cluster(profile, source))
                        != Some(cluster)
                })
        })
}

fn search_semantic_forest(
    search: &mut Search,
    request: &SemanticSearchRequest<'_>,
    family: usize,
    allocations: &mut Vec<FamilyAllocation>,
    best: &mut Vec<FamilyAllocation>,
    prune: bool,
) -> Result<()> {
    if family == request.hierarchy.families.len() {
        if semantic_score(request.hierarchy, request.profile, allocations)
            > semantic_score(request.hierarchy, request.profile, best)
        {
            *best = allocations.clone();
        }
        return Ok(());
    }

    // The bound deliberately overestimates every remaining score component, so
    // pruning cannot remove a feasible improvement.
    if prune
        && optimistic_semantic_score(request.hierarchy, request.profile, family, allocations)
            <= semantic_score(request.hierarchy, request.profile, best)
    {
        return Ok(());
    }

    let semantic = &request.hierarchy.families[family];
    let sources = if let Some(parent) = semantic.parent {
        let Some(parent) = allocations
            .iter()
            .find(|allocation| allocation.family == parent)
        else {
            return search_semantic_forest(search, request, family + 1, allocations, best, prune);
        };
        vec![parent.source]
    } else {
        root_sources(request.profile, semantic.source_preference)
    };

    for source in sources {
        if semantic.parent.is_none()
            && !root_source_is_available(request.hierarchy, request.profile, allocations, source)
        {
            continue;
        }

        let preferred_saliency = semantic
            .parent
            .map(|parent| {
                let parent = allocations
                    .iter()
                    .find(|allocation| allocation.family == parent)
                    .expect("active syntax branch must have its parent allocation");
                (parent.measured_saliency + semantic.parent_saliency_delta)
                    .clamp(request.subdued_saliency + 0.06, 0.96)
            })
            .unwrap_or(semantic.fallback_saliency);
        let candidate = match allocate_family(
            search,
            FamilyFitRequest {
                preference_contexts: request.preference_contexts,
                required_contexts: request.required_contexts,
                reference: request.reference,
                profile: request.profile,
                family,
                source,
                preferred_saliency,
            },
        ) {
            Ok(candidate) => candidate,
            Err(error) if error.is_infeasible() => continue,
            Err(error) => {
                panic!("validated syntax family request failed unexpectedly: {error}")
            }
        };

        allocations.push(candidate);

        if !semantic_above_subdued(allocations, request.subdued_saliency) {
            allocations.pop();
            continue;
        }

        let candidate = allocations
            .last()
            .expect("newly pushed syntax allocation must be present");
        let existing = request.fixed_outputs.iter().copied().chain(
            allocations[..allocations.len() - 1]
                .iter()
                .map(|allocation| allocation.output.as_str()),
        );
        if separated_from(&candidate.output, existing)? {
            search_semantic_forest(search, request, family + 1, allocations, best, prune)?;
        }

        allocations.pop();
    }

    search_semantic_forest(search, request, family + 1, allocations, best, prune)?;
    Ok(())
}

fn select_semantic_plan_with_pruning(
    search: &mut Search,
    preference_contexts: &[String],
    required_contexts: &[String],
    reference: &str,
    profile: &SyntaxProfile,
    fixed_outputs: [&str; 2],
    prune: bool,
) -> Result<SemanticPlan> {
    let subdued_saliency = geometric_contrast(fixed_outputs[1], preference_contexts)?.ln()
        / geometric_contrast(reference, preference_contexts)?
            .ln()
            .max(1e-12);
    let hierarchy = MergePlan::hierarchy();
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
    search_semantic_forest(search, &request, 0, &mut current, &mut allocations, prune)?;

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

fn select_semantic_plan(
    search: &mut Search,
    preference_contexts: &[String],
    required_contexts: &[String],
    reference: &str,
    profile: &SyntaxProfile,
    fixed_outputs: [&str; 2],
) -> Result<SemanticPlan> {
    select_semantic_plan_with_pruning(
        search,
        preference_contexts,
        required_contexts,
        reference,
        profile,
        fixed_outputs,
        true,
    )
}

fn fit_subdued(
    search: &mut Search,
    palette: &ResolvedPalette,
    profile: &SyntaxProfile,
    preference_contexts: &[String],
    required_contexts: &[String],
    reference: &str,
) -> Result<SaliencyFit> {
    let muted_chroma = oklab_to_oklch(lab(&palette.colors["muted"])?)[1];
    let subdued_chroma_cap = if profile.clusters.is_empty() {
        muted_chroma.max(0.005)
    } else {
        profile.chroma_envelope.ordinary_maximum
    };
    let fit = fit_tone(
        search,
        ToneFitRequest {
            seed: &palette.colors["muted"],
            reference,
            preference_contexts,
            required_contexts,
            preference_floor: SYNTAX_SUBDUED_FLOOR,
            required_floor: SYNTAX_SUBDUED_OVERLAY_FLOOR,
            preferred_saliency: SUBDUED_SALIENCY,
            bounds: FitBounds {
                upper_chroma: subdued_chroma_cap,
                ..FitBounds::default()
            },
        },
    )?;
    if fit.output != reference {
        return Ok(fit);
    }

    let reference_chroma = oklab_to_oklch(lab(reference)?)[1];
    let fit = fit_tone(
        search,
        ToneFitRequest {
            seed: reference,
            reference,
            preference_contexts,
            required_contexts,
            preference_floor: SYNTAX_SUBDUED_FLOOR,
            required_floor: SYNTAX_SUBDUED_OVERLAY_FLOOR,
            preferred_saliency: SUBDUED_SALIENCY,
            bounds: FitBounds {
                upper_chroma: reference_chroma.max(1e-9),
                ..FitBounds::default()
            },
        },
    )?;
    if fit.output == reference {
        return Err(crate::Error::infeasible(
            "base and subdued syntax roles collided exactly",
        ));
    }

    Ok(fit)
}

pub fn build_syntax(
    search: &mut Search,
    palette: &ResolvedPalette,
    contexts: SyntaxContexts<'_>,
    saliency_reference: &str,
    predictive: &str,
    diff_sources: [&str; 3],
) -> Result<Map<String, Value>> {
    palette.validate()?;
    let preference_contexts = contexts.ordinary;
    let required_contexts = contexts.rendered;
    if preference_contexts.is_empty() || required_contexts.is_empty() {
        return Err(crate::Error::invalid(
            "syntax generation requires ordinary and rendered contexts",
        ));
    }
    for (name, value) in [
        ("saliency reference", saliency_reference),
        ("predictive", predictive),
    ] {
        lab(value).map_err(|error| error.context(name))?;
    }
    for (index, source) in diff_sources.iter().enumerate() {
        lab(source).map_err(|error| error.context(format!("diff source {index}")))?;
    }
    for (kind, values) in [
        ("ordinary syntax context", preference_contexts),
        ("rendered syntax context", required_contexts),
    ] {
        for (index, value) in values.iter().enumerate() {
            lab(value).map_err(|error| error.context(format!("{kind} {index}")))?;
        }
    }
    match build_syntax_from_validated_inputs(
        search,
        palette,
        preference_contexts,
        required_contexts,
        saliency_reference,
        predictive,
        diff_sources,
    ) {
        Err(error) if error.kind() == crate::ErrorKind::InvalidInput => {
            panic!("validated syntax inputs produced invalid internal state: {error}")
        }
        result => result,
    }
}

fn build_syntax_from_validated_inputs(
    search: &mut Search,
    palette: &ResolvedPalette,
    preference_contexts: &[String],
    required_contexts: &[String],
    saliency_reference: &str,
    predictive: &str,
    diff_sources: [&str; 3],
) -> Result<Map<String, Value>> {
    let profile = profile::measure(palette)?;
    let base = saliency_reference.to_owned();

    let primary_minimum = minimum_contrast(&base, required_contexts)?;
    if primary_minimum < SYNTAX_PRIMARY_FLOOR - 1e-12 {
        return Err(crate::Error::infeasible(format!(
            "editor primary reaches only {primary_minimum:.3}:1 on a rendered syntax context"
        )));
    }

    let subdued_fit = fit_subdued(
        search,
        palette,
        &profile,
        preference_contexts,
        required_contexts,
        saliency_reference,
    )?;
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
        if semantic_plan.family_for(role).is_some() {
            continue;
        }

        let fallback = semantic_plan.fallback_for(role);
        role_colors.insert(
            role,
            role_colors
                .get(&fallback)
                .expect("syntax fallback role must already have a color")
                .clone(),
        );
    }

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
        .map_err(|error| error.context("syntax diff semantic pair"))?;
    let syntax_change =
        search.fit_color(diff_sources[1], required_contexts, SYNTAX_SEMANTIC_FLOOR)?;
    role_colors.extend([
        (SemanticRole::DiffChange, syntax_change.clone()),
        (SemanticRole::DiffAdd, syntax_add.clone()),
        (SemanticRole::DiffDelete, syntax_delete.clone()),
    ]);

    let mut output = Map::new();
    for capture in CAPTURE_POLICIES {
        let color = role_colors.get(&capture.role).unwrap_or_else(|| {
            panic!(
                "generated syntax role {} must have an allocated color",
                capture.role.as_str()
            )
        });
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
    use crate::color::gamut_map_oklch_unchecked;
    use crate::palette::Provenance;
    use proptest::prelude::*;

    fn allocation(family: usize) -> FamilyAllocation {
        FamilyAllocation {
            family,
            roles: Vec::new(),
            source: None,
            output: format!("#{family:06x}"),
            measured_saliency: 0.8,
        }
    }

    fn five_source_profile() -> SyntaxProfile {
        let mut colors = BTreeMap::new();
        let mut provenance = BTreeMap::new();
        for key in SOURCE_KEY_ORDER {
            colors.insert(key.to_owned(), "#777777".to_owned());
            provenance.insert(key.to_owned(), Provenance::Derived);
        }
        for (index, key) in SOURCE_KEY_ORDER[..5].iter().enumerate() {
            colors.insert(
                (*key).to_owned(),
                gamut_map_oklch_unchecked(0.65, 0.10, index as f64).opaque_hex(),
            );
            provenance.insert((*key).to_owned(), Provenance::Direct);
        }
        profile::measure(&ResolvedPalette {
            mode: "dark".into(),
            colors,
            provenance,
        })
        .unwrap()
    }

    fn generated_profile(
        source_count: usize,
        hue_degrees: [u16; 5],
        chroma_steps: [u8; 5],
        source_aliases: [u8; 5],
    ) -> SyntaxProfile {
        let mut colors = BTreeMap::new();
        let mut provenance = BTreeMap::new();
        for key in SOURCE_KEY_ORDER {
            colors.insert(key.to_owned(), "#777777".to_owned());
            provenance.insert(key.to_owned(), Provenance::Derived);
        }
        for (index, key) in SOURCE_KEY_ORDER[..source_count].iter().enumerate() {
            let source = usize::from(source_aliases[index]) % source_count;
            let hue = f64::from(hue_degrees[source]).to_radians();
            let chroma = 0.03 + f64::from(chroma_steps[source]) / 1_700.0;
            colors.insert(
                (*key).to_owned(),
                gamut_map_oklch_unchecked(0.65, chroma, hue).opaque_hex(),
            );
            provenance.insert((*key).to_owned(), Provenance::Direct);
        }
        profile::measure(&ResolvedPalette {
            mode: "dark".into(),
            colors,
            provenance,
        })
        .unwrap()
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(16))]

        #[test]
        fn pruned_semantic_planner_matches_exhaustive_search(
            source_count in 0_usize..=5,
            hue_degrees in any::<[u16; 5]>(),
            chroma_steps in any::<[u8; 5]>(),
            source_aliases in any::<[u8; 5]>(),
        ) {
            let profile = generated_profile(
                source_count,
                hue_degrees.map(|hue| hue % 360),
                chroma_steps,
                source_aliases,
            );
            let contexts = vec!["#111111".to_owned()];
            let mut pruned_search = Search::default();
            let mut exhaustive_search = Search::default();
            let pruned = select_semantic_plan_with_pruning(
                &mut pruned_search,
                &contexts,
                &contexts,
                "#eeeeee",
                &profile,
                ["#eeeeee", "#777777"],
                true,
            );
            let exhaustive = select_semantic_plan_with_pruning(
                &mut exhaustive_search,
                &contexts,
                &contexts,
                "#eeeeee",
                &profile,
                ["#eeeeee", "#777777"],
                false,
            );

            match (pruned, exhaustive) {
                (Ok(pruned), Ok(exhaustive)) => {
                    prop_assert_eq!(pruned.plan, exhaustive.plan);
                    prop_assert_eq!(pruned.allocations, exhaustive.allocations);
                }
                (Err(pruned), Err(exhaustive)) => {
                    prop_assert_eq!(pruned.kind(), exhaustive.kind());
                    prop_assert_eq!(pruned.to_string(), exhaustive.to_string());
                }
                (pruned, exhaustive) => prop_assert!(
                    false,
                    "pruned/exhaustive result mismatch: pruned={}, exhaustive={}",
                    pruned.is_ok(),
                    exhaustive.is_ok(),
                ),
            }
        }
    }

    #[test]
    fn semantic_score_preserves_roots_before_refinements() {
        let hierarchy = MergePlan::hierarchy();
        let profile = SyntaxProfile {
            chroma_envelope: profile::ChromaEnvelope {
                target_median: 0.035,
                ordinary_maximum: 0.055,
            },
            evidence: Vec::new(),
            clusters: Vec::new(),
        };
        let three_roots = [allocation(0), allocation(1), allocation(2)];
        let two_roots_and_all_branches = [
            allocation(0),
            allocation(1),
            allocation(3),
            allocation(4),
            allocation(5),
        ];
        assert!(
            semantic_score(&hierarchy, &profile, &three_roots)
                > semantic_score(&hierarchy, &profile, &two_roots_and_all_branches)
        );

        let two_roots = [allocation(0), allocation(1)];
        let two_roots_and_value = [allocation(0), allocation(1), allocation(3)];
        assert!(
            semantic_score(&hierarchy, &profile, &two_roots_and_value)
                > semantic_score(&hierarchy, &profile, &two_roots)
        );
    }

    #[test]
    fn root_sources_never_truncates_authored_candidates() {
        let profile = five_source_profile();
        let sources = root_sources(&profile, SemanticRole::Callable);
        assert_eq!(sources.len(), 6);
        assert_eq!(
            sources.into_iter().collect::<BTreeSet<_>>(),
            (0..5).map(Some).chain(std::iter::once(None)).collect()
        );
    }

    #[test]
    fn branch_and_bound_matches_exhaustive_search() {
        let profile = five_source_profile();
        let contexts = vec!["#111111".to_owned()];
        let mut pruned_search = Search::default();
        let mut exhaustive_search = Search::default();
        let pruned = select_semantic_plan_with_pruning(
            &mut pruned_search,
            &contexts,
            &contexts,
            "#eeeeee",
            &profile,
            ["#eeeeee", "#777777"],
            true,
        )
        .unwrap();
        let exhaustive = select_semantic_plan_with_pruning(
            &mut exhaustive_search,
            &contexts,
            &contexts,
            "#eeeeee",
            &profile,
            ["#eeeeee", "#777777"],
            false,
        )
        .unwrap();
        assert_eq!(pruned.plan, exhaustive.plan);
        assert_eq!(pruned.allocations, exhaustive.allocations);
    }
}
