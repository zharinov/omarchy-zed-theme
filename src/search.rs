//! Runs deterministic bounded searches over byte-quantized colors.
//!
//! Each source color is expanded once into a fidelity-sorted transform table. Query
//! caches reuse exact answers, while color-vision data is computed only for candidates
//! that reach a pair comparison. Independent source tables are prepared in parallel.

use crate::color::{
    ColorMetrics, Rgb24, Rgba, Rgba32, endpoint_chroma_taper, gamut_chroma_limit_with_components,
    gamut_map_oklch_rgb24_with_components, lab, normalize_hex, oklab_to_oklch,
    oklch_in_gamut_with_components,
};
use crate::constants::*;
use crate::{Error, Result};
use rayon::prelude::*;
use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, OnceLock};

// The widest gamut bracket is 2.0, so 32 bisections can leave less than
// 2 / 2^32 chroma below the true boundary. This larger margin makes skipping
// that search preserve the lower-bound clamp.
const GAMUT_LIMIT_SKIP_MARGIN: f64 = 1e-9;
const PAIR_BEST_EFFORT_FRONTIER_LIMIT: usize = 256;

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
    per_background_contrast: PerBackgroundContrast<Vec<u64>>,
    contrast: MetricBandQuery,
    avoid: Vec<String>,
    lower_lightness: u64,
    upper_lightness: u64,
    lower_chroma: u64,
    upper_chroma: u64,
    prefer_background: bool,
}

#[derive(Clone, Copy, Hash, PartialEq, Eq)]
enum PerBackgroundContrast<T> {
    None,
    Floors(T),
    Ceilings(T),
}

impl<'a> PerBackgroundContrast<&'a [f64]> {
    const fn floors(floors: &'a [f64]) -> Self {
        if floors.is_empty() {
            Self::None
        } else {
            Self::Floors(floors)
        }
    }

    const fn ceilings(ceilings: &'a [f64]) -> Self {
        if ceilings.is_empty() {
            Self::None
        } else {
            Self::Ceilings(ceilings)
        }
    }

    fn cache_key(self) -> PerBackgroundContrast<Vec<u64>> {
        let bits = |values: &[f64]| values.iter().map(|value| value.to_bits()).collect();
        match self {
            Self::None => PerBackgroundContrast::None,
            Self::Floors(values) => PerBackgroundContrast::Floors(bits(values)),
            Self::Ceilings(values) => PerBackgroundContrast::Ceilings(bits(values)),
        }
    }
}

#[derive(Hash, PartialEq, Eq)]
struct StateQuery {
    seed: String,
    backgrounds: Vec<String>,
    contrast: MetricBandQuery,
    delta_e: MetricBandQuery,
    minimum_chroma: u64,
    references: Vec<(String, u64, u64)>,
}

#[derive(Clone, Copy, Hash, PartialEq, Eq)]
struct MetricBandQuery {
    minimum: u64,
    preferred: Option<u64>,
    maximum: Option<u64>,
}

impl From<MetricBand> for MetricBandQuery {
    fn from(band: MetricBand) -> Self {
        Self {
            minimum: band.minimum.to_bits(),
            preferred: band.preferred.map(f64::to_bits),
            maximum: band.maximum.map(f64::to_bits),
        }
    }
}

#[derive(Default)]
pub struct Search {
    transform_tables: HashMap<String, TransformTable>,
    color_results: HashMap<ColorQuery, Result<String>>,
    state_results: HashMap<StateQuery, Result<String>>,
}

#[derive(Clone, Copy, Debug)]
pub struct MetricBand {
    minimum: f64,
    preferred: Option<f64>,
    maximum: Option<f64>,
}

impl MetricBand {
    pub const fn floor(minimum: f64) -> Self {
        Self {
            minimum,
            preferred: None,
            maximum: None,
        }
    }

    pub const fn bounded(minimum: f64, preferred: f64, maximum: f64) -> Self {
        Self {
            minimum,
            preferred: Some(preferred),
            maximum: Some(maximum),
        }
    }

    pub const fn with_preference(minimum: f64, preferred: f64) -> Self {
        Self {
            minimum,
            preferred: Some(preferred),
            maximum: None,
        }
    }

    pub const fn minimum(self) -> f64 {
        self.minimum
    }

    pub const fn preferred(self) -> Option<f64> {
        self.preferred
    }

    pub const fn maximum(self) -> Option<f64> {
        self.maximum
    }

    fn validate(self, name: &str, validate: impl Fn(&str, f64) -> Result<()>) -> Result<()> {
        validate(&format!("{name} minimum"), self.minimum)?;
        if let Some(preferred) = self.preferred {
            validate(&format!("{name} preferred"), preferred)?;
            if preferred < self.minimum {
                return Err(Error::invalid(format!(
                    "{name} preferred cannot be below its minimum"
                )));
            }
        }
        if let Some(maximum) = self.maximum {
            validate(&format!("{name} maximum"), maximum)?;
            if maximum < self.minimum {
                return Err(Error::invalid(format!(
                    "{name} maximum cannot be below its minimum"
                )));
            }
            if self.preferred.is_some_and(|preferred| preferred > maximum) {
                return Err(Error::invalid(format!(
                    "{name} preferred cannot exceed its maximum"
                )));
            }
        }

        Ok(())
    }
}

#[derive(Clone, Copy)]
pub struct FitBounds {
    pub contrast: MetricBand,
    pub lower_lightness: f64,
    pub upper_lightness: f64,
    pub lower_chroma: f64,
    pub upper_chroma: f64,
    pub prefer_background: bool,
}

impl FitBounds {
    pub const fn new(contrast: MetricBand) -> Self {
        Self {
            contrast,
            lower_lightness: 0.0,
            upper_lightness: 1.0,
            lower_chroma: 0.0,
            upper_chroma: f64::INFINITY,
            prefer_background: false,
        }
    }

    fn validate(self) -> Result<()> {
        self.contrast.validate("color contrast", valid_contrast)?;
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

        Ok(())
    }
}

#[derive(Clone, Copy)]
pub(crate) struct StateFitRequest<'a> {
    pub(crate) backgrounds: &'a [String],
    pub(crate) contrast: MetricBand,
    pub(crate) delta_e: MetricBand,
    pub(crate) minimum_chroma: f64,
    pub(crate) references: &'a [(String, f64, f64)],
}

impl<'a> StateFitRequest<'a> {
    pub(crate) const fn new(
        backgrounds: &'a [String],
        contrast: MetricBand,
        delta_e: MetricBand,
    ) -> Self {
        Self {
            backgrounds,
            contrast,
            delta_e,
            minimum_chroma: 0.0,
            references: &[],
        }
    }

    pub(crate) const fn with_minimum_chroma(mut self, minimum_chroma: f64) -> Self {
        self.minimum_chroma = minimum_chroma;
        self
    }

    pub(crate) const fn with_references(mut self, references: &'a [(String, f64, f64)]) -> Self {
        self.references = references;
        self
    }

    fn validate(self) -> Result<()> {
        self.contrast.validate("state contrast", valid_contrast)?;
        self.delta_e.validate("state delta E", |name, value| {
            finite_at_least(name, value, 0.0)
        })?;
        finite_at_least("state minimum chroma", self.minimum_chroma, 0.0)?;
        for (_, reference_target, reference_delta) in self.references {
            valid_contrast("state reference contrast", *reference_target)?;
            finite_at_least("state reference delta E", *reference_delta, 0.0)?;
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

    pub const fn with_minimum_chroma(mut self, minimum_chroma: f64) -> Self {
        self.minimum_chroma = minimum_chroma;
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
    pub contrast: MetricBand,
    pub delta_e: MetricBand,
    pub runtime_state: Option<(f64, f64, f64)>,
    pub readable_foregrounds: &'a [(String, f64)],
    pub rendered_references: &'a [(String, f64, f64)],
    pub runtime_rendered_references: &'a [(String, f64, f64, f64)],
    pub prefer_source_fidelity: bool,
}

impl<'a> OverlayFitRequest<'a> {
    pub const fn new(backgrounds: &'a [String], contrast: MetricBand, delta_e: MetricBand) -> Self {
        Self {
            backgrounds,
            readability_backgrounds: &[],
            contrast,
            delta_e,
            runtime_state: None,
            readable_foregrounds: &[],
            rendered_references: &[],
            runtime_rendered_references: &[],
            prefer_source_fidelity: false,
        }
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
        self.contrast.validate("overlay contrast", valid_contrast)?;
        self.delta_e.validate("overlay delta E", |name, value| {
            finite_at_least(name, value, 0.0)
        })?;
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

type FillRank = [f64; 7];
#[derive(Clone)]
struct FillCandidate {
    emitted: Rgba32,
    rank: FillRank,
    source_chroma: f64,
}

fn fill_candidate_cmp(left: &FillCandidate, right: &FillCandidate) -> Ordering {
    rank_cmp(&left.rank, &right.rank).then_with(|| left.emitted.hex_cmp(right.emitted))
}

#[inline]
fn select_alpha_candidates<const KEEP_HIGHEST: bool>(
    alpha_values: &[u8],
    mut evaluate: impl FnMut(u8) -> Option<FillCandidate>,
) -> (Option<FillCandidate>, Option<FillCandidate>) {
    // Pair frontiers need the highest feasible alpha as well as the best one;
    // single-overlay searches compile without retaining that second candidate.
    let mut best = None;
    let mut highest = None;

    for &alpha in alpha_values {
        let Some(candidate) = evaluate(alpha) else {
            continue;
        };
        let replace = best
            .as_ref()
            .is_none_or(|best| fill_candidate_cmp(&candidate, best) == Ordering::Less);
        if !KEEP_HIGHEST {
            if replace {
                best = Some(candidate);
            }
            continue;
        }

        if replace {
            best = Some(candidate.clone());
        }
        highest = Some(candidate);
    }

    (best, highest)
}

struct FrontierCandidate {
    core: FillCandidate,
    rendered: Box<[ColorMetrics]>,
    cvd: OnceLock<Box<[CvdLabs]>>,
}

struct RenderedReference {
    color: ColorMetrics,
    minimum_contrast: f64,
    minimum_delta_e: f64,
}

struct RuntimeRenderedReference {
    color: ColorMetrics,
    minimum_contrast: f64,
    minimum_delta_e: f64,
    minimum_background_contrast: f64,
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
    rank_cmp(
        &frontier_rank_prefix(&left.core),
        &frontier_rank_prefix(&right.core),
    )
    .then_with(|| left.core.emitted.hex_cmp(right.core.emitted))
}

fn frontier_rank_prefix(candidate: &FillCandidate) -> [f64; 3] {
    if candidate.rank[0] <= 1e-12 && candidate.rank[1] <= 1e-12 {
        [candidate.rank[2], candidate.rank[3], candidate.rank[4]]
    } else {
        [candidate.rank[0], candidate.rank[1], candidate.rank[2]]
    }
}

fn combined_frontier_rank(left: &FrontierCandidate, right: &FrontierCandidate) -> [f64; 3] {
    let left = frontier_rank_prefix(&left.core);
    let right = frontier_rank_prefix(&right.core);
    [left[0] + right[0], left[1] + right[1], left[2] + right[2]]
}

struct PreparedFill {
    backgrounds: Vec<ColorMetrics>,
    readability_backgrounds: Vec<ColorMetrics>,
    contrast: MetricBand,
    delta_e: MetricBand,
    runtime_state: Option<(f64, f64, f64)>,
    readable_foregrounds: Vec<(ColorMetrics, f64)>,
    rendered_references: Vec<Vec<RenderedReference>>,
    runtime_rendered_references: Vec<Vec<RuntimeRenderedReference>>,
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
            .then_with(|| fill_candidate_cmp(left, right))
    });
    selected.extend(alpha_order.into_iter().take(limit / 2).cloned());

    selected.sort_by_key(|candidate| candidate.emitted);
    selected.dedup_by_key(|candidate| candidate.emitted);
    selected
}

fn overlay_fallback_frontier(candidates: &[FillCandidate], limit: usize) -> Vec<FillCandidate> {
    let ranked_count = limit / 2;
    let salient_count = limit - ranked_count;
    let mut selected = candidates
        .iter()
        .take(ranked_count)
        .cloned()
        .collect::<Vec<_>>();

    let mut salient = candidates.iter().collect::<Vec<_>>();
    salient.sort_by(|left, right| {
        left.rank[0]
            .total_cmp(&right.rank[0])
            .then_with(|| right.rank[1].total_cmp(&left.rank[1]))
            .then_with(|| right.source_chroma.total_cmp(&left.source_chroma))
            .then_with(|| right.emitted.alpha().cmp(&left.emitted.alpha()))
            .then_with(|| left.emitted.hex_cmp(right.emitted))
    });
    selected.extend(salient.into_iter().take(salient_count).cloned());

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

        let rendered_reference_specs = request
            .rendered_references
            .iter()
            .map(|(color, minimum_contrast, minimum_delta_e)| {
                Ok((
                    ColorMetrics::from_hex(color)?,
                    *minimum_contrast,
                    *minimum_delta_e,
                ))
            })
            .collect::<Result<Vec<_>>>()?;

        let rendered_references = backgrounds
            .iter()
            .map(|background| {
                rendered_reference_specs
                    .iter()
                    .map(
                        |(reference, minimum_contrast, minimum_delta_e)| RenderedReference {
                            color: ColorMetrics::blend(*background, reference.rgba.rgba()),
                            minimum_contrast: *minimum_contrast,
                            minimum_delta_e: *minimum_delta_e,
                        },
                    )
                    .collect()
            })
            .collect();

        let runtime_reference_specs = request
            .runtime_rendered_references
            .iter()
            .map(
                |(color, minimum_contrast, minimum_delta_e, background_contrast_step)| {
                    Ok((
                        ColorMetrics::from_hex(color)?,
                        *minimum_contrast,
                        *minimum_delta_e,
                        *background_contrast_step,
                    ))
                },
            )
            .collect::<Result<Vec<_>>>()?;

        let runtime_rendered_references = backgrounds
            .iter()
            .map(|background| {
                runtime_reference_specs
                    .iter()
                    .map(
                        |(
                            reference,
                            minimum_contrast,
                            minimum_delta_e,
                            background_contrast_step,
                        )| {
                            let reference = ColorMetrics::blend(*background, reference.rgba.rgba());
                            RuntimeRenderedReference {
                                color: reference,
                                minimum_contrast: *minimum_contrast,
                                minimum_delta_e: *minimum_delta_e,
                                minimum_background_contrast: reference.contrast(*background)
                                    + *background_contrast_step,
                            }
                        },
                    )
                    .collect()
            })
            .collect();

        Ok(Self {
            backgrounds,
            readability_backgrounds,
            contrast: request.contrast,
            delta_e: request.delta_e,
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
        let opaque_rgb = opaque.rgb24();
        let source_chroma = lab_chroma(opaque.lab);
        select_alpha_candidates::<true>(alpha_values, |alpha| {
            self.evaluate_alpha(opaque_rgb, alpha, distance, retention, source_chroma)
        })
    }

    fn best_for(
        &self,
        opaque: ColorMetrics,
        distance: f64,
        retention: f64,
        alpha_values: &[u8],
    ) -> Option<FillCandidate> {
        let opaque_rgb = opaque.rgb24();
        let source_chroma = lab_chroma(opaque.lab);
        select_alpha_candidates::<false>(alpha_values, |alpha| {
            self.evaluate_alpha(opaque_rgb, alpha, distance, retention, source_chroma)
        })
        .0
    }

    fn best_effort_and_highest_for(
        &self,
        opaque: ColorMetrics,
        distance: f64,
        retention: f64,
        alpha_values: &[u8],
    ) -> (FillCandidate, FillCandidate) {
        let opaque_rgb = opaque.rgb24();
        let source_chroma = lab_chroma(opaque.lab);
        let (best, highest) = select_alpha_candidates::<true>(alpha_values, |alpha| {
            Some(self.evaluate_alpha_best_effort(
                opaque_rgb,
                alpha,
                distance,
                retention,
                source_chroma,
            ))
        });
        (
            best.expect("validated alpha range must contain a candidate"),
            highest.expect("validated alpha range must contain a candidate"),
        )
    }

    fn best_effort_for(
        &self,
        opaque: ColorMetrics,
        distance: f64,
        retention: f64,
        alpha_values: &[u8],
    ) -> FillCandidate {
        let opaque_rgb = opaque.rgb24();
        let source_chroma = lab_chroma(opaque.lab);
        select_alpha_candidates::<false>(alpha_values, |alpha| {
            Some(self.evaluate_alpha_best_effort(
                opaque_rgb,
                alpha,
                distance,
                retention,
                source_chroma,
            ))
        })
        .0
        .expect("validated alpha range must contain a candidate")
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
        let mut preference_distance = 0.0;

        for (background_index, background) in self.backgrounds.iter().enumerate() {
            let rendered_prepared = ColorMetrics::blend_rgb24(*background, opaque_rgb, alpha);
            if !rendered_prepared.contrast_at_least(*background, self.contrast.minimum() - 1e-12)
                || self.rendered_references[background_index]
                    .iter()
                    .any(|reference| {
                        !rendered_prepared
                            .contrast_at_least(reference.color, reference.minimum_contrast - 1e-12)
                    })
                || self
                    .readable_foregrounds
                    .iter()
                    .any(|(foreground, target)| {
                        !rendered_prepared.contrast_at_least(*foreground, *target - 1e-12)
                    })
            {
                return None;
            }

            let ratio = rendered_prepared.contrast(*background);
            if self
                .contrast
                .maximum()
                .is_some_and(|maximum| ratio > maximum + 1e-12)
            {
                return None;
            }
            let runtime = match self.runtime_state {
                Some((runtime_opacity, runtime_target, minimum_distance)) => {
                    let runtime_alpha = (f64::from(alpha) * runtime_opacity + 0.5).floor() as u8;
                    let prepared =
                        ColorMetrics::blend_rgb24(*background, opaque_rgb, runtime_alpha);
                    if !prepared.contrast_at_least(*background, runtime_target - 1e-12)
                        || self.runtime_rendered_references[background_index]
                            .iter()
                            .any(|reference| {
                                !prepared.contrast_at_least(
                                    reference.color,
                                    reference.minimum_contrast - 1e-12,
                                ) || !prepared.contrast_at_least(
                                    *background,
                                    reference.minimum_background_contrast - 1e-12,
                                )
                            })
                        || self
                            .readable_foregrounds
                            .iter()
                            .any(|(foreground, target)| {
                                !prepared.contrast_at_least(*foreground, *target - 1e-12)
                            })
                    {
                        return None;
                    }
                    let runtime_ratio = prepared.contrast(*background);
                    Some((
                        prepared,
                        runtime_ratio + self.contrast.minimum() - runtime_target,
                        minimum_distance,
                    ))
                }
                None => None,
            };

            let rendered = rendered_prepared.metrics();
            let rendered_distance = rendered.delta_e(*background);
            if rendered_distance < self.delta_e.minimum() - 1e-12
                || self
                    .delta_e
                    .maximum()
                    .is_some_and(|maximum| rendered_distance > maximum + 1e-12)
                || self.rendered_references[background_index]
                    .iter()
                    .any(|reference| {
                        rendered.delta_e(reference.color) < reference.minimum_delta_e - 1e-12
                    })
            {
                return None;
            }

            minimum_ratio = minimum_ratio.min(ratio);
            overshoot += (ratio - self.contrast.minimum()).max(0.0);
            final_distance += rendered_distance;
            if let Some(preferred) = self.contrast.preferred() {
                preference_distance += (ratio - preferred).abs() / preferred;
            }
            if let Some(preferred) = self.delta_e.preferred() {
                preference_distance += (rendered_distance - preferred).abs();
            }
            if let Some((prepared, adjusted_ratio, minimum_distance)) = runtime {
                let runtime_metrics = prepared.metrics();
                let runtime_distance = runtime_metrics.delta_e(*background);
                if runtime_distance < minimum_distance - 1e-12
                    || self.runtime_rendered_references[background_index]
                        .iter()
                        .any(|reference| {
                            runtime_metrics.delta_e(reference.color)
                                < reference.minimum_delta_e - 1e-12
                        })
                {
                    return None;
                }
                minimum_ratio = minimum_ratio.min(adjusted_ratio);
                overshoot += (adjusted_ratio - self.contrast.minimum()).max(0.0);
                final_distance += runtime_distance;
            }
        }

        for background in &self.readability_backgrounds {
            let rendered = ColorMetrics::blend_rgb24(*background, opaque_rgb, alpha);
            if self
                .readable_foregrounds
                .iter()
                .any(|(foreground, target)| {
                    !rendered.contrast_at_least(*foreground, *target - 1e-12)
                })
            {
                return None;
            }
            if let Some((runtime_opacity, _, _)) = self.runtime_state {
                let runtime_alpha = (f64::from(alpha) * runtime_opacity + 0.5).floor() as u8;
                let runtime = ColorMetrics::blend_rgb24(*background, opaque_rgb, runtime_alpha);
                if self
                    .readable_foregrounds
                    .iter()
                    .any(|(foreground, target)| {
                        !runtime.contrast_at_least(*foreground, *target - 1e-12)
                    })
                {
                    return None;
                }
            }
        }

        if minimum_ratio < self.contrast.minimum() - 1e-12 {
            return None;
        }

        let candidate = Rgba32::from_rgb_alpha(opaque_rgb, alpha);
        let presentation_distance =
            if self.contrast.preferred().is_some() || self.delta_e.preferred().is_some() {
                preference_distance
            } else {
                final_distance
            };
        let rank = if self.prefer_source_fidelity {
            [
                0.0,
                0.0,
                distance,
                overshoot,
                presentation_distance,
                -retention,
                -f64::from(alpha) / 255.0,
            ]
        } else {
            [
                0.0,
                0.0,
                presentation_distance,
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

    fn evaluate_alpha_best_effort(
        &self,
        opaque_rgb: Rgb24,
        alpha: u8,
        distance: f64,
        retention: f64,
        source_chroma: f64,
    ) -> FillCandidate {
        let mut hard_deficit = 0.0;
        let mut ceiling_excess = 0.0;
        let mut overshoot = 0.0;
        let mut final_distance = 0.0;
        let mut preference_distance = 0.0;

        for (background_index, background) in self.backgrounds.iter().enumerate() {
            let rendered_prepared = ColorMetrics::blend_rgb24(*background, opaque_rgb, alpha);
            let ratio = rendered_prepared.contrast(*background);
            hard_deficit += shortfall(ratio, self.contrast.minimum());
            if let Some(maximum) = self.contrast.maximum() {
                ceiling_excess += excess(ratio, maximum);
            }
            hard_deficit += self.rendered_references[background_index]
                .iter()
                .map(|reference| {
                    shortfall(
                        rendered_prepared.contrast(reference.color),
                        reference.minimum_contrast,
                    )
                })
                .sum::<f64>();
            hard_deficit += self
                .readable_foregrounds
                .iter()
                .map(|(foreground, target)| {
                    shortfall(rendered_prepared.contrast(*foreground), *target)
                })
                .sum::<f64>();

            let runtime = match self.runtime_state {
                Some((runtime_opacity, runtime_target, minimum_distance)) => {
                    let runtime_alpha = (f64::from(alpha) * runtime_opacity + 0.5).floor() as u8;
                    let prepared =
                        ColorMetrics::blend_rgb24(*background, opaque_rgb, runtime_alpha);
                    let runtime_ratio = prepared.contrast(*background);
                    hard_deficit += shortfall(runtime_ratio, runtime_target);
                    hard_deficit += self.runtime_rendered_references[background_index]
                        .iter()
                        .map(|reference| {
                            shortfall(
                                prepared.contrast(reference.color),
                                reference.minimum_contrast,
                            ) + shortfall(runtime_ratio, reference.minimum_background_contrast)
                        })
                        .sum::<f64>();
                    hard_deficit += self
                        .readable_foregrounds
                        .iter()
                        .map(|(foreground, target)| {
                            shortfall(prepared.contrast(*foreground), *target)
                        })
                        .sum::<f64>();

                    Some((
                        prepared,
                        runtime_ratio + self.contrast.minimum() - runtime_target,
                        minimum_distance,
                    ))
                }
                None => None,
            };

            let rendered = rendered_prepared.metrics();
            let rendered_distance = rendered.delta_e(*background);
            hard_deficit += shortfall(rendered_distance, self.delta_e.minimum());
            if let Some(maximum) = self.delta_e.maximum() {
                ceiling_excess += excess(rendered_distance, maximum);
            }
            hard_deficit += self.rendered_references[background_index]
                .iter()
                .map(|reference| {
                    shortfall(rendered.delta_e(reference.color), reference.minimum_delta_e)
                })
                .sum::<f64>();
            overshoot += (ratio - self.contrast.minimum()).max(0.0);
            final_distance += rendered_distance;
            if let Some(preferred) = self.contrast.preferred() {
                preference_distance += (ratio - preferred).abs() / preferred;
            }
            if let Some(preferred) = self.delta_e.preferred() {
                preference_distance += (rendered_distance - preferred).abs();
            }

            if let Some((prepared, adjusted_ratio, minimum_distance)) = runtime {
                let runtime_metrics = prepared.metrics();
                let runtime_distance = runtime_metrics.delta_e(*background);
                hard_deficit += shortfall(runtime_distance, minimum_distance);
                hard_deficit += self.runtime_rendered_references[background_index]
                    .iter()
                    .map(|reference| {
                        shortfall(
                            runtime_metrics.delta_e(reference.color),
                            reference.minimum_delta_e,
                        )
                    })
                    .sum::<f64>();
                overshoot += (adjusted_ratio - self.contrast.minimum()).max(0.0);
                final_distance += runtime_distance;
            }
        }

        for background in &self.readability_backgrounds {
            let rendered = ColorMetrics::blend_rgb24(*background, opaque_rgb, alpha);
            hard_deficit += self
                .readable_foregrounds
                .iter()
                .map(|(foreground, target)| shortfall(rendered.contrast(*foreground), *target))
                .sum::<f64>();
            if let Some((runtime_opacity, _, _)) = self.runtime_state {
                let runtime_alpha = (f64::from(alpha) * runtime_opacity + 0.5).floor() as u8;
                let runtime = ColorMetrics::blend_rgb24(*background, opaque_rgb, runtime_alpha);
                hard_deficit += self
                    .readable_foregrounds
                    .iter()
                    .map(|(foreground, target)| shortfall(runtime.contrast(*foreground), *target))
                    .sum::<f64>();
            }
        }

        let candidate = Rgba32::from_rgb_alpha(opaque_rgb, alpha);
        let presentation_distance =
            if self.contrast.preferred().is_some() || self.delta_e.preferred().is_some() {
                preference_distance
            } else {
                final_distance
            };
        let rank = if self.prefer_source_fidelity {
            [
                hard_deficit,
                ceiling_excess,
                distance,
                presentation_distance,
                overshoot,
                -retention,
                -f64::from(alpha) / 255.0,
            ]
        } else {
            [
                hard_deficit,
                ceiling_excess,
                presentation_distance,
                overshoot,
                distance,
                -retention,
                -f64::from(alpha) / 255.0,
            ]
        };

        FillCandidate {
            emitted: candidate,
            rank,
            source_chroma,
        }
    }
}

#[derive(Clone)]
struct PairCandidate {
    source_index: usize,
    background_distance: f64,
    constraint_deficit: f64,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum PairCandidateMode {
    Feasible,
    BestEffort,
}

struct PairCandidateInputs {
    backgrounds: Vec<ColorMetrics>,
    table: TransformTable,
}

struct DistinctCandidateContext<'a> {
    backgrounds: &'a [ColorMetrics],
    chosen: &'a [(ColorMetrics, CvdLabs)],
    target: f64,
    minimum_normal_delta_e: f64,
    minimum_cvd_delta_e: f64,
}

impl DistinctCandidateContext<'_> {
    fn feasible_rank(
        &self,
        metrics: ColorMetrics,
        transform: f64,
        retention: f64,
    ) -> Option<[f64; 3]> {
        let mut overshoot = 0.0;
        let mut contrast_deficit = 0.0;
        for background in self.backgrounds {
            let ratio = metrics.contrast(*background);
            contrast_deficit += shortfall(ratio, self.target);
            if contrast_deficit > 1e-12 {
                return None;
            }
            overshoot += (ratio - self.target).max(0.0);
        }

        let normal = self
            .chosen
            .iter()
            .map(|(reference, _)| metrics.delta_e(*reference))
            .fold(f64::INFINITY, f64::min);
        if normal < self.minimum_normal_delta_e - 1e-12 {
            return None;
        }

        let candidate_cvd = cvd_labs(metrics.rgba.rgba());
        let cvd = self
            .chosen
            .iter()
            .map(|(reference, reference_cvd)| {
                cvd_distance_facts(&candidate_cvd, reference_cvd, metrics.delta_e(*reference))
            })
            .fold(f64::INFINITY, f64::min);
        (cvd >= self.minimum_cvd_delta_e - 1e-12).then_some([transform, overshoot, -retention])
    }

    fn best_effort_rank(&self, metrics: ColorMetrics, transform: f64, retention: f64) -> [f64; 6] {
        let mut overshoot = 0.0;
        let mut contrast_deficit = 0.0;
        for background in self.backgrounds {
            let ratio = metrics.contrast(*background);
            contrast_deficit += shortfall(ratio, self.target);
            overshoot += (ratio - self.target).max(0.0);
        }

        let candidate_cvd = cvd_labs(metrics.rgba.rgba());
        let normal = self
            .chosen
            .iter()
            .map(|(reference, _)| metrics.delta_e(*reference))
            .fold(f64::INFINITY, f64::min);
        let cvd = self
            .chosen
            .iter()
            .map(|(reference, reference_cvd)| {
                cvd_distance_facts(&candidate_cvd, reference_cvd, metrics.delta_e(*reference))
            })
            .fold(f64::INFINITY, f64::min);
        [
            contrast_deficit,
            -normal.min(self.minimum_normal_delta_e),
            -cvd.min(self.minimum_cvd_delta_e),
            transform,
            overshoot,
            -retention,
        ]
    }
}

struct StateReference {
    color: ColorMetrics,
    minimum_contrast: f64,
    minimum_delta_e: f64,
}

struct StateCandidateContext<'a> {
    backgrounds: &'a [ColorMetrics],
    references: &'a [StateReference],
    contrast: MetricBand,
    delta_e: MetricBand,
    minimum_chroma: f64,
}

impl StateCandidateContext<'_> {
    fn feasible_rank(&self, candidate: &TransformCandidate) -> Option<[f64; 6]> {
        if self.minimum_chroma > 0.0
            && lab_chroma(candidate.metrics.lab) < self.minimum_chroma - 1e-12
        {
            return None;
        }

        let mut final_distance = 0.0;
        let mut overshoot = 0.0;
        let mut deficit = 0.0;
        let mut preference_distance = 0.0;

        for background in self.backgrounds {
            let ratio = candidate.metrics.contrast(*background);
            let distance = candidate.metrics.delta_e(*background);
            deficit += shortfall(ratio, self.contrast.minimum())
                + shortfall(distance, self.delta_e.minimum());
            if let Some(maximum) = self.contrast.maximum() {
                deficit += excess(ratio, maximum);
            }
            if let Some(maximum) = self.delta_e.maximum() {
                deficit += excess(distance, maximum);
            }
            if deficit > 1e-12 {
                return None;
            }
            overshoot += (ratio - self.contrast.minimum()).max(0.0);
            final_distance += distance;
            if let Some(preferred) = self.contrast.preferred() {
                preference_distance += (ratio - preferred).abs() / preferred;
            }
            if let Some(preferred) = self.delta_e.preferred() {
                preference_distance += (distance - preferred).abs();
            }
        }

        for reference in self.references {
            let ratio = candidate.metrics.contrast(reference.color);
            deficit += shortfall(ratio, reference.minimum_contrast)
                + shortfall(
                    candidate.metrics.delta_e(reference.color),
                    reference.minimum_delta_e,
                );
            if deficit > 1e-12 {
                return None;
            }
            overshoot += (ratio - reference.minimum_contrast).max(0.0);
        }

        if self.contrast.preferred().is_some() || self.delta_e.preferred().is_some() {
            Some([
                0.0,
                0.0,
                preference_distance,
                final_distance,
                candidate.distance,
                -candidate.retention,
            ])
        } else {
            Some([
                0.0,
                0.0,
                final_distance,
                overshoot,
                candidate.distance,
                -candidate.retention,
            ])
        }
    }

    fn best_effort_rank(&self, candidate: &TransformCandidate) -> [f64; 6] {
        let mut overshoot = 0.0;
        let mut hard_deficit = if self.minimum_chroma > 0.0 {
            shortfall(lab_chroma(candidate.metrics.lab), self.minimum_chroma)
        } else {
            0.0
        };
        let mut ceiling_excess = 0.0;
        let mut preference_distance = 0.0;

        for background in self.backgrounds {
            let ratio = candidate.metrics.contrast(*background);
            let distance = candidate.metrics.delta_e(*background);
            hard_deficit += shortfall(ratio, self.contrast.minimum())
                + shortfall(distance, self.delta_e.minimum());
            if let Some(maximum) = self.contrast.maximum() {
                ceiling_excess += excess(ratio, maximum);
            }
            if let Some(maximum) = self.delta_e.maximum() {
                ceiling_excess += excess(distance, maximum);
            }
            overshoot += (ratio - self.contrast.minimum()).max(0.0);
            if let Some(preferred) = self.contrast.preferred() {
                preference_distance += (ratio - preferred).abs() / preferred;
            }
            if let Some(preferred) = self.delta_e.preferred() {
                preference_distance += (distance - preferred).abs();
            }
        }

        for reference in self.references {
            let ratio = candidate.metrics.contrast(reference.color);
            hard_deficit += shortfall(ratio, reference.minimum_contrast)
                + shortfall(
                    candidate.metrics.delta_e(reference.color),
                    reference.minimum_delta_e,
                );
            overshoot += (ratio - reference.minimum_contrast).max(0.0);
        }

        let preference =
            if self.contrast.preferred().is_some() || self.delta_e.preferred().is_some() {
                preference_distance
            } else {
                candidate.distance
            };
        [
            hard_deficit,
            ceiling_excess,
            preference,
            overshoot,
            candidate.distance,
            -candidate.retention,
        ]
    }
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

fn pair_best_effort_frontier(
    candidates: &[PairCandidate],
    table: &TransformTable,
) -> Vec<PairCandidate> {
    if candidates.len() <= PAIR_BEST_EFFORT_FRONTIER_LIMIT {
        return candidates.to_vec();
    }

    let mut selected = vec![false; candidates.len()];
    let mut minimum_separation = vec![f64::INFINITY; candidates.len()];
    let mut frontier = Vec::with_capacity(PAIR_BEST_EFFORT_FRONTIER_LIMIT);
    let fidelity_prefix = 64.min(PAIR_BEST_EFFORT_FRONTIER_LIMIT);

    let add = |position: usize,
               selected: &mut [bool],
               minimum_separation: &mut [f64],
               frontier: &mut Vec<PairCandidate>| {
        selected[position] = true;
        let chosen = &candidates[position];
        let chosen_metrics = table.candidates[chosen.source_index].metrics;
        frontier.push(chosen.clone());
        for (index, candidate) in candidates.iter().enumerate() {
            if selected[index] {
                continue;
            }
            let metrics = table.candidates[candidate.source_index].metrics;
            let normal_delta = chosen_metrics.delta_e(metrics);
            let cvd_delta = cvd_distance_precomputed(chosen, table, candidate, table, normal_delta);
            minimum_separation[index] = minimum_separation[index].min(normal_delta.min(cvd_delta));
        }
    };

    for position in 0..fidelity_prefix {
        add(
            position,
            &mut selected,
            &mut minimum_separation,
            &mut frontier,
        );
    }
    while frontier.len() < PAIR_BEST_EFFORT_FRONTIER_LIMIT {
        let position = (0..candidates.len())
            .filter(|index| !selected[*index])
            .max_by(|left, right| {
                minimum_separation[*left]
                    .total_cmp(&minimum_separation[*right])
                    .then_with(|| right.cmp(left))
            })
            .expect("a truncated pair frontier must have unselected candidates");
        add(
            position,
            &mut selected,
            &mut minimum_separation,
            &mut frontier,
        );
    }

    frontier
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

fn shortfall(actual: f64, target: f64) -> f64 {
    ((target - actual) / target.max(1e-12)).max(0.0)
}

fn excess(actual: f64, maximum: f64) -> f64 {
    ((actual - maximum) / actual.max(1e-12)).max(0.0)
}

#[inline]
fn color_contrasts_satisfy(
    candidate: ColorMetrics,
    backgrounds: &[ColorMetrics],
    per_background: PerBackgroundContrast<&[f64]>,
    band: MetricBand,
    mut observe: impl FnMut(f64, f64),
) -> bool {
    let minimum = band.minimum();
    let maximum = band.maximum();
    // Most fits have no per-background override. Specialized loops avoid an
    // optional lookup for every candidate in this cold-generation hot path.
    match per_background {
        PerBackgroundContrast::None => {
            for background in backgrounds {
                let contrast = candidate.contrast(*background);
                if !candidate.contrast_at_least(*background, minimum - 1e-12)
                    || maximum.is_some_and(|maximum| contrast > maximum + 1e-12)
                {
                    return false;
                }
                observe(contrast, minimum);
            }
        }
        PerBackgroundContrast::Ceilings(ceilings) => {
            for (background, ceiling) in backgrounds.iter().zip(ceilings) {
                let contrast = candidate.contrast(*background);
                if !candidate.contrast_at_least(*background, minimum - 1e-12)
                    || maximum.is_some_and(|maximum| contrast > maximum + 1e-12)
                    || contrast > *ceiling + 1e-12
                {
                    return false;
                }
                observe(contrast, minimum);
            }
        }
        PerBackgroundContrast::Floors(floors) => {
            for (background, floor) in backgrounds.iter().zip(floors) {
                let floor = floor.max(minimum);
                let contrast = candidate.contrast(*background);
                if !candidate.contrast_at_least(*background, floor - 1e-12)
                    || maximum.is_some_and(|maximum| contrast > maximum + 1e-12)
                {
                    return false;
                }
                observe(contrast, floor);
            }
        }
    }

    true
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
                .any(|candidate| candidate.metrics.rgb24() == Rgb24::BLACK),
            "transform tables must contain the black endpoint"
        );
        assert!(
            table
                .iter()
                .any(|candidate| candidate.metrics.rgb24() == Rgb24::WHITE),
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

    fn best_effort_color(
        &mut self,
        seed: &str,
        backgrounds: &[ColorMetrics],
        preference_backgrounds: &[ColorMetrics],
        per_background: PerBackgroundContrast<&[f64]>,
        avoid: &[ColorMetrics],
        bounds: FitBounds,
    ) -> Result<String> {
        let source = opaque_color_metrics(seed, "fit_color seed")?;
        let source_chroma = lab_chroma(source.lab);
        let preferred_log = bounds.contrast.preferred().map(f64::ln);
        let background_lightness = preference_backgrounds
            .iter()
            .map(|metrics| metrics.lab[0])
            .sum::<f64>()
            / preference_backgrounds.len() as f64;
        let mut best: Option<(Rgb24, [f64; 5])> = None;
        let mut consider = |metrics: ColorMetrics, distance: f64, retention: f64| {
            let mut hard_deficit = shortfall(metrics.lab[0], bounds.lower_lightness)
                + shortfall(bounds.upper_lightness, metrics.lab[0]);
            if bounds.lower_chroma > 0.0 || bounds.upper_chroma.is_finite() {
                let chroma = lab_chroma(metrics.lab);
                hard_deficit += shortfall(chroma, bounds.lower_chroma);
                if bounds.upper_chroma.is_finite() {
                    hard_deficit += shortfall(bounds.upper_chroma, chroma);
                }
            }
            hard_deficit += match per_background {
                PerBackgroundContrast::Floors(floors) => backgrounds
                    .iter()
                    .zip(floors)
                    .map(|(background, floor)| {
                        shortfall(
                            metrics.contrast(*background),
                            floor.max(bounds.contrast.minimum()),
                        )
                    })
                    .sum::<f64>(),
                PerBackgroundContrast::None | PerBackgroundContrast::Ceilings(_) => backgrounds
                    .iter()
                    .map(|background| {
                        shortfall(metrics.contrast(*background), bounds.contrast.minimum())
                    })
                    .sum::<f64>(),
            };
            if let PerBackgroundContrast::Ceilings(ceilings) = per_background {
                hard_deficit += backgrounds
                    .iter()
                    .zip(ceilings)
                    .map(|(background, maximum)| excess(metrics.contrast(*background), *maximum))
                    .sum::<f64>();
            }
            hard_deficit += avoid
                .iter()
                .map(|other| {
                    shortfall((metrics.lab[0] - other.lab[0]).abs(), 0.05)
                        + shortfall(metrics.delta_e(*other), 0.10)
                })
                .sum::<f64>();
            let ceiling_excess = bounds.contrast.maximum().map_or(0.0, |maximum| {
                backgrounds
                    .iter()
                    .map(|background| excess(metrics.contrast(*background), maximum))
                    .sum::<f64>()
            });
            let preference = if let Some(preferred_log) = preferred_log {
                let mean_log_contrast = preference_backgrounds
                    .iter()
                    .map(|background| metrics.contrast(*background).ln())
                    .sum::<f64>()
                    / preference_backgrounds.len() as f64;
                (mean_log_contrast - preferred_log).abs()
            } else if bounds.prefer_background {
                (metrics.lab[0] - background_lightness).abs()
            } else {
                distance
            };
            let rank = [
                hard_deficit,
                ceiling_excess,
                preference,
                distance,
                -retention,
            ];
            let color = metrics.rgb24();
            if best.as_ref().is_none_or(|(best_color, best_rank)| {
                rank_cmp(&rank, best_rank).then_with(|| color.cmp(best_color)) == Ordering::Less
            }) {
                best = Some((color, rank));
            }
        };
        consider(
            source,
            0.0,
            lab_chroma(source.lab) / source_chroma.max(1e-12),
        );
        for candidate in self.transform_table(seed)?.candidates.iter() {
            consider(candidate.metrics, candidate.distance, candidate.retention);
        }
        Ok(best
            .expect("validated color search must have at least one candidate")
            .0
            .hex())
    }

    pub fn fit_color(&mut self, seed: &str, backgrounds: &[String], target: f64) -> Result<String> {
        self.fit_color_bounded(
            seed,
            backgrounds,
            &[],
            FitBounds::new(MetricBand::floor(target)),
        )
    }

    pub fn fit_color_avoiding(
        &mut self,
        seed: &str,
        backgrounds: &[String],
        target: f64,
        avoid: &[String],
    ) -> Result<String> {
        self.fit_color_bounded(
            seed,
            backgrounds,
            avoid,
            FitBounds::new(MetricBand::floor(target)),
        )
    }

    fn prepare_pair_candidate_inputs(
        &mut self,
        seed: &str,
        backgrounds: &[String],
    ) -> Result<PairCandidateInputs> {
        Ok(PairCandidateInputs {
            backgrounds: backgrounds
                .iter()
                .map(|background| ColorMetrics::from_hex(background))
                .collect::<Result<Vec<_>>>()?,
            table: self.transform_table(seed)?,
        })
    }

    fn collect_pair_candidates(
        inputs: &PairCandidateInputs,
        readable_foregrounds: &[(ColorMetrics, f64)],
        constraints: PairConstraints,
        mode: PairCandidateMode,
    ) -> Vec<PairCandidate> {
        let mut candidates = Vec::new();

        for (source_index, candidate) in inputs.table.candidates.iter().enumerate() {
            let metrics = candidate.metrics;
            if mode == PairCandidateMode::Feasible
                && (shortfall(lab_chroma(metrics.lab), constraints.minimum_chroma) > 1e-12
                    || inputs.backgrounds.iter().any(|background| {
                        shortfall(
                            metrics.contrast(*background),
                            constraints.foreground_contrast,
                        ) > 1e-12
                    })
                    || readable_foregrounds.iter().any(|(foreground, target)| {
                        shortfall(metrics.contrast(*foreground), *target) > 1e-12
                    }))
            {
                continue;
            }

            let constraint_deficit = shortfall(lab_chroma(metrics.lab), constraints.minimum_chroma)
                + inputs
                    .backgrounds
                    .iter()
                    .map(|background| {
                        shortfall(
                            metrics.contrast(*background),
                            constraints.foreground_contrast,
                        )
                    })
                    .sum::<f64>()
                + readable_foregrounds
                    .iter()
                    .map(|(foreground, target)| shortfall(metrics.contrast(*foreground), *target))
                    .sum::<f64>();
            if mode == PairCandidateMode::Feasible && constraint_deficit > 1e-12 {
                continue;
            }

            let background_distance = if constraints.prefer_background {
                inputs
                    .backgrounds
                    .iter()
                    .map(|background| metrics.delta_e(*background))
                    .sum()
            } else {
                0.0
            };
            candidates.push(PairCandidate {
                source_index,
                background_distance,
                constraint_deficit,
            });
        }

        candidates.sort_by(|left, right| {
            let left_facts = &inputs.table.candidates[left.source_index];
            let right_facts = &inputs.table.candidates[right.source_index];
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
            left.constraint_deficit
                .total_cmp(&right.constraint_deficit)
                .then_with(|| left_primary.total_cmp(&right_primary))
                .then_with(|| left_facts.distance.total_cmp(&right_facts.distance))
                .then_with(|| right_facts.retention.total_cmp(&left_facts.retention))
                .then_with(|| left_facts.metrics.rgb24().cmp(&right_facts.metrics.rgb24()))
        });

        if mode == PairCandidateMode::BestEffort {
            let minimum_deficit = candidates
                .first()
                .expect("transform table must contain pair candidates")
                .constraint_deficit;
            candidates.retain(|candidate| candidate.constraint_deficit <= minimum_deficit + 1e-12);
        }

        candidates
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

        let readable_metrics = readable_foregrounds
            .iter()
            .enumerate()
            .map(|(index, (foreground, target))| {
                let foreground = ColorMetrics::from_hex(foreground).map_err(|error| {
                    error.context(format!("readable_foregrounds[{index}].foreground"))
                })?;
                valid_contrast(&format!("readable_foregrounds[{index}].contrast"), *target)?;
                Ok((foreground, *target))
            })
            .collect::<Result<Vec<_>>>()?;

        let first_inputs = self.prepare_pair_candidate_inputs(first_seed, first_backgrounds)?;
        let second_inputs = self.prepare_pair_candidate_inputs(second_seed, second_backgrounds)?;
        let first = Self::collect_pair_candidates(
            &first_inputs,
            &readable_metrics,
            constraints,
            PairCandidateMode::Feasible,
        );
        let second = Self::collect_pair_candidates(
            &second_inputs,
            &readable_metrics,
            constraints,
            PairCandidateMode::Feasible,
        );
        let first_table = &first_inputs.table;
        let second_table = &second_inputs.table;

        let mut best: Option<([Rgb24; 2], [f64; 4])> = None;
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
                if pair_contrast < constraints.pair_contrast - 1e-12
                    || normal_delta < constraints.normal_delta - 1e-12
                    || (first_facts.metrics.lab[0] - second_facts.metrics.lab[0]).abs()
                        < constraints.lightness_delta - 1e-12
                {
                    continue;
                }
                let cvd_delta = cvd_distance_precomputed(
                    first_candidate,
                    first_table,
                    second_candidate,
                    second_table,
                    normal_delta,
                );
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

        if let Some((colors, _)) = best {
            return Ok([colors[0].hex(), colors[1].hex()]);
        }

        // Preserve the most source-faithful candidates, then cover the remaining
        // normal- and color-vision space before evaluating the combined deficit.
        let first_effort = Self::collect_pair_candidates(
            &first_inputs,
            &readable_metrics,
            constraints,
            PairCandidateMode::BestEffort,
        );
        let second_effort = Self::collect_pair_candidates(
            &second_inputs,
            &readable_metrics,
            constraints,
            PairCandidateMode::BestEffort,
        );
        let first_effort = pair_best_effort_frontier(&first_effort, first_table);
        let second_effort = pair_best_effort_frontier(&second_effort, second_table);
        let mut best_effort: Option<([Rgb24; 2], [f64; 5])> = None;
        for first_candidate in &first_effort {
            let first_facts = &first_table.candidates[first_candidate.source_index];
            for second_candidate in &second_effort {
                let second_facts = &second_table.candidates[second_candidate.source_index];
                let pair_contrast = first_facts.metrics.contrast(second_facts.metrics);
                let normal_delta = first_facts.metrics.delta_e(second_facts.metrics);
                let cvd_delta = cvd_distance_precomputed(
                    first_candidate,
                    first_table,
                    second_candidate,
                    second_table,
                    normal_delta,
                );
                let lightness_delta =
                    (first_facts.metrics.lab[0] - second_facts.metrics.lab[0]).abs();
                let alternative_deficit =
                    constraints
                        .separation_alternative
                        .map_or(0.0, |(contrast, normal, cvd)| {
                            shortfall(pair_contrast, contrast)
                                .min(shortfall(normal_delta, normal) + shortfall(cvd_delta, cvd))
                        });
                let pair_deficit = shortfall(pair_contrast, constraints.pair_contrast)
                    + shortfall(normal_delta, constraints.normal_delta)
                    + shortfall(cvd_delta, constraints.cvd_delta)
                    + shortfall(lightness_delta, constraints.lightness_delta)
                    + alternative_deficit;
                let transform = first_facts.distance + second_facts.distance;
                let background_distance =
                    first_candidate.background_distance + second_candidate.background_distance;
                let primary = if constraints.prefer_background {
                    background_distance
                } else {
                    transform
                };
                let rank = [
                    first_candidate.constraint_deficit + second_candidate.constraint_deficit,
                    pair_deficit,
                    primary,
                    transform,
                    -(first_facts.retention + second_facts.retention),
                ];
                let colors = [first_facts.metrics.rgb24(), second_facts.metrics.rgb24()];
                if best_effort.as_ref().is_none_or(|(best_colors, best_rank)| {
                    rank_cmp(&rank, best_rank).then_with(|| colors.cmp(best_colors))
                        == Ordering::Less
                }) {
                    best_effort = Some((colors, rank));
                }
            }
        }
        let colors = best_effort
            .expect("validated pair search must have candidate colors")
            .0;

        Ok([colors[0].hex(), colors[1].hex()])
    }

    pub fn fit_color_bounded(
        &mut self,
        seed: &str,
        backgrounds: &[String],
        avoid: &[String],
        bounds: FitBounds,
    ) -> Result<String> {
        self.fit_color_bounded_with_preference_backgrounds(
            seed,
            backgrounds,
            backgrounds,
            avoid,
            bounds,
        )
    }

    pub fn fit_color_bounded_with_preference_backgrounds(
        &mut self,
        seed: &str,
        backgrounds: &[String],
        preference_backgrounds: &[String],
        avoid: &[String],
        bounds: FitBounds,
    ) -> Result<String> {
        self.fit_color_bounded_with_per_background_contrast(
            seed,
            backgrounds,
            preference_backgrounds,
            PerBackgroundContrast::None,
            avoid,
            bounds,
        )
    }

    pub(crate) fn fit_color_bounded_with_contrast_ceilings(
        &mut self,
        seed: &str,
        backgrounds: &[String],
        preference_backgrounds: &[String],
        contrast_ceilings: &[f64],
        avoid: &[String],
        bounds: FitBounds,
    ) -> Result<String> {
        self.fit_color_bounded_with_per_background_contrast(
            seed,
            backgrounds,
            preference_backgrounds,
            PerBackgroundContrast::ceilings(contrast_ceilings),
            avoid,
            bounds,
        )
    }

    pub(crate) fn fit_color_bounded_with_contrast_floors(
        &mut self,
        seed: &str,
        backgrounds: &[String],
        preference_backgrounds: &[String],
        contrast_floors: &[f64],
        avoid: &[String],
        bounds: FitBounds,
    ) -> Result<String> {
        self.fit_color_bounded_with_per_background_contrast(
            seed,
            backgrounds,
            preference_backgrounds,
            PerBackgroundContrast::floors(contrast_floors),
            avoid,
            bounds,
        )
    }

    fn fit_color_bounded_with_per_background_contrast(
        &mut self,
        seed: &str,
        backgrounds: &[String],
        preference_backgrounds: &[String],
        per_background: PerBackgroundContrast<&[f64]>,
        avoid: &[String],
        bounds: FitBounds,
    ) -> Result<String> {
        bounds.validate()?;
        let validate_values = |values: &[f64], kind: &str| -> Result<()> {
            if values.len() != backgrounds.len() {
                return Err(Error::invalid(format!(
                    "contrast {kind}s must match the background count"
                )));
            }
            let label = format!("color contrast {kind}");
            for value in values {
                valid_contrast(&label, *value)?;
            }
            Ok(())
        };
        match per_background {
            PerBackgroundContrast::None => Ok(()),
            PerBackgroundContrast::Floors(values) => validate_values(values, "floor"),
            PerBackgroundContrast::Ceilings(values) => validate_values(values, "ceiling"),
        }?;

        let query = ColorQuery {
            seed: seed.to_owned(),
            backgrounds: backgrounds.to_vec(),
            preference_backgrounds: preference_backgrounds.to_vec(),
            per_background_contrast: per_background.cache_key(),
            contrast: bounds.contrast.into(),
            avoid: avoid.to_vec(),
            lower_lightness: bounds.lower_lightness.to_bits(),
            upper_lightness: bounds.upper_lightness.to_bits(),
            lower_chroma: bounds.lower_chroma.to_bits(),
            upper_chroma: bounds.upper_chroma.to_bits(),
            prefer_background: bounds.prefer_background,
        };
        if let Some(result) = self.color_results.get(&query) {
            return result.clone();
        }

        let result = self.fit_color_bounded_uncached(
            seed,
            backgrounds,
            preference_backgrounds,
            per_background,
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
        per_background: PerBackgroundContrast<&[f64]>,
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
        let passes_noncontrast = |candidate: ColorMetrics| {
            if candidate.lab[0] < bounds.lower_lightness - 1e-12
                || candidate.lab[0] > bounds.upper_lightness + 1e-12
            {
                return false;
            }

            if bounds.lower_chroma > 0.0 || bounds.upper_chroma.is_finite() {
                let chroma = lab_chroma(candidate.lab);
                if chroma < bounds.lower_chroma - 1e-12 || chroma > bounds.upper_chroma + 1e-12 {
                    return false;
                }
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
        let passes = |candidate: ColorMetrics| {
            passes_noncontrast(candidate)
                && color_contrasts_satisfy(
                    candidate,
                    &background_metrics,
                    per_background,
                    bounds.contrast,
                    |_, _| {},
                )
        };

        if passes(source_metrics)
            && !bounds.prefer_background
            && bounds.contrast.preferred().is_none()
        {
            return Ok(seed.to_owned());
        }

        if !bounds.prefer_background && bounds.contrast.preferred().is_none() {
            let mut best: Option<(Rgb24, [f64; 3])> = None;
            for candidate in self.transform_table(seed)?.candidates.iter() {
                if best.as_ref().is_some_and(|(_, rank)| {
                    candidate.distance.total_cmp(&rank[0]) == Ordering::Greater
                }) {
                    break;
                }
                if !passes_noncontrast(candidate.metrics) {
                    continue;
                }
                let mut overshoot = 0.0;
                if !color_contrasts_satisfy(
                    candidate.metrics,
                    &background_metrics,
                    per_background,
                    bounds.contrast,
                    |contrast, floor| overshoot += (contrast - floor).max(0.0),
                ) {
                    continue;
                }
                let rank = [candidate.distance, overshoot, -candidate.retention];
                if best.as_ref().is_none_or(|(best_color, best_rank)| {
                    rank_cmp(&rank, best_rank)
                        .then_with(|| candidate.metrics.rgb24().cmp(best_color))
                        == Ordering::Less
                }) {
                    best = Some((candidate.metrics.rgb24(), rank));
                }
            }
            if let Some((color, _)) = best {
                return Ok(color.hex());
            }
            return self.best_effort_color(
                seed,
                &background_metrics,
                &preference_background_metrics,
                per_background,
                &avoid_metrics,
                bounds,
            );
        }

        if let Some(preferred_contrast) = bounds.contrast.preferred() {
            let mut best: Option<(Rgb24, [f64; 3])> = None;
            let preferred_log = preferred_contrast.ln();
            let shared_preference_backgrounds = backgrounds == preference_backgrounds;
            let mut consider =
                |candidate: Rgb24, metrics: ColorMetrics, distance: f64, retention: f64| {
                    if !passes_noncontrast(metrics) {
                        return;
                    }
                    let mean_log_contrast = if shared_preference_backgrounds {
                        let mut sum = 0.0;
                        if !color_contrasts_satisfy(
                            metrics,
                            &background_metrics,
                            per_background,
                            bounds.contrast,
                            |contrast, _| sum += contrast.ln(),
                        ) {
                            return;
                        }
                        sum / background_metrics.len() as f64
                    } else {
                        if !color_contrasts_satisfy(
                            metrics,
                            &background_metrics,
                            per_background,
                            bounds.contrast,
                            |_, _| {},
                        ) {
                            return;
                        }
                        preference_background_metrics
                            .iter()
                            .map(|background| metrics.contrast(*background).ln())
                            .sum::<f64>()
                            / preference_background_metrics.len() as f64
                    };
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
            if let Some((color, _)) = best {
                return Ok(color.hex());
            }
            return self.best_effort_color(
                seed,
                &background_metrics,
                &preference_background_metrics,
                per_background,
                &avoid_metrics,
                bounds,
            );
        }

        let mut best: Option<(Rgb24, [f64; 4])> = None;
        let consider = |candidate: Rgb24,
                        metrics: ColorMetrics,
                        distance: f64,
                        retention: f64,
                        best: &mut Option<(Rgb24, [f64; 4])>| {
            if !passes_noncontrast(metrics) {
                return;
            }
            let mut overshoot = 0.0;
            if !color_contrasts_satisfy(
                metrics,
                &background_metrics,
                per_background,
                bounds.contrast,
                |contrast, floor| overshoot += (contrast - floor).max(0.0),
            ) {
                return;
            }
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
        if let Some((color, _)) = best {
            return Ok(color.hex());
        }
        self.best_effort_color(
            seed,
            &background_metrics,
            &preference_background_metrics,
            per_background,
            &avoid_metrics,
            bounds,
        )
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

    fn validate_alpha_range(minimum_alpha: u8, maximum_alpha: u8) -> Result<()> {
        if minimum_alpha > maximum_alpha {
            return Err(Error::invalid(
                "overlay minimum alpha cannot exceed maximum alpha",
            ));
        }

        Ok(())
    }

    fn find_feasible_overlay(
        &mut self,
        seed: &str,
        prepared: &PreparedFill,
        source: ColorMetrics,
        alpha_values: &[u8],
    ) -> Result<Option<FillCandidate>> {
        if let Some(candidate) = prepared.best_for(source, 0.0, 1.0, alpha_values) {
            return Ok(Some(candidate));
        }

        let table = self.transform_table(seed)?;
        Ok(table
            .candidates
            .par_iter()
            .map(|opaque| {
                prepared.best_for(
                    opaque.metrics,
                    opaque.distance,
                    opaque.retention,
                    alpha_values,
                )
            })
            .reduce(
                || None,
                |left, right| match (left, right) {
                    (None, other) | (other, None) => other,
                    (Some(left), Some(right)) => {
                        let order = fill_candidate_cmp(&left, &right);
                        Some(if order == Ordering::Greater {
                            right
                        } else {
                            left
                        })
                    }
                },
            ))
    }

    fn find_best_effort_overlay(
        &mut self,
        seed: &str,
        prepared: &PreparedFill,
        source: ColorMetrics,
        alpha_values: &[u8],
    ) -> Result<FillCandidate> {
        let source_candidate = prepared.best_effort_for(source, 0.0, 1.0, alpha_values);
        let table = self.transform_table(seed)?;
        let best = table
            .candidates
            .par_iter()
            .map(|opaque| {
                prepared.best_effort_for(
                    opaque.metrics,
                    opaque.distance,
                    opaque.retention,
                    alpha_values,
                )
            })
            .reduce_with(|left, right| {
                let order = fill_candidate_cmp(&left, &right);
                if order == Ordering::Greater {
                    right
                } else {
                    left
                }
            });
        Ok(best
            .filter(|candidate| fill_candidate_cmp(candidate, &source_candidate) == Ordering::Less)
            .unwrap_or(source_candidate))
    }

    fn find_feasible_overlay_in_tiers(
        &mut self,
        seed: &str,
        prepared: &PreparedFill,
        source: ColorMetrics,
        preferred_maximum_alpha: u8,
        maximum_alpha: u8,
    ) -> Result<Option<FillCandidate>> {
        Self::validate_alpha_range(1, preferred_maximum_alpha)?;
        Self::validate_alpha_range(preferred_maximum_alpha, maximum_alpha)?;
        let preferred_alphas = PreparedFill::alpha_values(1, preferred_maximum_alpha);
        if let Some(candidate) =
            self.find_feasible_overlay(seed, prepared, source, &preferred_alphas)?
        {
            return Ok(Some(candidate));
        }
        if preferred_maximum_alpha == maximum_alpha {
            return Ok(None);
        }

        let remaining_alphas = PreparedFill::alpha_values(1, maximum_alpha)
            .into_iter()
            .filter(|alpha| *alpha > preferred_maximum_alpha)
            .collect::<Vec<_>>();
        self.find_feasible_overlay(seed, prepared, source, &remaining_alphas)
    }

    pub(crate) fn try_fit_readable_overlay_preferred(
        &mut self,
        seed: &str,
        request: OverlayFitRequest<'_>,
        preferred_maximum_alpha: u8,
        maximum_alpha: u8,
    ) -> Result<Option<String>> {
        let prepared = PreparedFill::new(request)?;
        let source = ColorMetrics::from_hex(seed)?;
        Ok(self
            .find_feasible_overlay_in_tiers(
                seed,
                &prepared,
                source,
                preferred_maximum_alpha,
                maximum_alpha,
            )?
            .map(|candidate| candidate.emitted.hex()))
    }

    pub(crate) fn fit_readable_overlay_preferred(
        &mut self,
        seed: &str,
        request: OverlayFitRequest<'_>,
        preferred_maximum_alpha: u8,
        maximum_alpha: u8,
    ) -> Result<String> {
        let prepared = PreparedFill::new(request)?;
        let source = ColorMetrics::from_hex(seed)?;

        if let Some(candidate) = self.find_feasible_overlay_in_tiers(
            seed,
            &prepared,
            source,
            preferred_maximum_alpha,
            maximum_alpha,
        )? {
            return Ok(candidate.emitted.hex());
        }

        let alpha_values = PreparedFill::alpha_values(1, maximum_alpha);
        Ok(self
            .find_best_effort_overlay(seed, &prepared, source, &alpha_values)?
            .emitted
            .hex())
    }

    pub fn fit_readable_overlay_alpha_range(
        &mut self,
        seed: &str,
        request: OverlayFitRequest<'_>,
        minimum_alpha: u8,
        maximum_alpha: u8,
    ) -> Result<String> {
        Self::validate_alpha_range(minimum_alpha, maximum_alpha)?;
        let prepared = PreparedFill::new(request)?;
        let alpha_values = PreparedFill::alpha_values(minimum_alpha, maximum_alpha);
        let source = ColorMetrics::from_hex(seed)?;

        if let Some(candidate) =
            self.find_feasible_overlay(seed, &prepared, source, &alpha_values)?
        {
            return Ok(candidate.emitted.hex());
        }
        Ok(self
            .find_best_effort_overlay(seed, &prepared, source, &alpha_values)?
            .emitted
            .hex())
    }

    fn prepare_overlay_pair(
        &mut self,
        first_seed: &str,
        second_seed: &str,
        first_request: OverlayFitRequest<'_>,
        second_request: OverlayFitRequest<'_>,
        alpha_range: (u8, u8),
        include_best_effort: bool,
    ) -> Result<PreparedOverlayPair> {
        let (minimum_alpha, maximum_alpha) = alpha_range;
        let first = PreparedFill::new(first_request)?;
        let second = PreparedFill::new(second_request)?;
        if first.backgrounds.len() != second.backgrounds.len() {
            return Err(Error::invalid("overlay pair scene counts differ"));
        }

        let collect = |seed: &str,
                       table: &TransformTableData,
                       prepared: &PreparedFill,
                       minimum_alpha: u8,
                       maximum_alpha: u8,
                       include_best_effort: bool|
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

            if include_best_effort || candidates.is_empty() {
                candidates.extend(
                    table
                        .candidates
                        .par_iter()
                        .flat_map_iter(|opaque| {
                            let (best, highest) = prepared.best_effort_and_highest_for(
                                opaque.metrics,
                                opaque.distance,
                                opaque.retention,
                                &alpha_values,
                            );
                            [best, highest]
                        })
                        .collect::<Vec<_>>(),
                );
                candidates.push(prepared.best_effort_for(
                    ColorMetrics::from_hex(seed)?,
                    0.0,
                    1.0,
                    &alpha_values,
                ));
            }
            candidates.sort_by(fill_candidate_cmp);
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
                    include_best_effort,
                )
            },
            || {
                collect(
                    second_seed,
                    &second_table,
                    &second,
                    minimum_alpha,
                    maximum_alpha,
                    include_best_effort,
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
        constraints: PairConstraints,
        frontier_limit: usize,
        allow_fallback: bool,
    ) -> Result<Option<[String; 2]>> {
        let mut best: Option<([Rgba32; 2], [f64; 6])> = None;
        let mut maxima = [0.0_f64; 3];

        for frontier_size in [128_usize, 512]
            .into_iter()
            .filter(|size| *size <= frontier_limit)
        {
            let mut first_frontier = overlay_frontier(&prepared.first_candidates, frontier_size)
                .into_iter()
                .filter(|candidate| candidate.rank[0] <= 1e-12 && candidate.rank[1] <= 1e-12)
                .map(|candidate| FrontierCandidate::new(candidate, &prepared.first))
                .collect::<Vec<_>>();
            let mut second_frontier = overlay_frontier(&prepared.second_candidates, frontier_size)
                .into_iter()
                .filter(|candidate| candidate.rank[0] <= 1e-12 && candidate.rank[1] <= 1e-12)
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

        if let Some((colors, _)) = best {
            return Ok(Some([colors[0].hex(), colors[1].hex()]));
        }
        if !allow_fallback {
            return Ok(None);
        }

        let frontier_size = frontier_limit.min(256);
        let first = overlay_fallback_frontier(&prepared.first_candidates, frontier_size)
            .into_iter()
            .map(|candidate| FrontierCandidate::new(candidate, &prepared.first))
            .collect::<Vec<_>>();
        let second = overlay_fallback_frontier(&prepared.second_candidates, frontier_size)
            .into_iter()
            .map(|candidate| FrontierCandidate::new(candidate, &prepared.second))
            .collect::<Vec<_>>();
        let mut best_effort: Option<([Rgba32; 2], [f64; 7])> = None;
        for left in &first {
            for right in &second {
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
                    minimum_contrast =
                        minimum_contrast.min(left_rendered.contrast(*right_rendered));
                    minimum_normal = minimum_normal.min(left_rendered.delta_e(*right_rendered));
                    minimum_lightness =
                        minimum_lightness.min((left_rendered.lab[0] - right_rendered.lab[0]).abs());
                    salience_imbalance += (left_rendered.contrast(*left_base)
                        - right_rendered.contrast(*right_base))
                    .abs();
                }
                let mut minimum_cvd = minimum_normal;
                for (left_cvd, right_cvd) in left.cvd().iter().zip(right.cvd()) {
                    minimum_cvd =
                        minimum_cvd.min(cvd_distance_facts(left_cvd, right_cvd, f64::INFINITY));
                }
                let alternative_deficit =
                    constraints
                        .separation_alternative
                        .map_or(0.0, |(contrast, normal, cvd)| {
                            shortfall(minimum_contrast, contrast).min(
                                shortfall(minimum_normal, normal) + shortfall(minimum_cvd, cvd),
                            )
                        });
                let deficit = shortfall(minimum_contrast, constraints.pair_contrast)
                    + shortfall(minimum_normal, constraints.normal_delta)
                    + shortfall(minimum_cvd, constraints.cvd_delta)
                    + shortfall(minimum_lightness, constraints.lightness_delta)
                    + shortfall(left.core.source_chroma, constraints.minimum_chroma)
                    + shortfall(right.core.source_chroma, constraints.minimum_chroma)
                    + alternative_deficit;
                let rank = [
                    left.core.rank[0] + right.core.rank[0] + deficit,
                    left.core.rank[1] + right.core.rank[1],
                    salience_imbalance,
                    left.core.rank[2] + right.core.rank[2],
                    -minimum_normal,
                    -minimum_cvd,
                    -(minimum_contrast + minimum_lightness),
                ];
                let colors = [left.core.emitted, right.core.emitted];
                if best_effort.as_ref().is_none_or(|(best_colors, best_rank)| {
                    rank_cmp(&rank, best_rank).then_with(|| colors.cmp(best_colors))
                        == Ordering::Less
                }) {
                    best_effort = Some((colors, rank));
                }
            }
        }
        let colors = best_effort
            .expect("validated overlay pair search must have candidate colors")
            .0;

        Ok(Some([colors[0].hex(), colors[1].hex()]))
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
            (request.minimum_alpha, request.maximum_alpha),
            false,
        )?;
        if let Some(output) = Self::solve_overlay_pair(
            &prepared,
            request.constraints,
            request.frontier_limit,
            false,
        )? {
            return Ok(output);
        }

        let prepared = self.prepare_overlay_pair(
            first_seed,
            second_seed,
            request.first,
            request.second,
            (request.minimum_alpha, request.maximum_alpha),
            true,
        )?;
        Ok(
            Self::solve_overlay_pair(&prepared, request.constraints, request.frontier_limit, true)?
                .expect("best-effort overlay pair search must produce a result"),
        )
    }

    pub(crate) fn fit_state_request(
        &mut self,
        seed: &str,
        request: StateFitRequest<'_>,
    ) -> Result<String> {
        request.validate()?;
        let query = StateQuery {
            seed: seed.to_owned(),
            backgrounds: request.backgrounds.to_vec(),
            contrast: request.contrast.into(),
            delta_e: request.delta_e.into(),
            minimum_chroma: request.minimum_chroma.to_bits(),
            references: request
                .references
                .iter()
                .map(|(color, target, delta)| (color.clone(), target.to_bits(), delta.to_bits()))
                .collect(),
        };
        if let Some(result) = self.state_results.get(&query) {
            return result.clone();
        }

        let result = self.fit_state_uncached(seed, request);
        self.state_results.insert(query, result.clone());

        result
    }

    fn fit_state_uncached(&mut self, seed: &str, request: StateFitRequest<'_>) -> Result<String> {
        if request.backgrounds.is_empty() {
            return Err(Error::invalid("fit_state requires at least one background"));
        }

        let background_metrics = request
            .backgrounds
            .iter()
            .map(|background| ColorMetrics::from_hex(background))
            .collect::<Result<Vec<_>>>()?;
        let reference_metrics = request
            .references
            .iter()
            .map(|(reference, target, delta)| {
                Ok(StateReference {
                    color: ColorMetrics::from_hex(reference)?,
                    minimum_contrast: *target,
                    minimum_delta_e: *delta,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let context = StateCandidateContext {
            backgrounds: &background_metrics,
            references: &reference_metrics,
            contrast: request.contrast,
            delta_e: request.delta_e,
            minimum_chroma: request.minimum_chroma,
        };
        let mut best: Option<(Rgb24, [f64; 6])> = None;
        let table = self.transform_table(seed)?;

        for candidate in table.candidates.iter() {
            let Some(rank) = context.feasible_rank(candidate) else {
                continue;
            };
            if best.as_ref().is_none_or(|(best_color, best_rank)| {
                rank_cmp(&rank, best_rank).then_with(|| candidate.metrics.rgb24().cmp(best_color))
                    == Ordering::Less
            }) {
                best = Some((candidate.metrics.rgb24(), rank));
            }
        }

        if let Some((color, _)) = best {
            return Ok(color.hex());
        }

        let mut best_effort: Option<(Rgb24, [f64; 6])> = None;
        for candidate in table.candidates.iter() {
            let rank = context.best_effort_rank(candidate);
            if best_effort.as_ref().is_none_or(|(best_color, best_rank)| {
                rank_cmp(&rank, best_rank).then_with(|| candidate.metrics.rgb24().cmp(best_color))
                    == Ordering::Less
            }) {
                best_effort = Some((candidate.metrics.rgb24(), rank));
            }
        }

        Ok(best_effort
            .expect("validated state search must have at least one candidate")
            .0
            .hex())
    }

    pub fn fit_distinct_colors(
        &mut self,
        seeds: &[String],
        backgrounds: &[String],
        target: f64,
    ) -> Result<Vec<String>> {
        self.fit_distinct_colors_with_separation(
            seeds,
            backgrounds,
            target,
            ACCENT_NORMAL_DELTA_E,
            ACCENT_CVD_DELTA_E,
        )
    }

    pub fn fit_distinct_colors_with_separation(
        &mut self,
        seeds: &[String],
        backgrounds: &[String],
        target: f64,
        normal_delta_e: f64,
        cvd_delta_e: f64,
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

        let background_metrics = backgrounds
            .iter()
            .enumerate()
            .map(|(index, background)| {
                ColorMetrics::from_hex(background)
                    .map_err(|error| error.context(format!("distinct color background[{index}]")))
            })
            .collect::<Result<Vec<_>>>()?;
        let mut chosen: Vec<String> = Vec::new();

        for seed in seeds {
            let fitted = self.fit_color(seed, backgrounds, target)?;
            if chosen.is_empty() {
                chosen.push(fitted);
                continue;
            }

            let source_lab = lab(seed)?;
            let seed_chroma = oklab_to_oklch(source_lab)[1];
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
            let context = DistinctCandidateContext {
                backgrounds: &background_metrics,
                chosen: &chosen_metrics,
                target,
                minimum_normal_delta_e: normal_delta_e,
                minimum_cvd_delta_e: cvd_delta_e,
            };

            let mut considered = BTreeSet::new();
            let mut passing: Option<(Rgb24, [f64; 3])> = None;
            let mut consider_passing =
                |candidate: Rgb24,
                 metrics: ColorMetrics,
                 transform: f64,
                 retention: f64,
                 passing: &mut Option<(Rgb24, [f64; 3])>| {
                    if !considered.insert(candidate) || chosen_rgb.contains(&candidate) {
                        return;
                    }
                    let Some(passing_rank) = context.feasible_rank(metrics, transform, retention)
                    else {
                        return;
                    };
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
            consider_passing(
                fitted_rgb,
                fitted_metrics,
                fitted_transform,
                fitted_retention,
                &mut passing,
            );

            let table = self.transform_table(seed)?;
            for candidate in table.candidates.iter() {
                if passing.as_ref().is_some_and(|(_, rank)| {
                    candidate.distance.total_cmp(&rank[0]) == Ordering::Greater
                }) {
                    break;
                }
                consider_passing(
                    candidate.metrics.rgb24(),
                    candidate.metrics,
                    candidate.distance,
                    candidate.retention,
                    &mut passing,
                );
            }

            let output = if let Some((color, _)) = passing {
                color
            } else {
                let mut considered = BTreeSet::new();
                let mut fallback: Option<(Rgb24, [f64; 6])> = None;
                let mut consider_fallback =
                    |candidate: Rgb24, metrics: ColorMetrics, transform: f64, retention: f64| {
                        if !considered.insert(candidate) || chosen_rgb.contains(&candidate) {
                            return;
                        }
                        let rank = context.best_effort_rank(metrics, transform, retention);
                        if fallback.as_ref().is_none_or(|(best_color, best_rank)| {
                            rank_cmp(&rank, best_rank).then_with(|| candidate.cmp(best_color))
                                == Ordering::Less
                        }) {
                            fallback = Some((candidate, rank));
                        }
                    };

                consider_fallback(
                    fitted_rgb,
                    fitted_metrics,
                    fitted_transform,
                    fitted_retention,
                );
                for candidate in table.candidates.iter() {
                    consider_fallback(
                        candidate.metrics.rgb24(),
                        candidate.metrics,
                        candidate.distance,
                        candidate.retention,
                    );
                }
                fallback.map_or(fitted_rgb, |(color, _)| color)
            }
            .hex();

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
    use crate::color::{contrast_ratio, delta_e};
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
            let strict = direct.fit_color(&seed, &backgrounds, strict_target).unwrap();
            prop_assert_eq!(
                &strict,
                &direct.fit_color(&seed, &backgrounds, strict_target).unwrap()
            );

            let mut prewarmed = Search::default();
            prewarmed.prewarm([seed.as_str()]).unwrap();
            let warmed = prewarmed.fit_color(&seed, &backgrounds, strict_target).unwrap();
            prop_assert_eq!(&strict, &warmed);
            if endpoint_contrast(background) >= strict_target - 1e-12 {
                prop_assert!(
                    contrast_ratio(&strict, &backgrounds[0]).unwrap() >= strict_target - 1e-12
                );
            }
            let relaxed = direct.fit_color(&seed, &backgrounds, relaxed_target).unwrap();
            prop_assert!(
                contrast_ratio(&relaxed, &backgrounds[0]).unwrap()
                    >= relaxed_target.min(endpoint_contrast(background)) - 1e-12
            );
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
        let request = || {
            OverlayFitRequest::new(
                &backgrounds,
                MetricBand::floor(1.1),
                MetricBand::floor(0.01),
            )
        };
        let mut search = Search::default();

        assert!(search.fit_readable_overlay("#ffffff", request()).is_err());

        assert!(
            search
                .fit_state_request(
                    "#ffffff",
                    StateFitRequest::new(
                        &backgrounds,
                        MetricBand::floor(1.1),
                        MetricBand::floor(0.01),
                    ),
                )
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
                &[],
                FitBounds {
                    lower_lightness: 0.8,
                    upper_lightness: 0.2,
                    ..FitBounds::new(MetricBand::floor(4.5))
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
                OverlayFitRequest::new(
                    &backgrounds,
                    MetricBand::floor(1.2),
                    MetricBand::floor(f64::NEG_INFINITY),
                ),
            )
            .unwrap_err();
        assert_eq!(overlay_error.kind(), crate::ErrorKind::InvalidInput);

        let preferred_alpha_error = search
            .try_fit_readable_overlay_preferred(
                "#ffffff",
                OverlayFitRequest::new(
                    &backgrounds,
                    MetricBand::floor(1.2),
                    MetricBand::floor(0.01),
                ),
                0,
                0,
            )
            .unwrap_err();
        assert_eq!(preferred_alpha_error.kind(), crate::ErrorKind::InvalidInput);

        let frontier_error = search
            .fit_overlay_pair(
                "#ffffff",
                "#000000",
                OverlayPairRequest::new(
                    OverlayFitRequest::new(
                        &backgrounds,
                        MetricBand::floor(1.1),
                        MetricBand::floor(0.01),
                    ),
                    OverlayFitRequest::new(
                        &backgrounds,
                        MetricBand::floor(1.1),
                        MetricBand::floor(0.01),
                    ),
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
            )
            .unwrap_err();
        assert_eq!(separation_error.kind(), crate::ErrorKind::InvalidInput);
    }

    #[test]
    fn impossible_pair_request_maximizes_the_requested_separation() {
        let first_seed = "#3a7bd5";
        let second_seed = "#d53a7b";
        let mut search = Search::default();
        let result = search
            .fit_pair(
                first_seed,
                second_seed,
                &["#808080".into()],
                PairConstraints::new(1.0, 1.0, 2.0, 0.0),
            )
            .unwrap();

        assert!(
            delta_e(&result[0], &result[1]).unwrap()
                >= delta_e("#000000", "#ffffff").unwrap() - 1e-12
        );
        assert_eq!(
            result,
            search
                .fit_pair(
                    first_seed,
                    second_seed,
                    &["#808080".into()],
                    PairConstraints::new(1.0, 1.0, 2.0, 0.0),
                )
                .unwrap()
        );
    }

    #[test]
    fn zero_work_distinct_color_search_validates_backgrounds() {
        let mut search = Search::default();
        let error = search
            .fit_distinct_colors(&[], &["invalid".into()], 4.5)
            .unwrap_err();
        assert_eq!(error.kind(), crate::ErrorKind::InvalidInput);
    }

    #[test]
    fn distinct_color_fit_returns_the_best_available_set() {
        let mut search = Search::default();
        let colors = search
            .fit_distinct_colors(
                &["#ffffff".into(), "#ffffff".into()],
                &["#000000".into()],
                20.0,
            )
            .unwrap();
        assert_eq!(colors.len(), 2);
        assert_ne!(colors[0], colors[1]);
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
                &[],
                FitBounds {
                    upper_chroma: 0.05,
                    ..FitBounds::new(MetricBand::floor(3.0))
                },
            )
            .unwrap();
        let vivid = search
            .fit_color_bounded(
                "#ff0000",
                &backgrounds,
                &[],
                FitBounds {
                    upper_chroma: 0.20,
                    ..FitBounds::new(MetricBand::floor(3.0))
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
                &[],
                FitBounds::new(MetricBand::with_preference(1.52, 2.0)),
            )
            .unwrap();
        let focal = search
            .fit_color_bounded(
                "#cccccc",
                &backgrounds,
                &[],
                FitBounds::new(MetricBand::with_preference(1.52, 8.0)),
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
        let bounds = FitBounds::new(MetricBand::with_preference(1.05, 8.0));
        let dark = search
            .fit_color_bounded_with_preference_backgrounds(
                "#cccccc",
                &required,
                &dark_preference,
                &[],
                bounds,
            )
            .unwrap();
        let light = search
            .fit_color_bounded_with_preference_backgrounds(
                "#cccccc",
                &required,
                &light_preference,
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
                OverlayFitRequest::new(
                    &backgrounds,
                    MetricBand::floor(1.10),
                    MetricBand::floor(0.025),
                ),
                OverlayFitRequest::new(
                    &backgrounds,
                    MetricBand::floor(1.10),
                    MetricBand::floor(0.025),
                ),
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
    fn overlay_pair_fallback_prioritizes_pair_separation_over_individual_ceilings() {
        let backgrounds = vec!["#202020".to_owned()];
        let fill = || {
            OverlayFitRequest::new(
                &backgrounds,
                MetricBand::bounded(1.01, 1.05, 1.10),
                MetricBand::bounded(0.005, 0.020, 0.030),
            )
        };
        let output = Search::default()
            .fit_overlay_pair(
                "#d08080",
                "#80a0d0",
                OverlayPairRequest::new(fill(), fill(), PairConstraints::new(1.0, 1.0, 0.18, 0.0)),
            )
            .unwrap();
        let rendered: [String; 2] = std::array::from_fn(|index| {
            crate::color::render_layers(&backgrounds[0], &[&output[index]]).unwrap()
        });

        assert!(
            rendered
                .iter()
                .all(|color| contrast_ratio(color, &backgrounds[0]).unwrap() >= 1.01 - 1e-9)
        );
        let separation = delta_e(&rendered[0], &rendered[1]).unwrap();
        assert!(
            separation >= 0.18 - 1e-9,
            "pair separation {separation:.4} for {output:?} rendered as {rendered:?}"
        );
        assert!(
            rendered.iter().any(|color| {
                contrast_ratio(color, &backgrounds[0]).unwrap() > 1.10 + 1e-9
                    || delta_e(color, &backgrounds[0]).unwrap() > 0.030 + 1e-9
            }),
            "hard pair separation should be allowed to exceed an aesthetic ceiling"
        );
    }

    #[test]
    fn color_contrast_ceiling_is_enforced_and_cached_separately() {
        let backgrounds = vec!["#121212".to_owned()];
        let mut search = Search::default();
        let bounded = search
            .fit_color_bounded(
                "#ffffff",
                &backgrounds,
                &[],
                FitBounds::new(MetricBand::bounded(1.50, 2.0, 2.20)),
            )
            .unwrap();
        let relaxed = search
            .fit_color_bounded(
                "#ffffff",
                &backgrounds,
                &[],
                FitBounds::new(MetricBand::bounded(1.50, 3.0, 3.20)),
            )
            .unwrap();

        let bounded_contrast = contrast_ratio(&bounded, &backgrounds[0]).unwrap();
        let relaxed_contrast = contrast_ratio(&relaxed, &backgrounds[0]).unwrap();
        assert!((1.50 - 1e-9..=2.20 + 1e-9).contains(&bounded_contrast));
        assert!((1.50 - 1e-9..=3.20 + 1e-9).contains(&relaxed_contrast));
        assert!(bounded_contrast < relaxed_contrast);
        assert_eq!(search.color_results.len(), 2);
    }

    #[test]
    fn per_background_contrast_ceilings_are_enforced_and_cached_separately() {
        let backgrounds = vec!["#121212".to_owned(), "#303030".to_owned()];
        let bounds = FitBounds::new(MetricBand::with_preference(1.10, 2.0));
        let mut search = Search::default();
        let first_ceilings = [2.40, 1.80];
        let first = search
            .fit_color_bounded_with_contrast_ceilings(
                "#ffffff",
                &backgrounds,
                &backgrounds,
                &first_ceilings,
                &[],
                bounds,
            )
            .unwrap();
        let reversed_ceilings = [1.80, 2.40];
        let reversed = search
            .fit_color_bounded_with_contrast_ceilings(
                "#ffffff",
                &backgrounds,
                &backgrounds,
                &reversed_ceilings,
                &[],
                bounds,
            )
            .unwrap();

        for (output, ceilings) in [(&first, first_ceilings), (&reversed, reversed_ceilings)] {
            for (background, ceiling) in backgrounds.iter().zip(ceilings) {
                let contrast = contrast_ratio(output, background).unwrap();
                assert!((1.10 - 1e-9..=ceiling + 1e-9).contains(&contrast));
            }
        }
        assert_ne!(first, reversed);
        assert_eq!(search.color_results.len(), 2);

        let error = search
            .fit_color_bounded_with_contrast_ceilings(
                "#ffffff",
                &backgrounds,
                &backgrounds,
                &[2.20],
                &[],
                bounds,
            )
            .unwrap_err();
        assert_eq!(error.kind(), crate::ErrorKind::InvalidInput);
    }

    #[test]
    fn per_background_contrast_floors_are_enforced_and_cached_separately() {
        let backgrounds = vec!["#121212".to_owned(), "#303030".to_owned()];
        let bounds = FitBounds::new(MetricBand::floor(1.10));
        let mut search = Search::default();
        let first_floors = [4.00, 2.50];
        let first = search
            .fit_color_bounded_with_contrast_floors(
                "#777777",
                &backgrounds,
                &backgrounds,
                &first_floors,
                &[],
                bounds,
            )
            .unwrap();
        let second_floors = [5.00, 3.50];
        let second = search
            .fit_color_bounded_with_contrast_floors(
                "#777777",
                &backgrounds,
                &backgrounds,
                &second_floors,
                &[],
                bounds,
            )
            .unwrap();

        for (output, floors) in [(&first, first_floors), (&second, second_floors)] {
            for (background, floor) in backgrounds.iter().zip(floors) {
                assert!(contrast_ratio(output, background).unwrap() >= floor - 1e-9);
            }
        }
        assert_eq!(search.color_results.len(), 2);

        let error = search
            .fit_color_bounded_with_contrast_floors(
                "#777777",
                &backgrounds,
                &backgrounds,
                &[2.0],
                &[],
                bounds,
            )
            .unwrap_err();
        assert_eq!(error.kind(), crate::ErrorKind::InvalidInput);
    }

    #[test]
    fn state_minimum_chroma_is_enforced_and_cached_separately() {
        let backgrounds = vec!["#f5f5f5".to_owned()];
        let mut search = Search::default();
        let band = StateFitRequest::new(
            &backgrounds,
            MetricBand::bounded(1.20, 1.40, 1.65),
            MetricBand::bounded(0.030, 0.080, 0.250),
        );
        let neutral_allowed = search.fit_state_request("#3264eb", band).unwrap();
        let tinted = search
            .fit_state_request("#3264eb", band.with_minimum_chroma(0.040))
            .unwrap();

        assert!(lab_chroma(ColorMetrics::from_hex(&tinted).unwrap().lab) >= 0.040 - 1e-9);
        assert_eq!(search.state_results.len(), 2);
        assert_ne!(neutral_allowed, tinted);
    }

    #[test]
    fn rendered_overlay_respects_contrast_and_distance_bands() {
        let backgrounds = vec!["#181b20".to_owned(), "#23272e".to_owned()];
        let request = OverlayFitRequest::new(
            &backgrounds,
            MetricBand::bounded(1.10, 1.18, 1.35),
            MetricBand::bounded(0.025, 0.050, 0.100),
        );
        let output = Search::default()
            .fit_readable_overlay("#8aa0b8", request)
            .unwrap();

        for background in &backgrounds {
            let rendered = crate::color::render_layers(background, &[&output]).unwrap();
            let contrast = contrast_ratio(&rendered, background).unwrap();
            let distance = delta_e(&rendered, background).unwrap();
            assert!((1.10 - 1e-9..=1.35 + 1e-9).contains(&contrast));
            assert!((0.025 - 1e-9..=0.100 + 1e-9).contains(&distance));
        }
    }

    #[test]
    fn opaque_state_respects_bands_and_rejects_inverted_requests() {
        let backgrounds = vec!["#16191d".to_owned()];
        let mut search = Search::default();
        let output = search
            .fit_state_request(
                "#8ea4bc",
                StateFitRequest::new(
                    &backgrounds,
                    MetricBand::bounded(1.12, 1.22, 1.40),
                    MetricBand::bounded(0.030, 0.060, 0.120),
                ),
            )
            .unwrap();
        let contrast = contrast_ratio(&output, &backgrounds[0]).unwrap();
        let distance = delta_e(&output, &backgrounds[0]).unwrap();
        assert!((1.12 - 1e-9..=1.40 + 1e-9).contains(&contrast));
        assert!((0.030 - 1e-9..=0.120 + 1e-9).contains(&distance));

        let error = search
            .fit_state_request(
                "#8ea4bc",
                StateFitRequest::new(
                    &backgrounds,
                    MetricBand::bounded(1.30, 1.20, 1.40),
                    MetricBand::floor(0.030),
                ),
            )
            .unwrap_err();
        assert_eq!(error.kind(), crate::ErrorKind::InvalidInput);
    }

    #[test]
    fn best_effort_preserves_hard_contrast_before_aesthetic_ceiling() {
        let backgrounds = vec!["#000000".to_owned(), "#777777".to_owned()];
        let hard_deficit = |color: &str| {
            backgrounds
                .iter()
                .map(|background| shortfall(contrast_ratio(color, background).unwrap(), 7.0))
                .sum::<f64>()
        };

        let mut color_search = Search::default();
        let color_floor_only = color_search
            .fit_color("#ffffff", &backgrounds, 7.0)
            .unwrap();
        let color_bounded = color_search
            .fit_color_bounded(
                "#ffffff",
                &backgrounds,
                &[],
                FitBounds::new(MetricBand::bounded(7.0, 7.5, 8.0)),
            )
            .unwrap();
        assert!(hard_deficit(&color_bounded) <= hard_deficit(&color_floor_only) + 1e-12);

        let mut state_search = Search::default();
        let state_floor_only = state_search
            .fit_state_request(
                "#ffffff",
                StateFitRequest::new(&backgrounds, MetricBand::floor(7.0), MetricBand::floor(0.0)),
            )
            .unwrap();
        let state_bounded = state_search
            .fit_state_request(
                "#ffffff",
                StateFitRequest::new(
                    &backgrounds,
                    MetricBand::bounded(7.0, 7.5, 8.0),
                    MetricBand::floor(0.0),
                ),
            )
            .unwrap();
        assert!(hard_deficit(&state_bounded) <= hard_deficit(&state_floor_only) + 1e-12);

        let overlay_deficit = |overlay: &str| {
            backgrounds
                .iter()
                .map(|background| {
                    let rendered = crate::color::render_layers(background, &[overlay]).unwrap();
                    shortfall(contrast_ratio(&rendered, background).unwrap(), 7.0)
                })
                .sum::<f64>()
        };
        let mut overlay_search = Search::default();
        let overlay_floor_only = overlay_search
            .fit_readable_overlay_alpha_range(
                "#ffffff",
                OverlayFitRequest::new(
                    &backgrounds,
                    MetricBand::floor(7.0),
                    MetricBand::floor(0.0),
                ),
                u8::MAX,
                u8::MAX,
            )
            .unwrap();
        let overlay_bounded = overlay_search
            .fit_readable_overlay_alpha_range(
                "#ffffff",
                OverlayFitRequest::new(
                    &backgrounds,
                    MetricBand::bounded(7.0, 7.5, 8.0),
                    MetricBand::floor(0.0),
                ),
                u8::MAX,
                u8::MAX,
            )
            .unwrap();
        assert!(overlay_deficit(&overlay_bounded) <= overlay_deficit(&overlay_floor_only) + 1e-12);
    }
}
