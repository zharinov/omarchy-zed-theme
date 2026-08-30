//! Runs deterministic bounded searches over byte-quantized colors.
//!
//! Each source color is expanded once into a fidelity-sorted transform table. Query
//! caches reuse exact answers, while color-vision data is computed only for candidates
//! that reach a pair comparison. Independent source tables are prepared in parallel.

use crate::color::{
    ColorMetrics, Rgb24, Rgba, Rgba32, contrast_ratio, delta_e, endpoint_chroma_taper,
    gamut_chroma_limit_with_components, gamut_map_oklch_with_components, lab, oklab_to_oklch,
};
use crate::constants::*;
use crate::theme::Audit;
use crate::{Error, Result};
use rayon::prelude::*;
use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, OnceLock};

struct TransformCandidate {
    distance: f64,
    retention: f64,
    metrics: ColorMetrics,
}

type CvdLabs = [[f64; 3]; 3];

struct TransformTableData {
    candidates: Box<[TransformCandidate]>,
    // Tables that never participate in a color-vision pair allocate no CVD cache.
    cvd: OnceLock<Box<[OnceLock<Box<CvdLabs>>]>>,
}
type TransformTable = Arc<TransformTableData>;

#[derive(Hash, PartialEq, Eq)]
struct ColorQuery {
    seed: String,
    backgrounds: Vec<String>,
    target: u64,
    avoid: Vec<String>,
    lower_lightness: u64,
    upper_lightness: u64,
    prefer_background: bool,
}

#[derive(Hash, PartialEq, Eq)]
struct StateQuery {
    seed: String,
    backgrounds: Vec<String>,
    target: u64,
    minimum_delta_e: u64,
    references: Vec<(String, u64, u64)>,
}

#[derive(Default)]
pub struct Search {
    transform_tables: HashMap<String, TransformTable>,
    color_results: HashMap<ColorQuery, std::result::Result<String, String>>,
    state_results: HashMap<StateQuery, std::result::Result<String, String>>,
}

#[derive(Clone, Copy)]
pub struct FitBounds {
    pub lower_lightness: f64,
    pub upper_lightness: f64,
    pub prefer_background: bool,
}

impl Default for FitBounds {
    fn default() -> Self {
        Self {
            lower_lightness: 0.0,
            upper_lightness: 1.0,
            prefer_background: false,
        }
    }
}

#[derive(Clone, Copy)]
pub struct PairConstraints {
    pub foreground_contrast: f64,
    pub pair_contrast: f64,
    pub normal_delta: f64,
    pub cvd_delta: f64,
    pub lightness_delta: f64,
    pub separation_alternative: Option<(f64, f64, f64)>,
    pub prefer_background: bool,
}

pub struct FillRequest<'a> {
    pub backgrounds: &'a [String],
    pub target: f64,
    pub minimum_delta_e: f64,
    pub runtime_state: Option<(f64, f64, f64)>,
    pub readable_foregrounds: &'a [(String, f64)],
    pub rendered_references: &'a [(String, f64, f64)],
    pub runtime_rendered_references: &'a [(String, f64, f64, f64)],
}

type FillRank = [f64; 5];
type FillCandidate = (Rgba32, FillRank);

struct PreparedFill {
    backgrounds: Vec<ColorMetrics>,
    target: f64,
    minimum_delta_e: f64,
    runtime_state: Option<(f64, f64, f64)>,
    readable_foregrounds: Vec<(ColorMetrics, f64)>,
    rendered_references: Vec<Vec<(ColorMetrics, f64, f64)>>,
    runtime_rendered_references: Vec<Vec<(ColorMetrics, f64, f64, f64)>>,
}

impl PreparedFill {
    fn new(request: FillRequest<'_>) -> Result<Self> {
        let backgrounds = request
            .backgrounds
            .iter()
            .map(|background| ColorMetrics::from_hex(background))
            .collect::<Result<Vec<_>>>()?;
        let readable_foregrounds = request
            .readable_foregrounds
            .iter()
            .map(|(foreground, target)| Ok((ColorMetrics::from_hex(foreground)?, *target)))
            .collect::<Result<Vec<_>>>()?;
        let rendered_references = backgrounds
            .iter()
            .map(|background| {
                request
                    .rendered_references
                    .iter()
                    .map(|(reference, target, delta)| {
                        Ok((
                            ColorMetrics::blend(
                                *background,
                                ColorMetrics::from_hex(reference)?.rgba.rgba(),
                            ),
                            *target,
                            *delta,
                        ))
                    })
                    .collect::<Result<Vec<_>>>()
            })
            .collect::<Result<Vec<_>>>()?;
        let runtime_rendered_references = backgrounds
            .iter()
            .map(|background| {
                request
                    .runtime_rendered_references
                    .iter()
                    .map(|(reference, target, delta, base_step)| {
                        Ok((
                            ColorMetrics::blend(
                                *background,
                                ColorMetrics::from_hex(reference)?.rgba.rgba(),
                            ),
                            *target,
                            *delta,
                            *base_step,
                        ))
                    })
                    .collect::<Result<Vec<_>>>()
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            backgrounds,
            target: request.target,
            minimum_delta_e: request.minimum_delta_e,
            runtime_state: request.runtime_state,
            readable_foregrounds,
            rendered_references,
            runtime_rendered_references,
        })
    }

    fn best_for(
        &self,
        opaque: ColorMetrics,
        distance: f64,
        retention: f64,
    ) -> Option<FillCandidate> {
        let mut best: Option<FillCandidate> = None;
        let opaque_rgb = opaque.rgb24();

        for alpha_index in 2..=20 {
            let alpha = ((alpha_index * 255 + 10) / 20) as u8;
            let Some((candidate, rank)) =
                self.evaluate_alpha(opaque_rgb, alpha, distance, retention)
            else {
                continue;
            };

            if best.as_ref().is_none_or(|(best_color, best_rank)| {
                rank_cmp(&rank, best_rank).then_with(|| candidate.hex_cmp(*best_color))
                    == Ordering::Less
            }) {
                best = Some((candidate, rank));
            }
        }

        best
    }

    fn evaluate_alpha(
        &self,
        opaque_rgb: Rgb24,
        alpha: u8,
        distance: f64,
        retention: f64,
    ) -> Option<FillCandidate> {
        let mut minimum_ratio = f64::INFINITY;
        let mut overshoot = 0.0;
        let mut final_distance = 0.0;

        for (background_index, background) in self.backgrounds.iter().enumerate() {
            let rendered_prepared = ColorMetrics::blend_rgb24(*background, opaque_rgb, alpha);
            let ratio = rendered_prepared.contrast(*background);

            if ratio < self.target - 1e-12
                || self.rendered_references[background_index].iter().any(
                    |(reference, target, _)| {
                        rendered_prepared.contrast(*reference) < *target - 1e-12
                    },
                )
                || self
                    .readable_foregrounds
                    .iter()
                    .any(|(foreground, target)| {
                        rendered_prepared.contrast(*foreground) < *target - 1e-12
                    })
            {
                return None;
            }

            let runtime = match self.runtime_state {
                Some((runtime_opacity, runtime_target, minimum_distance)) => {
                    let runtime_alpha = (f64::from(alpha) * runtime_opacity + 0.5).floor() as u8;
                    let prepared =
                        ColorMetrics::blend_rgb24(*background, opaque_rgb, runtime_alpha);
                    let runtime_ratio = prepared.contrast(*background);

                    if runtime_ratio < runtime_target - 1e-12
                        || self.runtime_rendered_references[background_index]
                            .iter()
                            .any(|(reference, target, _, base_step)| {
                                prepared.contrast(*reference) < *target - 1e-12
                                    || runtime_ratio
                                        < reference.contrast(*background) + *base_step - 1e-12
                            })
                        || self
                            .readable_foregrounds
                            .iter()
                            .any(|(foreground, target)| {
                                prepared.contrast(*foreground) < *target - 1e-12
                            })
                    {
                        return None;
                    }

                    Some((
                        prepared,
                        runtime_ratio + self.target - runtime_target,
                        minimum_distance,
                    ))
                }
                None => None,
            };

            let rendered = rendered_prepared.metrics();
            let rendered_distance = rendered.delta_e(*background);

            if rendered_distance < self.minimum_delta_e - 1e-12
                || self.rendered_references[background_index]
                    .iter()
                    .any(|(reference, _, delta)| rendered.delta_e(*reference) < *delta - 1e-12)
            {
                return None;
            }

            minimum_ratio = minimum_ratio.min(ratio);
            overshoot += (ratio - self.target).max(0.0);
            final_distance += rendered_distance;

            if let Some((prepared, adjusted_ratio, minimum_distance)) = runtime {
                let runtime_metrics = prepared.metrics();
                let runtime_distance = runtime_metrics.delta_e(*background);

                if runtime_distance < minimum_distance - 1e-12
                    || self.runtime_rendered_references[background_index]
                        .iter()
                        .any(|(reference, _, delta, _)| {
                            runtime_metrics.delta_e(*reference) < *delta - 1e-12
                        })
                {
                    return None;
                }

                minimum_ratio = minimum_ratio.min(adjusted_ratio);
                overshoot += (adjusted_ratio - self.target).max(0.0);
                final_distance += runtime_distance;
            }
        }

        if minimum_ratio < self.target - 1e-12 {
            return None;
        }

        let candidate = Rgba32::from_rgb_alpha(opaque_rgb, alpha);
        let rank = [
            final_distance,
            overshoot,
            distance,
            -retention,
            -f64::from(alpha) / 255.0,
        ];

        Some((candidate, rank))
    }
}

struct PairCandidate {
    source_index: usize,
    background_distance: f64,
}

fn pair_is_separated(
    pair_contrast: f64,
    normal_delta: f64,
    cvd_delta: f64,
    constraints: PairConstraints,
) -> bool {
    constraints.separation_alternative.is_none_or(
        |(luminance_contrast, chromatic_normal, chromatic_cvd)| {
            pair_contrast >= luminance_contrast - 1e-12
                || (normal_delta >= chromatic_normal - 1e-12 && cvd_delta >= chromatic_cvd - 1e-12)
        },
    )
}

fn rank_cmp(left: &[f64], right: &[f64]) -> Ordering {
    left.iter()
        .zip(right)
        .find_map(|(left, right)| {
            let order = left.total_cmp(right);
            (order != Ordering::Equal).then_some(order)
        })
        .unwrap_or_else(|| left.len().cmp(&right.len()))
}

fn lab_distance(left: [f64; 3], right: [f64; 3]) -> f64 {
    left.into_iter()
        .zip(right)
        .map(|(left, right)| (left - right).powi(2))
        .sum::<f64>()
        .sqrt()
}

impl Search {
    fn candidate_colors(seed: &str) -> Result<Vec<Rgb24>> {
        let [_, chroma, hue] = oklab_to_oklch(lab(seed)?);
        let hue_cos = hue.cos();
        let hue_sin = hue.sin();
        let mut unique = Vec::with_capacity(
            usize::from(CANDIDATE_LIGHTNESS_STEPS + 1) * usize::from(CANDIDATE_CHROMA_STEPS + 1),
        );
        for tone_index in 0..=CANDIDATE_LIGHTNESS_STEPS {
            let lightness = f64::from(tone_index) / f64::from(CANDIDATE_LIGHTNESS_STEPS);
            let chroma_limit = gamut_chroma_limit_with_components(lightness, hue_cos, hue_sin);
            for chroma_index in 0..=CANDIDATE_CHROMA_STEPS {
                let scale = 1.0 - f64::from(chroma_index) / f64::from(CANDIDATE_CHROMA_STEPS);
                unique.push(Rgb24::from_rgba(gamut_map_oklch_with_components(
                    lightness,
                    chroma * scale * endpoint_chroma_taper(lightness),
                    hue_cos,
                    hue_sin,
                    chroma_limit,
                )));
            }
        }
        unique.sort_unstable();
        unique.dedup();
        Ok(unique)
    }

    fn build_transform_table(seed: &str) -> Result<TransformTable> {
        let source_lab = lab(seed)?;
        let seed_chroma = oklab_to_oklch(source_lab)[1];
        let mut table = Self::candidate_colors(seed)?
            .into_iter()
            .map(|color| {
                let metrics = ColorMetrics::from_rgb24(color);
                Ok(TransformCandidate {
                    distance: lab_distance(metrics.lab, source_lab),
                    retention: oklab_to_oklch(metrics.lab)[1] / seed_chroma.max(1e-12),
                    metrics,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        table.sort_by(|left, right| {
            left.distance
                .total_cmp(&right.distance)
                .then_with(|| left.metrics.rgb24().cmp(&right.metrics.rgb24()))
        });
        Ok(Arc::new(TransformTableData {
            candidates: table.into(),
            cvd: OnceLock::new(),
        }))
    }

    pub fn prewarm<'a>(&mut self, seeds: impl IntoIterator<Item = &'a str>) -> Result<()> {
        let seeds = seeds
            .into_iter()
            .filter(|seed| !self.transform_tables.contains_key(*seed))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let tables = seeds
            .par_iter()
            .map(|seed| Self::build_transform_table(seed).map(|table| ((*seed).to_owned(), table)))
            .collect::<Vec<_>>();
        for table in tables {
            let (seed, table) = table?;
            self.transform_tables.insert(seed, table);
        }
        Ok(())
    }

    fn transform_table(&mut self, seed: &str) -> Result<TransformTable> {
        if let Some(table) = self.transform_tables.get(seed) {
            return Ok(Arc::clone(table));
        }
        let table = Self::build_transform_table(seed)?;
        self.transform_tables
            .insert(seed.to_owned(), Arc::clone(&table));
        Ok(table)
    }

    pub fn fit_color(&mut self, seed: &str, backgrounds: &[String], target: f64) -> Result<String> {
        self.fit_color_bounded(seed, backgrounds, target, &[], FitBounds::default())
    }

    pub fn fit_color_avoiding(
        &mut self,
        seed: &str,
        backgrounds: &[String],
        target: f64,
        avoid: &[String],
    ) -> Result<String> {
        self.fit_color_bounded(seed, backgrounds, target, avoid, FitBounds::default())
    }

    pub fn fit_pair(
        &mut self,
        first_seed: &str,
        second_seed: &str,
        backgrounds: &[String],
        constraints: PairConstraints,
    ) -> Result<[String; 2]> {
        self.fit_pair_on_backgrounds(
            first_seed,
            backgrounds,
            second_seed,
            backgrounds,
            constraints,
        )
    }

    pub fn fit_pair_on_backgrounds(
        &mut self,
        first_seed: &str,
        first_backgrounds: &[String],
        second_seed: &str,
        second_backgrounds: &[String],
        constraints: PairConstraints,
    ) -> Result<[String; 2]> {
        self.fit_pair_on_backgrounds_readable(
            first_seed,
            first_backgrounds,
            second_seed,
            second_backgrounds,
            constraints,
            &[],
        )
    }

    pub fn fit_pair_on_backgrounds_readable(
        &mut self,
        first_seed: &str,
        first_backgrounds: &[String],
        second_seed: &str,
        second_backgrounds: &[String],
        constraints: PairConstraints,
        readable_foregrounds: &[(String, f64)],
    ) -> Result<[String; 2]> {
        let collect = |search: &mut Self,
                       seed: &str,
                       backgrounds: &[String]|
         -> Result<(Vec<PairCandidate>, TransformTable)> {
            let background_metrics = backgrounds
                .iter()
                .map(|background| ColorMetrics::from_hex(background))
                .collect::<Result<Vec<_>>>()?;
            let readable_metrics = readable_foregrounds
                .iter()
                .map(|(foreground, target)| Ok((ColorMetrics::from_hex(foreground)?, *target)))
                .collect::<Result<Vec<_>>>()?;
            let mut values = Vec::new();
            let table = search.transform_table(seed)?;
            for (index, candidate) in table.candidates.iter().enumerate() {
                let metrics = candidate.metrics;
                if background_metrics.iter().any(|background| {
                    metrics.contrast(*background) < constraints.foreground_contrast - 1e-12
                }) || readable_metrics
                    .iter()
                    .any(|(foreground, target)| metrics.contrast(*foreground) < *target - 1e-12)
                {
                    continue;
                }
                let background_distance = background_metrics
                    .iter()
                    .map(|background| metrics.delta_e(*background))
                    .sum();
                values.push(PairCandidate {
                    source_index: index,
                    background_distance,
                });
            }
            values.sort_by(|left, right| {
                let left_facts = &table.candidates[left.source_index];
                let right_facts = &table.candidates[right.source_index];
                let left_primary = if constraints.prefer_background {
                    left.background_distance
                } else {
                    left_facts.distance
                };
                let right_primary = if constraints.prefer_background {
                    right.background_distance
                } else {
                    right_facts.distance
                };
                left_primary
                    .total_cmp(&right_primary)
                    .then_with(|| left_facts.distance.total_cmp(&right_facts.distance))
                    .then_with(|| right_facts.retention.total_cmp(&left_facts.retention))
                    .then_with(|| left_facts.metrics.rgb24().cmp(&right_facts.metrics.rgb24()))
            });
            Ok((values, table))
        };
        let (first, first_table) = collect(self, first_seed, first_backgrounds)?;
        let (second, second_table) = collect(self, second_seed, second_backgrounds)?;
        let mut best: Option<([Rgb24; 2], [f64; 4])> = None;
        let mut maxima = [0.0_f64; 3];
        let minimum_second_primary = second
            .first()
            .map(|candidate| {
                if constraints.prefer_background {
                    candidate.background_distance
                } else {
                    second_table.candidates[candidate.source_index].distance
                }
            })
            .unwrap_or(0.0);
        for first_candidate in &first {
            let first_facts = &first_table.candidates[first_candidate.source_index];
            let first_primary = if constraints.prefer_background {
                first_candidate.background_distance
            } else {
                first_facts.distance
            };
            if best
                .as_ref()
                .is_some_and(|(_, rank)| first_primary + minimum_second_primary > rank[0] + 1e-12)
            {
                break;
            }
            for second_candidate in &second {
                let second_facts = &second_table.candidates[second_candidate.source_index];
                let transform = first_facts.distance + second_facts.distance;
                let background_distance =
                    first_candidate.background_distance + second_candidate.background_distance;
                let primary_cost = if constraints.prefer_background {
                    background_distance
                } else {
                    transform
                };
                if best
                    .as_ref()
                    .is_some_and(|(_, rank)| primary_cost > rank[0] + 1e-12)
                {
                    break;
                }
                let pair_contrast = first_facts.metrics.contrast(second_facts.metrics);
                let normal_delta = first_facts.metrics.delta_e(second_facts.metrics);
                maxima[0] = maxima[0].max(pair_contrast);
                maxima[1] = maxima[1].max(normal_delta);
                if pair_contrast < constraints.pair_contrast - 1e-12
                    || normal_delta < constraints.normal_delta - 1e-12
                    || (first_facts.metrics.lab[0] - second_facts.metrics.lab[0]).abs()
                        < constraints.lightness_delta - 1e-12
                {
                    continue;
                }
                let cvd_delta = cvd_distance_precomputed(
                    first_candidate,
                    &first_table,
                    second_candidate,
                    &second_table,
                    normal_delta,
                );
                maxima[2] = maxima[2].max(cvd_delta);
                if cvd_delta < constraints.cvd_delta - 1e-12
                    || !pair_is_separated(pair_contrast, normal_delta, cvd_delta, constraints)
                {
                    continue;
                }
                let rank = [
                    primary_cost,
                    transform,
                    -(first_facts.retention + second_facts.retention),
                    first_facts.distance,
                ];
                let replace = best.as_ref().is_none_or(|(colors, current)| {
                    rank_cmp(&rank, current).then_with(|| {
                        [first_facts.metrics.rgb24(), second_facts.metrics.rgb24()].cmp(colors)
                    }) == Ordering::Less
                });
                if replace {
                    best = Some((
                        [first_facts.metrics.rgb24(), second_facts.metrics.rgb24()],
                        rank,
                    ));
                }
                // The second side is ordered by every second-dependent component of
                // the total rank, so its first passing member is this first color's
                // exact optimum.
                break;
            }
        }
        best.map(|(colors, _)| [colors[0].hex(), colors[1].hex()]).ok_or_else(|| {
            for first_candidate in &first {
                let first_facts = &first_table.candidates[first_candidate.source_index];
                for second_candidate in &second {
                    let second_facts = &second_table.candidates[second_candidate.source_index];
                    let pair_contrast = first_facts.metrics.contrast(second_facts.metrics);
                    let normal_delta = first_facts.metrics.delta_e(second_facts.metrics);
                    if pair_contrast < constraints.pair_contrast - 1e-12
                        || normal_delta < constraints.normal_delta - 1e-12
                        || (first_facts.metrics.lab[0] - second_facts.metrics.lab[0]).abs()
                            < constraints.lightness_delta - 1e-12
                    {
                        continue;
                    }
                    maxima[2] = maxima[2].max(cvd_distance_precomputed(
                        first_candidate,
                        &first_table,
                        second_candidate,
                        &second_table,
                        normal_delta,
                    ));
                }
            }
            Error(format!(
                "no jointly fitted pair remains for {first_seed}/{second_seed} ({} first candidates, {} second candidates; maxima contrast {:.3}, delta E {:.3}, cheap-feasible CVD {:.3})",
                first.len(),
                second.len(),
                maxima[0], maxima[1], maxima[2]
            ))
        })
    }

    pub fn fit_color_bounded(
        &mut self,
        seed: &str,
        backgrounds: &[String],
        target: f64,
        avoid: &[String],
        bounds: FitBounds,
    ) -> Result<String> {
        let query = ColorQuery {
            seed: seed.to_owned(),
            backgrounds: backgrounds.to_vec(),
            target: target.to_bits(),
            avoid: avoid.to_vec(),
            lower_lightness: bounds.lower_lightness.to_bits(),
            upper_lightness: bounds.upper_lightness.to_bits(),
            prefer_background: bounds.prefer_background,
        };
        if let Some(result) = self.color_results.get(&query) {
            return result.clone().map_err(Error);
        }
        let result = self.fit_color_bounded_uncached(seed, backgrounds, target, avoid, bounds);
        self.color_results.insert(
            query,
            result
                .as_ref()
                .map(|color| color.clone())
                .map_err(|error| error.0.clone()),
        );
        result
    }

    fn fit_color_bounded_uncached(
        &mut self,
        seed: &str,
        backgrounds: &[String],
        target: f64,
        avoid: &[String],
        bounds: FitBounds,
    ) -> Result<String> {
        if backgrounds.is_empty() {
            return Err(Error("fit_color requires at least one background".into()));
        }
        let source_metrics = ColorMetrics::from_hex(seed)?;
        let source_chroma = oklab_to_oklch(source_metrics.lab)[1];
        let source_retention = source_chroma / source_chroma.max(1e-12);
        let background_metrics = backgrounds
            .iter()
            .map(|value| ColorMetrics::from_hex(value))
            .collect::<Result<Vec<_>>>()?;
        let avoid_metrics = avoid
            .iter()
            .map(|value| ColorMetrics::from_hex(value))
            .collect::<Result<Vec<_>>>()?;
        let background_lightness = background_metrics
            .iter()
            .map(|metrics| metrics.lab[0])
            .sum::<f64>()
            / backgrounds.len() as f64;
        let passes = |candidate: ColorMetrics| {
            if candidate.lab[0] < bounds.lower_lightness - 1e-12
                || candidate.lab[0] > bounds.upper_lightness + 1e-12
            {
                return false;
            }
            if background_metrics
                .iter()
                .any(|background| candidate.contrast(*background) < target - 1e-12)
            {
                return false;
            }
            for other in &avoid_metrics {
                if (candidate.lab[0] - other.lab[0]).abs() < 0.05 - 1e-12
                    || candidate.delta_e(*other) < 0.10 - 1e-12
                {
                    return false;
                }
            }
            true
        };
        if passes(source_metrics) && !bounds.prefer_background {
            return Ok(seed.to_owned());
        }
        if !bounds.prefer_background {
            let mut best: Option<(Rgb24, [f64; 3])> = None;
            for candidate in self.transform_table(seed)?.candidates.iter() {
                if best.as_ref().is_some_and(|(_, rank)| {
                    candidate.distance.total_cmp(&rank[0]) == Ordering::Greater
                }) {
                    break;
                }
                if !passes(candidate.metrics) {
                    continue;
                }
                let overshoot = background_metrics
                    .iter()
                    .map(|background| (candidate.metrics.contrast(*background) - target).max(0.0))
                    .sum();
                let rank = [candidate.distance, overshoot, -candidate.retention];
                if best.as_ref().is_none_or(|(best_color, best_rank)| {
                    rank_cmp(&rank, best_rank)
                        .then_with(|| candidate.metrics.rgb24().cmp(best_color))
                        == Ordering::Less
                }) {
                    best = Some((candidate.metrics.rgb24(), rank));
                }
            }
            return best.map(|(color, _)| color.hex()).ok_or_else(|| {
                Error(format!(
                    "no candidate in the defined hue-preserving candidate space for {seed} at {target:.2}:1 over {}",
                    backgrounds.join(",")
                ))
            });
        }
        let mut best: Option<(Rgb24, [f64; 4])> = None;
        let consider = |candidate: Rgb24,
                        metrics: ColorMetrics,
                        distance: f64,
                        retention: f64,
                        best: &mut Option<(Rgb24, [f64; 4])>| {
            if !passes(metrics) {
                return;
            }
            let overshoot = background_metrics
                .iter()
                .map(|background| (metrics.contrast(*background) - target).max(0.0))
                .sum();
            let rank = [
                (metrics.lab[0] - background_lightness).abs(),
                overshoot,
                distance,
                -retention,
            ];
            if best.as_ref().is_none_or(|(best_color, best_rank)| {
                rank_cmp(&rank, best_rank).then_with(|| candidate.cmp(best_color)) == Ordering::Less
            }) {
                *best = Some((candidate, rank));
            }
        };
        consider(
            Rgb24::from_rgba(source_metrics.rgba.rgba()),
            source_metrics,
            0.0,
            source_retention,
            &mut best,
        );
        let table = self.transform_table(seed)?;
        for candidate in table.candidates.iter() {
            consider(
                candidate.metrics.rgb24(),
                candidate.metrics,
                candidate.distance,
                candidate.retention,
                &mut best,
            );
        }
        best.map(|(color, _)| color.hex()).ok_or_else(|| {
            Error(format!(
                "no candidate in the defined hue-preserving candidate space for {seed} at {target:.2}:1 over {}",
                backgrounds.join(",")
            ))
        })
    }

    pub fn fit_fill_readable(&mut self, seed: &str, request: FillRequest<'_>) -> Result<String> {
        let prepared = PreparedFill::new(request)?;
        let source = ColorMetrics::from_hex(seed)?;
        if let Some((color, _)) = prepared.best_for(source, 0.0, 1.0) {
            return Ok(color.hex());
        }
        let table = self.transform_table(seed)?;
        let best = table
            .candidates
            .par_iter()
            .map(|opaque| prepared.best_for(opaque.metrics, opaque.distance, opaque.retention))
            .reduce(
                || None,
                |left, right| match (left, right) {
                    (None, other) | (other, None) => other,
                    (Some(left), Some(right)) => {
                        let order =
                            rank_cmp(&left.1, &right.1).then_with(|| left.0.hex_cmp(right.0));
                        Some(if order == Ordering::Greater {
                            right
                        } else {
                            left
                        })
                    }
                },
            );
        best.map(|(color, _)| color.hex())
            .ok_or_else(|| Error(format!("no candidate for fill {seed}")))
    }

    pub fn fit_exact_fill_readable(
        &self,
        seed: &str,
        request: FillRequest<'_>,
    ) -> Result<Option<String>> {
        let prepared = PreparedFill::new(request)?;
        let metrics = ColorMetrics::from_hex(seed)?;
        Ok(prepared
            .best_for(metrics, 0.0, 1.0)
            .map(|(color, _)| color.hex()))
    }

    pub fn fit_state(
        &mut self,
        seed: &str,
        backgrounds: &[String],
        target: f64,
        minimum_delta_e: f64,
        references: &[(String, f64, f64)],
    ) -> Result<String> {
        let query = StateQuery {
            seed: seed.to_owned(),
            backgrounds: backgrounds.to_vec(),
            target: target.to_bits(),
            minimum_delta_e: minimum_delta_e.to_bits(),
            references: references
                .iter()
                .map(|(color, target, delta)| (color.clone(), target.to_bits(), delta.to_bits()))
                .collect(),
        };
        if let Some(result) = self.state_results.get(&query) {
            return result.clone().map_err(Error);
        }
        let result =
            self.fit_state_uncached(seed, backgrounds, target, minimum_delta_e, references);
        self.state_results.insert(
            query,
            result
                .as_ref()
                .map(|color| color.clone())
                .map_err(|error| error.0.clone()),
        );
        result
    }

    fn fit_state_uncached(
        &mut self,
        seed: &str,
        backgrounds: &[String],
        target: f64,
        minimum_delta_e: f64,
        references: &[(String, f64, f64)],
    ) -> Result<String> {
        let background_metrics = backgrounds
            .iter()
            .map(|background| ColorMetrics::from_hex(background))
            .collect::<Result<Vec<_>>>()?;
        let reference_metrics = references
            .iter()
            .map(|(reference, target, delta)| {
                Ok((ColorMetrics::from_hex(reference)?, *target, *delta))
            })
            .collect::<Result<Vec<_>>>()?;
        let mut best: Option<(Rgb24, [f64; 4])> = None;
        let table = self.transform_table(seed)?;

        'candidates: for candidate in table.candidates.iter() {
            let mut final_distance = 0.0;
            let mut overshoot = 0.0;

            for background in &background_metrics {
                let ratio = candidate.metrics.contrast(*background);
                if ratio < target - 1e-12 {
                    continue 'candidates;
                }

                let distance = candidate.metrics.delta_e(*background);
                if distance < minimum_delta_e - 1e-12 {
                    continue 'candidates;
                }

                overshoot += (ratio - target).max(0.0);
                final_distance += distance;
            }

            for (reference, reference_target, reference_delta) in &reference_metrics {
                let ratio = candidate.metrics.contrast(*reference);
                if ratio < *reference_target - 1e-12
                    || candidate.metrics.delta_e(*reference) < *reference_delta - 1e-12
                {
                    continue 'candidates;
                }

                overshoot += (ratio - *reference_target).max(0.0);
            }

            let rank = [
                final_distance,
                overshoot,
                candidate.distance,
                -candidate.retention,
            ];

            if best.as_ref().is_none_or(|(best_color, best_rank)| {
                rank_cmp(&rank, best_rank).then_with(|| candidate.metrics.rgb24().cmp(best_color))
                    == Ordering::Less
            }) {
                best = Some((candidate.metrics.rgb24(), rank));
            }
        }

        best.map(|(color, _)| color.hex())
            .ok_or_else(|| Error(format!("no quantized state candidate for {seed}")))
    }

    pub fn fit_state_ladder(
        &mut self,
        seed: &str,
        backgrounds: &[String],
        rungs: &[(f64, f64)],
        protected: &[(String, f64, f64)],
    ) -> Result<Vec<String>> {
        let mut results: Vec<String> = Vec::new();

        for (index, (target, delta)) in rungs.iter().enumerate() {
            let effective_target = if let Some(previous) = results.last() {
                let previous_base = backgrounds
                    .iter()
                    .map(|background| contrast_ratio(previous, background))
                    .collect::<Result<Vec<_>>>()?
                    .into_iter()
                    .fold(f64::INFINITY, f64::min);
                target.max(previous_base + STATE_BASE_CONTRAST_STEP)
            } else {
                *target
            };

            let mut references = protected.to_vec();
            references.extend(
                index
                    .checked_sub(1)
                    .map(|previous| {
                        vec![(
                            results[previous].clone(),
                            STATE_CONSECUTIVE_CONTRAST,
                            STATE_CONSECUTIVE_DELTA_E,
                        )]
                    })
                    .unwrap_or_default(),
            );

            results.push(self.fit_state(
                seed,
                backgrounds,
                effective_target,
                *delta,
                &references,
            )?);
        }

        Ok(results)
    }

    pub fn fit_distinct_colors(
        &mut self,
        seeds: &[String],
        backgrounds: &[String],
        target: f64,
        audit: &mut Audit,
        role: &str,
    ) -> Result<Vec<String>> {
        let mut chosen: Vec<String> = Vec::new();

        for (index, seed) in seeds.iter().enumerate() {
            let fitted = self.fit_color(seed, backgrounds, target)?;
            if chosen.is_empty() {
                chosen.push(fitted);
                continue;
            }

            let source_lab = lab(seed)?;
            let seed_chroma = oklab_to_oklch(source_lab)[1];
            let background_metrics = backgrounds
                .iter()
                .map(|background| ColorMetrics::from_hex(background))
                .collect::<Result<Vec<_>>>()?;
            let chosen_metrics = chosen
                .iter()
                .map(|color| {
                    let metrics = ColorMetrics::from_hex(color)?;
                    Ok((metrics, cvd_labs(metrics.rgba.rgba())))
                })
                .collect::<Result<Vec<_>>>()?;
            let chosen_rgb = chosen_metrics
                .iter()
                .map(|(metrics, _)| Rgb24::from_rgba(metrics.rgba.rgba()))
                .collect::<Vec<_>>();

            let mut considered = BTreeSet::new();
            let mut passing: Option<(Rgb24, [f64; 3])> = None;
            let mut fallback: Option<(Rgb24, [f64; 5])> = None;
            let mut consider =
                |candidate: Rgb24,
                 metrics: ColorMetrics,
                 transform: f64,
                 retention: f64,
                 passing: &mut Option<(Rgb24, [f64; 3])>,
                 fallback: &mut Option<(Rgb24, [f64; 5])>| {
                    if !considered.insert(candidate) || chosen_rgb.contains(&candidate) {
                        return;
                    }

                    let mut overshoot = 0.0;
                    for background in &background_metrics {
                        let ratio = metrics.contrast(*background);
                        if ratio < target - 1e-12 {
                            return;
                        }
                        overshoot += (ratio - target).max(0.0);
                    }

                    let candidate_cvd = cvd_labs(metrics.rgba.rgba());
                    let normal = chosen_metrics
                        .iter()
                        .map(|(reference, _)| metrics.delta_e(*reference))
                        .fold(f64::INFINITY, f64::min);
                    let cvd = chosen_metrics
                        .iter()
                        .map(|(reference, reference_cvd)| {
                            cvd_distance_facts(
                                &candidate_cvd,
                                reference_cvd,
                                metrics.delta_e(*reference),
                            )
                        })
                        .fold(f64::INFINITY, f64::min);

                    let fallback_rank = [
                        -normal.min(ACCENT_NORMAL_DELTA_E),
                        -cvd.min(ACCENT_CVD_DELTA_E),
                        transform,
                        overshoot,
                        -retention,
                    ];
                    if fallback.as_ref().is_none_or(|(best_color, best_rank)| {
                        rank_cmp(&fallback_rank, best_rank).then_with(|| candidate.cmp(best_color))
                            == Ordering::Less
                    }) {
                        *fallback = Some((candidate, fallback_rank));
                    }

                    if normal < ACCENT_NORMAL_DELTA_E - 1e-12 || cvd < ACCENT_CVD_DELTA_E - 1e-12 {
                        return;
                    }

                    let passing_rank = [transform, overshoot, -retention];
                    if passing.as_ref().is_none_or(|(best_color, best_rank)| {
                        rank_cmp(&passing_rank, best_rank).then_with(|| candidate.cmp(best_color))
                            == Ordering::Less
                    }) {
                        *passing = Some((candidate, passing_rank));
                    }
                };

            let fitted_metrics = ColorMetrics::from_hex(&fitted)?;
            let fitted_rgb = Rgb24::from_rgba(fitted_metrics.rgba.rgba());
            let fitted_transform = lab_distance(fitted_metrics.lab, source_lab);
            let fitted_retention = oklab_to_oklch(fitted_metrics.lab)[1] / seed_chroma.max(1e-12);
            consider(
                fitted_rgb,
                fitted_metrics,
                fitted_transform,
                fitted_retention,
                &mut passing,
                &mut fallback,
            );

            let table = self.transform_table(seed)?;
            for candidate in table.candidates.iter() {
                if passing.as_ref().is_some_and(|(_, rank)| {
                    candidate.distance.total_cmp(&rank[0]) == Ordering::Greater
                }) {
                    break;
                }
                consider(
                    candidate.metrics.rgb24(),
                    candidate.metrics,
                    candidate.distance,
                    candidate.retention,
                    &mut passing,
                    &mut fallback,
                );
            }

            let output = passing
                .map(|(color, _)| color.hex())
                .or_else(|| fallback.map(|(color, _)| color.hex()))
                .ok_or_else(|| {
                    Error(format!(
                        "no distinct passing color remains for {role}[{index}]"
                    ))
                })?;

            let actual_normal = chosen
                .iter()
                .map(|reference| delta_e(&output, reference))
                .collect::<Result<Vec<_>>>()?
                .into_iter()
                .fold(f64::INFINITY, f64::min);
            let actual_cvd = chosen
                .iter()
                .map(|reference| cvd_distance(&output, reference))
                .collect::<Result<Vec<_>>>()?
                .into_iter()
                .fold(f64::INFINITY, f64::min);

            if actual_normal < ACCENT_NORMAL_DELTA_E - 1e-12
                || actual_cvd < ACCENT_CVD_DELTA_E - 1e-12
            {
                audit.degradation(format!("{role}[{index}]"), "palette_separation", serde_json::json!({
                    "normal_goal": ACCENT_NORMAL_DELTA_E, "normal_actual": round6(actual_normal),
                    "cvd_goal": ACCENT_CVD_DELTA_E, "cvd_actual": round6(actual_cvd),
                }));
            }

            chosen.push(output);
        }

        Ok(chosen)
    }
}

pub fn cvd_greedy_order(values: &[String]) -> Result<Vec<String>> {
    let mut remaining: Vec<(usize, String)> = values.iter().cloned().enumerate().collect();
    let (_, first) = remaining.remove(0);
    let mut ordered = vec![first];

    while !remaining.is_empty() {
        let best_index = (0..remaining.len())
            .max_by(|left, right| {
                let score = |index: usize| remaining[index].1.as_str();
                let left_score = ordered
                    .iter()
                    .map(|chosen| cvd_distance(score(*left), chosen).unwrap())
                    .fold(f64::INFINITY, f64::min);
                let right_score = ordered
                    .iter()
                    .map(|chosen| cvd_distance(score(*right), chosen).unwrap())
                    .fold(f64::INFINITY, f64::min);
                left_score
                    .total_cmp(&right_score)
                    .then_with(|| remaining[*right].0.cmp(&remaining[*left].0))
            })
            .unwrap();

        ordered.push(remaining.remove(best_index).1);
    }

    Ok(ordered)
}

const CVD_MATRICES: [[[f64; 3]; 3]; 3] = [
    [
        [0.152286, 1.052583, -0.204868],
        [0.114503, 0.786281, 0.099216],
        [-0.003882, -0.048116, 1.051998],
    ],
    [
        [0.367322, 0.860646, -0.227968],
        [0.280085, 0.672501, 0.047413],
        [-0.011820, 0.042940, 0.968881],
    ],
    [
        [1.255528, -0.076749, -0.178779],
        [-0.078411, 0.930809, 0.147602],
        [0.004733, 0.691367, 0.303900],
    ],
];

fn simulate_cvd(value: &str, matrix: [[f64; 3]; 3]) -> Result<[f64; 3]> {
    use crate::color::parse_hex;
    let color = parse_hex(value)?;
    Ok(simulate_cvd_rgba(color, matrix))
}

fn simulate_cvd_rgba(color: Rgba, matrix: [[f64; 3]; 3]) -> [f64; 3] {
    simulate_cvd_linear(crate::color::linear_rgb(color), matrix)
}

fn simulate_cvd_linear(rgb: [f64; 3], matrix: [[f64; 3]; 3]) -> [f64; 3] {
    use crate::color::{linear_to_srgb, rgb_to_oklab};
    let transformed = matrix.map(|row| {
        row.into_iter()
            .zip(rgb)
            .map(|(factor, value)| factor * value)
            .sum::<f64>()
            .clamp(0.0, 1.0)
    });
    rgb_to_oklab(Rgba {
        r: linear_to_srgb(transformed[0]),
        g: linear_to_srgb(transformed[1]),
        b: linear_to_srgb(transformed[2]),
        a: 1.0,
    })
}

fn cvd_labs(color: Rgba) -> CvdLabs {
    let rgb = crate::color::linear_rgb(color);
    CVD_MATRICES.map(|matrix| simulate_cvd_linear(rgb, matrix))
}

fn cvd_distance_precomputed(
    first: &PairCandidate,
    first_table: &TransformTableData,
    second: &PairCandidate,
    second_table: &TransformTableData,
    normal_delta: f64,
) -> f64 {
    let first_metrics = first_table.candidates[first.source_index].metrics;
    let second_metrics = second_table.candidates[second.source_index].metrics;
    let first_cache = first_table.cvd.get_or_init(|| {
        std::iter::repeat_with(OnceLock::new)
            .take(first_table.candidates.len())
            .collect()
    });
    let second_cache = second_table.cvd.get_or_init(|| {
        std::iter::repeat_with(OnceLock::new)
            .take(second_table.candidates.len())
            .collect()
    });
    let first_cvd = &**first_cache[first.source_index]
        .get_or_init(|| Box::new(cvd_labs(first_metrics.rgba.rgba())));
    let second_cvd = &**second_cache[second.source_index]
        .get_or_init(|| Box::new(cvd_labs(second_metrics.rgba.rgba())));
    cvd_distance_facts(first_cvd, second_cvd, normal_delta)
}

fn cvd_distance_facts(first_cvd: &CvdLabs, second_cvd: &CvdLabs, normal_delta: f64) -> f64 {
    first_cvd
        .iter()
        .zip(second_cvd)
        .map(|(first, second)| lab_distance(*first, *second))
        .fold(normal_delta, f64::min)
}

pub fn cvd_distance(first: &str, second: &str) -> Result<f64> {
    let mut minimum = lab_distance(lab(first)?, lab(second)?);
    for matrix in CVD_MATRICES {
        minimum = minimum.min(lab_distance(
            simulate_cvd(first, matrix)?,
            simulate_cvd(second, matrix)?,
        ));
    }
    Ok(minimum)
}

pub fn round6(value: f64) -> f64 {
    (value * 1_000_000.0).round() / 1_000_000.0
}
