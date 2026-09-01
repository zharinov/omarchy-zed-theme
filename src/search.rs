//! Runs deterministic bounded searches over byte-quantized colors.
//!
//! Each source color is expanded once into a fidelity-sorted transform table. Query
//! caches reuse exact answers, while color-vision data is computed only for candidates
//! that reach a pair comparison. Independent source tables are prepared in parallel.

use crate::color::{
    ColorMetrics, Rgb24, Rgba, Rgba32, contrast_ratio, endpoint_chroma_taper,
    gamut_chroma_limit_with_components, gamut_map_oklch_rgb24_with_components, lab, normalize_hex,
    oklab_to_oklch, oklch_in_gamut_with_components,
};
use crate::constants::*;
use crate::{Error, Result};
use rayon::prelude::*;
use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, OnceLock};

// The widest gamut bracket is 2.0, so 32 bisections can leave less than
// 2 / 2^32 chroma below the true boundary. This larger margin makes skipping
// that search preserve the old lower-bound clamp.
const GAMUT_LIMIT_SKIP_MARGIN: f64 = 1e-9;

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
    preference_backgrounds: Vec<String>,
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
    color_results: HashMap<ColorQuery, Result<String>>,
    state_results: HashMap<StateQuery, Result<String>>,
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

impl FitBounds {
    fn validate(self) -> Result<()> {
        finite_in_range("lower lightness", self.lower_lightness, 0.0, 1.0)?;
        finite_in_range("upper lightness", self.upper_lightness, 0.0, 1.0)?;
        if self.lower_lightness > self.upper_lightness {
            return Err(Error::invalid(
                "lower lightness cannot exceed upper lightness",
            ));
        }
        finite_at_least("lower chroma", self.lower_chroma, 0.0)?;
        if self.upper_chroma.is_nan() || self.upper_chroma < 0.0 {
            return Err(Error::invalid(
                "upper chroma must be non-negative and not NaN",
            ));
        }
        if self.lower_chroma > self.upper_chroma {
            return Err(Error::invalid("lower chroma cannot exceed upper chroma"));
        }
        if let Some(preferred) = self.preferred_contrast {
            valid_contrast("preferred contrast", preferred)?;
        }
        Ok(())
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
    pub balance_rendered_salience: bool,
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
            balance_rendered_salience: false,
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
            balance_rendered_salience: false,
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

    pub const fn balance_rendered_salience(mut self) -> Self {
        self.balance_rendered_salience = true;
        self
    }

    fn validate(self) -> Result<()> {
        valid_contrast("foreground contrast", self.foreground_contrast)?;
        valid_contrast("pair contrast", self.pair_contrast)?;
        finite_at_least("normal delta E", self.normal_delta, 0.0)?;
        finite_at_least("CVD delta E", self.cvd_delta, 0.0)?;
        finite_at_least("lightness delta", self.lightness_delta, 0.0)?;
        finite_at_least("minimum chroma", self.minimum_chroma, 0.0)?;
        if let Some((contrast, normal, cvd)) = self.separation_alternative {
            valid_contrast("alternative pair contrast", contrast)?;
            finite_at_least("alternative normal delta E", normal, 0.0)?;
            finite_at_least("alternative CVD delta E", cvd, 0.0)?;
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
pub struct OverlayFitRequest<'a> {
    pub backgrounds: &'a [String],
    pub readability_backgrounds: &'a [String],
    pub target: f64,
    pub minimum_delta_e: f64,
    pub runtime_state: Option<(f64, f64, f64)>,
    pub readable_foregrounds: &'a [(String, f64)],
    pub rendered_references: &'a [(String, f64, f64)],
    pub runtime_rendered_references: &'a [(String, f64, f64, f64)],
    pub prefer_source_fidelity: bool,
}

impl<'a> OverlayFitRequest<'a> {
    pub const fn new(backgrounds: &'a [String], target: f64, minimum_delta_e: f64) -> Self {
        Self {
            backgrounds,
            readability_backgrounds: &[],
            target,
            minimum_delta_e,
            runtime_state: None,
            readable_foregrounds: &[],
            rendered_references: &[],
            runtime_rendered_references: &[],
            prefer_source_fidelity: false,
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

    pub const fn with_readability_backgrounds(
        mut self,
        readability_backgrounds: &'a [String],
    ) -> Self {
        self.readability_backgrounds = readability_backgrounds;
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

    pub const fn prefer_source_fidelity(mut self) -> Self {
        self.prefer_source_fidelity = true;
        self
    }

    fn validate(self) -> Result<()> {
        valid_contrast("overlay target contrast", self.target)?;
        finite_at_least("overlay minimum delta E", self.minimum_delta_e, 0.0)?;
        if let Some((factor, contrast, delta)) = self.runtime_state {
            finite_in_range("runtime opacity factor", factor, 0.0, 1.0)?;
            valid_contrast("runtime target contrast", contrast)?;
            finite_at_least("runtime minimum delta E", delta, 0.0)?;
        }
        for (_, target) in self.readable_foregrounds {
            valid_contrast("readable foreground contrast", *target)?;
        }
        for (_, target, delta) in self.rendered_references {
            valid_contrast("rendered reference contrast", *target)?;
            finite_at_least("rendered reference delta E", *delta, 0.0)?;
        }
        for (_, target, delta, base_step) in self.runtime_rendered_references {
            valid_contrast("runtime reference contrast", *target)?;
            finite_at_least("runtime reference delta E", *delta, 0.0)?;
            finite_at_least("runtime reference base step", *base_step, 0.0)?;
        }
        Ok(())
    }
}

pub struct OverlayPairRequest<'a> {
    pub first: OverlayFitRequest<'a>,
    pub second: OverlayFitRequest<'a>,
    pub constraints: PairConstraints,
    pub minimum_alpha: u8,
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
            minimum_alpha: 1,
            maximum_alpha: OVERLAY_MAX_ALPHA,
            frontier_limit: 512,
        }
    }

    pub const fn with_limits(mut self, maximum_alpha: u8, frontier_limit: usize) -> Self {
        self.maximum_alpha = maximum_alpha;
        self.frontier_limit = frontier_limit;
        self
    }

    pub const fn with_alpha_range(
        mut self,
        minimum_alpha: u8,
        maximum_alpha: u8,
        frontier_limit: usize,
    ) -> Self {
        self.minimum_alpha = minimum_alpha;
        self.maximum_alpha = maximum_alpha;
        self.frontier_limit = frontier_limit;
        self
    }

    fn validate(&self) -> Result<()> {
        self.first.validate()?;
        self.second.validate()?;
        self.constraints.validate()?;
        if self.minimum_alpha > self.maximum_alpha {
            return Err(Error::invalid(
                "overlay minimum alpha cannot exceed maximum alpha",
            ));
        }
        if self.frontier_limit < 128 {
            return Err(Error::invalid(
                "overlay frontier limit must be at least 128",
            ));
        }
        Ok(())
    }
}

fn finite_at_least(name: &str, value: f64, minimum: f64) -> Result<()> {
    if !value.is_finite() || value < minimum {
        return Err(Error::invalid(format!(
            "{name} must be finite and at least {minimum}, got {value:?}"
        )));
    }
    Ok(())
}

fn finite_in_range(name: &str, value: f64, minimum: f64, maximum: f64) -> Result<()> {
    if !value.is_finite() || !(minimum..=maximum).contains(&value) {
        return Err(Error::invalid(format!(
            "{name} must be finite and in {minimum}..={maximum}, got {value:?}"
        )));
    }
    Ok(())
}

fn valid_contrast(name: &str, value: f64) -> Result<()> {
    finite_in_range(name, value, 1.0, 21.0)
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
    readability_backgrounds: Vec<ColorMetrics>,
    target: f64,
    minimum_delta_e: f64,
    runtime_state: Option<(f64, f64, f64)>,
    readable_foregrounds: Vec<(ColorMetrics, f64)>,
    rendered_references: Vec<Vec<(ColorMetrics, f64, f64)>>,
    runtime_rendered_references: Vec<Vec<(ColorMetrics, f64, f64, f64)>>,
    prefer_source_fidelity: bool,
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
        request.validate()?;
        if request.backgrounds.is_empty() {
            return Err(Error::invalid(
                "overlay fit requires at least one background",
            ));
        }

        let backgrounds = request
            .backgrounds
            .iter()
            .map(|background| ColorMetrics::from_hex(background))
            .collect::<Result<Vec<_>>>()?;
        let readability_backgrounds = request
            .readability_backgrounds
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
            readability_backgrounds,
            target: request.target,
            minimum_delta_e: request.minimum_delta_e,
            runtime_state: request.runtime_state,
            readable_foregrounds,
            rendered_references,
            runtime_rendered_references,
            prefer_source_fidelity: request.prefer_source_fidelity,
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

        for background in &self.readability_backgrounds {
            let rendered = ColorMetrics::blend_rgb24(*background, opaque_rgb, alpha);
            if self
                .readable_foregrounds
                .iter()
                .any(|(foreground, target)| rendered.contrast(*foreground) < *target - 1e-12)
            {
                return None;
            }
            if let Some((runtime_opacity, _, _)) = self.runtime_state {
                let runtime_alpha = (f64::from(alpha) * runtime_opacity + 0.5).floor() as u8;
                let runtime = ColorMetrics::blend_rgb24(*background, opaque_rgb, runtime_alpha);
                if self
                    .readable_foregrounds
                    .iter()
                    .any(|(foreground, target)| runtime.contrast(*foreground) < *target - 1e-12)
                {
                    return None;
                }
            }
        }

        if minimum_ratio < self.target - 1e-12 {
            return None;
        }

        let candidate = Rgba32::from_rgb_alpha(opaque_rgb, alpha);
        let rank = if self.prefer_source_fidelity {
            [
                distance,
                overshoot,
                final_distance,
                -retention,
                -f64::from(alpha) / 255.0,
            ]
        } else {
            [
                final_distance,
                overshoot,
                distance,
                -retention,
                -f64::from(alpha) / 255.0,
            ]
        };

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

pub(crate) fn pair_constraints_satisfied(
    pair_contrast: f64,
    normal_delta: f64,
    cvd_delta: f64,
    lightness_delta: f64,
    constraints: PairConstraints,
) -> bool {
    pair_contrast >= constraints.pair_contrast - 1e-12
        && normal_delta >= constraints.normal_delta - 1e-12
        && cvd_delta >= constraints.cvd_delta - 1e-12
        && lightness_delta >= constraints.lightness_delta - 1e-12
        && pair_is_separated(pair_contrast, normal_delta, cvd_delta, constraints)
}

fn rank_cmp(left: &[f64], right: &[f64]) -> Ordering {
    assert_eq!(
        left.len(),
        right.len(),
        "semantic rank vectors must have equal dimensions"
    );
    left.iter()
        .zip(right)
        .find_map(|(left, right)| {
            let order = left.total_cmp(right);
            (order != Ordering::Equal).then_some(order)
        })
        .unwrap_or(Ordering::Equal)
}

fn opaque_color_metrics(value: &str, label: &str) -> Result<ColorMetrics> {
    normalize_hex(value, label)?;
    ColorMetrics::from_hex(value)
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
            let taper = endpoint_chroma_taper(lightness);
            let maximum_chroma = chroma * taper;
            let chroma_limit = if oklch_in_gamut_with_components(
                lightness,
                maximum_chroma + GAMUT_LIMIT_SKIP_MARGIN,
                hue_cos,
                hue_sin,
            ) {
                f64::INFINITY
            } else {
                gamut_chroma_limit_with_components(lightness, hue_cos, hue_sin)
            };
            for chroma_index in 0..=CANDIDATE_CHROMA_STEPS {
                let scale = 1.0 - f64::from(chroma_index) / f64::from(CANDIDATE_CHROMA_STEPS);
                unique.push(gamut_map_oklch_rgb24_with_components(
                    lightness,
                    chroma * scale * taper,
                    hue_cos,
                    hue_sin,
                    chroma_limit,
                ));
            }
        }
        unique.sort_unstable();
        unique.dedup();
        Ok(unique)
    }

    fn build_transform_table(seed: &str) -> Result<TransformTable> {
        normalize_hex(seed, "transform seed")?;
        let source_lab = lab(seed)?;
        let seed_chroma = oklab_to_oklch(source_lab)[1];
        let mut table = Self::candidate_colors(seed)?
            .into_iter()
            .map(|color| {
                let metrics = ColorMetrics::from_rgb24(color);
                TransformCandidate {
                    distance: lab_distance(metrics.lab, source_lab),
                    retention: lab_chroma(metrics.lab) / seed_chroma.max(1e-12),
                    metrics,
                }
            })
            .collect::<Vec<_>>();

        table.sort_unstable_by(|left, right| {
            left.distance
                .total_cmp(&right.distance)
                .then_with(|| left.metrics.rgb24().cmp(&right.metrics.rgb24()))
        });
        assert!(
            table
                .iter()
                .any(|candidate| candidate.metrics.rgb24().hex() == "#000000"),
            "transform tables must contain the black endpoint"
        );
        assert!(
            table
                .iter()
                .any(|candidate| candidate.metrics.rgb24().hex() == "#ffffff"),
            "transform tables must contain the white endpoint"
        );

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
        constraints.validate()?;
        if first_backgrounds.is_empty() || second_backgrounds.is_empty() {
            return Err(Error::invalid("fit_pair requires at least one background"));
        }
        for (index, (foreground, target)) in readable_foregrounds.iter().enumerate() {
            ColorMetrics::from_hex(foreground).map_err(|error| {
                error.context(format!("readable_foregrounds[{index}].foreground"))
            })?;
            valid_contrast(&format!("readable_foregrounds[{index}].contrast"), *target)?;
        }

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
                let lightness_delta =
                    (first_facts.metrics.lab[0] - second_facts.metrics.lab[0]).abs();
                if !pair_constraints_satisfied(
                    pair_contrast,
                    normal_delta,
                    cvd_delta,
                    lightness_delta,
                    constraints,
                ) {
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

            return Err(Error::infeasible(format!(
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
        self.fit_color_bounded_with_preference_backgrounds(
            seed,
            backgrounds,
            backgrounds,
            target,
            avoid,
            bounds,
        )
    }

    pub fn fit_color_bounded_with_preference_backgrounds(
        &mut self,
        seed: &str,
        backgrounds: &[String],
        preference_backgrounds: &[String],
        target: f64,
        avoid: &[String],
        bounds: FitBounds,
    ) -> Result<String> {
        valid_contrast("color target contrast", target)?;
        bounds.validate()?;
        let query = ColorQuery {
            seed: seed.to_owned(),
            backgrounds: backgrounds.to_vec(),
            preference_backgrounds: preference_backgrounds.to_vec(),
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
            return result.clone();
        }
        let result = self.fit_color_bounded_uncached(
            seed,
            backgrounds,
            preference_backgrounds,
            target,
            avoid,
            bounds,
        );
        self.color_results.insert(query, result.clone());
        result
    }

    fn fit_color_bounded_uncached(
        &mut self,
        seed: &str,
        backgrounds: &[String],
        preference_backgrounds: &[String],
        target: f64,
        avoid: &[String],
        bounds: FitBounds,
    ) -> Result<String> {
        if backgrounds.is_empty() || preference_backgrounds.is_empty() {
            return Err(Error::invalid("fit_color requires at least one background"));
        }

        let source_metrics = opaque_color_metrics(seed, "fit_color seed")?;
        let source_chroma = lab_chroma(source_metrics.lab);
        let source_retention = source_chroma / source_chroma.max(1e-12);
        let background_metrics = backgrounds
            .iter()
            .map(|value| opaque_color_metrics(value, "fit_color background"))
            .collect::<Result<Vec<_>>>()?;
        let preference_background_metrics = preference_backgrounds
            .iter()
            .map(|value| opaque_color_metrics(value, "fit_color preference background"))
            .collect::<Result<Vec<_>>>()?;
        let avoid_metrics = avoid
            .iter()
            .map(|value| opaque_color_metrics(value, "fit_color avoided color"))
            .collect::<Result<Vec<_>>>()?;
        let background_lightness = preference_background_metrics
            .iter()
            .map(|metrics| metrics.lab[0])
            .sum::<f64>()
            / preference_background_metrics.len() as f64;
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
                Error::infeasible(format!(
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
                    let mean_log_contrast = preference_background_metrics
                        .iter()
                        .map(|background| metrics.contrast(*background).ln())
                        .sum::<f64>()
                        / preference_background_metrics.len() as f64;
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
                Error::infeasible(format!(
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
            Error::infeasible(format!(
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
        if minimum_alpha > maximum_alpha {
            return Err(Error::invalid(
                "overlay minimum alpha cannot exceed maximum alpha",
            ));
        }
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
            .ok_or_else(|| Error::infeasible(format!("no candidate for fill {seed}")))
    }

    fn prepare_overlay_pair(
        &mut self,
        first_seed: &str,
        second_seed: &str,
        first_request: OverlayFitRequest<'_>,
        second_request: OverlayFitRequest<'_>,
        minimum_alpha: u8,
        maximum_alpha: u8,
    ) -> Result<PreparedOverlayPair> {
        let first = PreparedFill::new(first_request)?;
        let second = PreparedFill::new(second_request)?;
        if first.backgrounds.len() != second.backgrounds.len() {
            return Err(Error::invalid("overlay pair scene counts differ"));
        }

        let collect = |seed: &str,
                       table: &TransformTableData,
                       prepared: &PreparedFill,
                       minimum_alpha: u8,
                       maximum_alpha: u8|
         -> Result<Vec<FillCandidate>> {
            let alpha_values = PreparedFill::alpha_values(minimum_alpha, maximum_alpha);
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
        let (first_candidates, second_candidates) = rayon::join(
            || {
                collect(
                    first_seed,
                    &first_table,
                    &first,
                    minimum_alpha,
                    maximum_alpha,
                )
            },
            || {
                collect(
                    second_seed,
                    &second_table,
                    &second,
                    minimum_alpha,
                    maximum_alpha,
                )
            },
        );
        Ok(PreparedOverlayPair {
            first,
            second,
            first_candidates: first_candidates?,
            second_candidates: second_candidates?,
        })
    }

    fn solve_overlay_pair(
        prepared: &PreparedOverlayPair,
        first_seed: &str,
        second_seed: &str,
        constraints: PairConstraints,
        frontier_limit: usize,
    ) -> Result<[String; 2]> {
        let mut best: Option<([Rgba32; 2], [f64; 6])> = None;
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
                if !constraints.balance_rendered_salience
                    && let (Some((_, best_rank)), Some(minimum_second)) = (&best, minimum_second)
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
                    if !constraints.balance_rendered_salience
                        && best.as_ref().is_some_and(|(_, best_rank)| {
                            rank_cmp(&prefix, &best_rank[..3]) == Ordering::Greater
                        })
                    {
                        break;
                    }
                    let mut minimum_contrast = f64::INFINITY;
                    let mut minimum_normal = f64::INFINITY;
                    let mut minimum_lightness = f64::INFINITY;
                    let mut salience_imbalance = 0.0;
                    for ((left_rendered, right_rendered), (left_base, right_base)) in
                        left.rendered.iter().zip(right.rendered.iter()).zip(
                            prepared
                                .first
                                .backgrounds
                                .iter()
                                .zip(prepared.second.backgrounds.iter()),
                        )
                    {
                        let contrast = left_rendered.contrast(*right_rendered);
                        let normal = left_rendered.delta_e(*right_rendered);
                        let lightness = (left_rendered.lab[0] - right_rendered.lab[0]).abs();
                        minimum_contrast = minimum_contrast.min(contrast);
                        minimum_normal = minimum_normal.min(normal);
                        minimum_lightness = minimum_lightness.min(lightness);
                        salience_imbalance += (left_rendered.contrast(*left_base)
                            - right_rendered.contrast(*right_base))
                        .abs();
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
                    if !pair_constraints_satisfied(
                        minimum_contrast,
                        minimum_normal,
                        minimum_cvd,
                        minimum_lightness,
                        constraints,
                    ) {
                        continue;
                    }
                    let rank = if constraints.balance_rendered_salience {
                        [
                            prefix[1],
                            salience_imbalance,
                            prefix[0],
                            prefix[2],
                            -(minimum_normal + minimum_cvd),
                            -(minimum_contrast + minimum_lightness),
                        ]
                    } else {
                        [
                            prefix[0],
                            prefix[1],
                            prefix[2],
                            -(minimum_normal + minimum_cvd),
                            -(minimum_contrast + minimum_lightness),
                            0.0,
                        ]
                    };
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
                Error::infeasible(format!(
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
        request.validate()?;
        let prepared = self.prepare_overlay_pair(
            first_seed,
            second_seed,
            request.first,
            request.second,
            request.minimum_alpha,
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
        request.validate()?;
        fallback_constraints.validate()?;
        if fallback_frontier_limit < 128 {
            return Err(Error::invalid(
                "fallback overlay frontier limit must be at least 128",
            ));
        }
        let prepared = self.prepare_overlay_pair(
            first_seed,
            second_seed,
            request.first,
            request.second,
            request.minimum_alpha,
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
            Err(strong_error) if strong_error.is_infeasible() => {
                let colors = Self::solve_overlay_pair(
                    &prepared,
                    first_seed,
                    second_seed,
                    fallback_constraints,
                    fallback_frontier_limit,
                )
                .map_err(|fallback_error| {
                    if fallback_error.is_infeasible() {
                        Error::infeasible(format!(
                            "strong overlay contract failed ({strong_error}); fallback overlay contract failed ({fallback_error})"
                        ))
                    } else {
                        fallback_error.context(format!(
                            "fallback overlay search after strong contract failed ({strong_error})"
                        ))
                    }
                })?;
                Ok(OverlayPairFallback {
                    colors,
                    strong_error: Some(strong_error.to_string()),
                })
            }
            Err(error) => Err(error),
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
        valid_contrast("state target contrast", target)?;
        finite_at_least("state minimum delta E", minimum_delta_e, 0.0)?;
        for (_, reference_target, reference_delta) in references {
            valid_contrast("state reference contrast", *reference_target)?;
            finite_at_least("state reference delta E", *reference_delta, 0.0)?;
        }
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
            return result.clone();
        }
        let result =
            self.fit_state_uncached(seed, backgrounds, target, minimum_delta_e, references);
        self.state_results.insert(query, result.clone());
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
        if backgrounds.is_empty() {
            return Err(Error::invalid("fit_state requires at least one background"));
        }

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
            .ok_or_else(|| Error::infeasible(format!("no quantized state candidate for {seed}")))
    }

    pub fn fit_state_ladder(
        &mut self,
        seed: &str,
        backgrounds: &[String],
        rungs: &[(f64, f64)],
        protected: &[(String, f64, f64)],
    ) -> Result<Vec<String>> {
        if backgrounds.is_empty() {
            return Err(Error::invalid(
                "fit_state_ladder requires at least one background",
            ));
        }
        ColorMetrics::from_hex(seed).map_err(|error| error.context("state ladder seed"))?;
        for (index, background) in backgrounds.iter().enumerate() {
            ColorMetrics::from_hex(background)
                .map_err(|error| error.context(format!("state ladder background[{index}]")))?;
        }
        for (target, delta) in rungs {
            valid_contrast("state ladder target contrast", *target)?;
            finite_at_least("state ladder minimum delta E", *delta, 0.0)?;
        }
        for (index, (color, target, delta)) in protected.iter().enumerate() {
            ColorMetrics::from_hex(color)
                .map_err(|error| error.context(format!("protected state[{index}].color")))?;
            valid_contrast("protected state contrast", *target)?;
            finite_at_least("protected state delta E", *delta, 0.0)?;
        }
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
            if effective_target > 21.0 {
                return Err(Error::infeasible(format!(
                    "state ladder rung {index} requires impossible contrast {effective_target:.3}:1"
                )));
            }

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
        role: &str,
    ) -> Result<Vec<String>> {
        self.fit_distinct_colors_with_separation(
            seeds,
            backgrounds,
            target,
            ACCENT_NORMAL_DELTA_E,
            ACCENT_CVD_DELTA_E,
            role,
        )
    }

    pub fn fit_distinct_colors_with_separation(
        &mut self,
        seeds: &[String],
        backgrounds: &[String],
        target: f64,
        normal_delta_e: f64,
        cvd_delta_e: f64,
        role: &str,
    ) -> Result<Vec<String>> {
        valid_contrast("distinct color target contrast", target)?;
        finite_at_least("distinct color normal delta E", normal_delta_e, 0.0)?;
        finite_at_least("distinct color CVD delta E", cvd_delta_e, 0.0)?;
        if backgrounds.is_empty() {
            return Err(Error::invalid(
                "fit_distinct_colors requires at least one background",
            ));
        }
        for (index, seed) in seeds.iter().enumerate() {
            ColorMetrics::from_hex(seed)
                .map_err(|error| error.context(format!("distinct color seed[{index}]")))?;
        }
        for (index, background) in backgrounds.iter().enumerate() {
            ColorMetrics::from_hex(background)
                .map_err(|error| error.context(format!("distinct color background[{index}]")))?;
        }
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
                        -normal.min(normal_delta_e),
                        -cvd.min(cvd_delta_e),
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

                    if normal < normal_delta_e - 1e-12 || cvd < cvd_delta_e - 1e-12 {
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

            let output = passing.map(|(color, _)| color.hex()).ok_or_else(|| {
                let achieved = fallback
                    .map(|(_, rank)| format!(", best normal {:.3}, CVD {:.3}", -rank[0], -rank[1]))
                    .unwrap_or_default();
                Error::infeasible(format!(
                    "no distinct passing color remains for {role}[{index}]{achieved}"
                ))
            })?;

            chosen.push(output);
        }

        Ok(chosen)
    }
}

pub fn cvd_greedy_order(values: &[String]) -> Result<Vec<String>> {
    if values.is_empty() {
        return Ok(Vec::new());
    }

    const MAX_COLORS: usize = 1024;
    if values.len() > MAX_COLORS {
        return Err(Error::invalid(format!(
            "CVD ordering accepts at most {MAX_COLORS} colors, got {}",
            values.len()
        )));
    }

    let prepared = values
        .iter()
        .map(|value| PreparedCvd::new(value))
        .collect::<Result<Vec<_>>>()?;
    let matrix_len = values
        .len()
        .checked_mul(values.len())
        .expect("bounded CVD color count must have a representable square");
    let mut distances = vec![0.0; matrix_len];
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::delta_e;
    use proptest::prelude::*;

    fn rgb_hex([red, green, blue]: [u8; 3]) -> String {
        format!("#{red:02x}{green:02x}{blue:02x}")
    }

    fn standalone_luminance([red, green, blue]: [u8; 3]) -> f64 {
        let linear = |value: u8| {
            let value = f64::from(value) / 255.0;
            if value <= 0.04045 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(red) + 0.7152 * linear(green) + 0.0722 * linear(blue)
    }

    fn endpoint_contrast(background: [u8; 3]) -> f64 {
        let luminance = standalone_luminance(background);
        ((luminance + 0.05) / 0.05).max(1.05 / (luminance + 0.05))
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(24))]

        #[test]
        fn color_fit_obeys_contract_cache_determinism_and_relaxation(
            seed in any::<[u8; 3]>(),
            background in any::<[u8; 3]>(),
            strict_basis_points in 110_u16..=700,
            relaxation_basis_points in 0_u16..=100,
        ) {
            let seed = rgb_hex(seed);
            let backgrounds = vec![rgb_hex(background)];
            let strict_target = f64::from(strict_basis_points) / 100.0;
            let relaxed_target =
                (strict_target - f64::from(relaxation_basis_points) / 100.0).max(1.0);

            let mut direct = Search::default();
            let strict = direct.fit_color(&seed, &backgrounds, strict_target);
            prop_assert_eq!(&strict, &direct.fit_color(&seed, &backgrounds, strict_target));

            let mut prewarmed = Search::default();
            prewarmed.prewarm([seed.as_str()]).unwrap();
            let warmed = prewarmed.fit_color(&seed, &backgrounds, strict_target);
            prop_assert_eq!(&strict, &warmed);
            prop_assert_eq!(
                strict.is_ok(),
                endpoint_contrast(background) >= strict_target - 1e-12,
                "solver feasibility disagrees with black/white endpoint witness"
            );

            match strict {
                Ok(output) => {
                    prop_assert!(
                        contrast_ratio(&output, &backgrounds[0]).unwrap() >= strict_target - 1e-12
                    );
                    prop_assert!(
                        direct
                            .fit_color(&seed, &backgrounds, relaxed_target)
                            .is_ok(),
                        "strict target {strict_target} succeeded but relaxed target {relaxed_target} failed"
                    );
                }
                Err(error) => prop_assert_eq!(error.kind(), crate::ErrorKind::Infeasible),
            }
        }
    }

    #[test]
    fn opaque_color_fit_rejects_rgba_inputs() {
        let mut search = Search::default();
        for error in [
            search
                .fit_color("#ffffff00", &["#000000".into()], 21.0)
                .unwrap_err(),
            search
                .fit_color("#ffffff", &["#000000ff".into()], 21.0)
                .unwrap_err(),
        ] {
            assert_eq!(error.kind(), crate::ErrorKind::InvalidInput);
        }
    }

    #[test]
    fn empty_cvd_order_is_empty() {
        assert!(cvd_greedy_order(&[]).unwrap().is_empty());
    }

    #[test]
    fn fits_require_a_background() {
        let backgrounds = Vec::new();
        let request = || OverlayFitRequest::new(&backgrounds, 1.1, 0.01);
        let mut search = Search::default();

        assert!(search.fit_readable_overlay("#ffffff", request()).is_err());

        assert!(
            search
                .fit_state("#ffffff", &backgrounds, 1.1, 0.01, &[])
                .is_err()
        );

        assert!(
            search
                .fit_overlay_pair(
                    "#ffffff",
                    "#000000",
                    OverlayPairRequest::new(
                        request(),
                        request(),
                        PairConstraints::new(1.1, 1.01, 0.01, 0.01),
                    ),
                )
                .is_err()
        );

        assert!(
            search
                .fit_pair(
                    "#ffffff",
                    "#000000",
                    &backgrounds,
                    PairConstraints::new(1.1, 1.01, 0.01, 0.01),
                )
                .is_err()
        );
    }

    #[test]
    fn invalid_numeric_requests_are_rejected_before_caching() {
        let backgrounds = vec!["#101010".to_owned()];
        let mut search = Search::default();

        let target_error = search
            .fit_color("#ffffff", &backgrounds, f64::NAN)
            .unwrap_err();
        assert_eq!(target_error.kind(), crate::ErrorKind::InvalidInput);
        assert!(search.color_results.is_empty());

        let bounds_error = search
            .fit_color_bounded(
                "#ffffff",
                &backgrounds,
                4.5,
                &[],
                FitBounds {
                    lower_lightness: 0.8,
                    upper_lightness: 0.2,
                    ..FitBounds::default()
                },
            )
            .unwrap_err();
        assert_eq!(bounds_error.kind(), crate::ErrorKind::InvalidInput);
        assert!(search.color_results.is_empty());

        let pair_error = search
            .fit_pair(
                "#ffffff",
                "#000000",
                &backgrounds,
                PairConstraints::new(4.5, 1.0, f64::NAN, 0.0),
            )
            .unwrap_err();
        assert_eq!(pair_error.kind(), crate::ErrorKind::InvalidInput);

        let readable_error = search
            .fit_pair_on_backgrounds_readable(
                "#ffffff",
                &backgrounds,
                "#000000",
                &backgrounds,
                PairConstraints::new(4.5, 1.0, 0.0, 0.0),
                &[("#ffffff".into(), f64::NAN)],
            )
            .unwrap_err();
        assert_eq!(readable_error.kind(), crate::ErrorKind::InvalidInput);
        assert!(
            readable_error
                .to_string()
                .contains("readable_foregrounds[0]")
        );

        let overlay_error = search
            .fit_readable_overlay(
                "#ffffff",
                OverlayFitRequest::new(&backgrounds, 1.2, f64::NEG_INFINITY),
            )
            .unwrap_err();
        assert_eq!(overlay_error.kind(), crate::ErrorKind::InvalidInput);

        let frontier_error = search
            .fit_overlay_pair(
                "#ffffff",
                "#000000",
                OverlayPairRequest::new(
                    OverlayFitRequest::new(&backgrounds, 1.1, 0.01),
                    OverlayFitRequest::new(&backgrounds, 1.1, 0.01),
                    PairConstraints::new(1.0, 1.0, 0.0, 0.0),
                )
                .with_limits(OVERLAY_MAX_ALPHA, 1),
            )
            .unwrap_err();
        assert_eq!(frontier_error.kind(), crate::ErrorKind::InvalidInput);

        let separation_error = search
            .fit_distinct_colors_with_separation(
                &["#ffffff".into()],
                &backgrounds,
                4.5,
                f64::NAN,
                0.0,
                "test",
            )
            .unwrap_err();
        assert_eq!(separation_error.kind(), crate::ErrorKind::InvalidInput);
    }

    #[test]
    fn derived_impossible_state_ladder_is_infeasible() {
        let mut search = Search::default();
        let error = search
            .fit_state_ladder(
                "#ffffff",
                &["#000000".into()],
                &[(21.0, 0.0), (21.0, 0.0)],
                &[],
            )
            .unwrap_err();
        assert_eq!(error.kind(), crate::ErrorKind::Infeasible);
        assert!(error.to_string().contains("rung 1"));
    }

    #[test]
    fn zero_work_searches_still_validate_public_color_inputs() {
        let mut search = Search::default();
        for error in [
            search
                .fit_state_ladder("invalid", &["#000000".into()], &[], &[])
                .unwrap_err(),
            search
                .fit_state_ladder("#ffffff", &["invalid".into()], &[], &[])
                .unwrap_err(),
            search
                .fit_state_ladder(
                    "#ffffff",
                    &["#000000".into()],
                    &[],
                    &[("invalid".into(), 1.0, 0.0)],
                )
                .unwrap_err(),
            search
                .fit_distinct_colors(&[], &["invalid".into()], 4.5, "test")
                .unwrap_err(),
        ] {
            assert_eq!(error.kind(), crate::ErrorKind::InvalidInput);
        }
    }

    #[test]
    fn distinct_color_fit_never_returns_a_weak_success() {
        let mut search = Search::default();
        let error = search
            .fit_distinct_colors(
                &["#ffffff".into(), "#ffffff".into()],
                &["#000000".into()],
                20.0,
                "test",
            )
            .unwrap_err();
        assert_eq!(error.kind(), crate::ErrorKind::Infeasible);
    }

    #[test]
    fn cvd_order_rejects_unbounded_quadratic_input() {
        let values = vec!["#000000".to_owned(); 1025];
        let error = cvd_greedy_order(&values).unwrap_err();
        assert_eq!(error.kind(), crate::ErrorKind::InvalidInput);
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
    fn required_and_preference_backgrounds_are_independent_and_cached() {
        let mut search = Search::default();
        let required = vec!["#777777".to_owned()];
        let dark_preference = vec!["#101010".to_owned()];
        let light_preference = vec!["#f0f0f0".to_owned()];
        let bounds = FitBounds {
            preferred_contrast: Some(8.0),
            ..FitBounds::default()
        };
        let dark = search
            .fit_color_bounded_with_preference_backgrounds(
                "#cccccc",
                &required,
                &dark_preference,
                1.05,
                &[],
                bounds,
            )
            .unwrap();
        let light = search
            .fit_color_bounded_with_preference_backgrounds(
                "#cccccc",
                &required,
                &light_preference,
                1.05,
                &[],
                bounds,
            )
            .unwrap();

        assert_ne!(dark, light);
        assert!(contrast_ratio(&dark, &dark_preference[0]).unwrap() >= 7.5);
        assert!(contrast_ratio(&light, &light_preference[0]).unwrap() >= 7.5);
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
