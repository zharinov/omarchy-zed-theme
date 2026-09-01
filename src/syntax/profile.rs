//! Measures the authored Omarchy palette's syntax character.
//!
//! Breadth and intensity are independent continuous signals. Provenance decides
//! which colors are evidence; normal and bright authored slots participate in the
//! same perceptual budget.

use crate::Result;
use crate::color::{lab, oklab_to_oklch};
use crate::palette::{Provenance, ResolvedPalette};
use std::collections::BTreeMap;
use std::f64::consts::{PI, TAU};

pub const CHROMA_EVIDENCE: f64 = 0.025;
const HUE_CLUSTER_LIMIT: f64 = 35.0 * PI / 180.0;
const HUE_SIMILARITY_FULL: f64 = 25.0 * PI / 180.0;
const HUE_SIMILARITY_ZERO: f64 = 45.0 * PI / 180.0;
const NEUTRAL_MEDIAN_CHROMA: f64 = 0.035;
const NEUTRAL_MAXIMUM_CHROMA: f64 = 0.055;

const EVIDENCE_KEYS: [&str; 15] = [
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
pub struct EvidenceColor {
    pub value: String,
    pub keys: Vec<&'static str>,
    pub lightness: f64,
    pub chroma: f64,
    pub hue: f64,
    weight: f64,
}

#[derive(Clone, Debug)]
pub struct HueCluster {
    pub members: Vec<usize>,
    pub representative: usize,
}

#[derive(Clone, Debug)]
pub struct ChromaEnvelope {
    pub target_median: f64,
    pub ordinary_maximum: f64,
}

#[derive(Clone, Debug)]
pub struct SyntaxProfile {
    pub chroma_envelope: ChromaEnvelope,
    pub evidence: Vec<EvidenceColor>,
    pub clusters: Vec<HueCluster>,
}

fn circular_distance(left: f64, right: f64) -> f64 {
    let difference = (left - right).abs();
    difference.min(TAU - difference)
}

fn quantile(sorted: &[f64], probability: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let position = probability * (sorted.len() - 1) as f64;
    let lower = position.floor() as usize;
    let upper = position.ceil() as usize;
    sorted[lower] + (sorted[upper] - sorted[lower]) * position.fract()
}

fn smoothstep(lower: f64, upper: f64, value: f64) -> f64 {
    let position = ((value - lower) / (upper - lower)).clamp(0.0, 1.0);
    position * position * (3.0 - 2.0 * position)
}

fn hue_similarity(distance: f64) -> f64 {
    1.0 - smoothstep(HUE_SIMILARITY_FULL, HUE_SIMILARITY_ZERO, distance)
}

fn complete_link_clusters(evidence: &[EvidenceColor]) -> Vec<HueCluster> {
    let mut clusters = evidence
        .iter()
        .enumerate()
        .filter(|(_, color)| color.chroma >= CHROMA_EVIDENCE - 1e-12)
        .map(|(index, _)| vec![index])
        .collect::<Vec<_>>();

    loop {
        let mut best: Option<(f64, Vec<usize>, usize, usize)> = None;
        for left in 0..clusters.len() {
            for right in left + 1..clusters.len() {
                let maximum = clusters[left]
                    .iter()
                    .flat_map(|left_index| {
                        clusters[right].iter().map(move |right_index| {
                            circular_distance(evidence[*left_index].hue, evidence[*right_index].hue)
                        })
                    })
                    .fold(0.0, f64::max);
                if maximum > HUE_CLUSTER_LIMIT + 1e-12 {
                    continue;
                }
                let mut members = clusters[left].clone();
                members.extend(&clusters[right]);
                members.sort_unstable();
                let candidate = (maximum, members, left, right);
                if best.as_ref().is_none_or(|current| {
                    candidate
                        .0
                        .total_cmp(&current.0)
                        .then_with(|| candidate.1.cmp(&current.1))
                        .is_lt()
                }) {
                    best = Some(candidate);
                }
            }
        }

        let Some((_, members, left, right)) = best else {
            break;
        };
        clusters[left] = members;
        clusters.remove(right);
    }

    let mut output = clusters
        .into_iter()
        .map(|members| {
            let representative = *members
                .iter()
                .min_by(|left, right| {
                    evidence[**right]
                        .chroma
                        .total_cmp(&evidence[**left].chroma)
                        .then_with(|| evidence[**left].value.cmp(&evidence[**right].value))
                })
                .expect("a syntax hue cluster must contain evidence");
            HueCluster {
                members,
                representative,
            }
        })
        .collect::<Vec<_>>();
    output.sort_by(|left, right| {
        evidence[left.representative]
            .hue
            .total_cmp(&evidence[right.representative].hue)
            .then_with(|| {
                evidence[left.representative]
                    .value
                    .cmp(&evidence[right.representative].value)
            })
    });
    output
}

pub fn measure(palette: &ResolvedPalette) -> Result<SyntaxProfile> {
    palette.validate_keys(&EVIDENCE_KEYS)?;
    let mut deduplicated: BTreeMap<String, Vec<&'static str>> = BTreeMap::new();
    for key in EVIDENCE_KEYS {
        let value = palette
            .colors
            .get(key)
            .expect("validated syntax evidence color must be present")
            .clone();
        let provenance = palette
            .provenance
            .get(key)
            .copied()
            .expect("validated syntax evidence provenance must be present");

        if provenance == Provenance::Derived {
            continue;
        }

        deduplicated.entry(value).or_default().push(key);
    }

    let mut evidence = deduplicated
        .into_iter()
        .map(|(value, keys)| {
            let [lightness, chroma, hue] = oklab_to_oklch(lab(&value)?);
            Ok(EvidenceColor {
                value,
                keys,
                lightness,
                chroma,
                hue,
                weight: (chroma - CHROMA_EVIDENCE).max(0.0),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    evidence.sort_by(|left, right| {
        left.hue
            .total_cmp(&right.hue)
            .then_with(|| left.value.cmp(&right.value))
    });

    let clusters = complete_link_clusters(&evidence);

    let total_weight: f64 = evidence.iter().map(|color| color.weight).sum();
    let effective_hue_families = if total_weight <= 1e-12 {
        0.0
    } else {
        let concentration = evidence
            .iter()
            .map(|left| {
                let left_weight = left.weight / total_weight;
                evidence
                    .iter()
                    .map(|right| {
                        left_weight
                            * (right.weight / total_weight)
                            * hue_similarity(circular_distance(left.hue, right.hue))
                    })
                    .sum::<f64>()
            })
            .sum::<f64>();
        1.0 / concentration.max(1e-12)
    };

    let mut chromas = evidence
        .iter()
        .filter(|color| color.chroma >= CHROMA_EVIDENCE - 1e-12)
        .map(|color| color.chroma)
        .collect::<Vec<_>>();
    chromas.sort_by(f64::total_cmp);
    let source_median_chroma = quantile(&chromas, 0.5);
    let source_q90_chroma = quantile(&chromas, 0.9);

    let native_median = source_median_chroma.clamp(0.045, 0.140);
    let palette_native_weight = smoothstep(1.0, 2.5, effective_hue_families);
    let target_median =
        NEUTRAL_MEDIAN_CHROMA + palette_native_weight * (native_median - NEUTRAL_MEDIAN_CHROMA);
    let native_maximum = source_q90_chroma.clamp(0.070, 0.180);
    let peak_support = ((source_q90_chroma - CHROMA_EVIDENCE) / 0.045).clamp(0.0, 1.0);
    let envelope_support = peak_support.max(palette_native_weight);
    let ordinary_maximum = (NEUTRAL_MAXIMUM_CHROMA
        + envelope_support * (native_maximum - NEUTRAL_MAXIMUM_CHROMA))
        .max(target_median);

    Ok(SyntaxProfile {
        chroma_envelope: ChromaEnvelope {
            target_median,
            ordinary_maximum,
        },
        evidence,
        clusters,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::gamut_map_oklch_unchecked;
    use std::collections::BTreeMap;

    fn palette(samples: &[(&str, f64, f64, Provenance)]) -> ResolvedPalette {
        let mut colors = BTreeMap::new();
        let mut provenance = BTreeMap::new();
        for key in EVIDENCE_KEYS {
            colors.insert(key.to_owned(), "#777777".to_owned());
            provenance.insert(key.to_owned(), Provenance::Derived);
        }
        colors.insert("background".into(), "#111111".into());
        provenance.insert("background".into(), Provenance::Direct);
        for (key, chroma, hue, source) in samples {
            colors.insert(
                (*key).to_owned(),
                gamut_map_oklch_unchecked(0.65, *chroma, *hue).opaque_hex(),
            );
            provenance.insert((*key).to_owned(), *source);
        }
        ResolvedPalette {
            mode: "dark".into(),
            colors,
            provenance,
        }
    }

    #[test]
    fn duplicate_rgb_produces_one_evidence_color() {
        let distinct = palette(&[
            ("accent", 0.12, 0.2, Provenance::Direct),
            ("blue", 0.12, 2.5, Provenance::Direct),
        ]);
        let mut duplicate = distinct.clone();
        duplicate
            .colors
            .insert("blue".into(), duplicate.colors["accent"].clone());
        assert_eq!(measure(&duplicate).unwrap().evidence.len(), 1);
    }

    #[test]
    fn derived_colors_are_not_profile_evidence() {
        let profile = measure(&palette(&[
            ("red", 0.18, 0.2, Provenance::Derived),
            ("blue", 0.18, 3.5, Provenance::Derived),
        ]))
        .unwrap();
        assert!(profile.evidence.is_empty());
        assert!(profile.clusters.is_empty());
    }

    #[test]
    fn evidence_threshold_has_no_large_envelope_jump() {
        let below = measure(&palette(&[
            ("red", 0.10, 0.2, Provenance::Direct),
            ("blue", CHROMA_EVIDENCE - 0.0002, 3.5, Provenance::Direct),
        ]))
        .unwrap();
        let above = measure(&palette(&[
            ("red", 0.10, 0.2, Provenance::Direct),
            ("blue", CHROMA_EVIDENCE + 0.0002, 3.5, Provenance::Direct),
        ]))
        .unwrap();
        assert!(
            (above.chroma_envelope.target_median - below.chroma_envelope.target_median).abs()
                < 0.01
        );
        assert!(
            (above.chroma_envelope.ordinary_maximum - below.chroma_envelope.ordinary_maximum).abs()
                < 0.01
        );
    }

    #[test]
    fn every_perceptible_authored_color_contributes_a_hue_cluster() {
        let neutral = measure(&palette(&[])).unwrap();
        assert!(neutral.clusters.is_empty());

        let weak = measure(&palette(&[(
            "accent",
            CHROMA_EVIDENCE + 0.005,
            0.2,
            Provenance::Direct,
        )]))
        .unwrap();
        assert!(!weak.evidence.is_empty());
        assert_eq!(weak.clusters.len(), 1);

        let accent = measure(&palette(&[("accent", 0.14, 0.2, Provenance::Direct)])).unwrap();
        assert_eq!(accent.clusters.len(), 1);
    }

    #[test]
    fn hue_clustering_has_no_three_cluster_gate() {
        let native = measure(&palette(&[
            ("red", 0.12, 0.2, Provenance::Direct),
            ("green", 0.12, 2.1, Provenance::Direct),
            ("blue", 0.12, 4.0, Provenance::Direct),
        ]))
        .unwrap();
        assert_eq!(native.clusters.len(), 3);

        let dominant_cluster = measure(&palette(&[
            ("red", 0.18, 0.1, Provenance::Direct),
            ("orange", 0.18, 0.2, Provenance::Direct),
            ("green", 0.03, 2.1, Provenance::Direct),
            ("blue", 0.03, 4.0, Provenance::Direct),
        ]))
        .unwrap();
        assert_eq!(dominant_cluster.clusters.len(), 3);

        let two_clusters = measure(&palette(&[
            ("red", 0.12, 0.0, Provenance::Direct),
            ("orange", 0.12, 34.0 * PI / 180.0, Provenance::Direct),
            ("green", 0.12, PI, Provenance::Direct),
            ("cyan", 0.12, 214.0 * PI / 180.0, Provenance::Direct),
        ]))
        .unwrap();
        assert_eq!(two_clusters.clusters.len(), 2);
    }

    #[test]
    fn authored_neutral_colors_are_tone_evidence_not_hue_clusters() {
        let profile = measure(&palette(&[("accent", 0.0, 0.0, Provenance::Direct)])).unwrap();
        assert_eq!(profile.evidence.len(), 1);
        assert!(profile.clusters.is_empty());
    }

    #[test]
    fn hue_families_use_complete_link_clustering() {
        let profile = measure(&palette(&[
            ("red", 0.10, 0.0, Provenance::Direct),
            ("orange", 0.10, 30.0 * PI / 180.0, Provenance::Direct),
            ("yellow", 0.10, 60.0 * PI / 180.0, Provenance::Direct),
        ]))
        .unwrap();
        assert_eq!(profile.clusters.len(), 2);
        assert!(profile.clusters.iter().all(|cluster| {
            cluster.members.iter().all(|left| {
                cluster.members.iter().all(|right| {
                    circular_distance(profile.evidence[*left].hue, profile.evidence[*right].hue)
                        <= HUE_CLUSTER_LIMIT + 1e-12
                })
            })
        }));
    }
}
