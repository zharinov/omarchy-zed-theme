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
    lower_chroma: u64,
    upper_chroma: u64,
    preferred_contrast: Option<u64>,
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
    pub lower_chroma: f64,
    pub upper_chroma: f64,
    pub preferred_contrast: Option<f64>,
    pub prefer_background: bool,
}

impl Default for FitBounds {
    fn default() -> Self {
        Self {
            lower_lightness: 0.0,
            upper_lightness: 1.0,
            lower_chroma: 0.0,
            upper_chroma: f64::INFINITY,
            preferred_contrast: None,
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
    pub minimum_chroma: f64,
    pub separation_alternative: Option<(f64, f64, f64)>,
    pub prefer_background: bool,
}

impl PairConstraints {
    pub const fn new(
        foreground_contrast: f64,
        pair_contrast: f64,
        normal_delta: f64,
        cvd_delta: f64,
    ) -> Self {
        Self {
            foreground_contrast,
            pair_contrast,
            normal_delta,
            cvd_delta,
            lightness_delta: 0.0,
            minimum_chroma: 0.0,
            separation_alternative: None,
            prefer_background: false,
        }
    }

    pub const fn from_contract(foreground_contrast: f64, contract: PairContract) -> Self {
        Self {
            foreground_contrast,
            pair_contrast: contract.contrast,
            normal_delta: contract.normal_delta_e,
            cvd_delta: contract.cvd_delta_e,
            lightness_delta: 0.0,
            minimum_chroma: 0.0,
            separation_alternative: contract.separation_alternative,
            prefer_background: false,
        }
    }

    pub const fn with_foreground_contrast(mut self, foreground_contrast: f64) -> Self {
        self.foreground_contrast = foreground_contrast;
        self
    }

    pub const fn with_minimum_chroma(mut self, minimum_chroma: f64) -> Self {
        self.minimum_chroma = minimum_chroma;
        self
    }

    pub const fn with_cvd_delta(mut self, cvd_delta: f64) -> Self {
        self.cvd_delta = cvd_delta;
        self
    }

    pub const fn with_separation_alternative(
        mut self,
        separation_alternative: Option<(f64, f64, f64)>,
    ) -> Self {
        self.separation_alternative = separation_alternative;
        self
    }

    pub const fn prefer_background(mut self) -> Self {
        self.prefer_background = true;
        self
    }
}

#[derive(Clone, Copy)]
pub struct OverlayFitRequest<'a> {
    pub backgrounds: &'a [String],
    pub target: f64,
    pub minimum_delta_e: f64,
    pub runtime_state: Option<(f64, f64, f64)>,
    pub readable_foregrounds: &'a [(String, f64)],
    pub rendered_references: &'a [(String, f64, f64)],
    pub runtime_rendered_references: &'a [(String, f64, f64, f64)],
}

impl<'a> OverlayFitRequest<'a> {
    pub const fn new(backgrounds: &'a [String], target: f64, minimum_delta_e: f64) -> Self {
        Self {
            backgrounds,
            target,
            minimum_delta_e,
            runtime_state: None,
            readable_foregrounds: &[],
            rendered_references: &[],
            runtime_rendered_references: &[],
        }
    }

    pub const fn with_target(mut self, target: f64) -> Self {
        self.target = target;
        self
    }

    pub const fn with_runtime_state(mut self, runtime_state: (f64, f64, f64)) -> Self {
        self.runtime_state = Some(runtime_state);
        self
    }

    pub const fn with_readable_foregrounds(
        mut self,
        readable_foregrounds: &'a [(String, f64)],
    ) -> Self {
        self.readable_foregrounds = readable_foregrounds;
        self
    }

    pub const fn with_rendered_references(
        mut self,
        rendered_references: &'a [(String, f64, f64)],
    ) -> Self {
        self.rendered_references = rendered_references;
        self
    }

    pub const fn with_runtime_rendered_references(
        mut self,
        runtime_rendered_references: &'a [(String, f64, f64, f64)],
    ) -> Self {
        self.runtime_rendered_references = runtime_rendered_references;
        self
    }
}

pub struct OverlayPairRequest<'a> {
    pub first: OverlayFitRequest<'a>,
    pub second: OverlayFitRequest<'a>,
    pub constraints: PairConstraints,
    pub maximum_alpha: u8,
    pub frontier_limit: usize,
}

impl<'a> OverlayPairRequest<'a> {
    pub const fn new(
        first: OverlayFitRequest<'a>,
        second: OverlayFitRequest<'a>,
        constraints: PairConstraints,
    ) -> Self {
        Self {
            first,
            second,
            constraints,
            maximum_alpha: OVERLAY_MAX_ALPHA,
            frontier_limit: 512,
        }
    }

    pub const fn with_limits(mut self, maximum_alpha: u8, frontier_limit: usize) -> Self {
        self.maximum_alpha = maximum_alpha;
        self.frontier_limit = frontier_limit;
        self
    }
}

pub struct OverlayPairFallback {
    pub colors: [String; 2],
    pub strong_error: Option<String>,
}

type FillRank = [f64; 5];
#[derive(Clone)]
struct FillCandidate {
    emitted: Rgba32,
    rank: FillRank,
    source_chroma: f64,
}

struct FrontierCandidate {
    core: FillCandidate,
    rendered: Box<[ColorMetrics]>,
    cvd: OnceLock<Box<[CvdLabs]>>,
}

impl FrontierCandidate {
    fn new(core: FillCandidate, prepared: &PreparedFill) -> Self {
        let rendered = prepared
            .backgrounds
            .iter()
            .map(|background| {
                ColorMetrics::blend_rgb24(*background, core.emitted.rgb24(), core.emitted.alpha())
                    .metrics()
            })
            .collect();
        Self {
            core,
            rendered,
            cvd: OnceLock::new(),
        }
    }

    fn cvd(&self) -> &[CvdLabs] {
        self.cvd.get_or_init(|| {
            self.rendered
                .iter()
                .map(|rendered| cvd_labs(rendered.rgba.rgba()))
                .collect()
        })
    }
}

fn frontier_rank_cmp(left: &FrontierCandidate, right: &FrontierCandidate) -> Ordering {
    rank_cmp(&left.core.rank[..3], &right.core.rank[..3])
        .then_with(|| left.core.emitted.hex_cmp(right.core.emitted))
}

fn combined_frontier_rank(left: &FrontierCandidate, right: &FrontierCandidate) -> [f64; 3] {
    [
        left.core.rank[0] + right.core.rank[0],
        left.core.rank[1] + right.core.rank[1],
        left.core.rank[2] + right.core.rank[2],
    ]
}

struct PreparedFill {
    backgrounds: Vec<ColorMetrics>,
    target: f64,
    minimum_delta_e: f64,
    runtime_state: Option<(f64, f64, f64)>,
    readable_foregrounds: Vec<(ColorMetrics, f64)>,
    rendered_references: Vec<Vec<(ColorMetrics, f64, f64)>>,
    runtime_rendered_references: Vec<Vec<(ColorMetrics, f64, f64, f64)>>,
}

struct PreparedOverlayPair {
    first: PreparedFill,
    second: PreparedFill,
    first_candidates: Vec<FillCandidate>,
    second_candidates: Vec<FillCandidate>,
}

fn overlay_frontier(candidates: &[FillCandidate], limit: usize) -> Vec<FillCandidate> {
    let mut selected = candidates
        .iter()
        .take(limit / 2)
        .cloned()
        .collect::<Vec<_>>();
    let mut alpha_order = candidates.iter().collect::<Vec<_>>();
    alpha_order.sort_by(|left, right| {
        right
            .emitted
            .alpha()
            .cmp(&left.emitted.alpha())
            .then_with(|| rank_cmp(&left.rank, &right.rank))
            .then_with(|| left.emitted.hex_cmp(right.emitted))
    });
    selected.extend(alpha_order.into_iter().take(limit / 2).cloned());
    selected.sort_by_key(|candidate| candidate.emitted);
    selected.dedup_by_key(|candidate| candidate.emitted);
    selected
}

impl PreparedFill {
    fn alpha_values(minimum_alpha: u8, maximum_alpha: u8) -> Vec<u8> {
        let mut values = (2..=20)
            .map(|alpha_index| ((alpha_index * 255 + 10) / 20) as u8)
            .filter(|alpha| *alpha >= minimum_alpha && *alpha <= maximum_alpha)
            .collect::<Vec<_>>();
        if minimum_alpha <= maximum_alpha {
            values.push(minimum_alpha);
        }
        if maximum_alpha > 0 {
            values.push(maximum_alpha);
        }
        values.sort_unstable();
        values.dedup();
        values
    }

    fn new(request: OverlayFitRequest<'_>) -> Result<Self> {
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

    fn best_and_highest_for(
        &self,
        opaque: ColorMetrics,
        distance: f64,
        retention: f64,
        alpha_values: &[u8],
    ) -> (Option<FillCandidate>, Option<FillCandidate>) {
        let mut best: Option<FillCandidate> = None;
        let mut highest: Option<FillCandidate> = None;
        let opaque_rgb = opaque.rgb24();
        let source_chroma = lab_chroma(opaque.lab);

        for &alpha in alpha_values {
            let Some(candidate) =
                self.evaluate_alpha(opaque_rgb, alpha, distance, retention, source_chroma)
            else {
                continue;
            };

            highest = Some(candidate.clone());
            if best.as_ref().is_none_or(|best| {
                rank_cmp(&candidate.rank, &best.rank)
                    .then_with(|| candidate.emitted.hex_cmp(best.emitted))
                    == Ordering::Less
            }) {
                best = Some(candidate);
            }
        }

        (best, highest)
    }

    fn best_for(
        &self,
        opaque: ColorMetrics,
        distance: f64,
        retention: f64,
        alpha_values: &[u8],
    ) -> Option<FillCandidate> {
        self.best_and_highest_for(opaque, distance, retention, alpha_values)
            .0
    }

    fn evaluate_alpha(
        &self,
        opaque_rgb: Rgb24,
        alpha: u8,
        distance: f64,
        retention: f64,
        source_chroma: f64,
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

        Some(FillCandidate {
            emitted: candidate,
            rank,
            source_chroma,
        })
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

fn lab_chroma([_, a, b]: [f64; 3]) -> f64 {
    a.hypot(b)
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
                    retention: lab_chroma(metrics.lab) / seed_chroma.max(1e-12),
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
                if lab_chroma(metrics.lab) < constraints.minimum_chroma - 1e-12 {
                    continue;
                }
                if background_metrics.iter().any(|background| {
                    metrics.contrast(*background) < constraints.foreground_contrast - 1e-12
                }) || readable_metrics
                    .iter()
                    .any(|(foreground, target)| metrics.contrast(*foreground) < *target - 1e-12)
                {
                    continue;
                }
                let background_distance = if constraints.prefer_background {
                    background_metrics
                        .iter()
                        .map(|background| metrics.delta_e(*background))
                        .sum()
                } else {
                    0.0
                };
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

        let Some((colors, _)) = best else {
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

            return Err(Error(format!(
                "no jointly fitted pair remains for {first_seed}/{second_seed} ({} first candidates, {} second candidates; maxima contrast {:.3}, delta E {:.3}, cheap-feasible CVD {:.3})",
                first.len(),
                second.len(),
                maxima[0],
                maxima[1],
                maxima[2]
            )));
        };

        Ok([colors[0].hex(), colors[1].hex()])
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
            lower_chroma: bounds.lower_chroma.to_bits(),
            upper_chroma: bounds.upper_chroma.to_bits(),
            preferred_contrast: bounds.preferred_contrast.map(f64::to_bits),
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
        let source_chroma = lab_chroma(source_metrics.lab);
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
            let chroma = lab_chroma(candidate.lab);
            if chroma < bounds.lower_chroma - 1e-12 || chroma > bounds.upper_chroma + 1e-12 {
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

        if passes(source_metrics)
            && !bounds.prefer_background
            && bounds.preferred_contrast.is_none()
        {
            return Ok(seed.to_owned());
        }

        if !bounds.prefer_background && bounds.preferred_contrast.is_none() {
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

        if let Some(preferred_contrast) = bounds.preferred_contrast {
            let mut best: Option<(Rgb24, [f64; 3])> = None;
            let preferred_log = preferred_contrast.ln();
            let mut consider =
                |candidate: Rgb24, metrics: ColorMetrics, distance: f64, retention: f64| {
                    if !passes(metrics) {
                        return;
                    }
                    let mean_log_contrast = background_metrics
                        .iter()
                        .map(|background| metrics.contrast(*background).ln())
                        .sum::<f64>()
                        / background_metrics.len() as f64;
                    let primary = (mean_log_contrast - preferred_log).abs();
                    let secondary = if bounds.prefer_background {
                        (metrics.lab[0] - background_lightness).abs()
                    } else {
                        distance
                    };
                    let rank = [primary, secondary, -retention];
                    if best.as_ref().is_none_or(|(best_color, best_rank)| {
                        rank_cmp(&rank, best_rank).then_with(|| candidate.cmp(best_color))
                            == Ordering::Less
                    }) {
                        best = Some((candidate, rank));
                    }
                };
            consider(
                Rgb24::from_rgba(source_metrics.rgba.rgba()),
                source_metrics,
                0.0,
                source_retention,
            );
            for candidate in self.transform_table(seed)?.candidates.iter() {
                consider(
                    candidate.metrics.rgb24(),
                    candidate.metrics,
                    candidate.distance,
                    candidate.retention,
                );
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

    pub fn fit_readable_overlay(
        &mut self,
        seed: &str,
        request: OverlayFitRequest<'_>,
    ) -> Result<String> {
        self.fit_readable_overlay_bounded(seed, request, OVERLAY_MAX_ALPHA)
    }

    pub fn fit_readable_overlay_bounded(
        &mut self,
        seed: &str,
        request: OverlayFitRequest<'_>,
        maximum_alpha: u8,
    ) -> Result<String> {
        self.fit_readable_overlay_alpha_range(seed, request, 1, maximum_alpha)
    }

    pub fn fit_readable_overlay_alpha_range(
        &mut self,
        seed: &str,
        request: OverlayFitRequest<'_>,
        minimum_alpha: u8,
        maximum_alpha: u8,
    ) -> Result<String> {
        let prepared = PreparedFill::new(request)?;
        let alpha_values = PreparedFill::alpha_values(minimum_alpha, maximum_alpha);
        let source = ColorMetrics::from_hex(seed)?;
        if let Some(candidate) = prepared.best_for(source, 0.0, 1.0, &alpha_values) {
            return Ok(candidate.emitted.hex());
        }

        let table = self.transform_table(seed)?;
        let best = table
            .candidates
            .par_iter()
            .map(|opaque| {
                prepared.best_for(
                    opaque.metrics,
                    opaque.distance,
                    opaque.retention,
                    &alpha_values,
                )
            })
            .reduce(
                || None,
                |left, right| match (left, right) {
                    (None, other) | (other, None) => other,
                    (Some(left), Some(right)) => {
                        let order = rank_cmp(&left.rank, &right.rank)
                            .then_with(|| left.emitted.hex_cmp(right.emitted));
                        Some(if order == Ordering::Greater {
                            right
                        } else {
                            left
                        })
                    }
                },
            );
        best.map(|candidate| candidate.emitted.hex())
            .ok_or_else(|| Error(format!("no candidate for fill {seed}")))
    }

    pub fn fit_exact_readable_overlay(
        &self,
        seed: &str,
        request: OverlayFitRequest<'_>,
    ) -> Result<Option<String>> {
        let prepared = PreparedFill::new(request)?;
        let metrics = ColorMetrics::from_hex(seed)?;
        let alpha_values = PreparedFill::alpha_values(1, u8::MAX);
        Ok(prepared
            .best_for(metrics, 0.0, 1.0, &alpha_values)
            .map(|candidate| candidate.emitted.hex()))
    }

    fn prepare_overlay_pair(
        &mut self,
        first_seed: &str,
        second_seed: &str,
        first_request: OverlayFitRequest<'_>,
        second_request: OverlayFitRequest<'_>,
        maximum_alpha: u8,
    ) -> Result<PreparedOverlayPair> {
        let first = PreparedFill::new(first_request)?;
        let second = PreparedFill::new(second_request)?;
        if first.backgrounds.len() != second.backgrounds.len() {
            return Err(Error("overlay pair scene counts differ".into()));
        }

        let collect = |seed: &str,
                       table: &TransformTableData,
                       prepared: &PreparedFill,
                       maximum_alpha: u8|
         -> Result<Vec<FillCandidate>> {
            let alpha_values = PreparedFill::alpha_values(1, maximum_alpha);
            let mut candidates = table
                .candidates
                .par_iter()
                .flat_map_iter(|opaque| {
                    let (best, highest) = prepared.best_and_highest_for(
                        opaque.metrics,
                        opaque.distance,
                        opaque.retention,
                        &alpha_values,
                    );
                    [best, highest].into_iter().flatten()
                })
                .collect::<Vec<_>>();
            if let Some(source) =
                prepared.best_for(ColorMetrics::from_hex(seed)?, 0.0, 1.0, &alpha_values)
            {
                candidates.push(source);
            }
            candidates.sort_by(|left, right| {
                rank_cmp(&left.rank, &right.rank).then_with(|| left.emitted.hex_cmp(right.emitted))
            });
            candidates.dedup_by_key(|candidate| candidate.emitted);
            Ok(candidates)
        };

        let first_table = self.transform_table(first_seed)?;
        let second_table = self.transform_table(second_seed)?;
        let first_candidates = collect(first_seed, &first_table, &first, maximum_alpha)?;
        let second_candidates = collect(second_seed, &second_table, &second, maximum_alpha)?;
        Ok(PreparedOverlayPair {
            first,
            second,
            first_candidates,
            second_candidates,
        })
    }

    fn solve_overlay_pair(
        prepared: &PreparedOverlayPair,
        first_seed: &str,
        second_seed: &str,
        constraints: PairConstraints,
        frontier_limit: usize,
    ) -> Result<[String; 2]> {
        let mut best: Option<([Rgba32; 2], [f64; 5])> = None;
        let mut maxima = [0.0_f64; 3];

        for frontier_size in [128_usize, 512]
            .into_iter()
            .filter(|size| *size <= frontier_limit)
        {
            let mut first_frontier = overlay_frontier(&prepared.first_candidates, frontier_size)
                .into_iter()
                .map(|candidate| FrontierCandidate::new(candidate, &prepared.first))
                .collect::<Vec<_>>();
            let mut second_frontier = overlay_frontier(&prepared.second_candidates, frontier_size)
                .into_iter()
                .map(|candidate| FrontierCandidate::new(candidate, &prepared.second))
                .collect::<Vec<_>>();
            first_frontier.sort_by(frontier_rank_cmp);
            second_frontier.sort_by(frontier_rank_cmp);
            let minimum_second = second_frontier.first();
            for left in &first_frontier {
                if left.core.source_chroma < constraints.minimum_chroma - 1e-12 {
                    continue;
                }
                if let (Some((_, best_rank)), Some(minimum_second)) = (&best, minimum_second)
                    && rank_cmp(
                        &combined_frontier_rank(left, minimum_second),
                        &best_rank[..3],
                    ) == Ordering::Greater
                {
                    break;
                }
                for right in &second_frontier {
                    if right.core.source_chroma < constraints.minimum_chroma - 1e-12 {
                        continue;
                    }
                    let prefix = combined_frontier_rank(left, right);
                    if best.as_ref().is_some_and(|(_, best_rank)| {
                        rank_cmp(&prefix, &best_rank[..3]) == Ordering::Greater
                    }) {
                        break;
                    }
                    let mut minimum_contrast = f64::INFINITY;
                    let mut minimum_normal = f64::INFINITY;
                    let mut minimum_lightness = f64::INFINITY;
                    for (left_rendered, right_rendered) in
                        left.rendered.iter().zip(right.rendered.iter())
                    {
                        let contrast = left_rendered.contrast(*right_rendered);
                        let normal = left_rendered.delta_e(*right_rendered);
                        let lightness = (left_rendered.lab[0] - right_rendered.lab[0]).abs();
                        minimum_contrast = minimum_contrast.min(contrast);
                        minimum_normal = minimum_normal.min(normal);
                        minimum_lightness = minimum_lightness.min(lightness);
                    }
                    maxima[0] = maxima[0].max(minimum_contrast);
                    maxima[1] = maxima[1].max(minimum_normal);
                    if minimum_contrast < constraints.pair_contrast - 1e-12
                        || minimum_normal < constraints.normal_delta - 1e-12
                        || minimum_lightness < constraints.lightness_delta - 1e-12
                    {
                        continue;
                    }
                    let mut minimum_cvd = minimum_normal;
                    for (left_cvd, right_cvd) in left.cvd().iter().zip(right.cvd()) {
                        let cvd = cvd_distance_facts(left_cvd, right_cvd, f64::INFINITY);
                        minimum_cvd = minimum_cvd.min(cvd);
                    }
                    maxima[2] = maxima[2].max(minimum_cvd);
                    if minimum_cvd < constraints.cvd_delta - 1e-12
                        || !pair_is_separated(
                            minimum_contrast,
                            minimum_normal,
                            minimum_cvd,
                            constraints,
                        )
                    {
                        continue;
                    }
                    let rank = [
                        prefix[0],
                        prefix[1],
                        prefix[2],
                        -(minimum_normal + minimum_cvd),
                        -(minimum_contrast + minimum_lightness),
                    ];
                    if best.as_ref().is_none_or(|(best_colors, best_rank)| {
                        rank_cmp(&rank, best_rank)
                            .then_with(|| [left.core.emitted, right.core.emitted].cmp(best_colors))
                            == Ordering::Less
                    }) {
                        best = Some(([left.core.emitted, right.core.emitted], rank));
                    }
                }
            }
            if best.is_some() {
                break;
            }
        }

        best.map(|(colors, _)| [colors[0].hex(), colors[1].hex()])
            .ok_or_else(|| {
                Error(format!(
                    "no rendered overlay pair for {first_seed} and {second_seed} ({} and {} local candidates; maxima contrast {:.3}, delta E {:.3}, CVD {:.3})",
                    prepared.first_candidates.len(),
                    prepared.second_candidates.len(),
                    maxima[0],
                    maxima[1],
                    maxima[2],
                ))
            })
    }

    pub fn fit_overlay_pair(
        &mut self,
        first_seed: &str,
        second_seed: &str,
        request: OverlayPairRequest<'_>,
    ) -> Result<[String; 2]> {
        let prepared = self.prepare_overlay_pair(
            first_seed,
            second_seed,
            request.first,
            request.second,
            request.maximum_alpha,
        )?;
        Self::solve_overlay_pair(
            &prepared,
            first_seed,
            second_seed,
            request.constraints,
            request.frontier_limit,
        )
    }

    pub fn fit_overlay_pair_with_fallback(
        &mut self,
        first_seed: &str,
        second_seed: &str,
        request: OverlayPairRequest<'_>,
        fallback_constraints: PairConstraints,
        fallback_frontier_limit: usize,
    ) -> Result<OverlayPairFallback> {
        let prepared = self.prepare_overlay_pair(
            first_seed,
            second_seed,
            request.first,
            request.second,
            request.maximum_alpha,
        )?;
        match Self::solve_overlay_pair(
            &prepared,
            first_seed,
            second_seed,
            request.constraints,
            request.frontier_limit,
        ) {
            Ok(colors) => Ok(OverlayPairFallback {
                colors,
                strong_error: None,
            }),
            Err(strong_error) => {
                let colors = Self::solve_overlay_pair(
                    &prepared,
                    first_seed,
                    second_seed,
                    fallback_constraints,
                    fallback_frontier_limit,
                )?;
                Ok(OverlayPairFallback {
                    colors,
                    strong_error: Some(strong_error.to_string()),
                })
            }
        }
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
            let fitted_retention = lab_chroma(fitted_metrics.lab) / seed_chroma.max(1e-12);
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
    if values.is_empty() {
        return Ok(Vec::new());
    }

    let prepared = values
        .iter()
        .map(|value| PreparedCvd::new(value))
        .collect::<Result<Vec<_>>>()?;
    let mut distances = vec![0.0; values.len() * values.len()];
    for first in 0..values.len() {
        for second in (first + 1)..values.len() {
            let distance = prepared[first].distance(prepared[second]);
            distances[first * values.len() + second] = distance;
            distances[second * values.len() + first] = distance;
        }
    }

    let mut remaining: Vec<usize> = (1..values.len()).collect();
    let mut ordered = vec![0];

    while !remaining.is_empty() {
        let best_index = (0..remaining.len())
            .max_by(|left, right| {
                let score = |index: usize| remaining[index];
                let left_score = ordered
                    .iter()
                    .map(|chosen| distances[score(*left) * values.len() + chosen])
                    .fold(f64::INFINITY, f64::min);
                let right_score = ordered
                    .iter()
                    .map(|chosen| distances[score(*right) * values.len() + chosen])
                    .fold(f64::INFINITY, f64::min);
                left_score
                    .total_cmp(&right_score)
                    .then_with(|| remaining[*right].cmp(&remaining[*left]))
            })
            .expect("remaining colors are non-empty");

        ordered.push(remaining.remove(best_index));
    }

    Ok(ordered
        .into_iter()
        .map(|index| values[index].clone())
        .collect())
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

#[derive(Clone, Copy)]
struct PreparedCvd {
    normal: [f64; 3],
    simulated: CvdLabs,
}

impl PreparedCvd {
    fn new(value: &str) -> Result<Self> {
        let metrics = ColorMetrics::from_hex(value)?;
        Ok(Self {
            normal: metrics.lab,
            simulated: cvd_labs(metrics.rgba.rgba()),
        })
    }

    fn distance(self, other: Self) -> f64 {
        cvd_distance_facts(
            &self.simulated,
            &other.simulated,
            lab_distance(self.normal, other.normal),
        )
    }
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
    Ok(PreparedCvd::new(first)?.distance(PreparedCvd::new(second)?))
}

pub fn round6(value: f64) -> f64 {
    (value * 1_000_000.0).round() / 1_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_cvd_order_is_empty() {
        assert!(cvd_greedy_order(&[]).unwrap().is_empty());
    }

    #[test]
    fn cvd_order_reports_invalid_colors() {
        assert!(cvd_greedy_order(&["invalid".into()]).is_err());
    }

    #[test]
    fn chroma_bounds_are_enforced_and_part_of_the_query_cache_key() {
        let mut search = Search::default();
        let backgrounds = vec!["#101010".to_owned()];
        let restrained = search
            .fit_color_bounded(
                "#ff0000",
                &backgrounds,
                3.0,
                &[],
                FitBounds {
                    upper_chroma: 0.05,
                    ..FitBounds::default()
                },
            )
            .unwrap();
        let vivid = search
            .fit_color_bounded(
                "#ff0000",
                &backgrounds,
                3.0,
                &[],
                FitBounds {
                    upper_chroma: 0.20,
                    ..FitBounds::default()
                },
            )
            .unwrap();

        assert!(oklab_to_oklch(lab(&restrained).unwrap())[1] <= 0.05 + 1e-12);
        assert!(oklab_to_oklch(lab(&vivid).unwrap())[1] <= 0.20 + 1e-12);
        assert_ne!(restrained, vivid);
        assert_eq!(search.color_results.len(), 2);
    }

    #[test]
    fn preferred_contrast_is_ranked_and_part_of_the_query_cache_key() {
        let mut search = Search::default();
        let backgrounds = vec!["#121212".to_owned()];
        let subtle = search
            .fit_color_bounded(
                "#cccccc",
                &backgrounds,
                1.52,
                &[],
                FitBounds {
                    preferred_contrast: Some(2.0),
                    ..FitBounds::default()
                },
            )
            .unwrap();
        let focal = search
            .fit_color_bounded(
                "#cccccc",
                &backgrounds,
                1.52,
                &[],
                FitBounds {
                    preferred_contrast: Some(8.0),
                    ..FitBounds::default()
                },
            )
            .unwrap();

        assert!(
            contrast_ratio(&subtle, &backgrounds[0]).unwrap()
                < contrast_ratio(&focal, &backgrounds[0]).unwrap()
        );
        assert_eq!(search.color_results.len(), 2);
    }

    #[test]
    fn overlay_pair_is_bounded_deterministic_and_valid_after_composition() {
        let backgrounds = vec!["#16181d".to_owned(), "#242730".to_owned()];
        let request = || {
            OverlayPairRequest::new(
                OverlayFitRequest::new(&backgrounds, 1.10, 0.025),
                OverlayFitRequest::new(&backgrounds, 1.10, 0.025),
                PairConstraints::new(1.10, 1.01, 0.030, 0.020).prefer_background(),
            )
            .with_limits(198, 512)
        };
        let mut search = Search::default();
        let first = search
            .fit_overlay_pair("#4fa66b", "#d75b68", request())
            .unwrap();
        let second = search
            .fit_overlay_pair("#4fa66b", "#d75b68", request())
            .unwrap();

        assert_eq!(first, second);
        assert!(
            first
                .iter()
                .all(|value| crate::color::parse_hex(value).unwrap().a <= 198.0 / 255.0 + 1e-12)
        );
        for background in &backgrounds {
            let left = crate::color::render_layers(background, &[&first[0]]).unwrap();
            let right = crate::color::render_layers(background, &[&first[1]]).unwrap();
            assert!(contrast_ratio(&left, background).unwrap() >= 1.10 - 1e-9);
            assert!(contrast_ratio(&right, background).unwrap() >= 1.10 - 1e-9);
            assert!(delta_e(&left, &right).unwrap() >= 0.030 - 1e-9);
            assert!(cvd_distance(&left, &right).unwrap() >= 0.020 - 1e-9);
        }
    }

    #[test]
    fn overlay_pair_fallback_reuses_candidates_without_changing_the_weak_result() {
        let backgrounds = vec!["#16181d".to_owned(), "#242730".to_owned()];
        let constraints = PairConstraints::new(1.10, 1.01, 0.030, 0.020).prefer_background();
        let request = |constraints| {
            OverlayPairRequest::new(
                OverlayFitRequest::new(&backgrounds, 1.10, 0.025),
                OverlayFitRequest::new(&backgrounds, 1.10, 0.025),
                constraints,
            )
            .with_limits(198, 128)
        };
        let impossible = constraints.with_cvd_delta(1.0);
        let mut search = Search::default();
        let fallback = search
            .fit_overlay_pair_with_fallback(
                "#4fa66b",
                "#d75b68",
                request(impossible),
                constraints,
                512,
            )
            .unwrap();
        let direct = search
            .fit_overlay_pair("#4fa66b", "#d75b68", request(constraints))
            .unwrap();

        assert!(fallback.strong_error.is_some());
        assert_eq!(fallback.colors, direct);
    }
}
