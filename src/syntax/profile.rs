//! Measures the authored Omarchy palette's syntax character.
//!
//! Breadth and intensity are independent continuous signals. Provenance decides
//! which colors are evidence; the descriptive baseline kind is never consumed by
//! construction or validation.

use crate::Result;
use crate::color::{lab, oklab_to_oklch};
use crate::palette::{Provenance, ResolvedPalette};
use crate::search::round6;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::f64::consts::{PI, TAU};

pub const CHROMA_EVIDENCE: f64 = 0.025;
const HUE_CLUSTER_LIMIT: f64 = 35.0 * PI / 180.0;
const HUE_SIMILARITY_FULL: f64 = 25.0 * PI / 180.0;
const HUE_SIMILARITY_ZERO: f64 = 45.0 * PI / 180.0;
const ALLOCATION_CHROMA_FLOOR: f64 = 0.005;
const NEUTRAL_MEDIAN_CHROMA: f64 = 0.035;
const NEUTRAL_MAXIMUM_CHROMA: f64 = 0.055;
pub const SCAFFOLD_MAXIMUM_CHROMA: f64 = NEUTRAL_MAXIMUM_CHROMA;

const EVIDENCE_KEYS: [&str; 8] = [
    "green", "blue", "magenta", "yellow", "red", "cyan", "orange", "accent",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BaselineKind {
    Neutral,
    AccentLed,
    PaletteNative,
}

impl BaselineKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Neutral => "neutral",
            Self::AccentLed => "accent_led",
            Self::PaletteNative => "palette_native",
        }
    }
}

#[derive(Clone, Debug)]
pub struct EvidenceColor {
    pub value: String,
    pub keys: Vec<&'static str>,
    pub provenance: Provenance,
    pub lightness: f64,
    pub chroma: f64,
    pub hue: f64,
    weight: f64,
}

#[derive(Clone, Debug)]
pub struct HueCluster {
    pub members: Vec<usize>,
    pub weight: f64,
    pub representative: usize,
}

#[derive(Clone, Debug)]
pub struct ChromaEnvelope {
    pub target_median: f64,
    pub ordinary_maximum: f64,
}

#[derive(Clone, Debug)]
pub struct SyntaxProfile {
    pub authored_breadth: f64,
    pub authored_intensity: f64,
    pub effective_hue_families: f64,
    pub source_median_chroma: f64,
    pub source_q90_chroma: f64,
    pub requested_family_count: usize,
    pub chroma_envelope: ChromaEnvelope,
    pub scaffold_weight: f64,
    pub scaffold_phase: f64,
    baseline_kind: BaselineKind,
    pub authored_colors: Vec<EvidenceColor>,
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
    let mut clusters: Vec<Vec<usize>> = (0..evidence.len()).map(|index| vec![index]).collect();

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
            let weight = members.iter().map(|index| evidence[*index].weight).sum();
            let representative = *members
                .iter()
                .min_by(|left, right| {
                    evidence[**right]
                        .chroma
                        .total_cmp(&evidence[**left].chroma)
                        .then_with(|| evidence[**left].value.cmp(&evidence[**right].value))
                })
                .unwrap();
            HueCluster {
                members,
                weight,
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
    let mut deduplicated: BTreeMap<String, (Vec<&'static str>, Provenance)> = BTreeMap::new();
    for key in EVIDENCE_KEYS {
        let value = palette.colors[key].clone();
        let provenance = palette
            .provenance
            .get(key)
            .copied()
            .unwrap_or(Provenance::Derived);
        if provenance == Provenance::Derived {
            continue;
        }
        let entry = deduplicated
            .entry(value)
            .or_insert_with(|| (Vec::new(), provenance));
        entry.0.push(key);
        if provenance == Provenance::Direct {
            entry.1 = Provenance::Direct;
        }
    }

    let mut authored_colors = deduplicated
        .into_iter()
        .map(|(value, (keys, provenance))| {
            let [lightness, chroma, hue] = oklab_to_oklch(lab(&value)?);
            Ok(
                (chroma >= ALLOCATION_CHROMA_FLOOR - 1e-12).then_some(EvidenceColor {
                    value,
                    keys,
                    provenance,
                    lightness,
                    chroma,
                    hue,
                    weight: (chroma - CHROMA_EVIDENCE).max(0.0),
                }),
            )
        })
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    authored_colors.sort_by(|left, right| {
        left.hue
            .total_cmp(&right.hue)
            .then_with(|| left.value.cmp(&right.value))
    });
    let evidence = authored_colors
        .iter()
        .filter(|color| color.chroma >= CHROMA_EVIDENCE - 1e-12)
        .cloned()
        .collect::<Vec<_>>();

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
    let authored_breadth = ((effective_hue_families - 1.0) / 5.0).clamp(0.0, 1.0);

    let mut chromas = evidence
        .iter()
        .map(|color| color.chroma)
        .collect::<Vec<_>>();
    chromas.sort_by(f64::total_cmp);
    let source_median_chroma = quantile(&chromas, 0.5);
    let source_q90_chroma = quantile(&chromas, 0.9);
    let median_intensity = ((source_median_chroma - CHROMA_EVIDENCE) / 0.115).clamp(0.0, 1.0);
    let peak_intensity = ((source_q90_chroma - CHROMA_EVIDENCE) / 0.155).clamp(0.0, 1.0);
    let authored_intensity = 0.55 * median_intensity + 0.45 * peak_intensity;

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
    let requested_family_count = (4.0 + 4.0 * authored_breadth).round() as usize;
    let scaffold_weight = 1.0 - palette_native_weight;

    let baseline_kind = if effective_hue_families <= 1e-12 {
        BaselineKind::Neutral
    } else if effective_hue_families < 2.5 {
        BaselineKind::AccentLed
    } else {
        BaselineKind::PaletteNative
    };
    let scaffold_phase = authored_colors
        .iter()
        .max_by(|left, right| {
            left.chroma
                .total_cmp(&right.chroma)
                .then_with(|| right.value.cmp(&left.value))
        })
        .map(|color| color.hue)
        .or_else(|| {
            ["accent", "background"].into_iter().find_map(|key| {
                let [_, chroma, hue] = oklab_to_oklch(lab(&palette.colors[key]).ok()?);
                (chroma >= 0.005).then_some(hue)
            })
        })
        .unwrap_or(if palette.mode == "dark" { 35.0 } else { 215.0 } * PI / 180.0);

    Ok(SyntaxProfile {
        authored_breadth,
        authored_intensity,
        effective_hue_families,
        source_median_chroma,
        source_q90_chroma,
        requested_family_count,
        chroma_envelope: ChromaEnvelope {
            target_median,
            ordinary_maximum,
        },
        scaffold_weight,
        scaffold_phase,
        baseline_kind,
        authored_colors,
        evidence,
        clusters,
    })
}

impl SyntaxProfile {
    pub fn audit(&self) -> Value {
        let color_audit = |colors: &[EvidenceColor]| {
            colors
                .iter()
                .map(|color| {
                    json!({
                        "value": color.value,
                        "keys": color.keys,
                        "provenance": format!("{:?}", color.provenance).to_ascii_lowercase(),
                        "oklch": [round6(color.lightness), round6(color.chroma), round6(color.hue)],
                        "breadth_weight": round6(color.weight),
                    })
                })
                .collect::<Vec<_>>()
        };
        let authored_colors = color_audit(&self.authored_colors);
        let evidence = color_audit(&self.evidence);

        let clusters = self
            .clusters
            .iter()
            .map(|cluster| {
                json!({
                    "members": cluster.members.iter().map(|index| self.evidence[*index].value.clone()).collect::<Vec<_>>(),
                    "weight": round6(cluster.weight),
                    "representative": self.evidence[cluster.representative].value,
                })
            })
            .collect::<Vec<_>>();

        json!({
            "thresholds": {
                "chroma_evidence": CHROMA_EVIDENCE,
                "complete_link_hue_degrees": 35.0,
                "continuous_hue_similarity_full_degrees": 25.0,
                "continuous_hue_similarity_zero_degrees": 45.0,
                "neutral_target_median_chroma": NEUTRAL_MEDIAN_CHROMA,
                "neutral_maximum_ordinary_chroma": NEUTRAL_MAXIMUM_CHROMA,
            },
            "scores": {
                "authored_breadth": round6(self.authored_breadth),
                "authored_intensity": round6(self.authored_intensity),
                "effective_hue_families": round6(self.effective_hue_families),
            },
            "source_chroma": {
                "median": round6(self.source_median_chroma),
                "q90": round6(self.source_q90_chroma),
            },
            "chroma_envelope": {
                "target_median_chroma": round6(self.chroma_envelope.target_median),
                "maximum_ordinary_chroma": round6(self.chroma_envelope.ordinary_maximum),
            },
            "requested_family_count": self.requested_family_count,
            "scaffold_weight": round6(self.scaffold_weight),
            "baseline_kind": self.baseline_kind.as_str(),
            "baseline_kind_authoritative": false,
            "authored_colors": authored_colors,
            "evidence": evidence,
            "hue_clusters": clusters,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::gamut_map_oklch;
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
                gamut_map_oklch(0.65, *chroma, *hue).opaque_hex(),
            );
            provenance.insert((*key).to_owned(), *source);
        }
        ResolvedPalette {
            mode: "dark".into(),
            colors,
            extras: BTreeMap::new(),
            resolver_stderr: String::new(),
            provenance,
        }
    }

    #[test]
    fn duplicate_rgb_does_not_increase_breadth() {
        let distinct = palette(&[
            ("accent", 0.12, 0.2, Provenance::Direct),
            ("blue", 0.12, 2.5, Provenance::Direct),
        ]);
        let mut duplicate = distinct.clone();
        duplicate
            .colors
            .insert("blue".into(), duplicate.colors["accent"].clone());
        assert!(
            measure(&duplicate).unwrap().authored_breadth
                <= measure(&distinct).unwrap().authored_breadth
        );
        assert_eq!(measure(&duplicate).unwrap().evidence.len(), 1);
    }

    #[test]
    fn derived_colors_are_not_profile_evidence() {
        let profile = measure(&palette(&[
            ("red", 0.18, 0.2, Provenance::Derived),
            ("blue", 0.18, 3.5, Provenance::Derived),
        ]))
        .unwrap();
        assert_eq!(profile.effective_hue_families, 0.0);
        assert_eq!(profile.authored_intensity, 0.0);
    }

    #[test]
    fn reducing_authored_chroma_cannot_increase_intensity() {
        let vivid = measure(&palette(&[
            ("red", 0.14, 0.2, Provenance::Direct),
            ("blue", 0.12, 3.5, Provenance::Direct),
            ("green", 0.10, 2.1, Provenance::Alias),
        ]))
        .unwrap();
        let soft = measure(&palette(&[
            ("red", 0.07, 0.2, Provenance::Direct),
            ("blue", 0.06, 3.5, Provenance::Direct),
            ("green", 0.05, 2.1, Provenance::Alias),
        ]))
        .unwrap();
        assert!(soft.authored_intensity <= vivid.authored_intensity);
    }

    #[test]
    fn evidence_threshold_has_no_large_score_jump() {
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
        assert!((above.authored_breadth - below.authored_breadth).abs() < 0.01);
        assert!((above.authored_intensity - below.authored_intensity).abs() < 0.05);
    }

    #[test]
    fn chroma_envelope_preserves_neutral_accent_led_and_palette_native_character() {
        let neutral = measure(&palette(&[])).unwrap();
        assert_eq!(neutral.chroma_envelope.target_median, 0.035);
        assert_eq!(neutral.chroma_envelope.ordinary_maximum, 0.055);

        let accent_led = measure(&palette(&[("accent", 0.14, 0.2, Provenance::Direct)])).unwrap();
        assert!((accent_led.chroma_envelope.target_median - 0.035).abs() < 1e-12);
        assert!(
            (accent_led.chroma_envelope.ordinary_maximum
                - accent_led.source_q90_chroma.clamp(0.070, 0.180))
            .abs()
                < 1e-12
        );

        let native = measure(&palette(&[
            ("red", 0.08, 0.2, Provenance::Direct),
            ("green", 0.09, 2.1, Provenance::Direct),
            ("blue", 0.10, 4.0, Provenance::Direct),
        ]))
        .unwrap();
        assert!(
            (native.chroma_envelope.target_median
                - native.source_median_chroma.clamp(0.045, 0.140))
            .abs()
                < 1e-12
        );
        assert!(
            (native.chroma_envelope.ordinary_maximum
                - native.source_q90_chroma.clamp(0.070, 0.180))
            .abs()
                < 1e-12
        );
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
