//! Shared contrast-relative saliency policy for editor foreground roles.

use crate::color::{contrast_ratio, geometric_contrast};
use crate::search::{FitBounds, MetricBand, Search};
use crate::{Error, Result};

pub const INACTIVE_LINE_NUMBER_SALIENCY: f64 = 0.394;
pub const HOVER_LINE_NUMBER_SALIENCY: f64 = 0.70;
pub const PRIMARY_SALIENCY: f64 = 1.0;

#[derive(Clone, Debug)]
pub struct SaliencyFit {
    pub output: String,
    pub actual_saliency: f64,
}

pub struct SaliencyRequest<'a> {
    pub backgrounds: &'a [String],
    pub hard_floor: f64,
    pub preferred_saliency: f64,
    pub maximum_saliency: Option<f64>,
    pub contrast_ceiling: Option<&'a str>,
}

impl<'a> SaliencyRequest<'a> {
    pub fn new(backgrounds: &'a [String], hard_floor: f64, preferred_saliency: f64) -> Self {
        Self {
            backgrounds,
            hard_floor,
            preferred_saliency,
            maximum_saliency: None,
            contrast_ceiling: None,
        }
    }

    pub fn with_maximum_saliency(mut self, maximum_saliency: f64) -> Self {
        self.maximum_saliency = Some(maximum_saliency);
        self
    }

    pub fn with_contrast_ceiling(mut self, reference: &'a str) -> Self {
        self.contrast_ceiling = Some(reference);
        self
    }
}

pub fn fit_relative(
    search: &mut Search,
    seed: &str,
    reference: &str,
    request: SaliencyRequest<'_>,
) -> Result<SaliencyFit> {
    let SaliencyRequest {
        backgrounds,
        hard_floor,
        preferred_saliency,
        maximum_saliency,
        contrast_ceiling,
    } = request;

    if !hard_floor.is_finite() || !(1.0..=21.0).contains(&hard_floor) {
        return Err(Error::invalid(format!(
            "saliency hard floor must be finite and in 1..=21, got {hard_floor:?}"
        )));
    }
    if !preferred_saliency.is_finite() || !(0.0..=1.0).contains(&preferred_saliency) {
        return Err(Error::invalid(format!(
            "preferred saliency must be finite and in 0..=1, got {preferred_saliency:?}"
        )));
    }
    if let Some(maximum) = maximum_saliency
        && (!maximum.is_finite() || !(preferred_saliency..=1.0).contains(&maximum))
    {
        return Err(Error::invalid(format!(
            "saliency maximum must be finite and in {preferred_saliency}..=1, got {maximum:?}"
        )));
    }
    if backgrounds.is_empty() {
        return Err(Error::invalid(
            "relative saliency requires at least one background",
        ));
    }

    let reference_contrast = geometric_contrast(reference, backgrounds)?;
    let preferred_contrast = (reference_contrast.ln() * preferred_saliency)
        .exp()
        .max(hard_floor);
    let maximum_contrast =
        maximum_saliency.map(|maximum| (reference_contrast.ln() * maximum).exp().max(hard_floor));
    let contrast = maximum_contrast.map_or_else(
        || MetricBand::with_preference(hard_floor, preferred_contrast),
        |maximum| MetricBand::bounded(hard_floor, preferred_contrast, maximum),
    );
    let bounds = FitBounds::new(contrast);
    let contrast_ceilings = contrast_ceiling
        .map(|reference| {
            backgrounds
                .iter()
                .map(|background| {
                    contrast_ratio(reference, background).map(|value| value.max(hard_floor))
                })
                .collect::<Result<Vec<_>>>()
        })
        .transpose()?
        .unwrap_or_default();

    let fit = |search: &mut Search, seed: &str| {
        search.fit_color_bounded_with_contrast_ceilings(
            seed,
            backgrounds,
            backgrounds,
            &contrast_ceilings,
            &[],
            bounds,
        )
    };
    let violates_hierarchy_bounds = |color: &str| -> Result<bool> {
        backgrounds.iter().zip(&contrast_ceilings).try_fold(
            false,
            |exceeded, (background, ceiling)| {
                let contrast = contrast_ratio(color, background)?;
                Ok(exceeded || contrast < hard_floor - 1e-12 || contrast > *ceiling + 1e-12)
            },
        )
    };

    let mut output = fit(search, seed)?;
    if let Some(reference) = contrast_ceiling
        && violates_hierarchy_bounds(&output)?
    {
        // A fixed-hue best-effort search may have no candidate that satisfies
        // both bounds. The parent hue makes hierarchy-safe candidates reachable
        // without abandoning the requested saliency first.
        output = fit(search, reference)?;
        let reference_meets_floor = backgrounds.iter().try_fold(true, |passes, background| {
            Ok::<bool, Error>(
                passes && contrast_ratio(reference, background)? >= hard_floor - 1e-12,
            )
        })?;
        if violates_hierarchy_bounds(&output)? && reference_meets_floor {
            output = reference.to_owned();
        }
    }
    let actual_contrast = geometric_contrast(&output, backgrounds)?;
    let actual_saliency = actual_contrast.ln() / reference_contrast.ln().max(1e-12);

    Ok(SaliencyFit {
        output,
        actual_saliency,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_target_uses_the_documented_log_contrast_metric() {
        let mut search = Search::default();
        let backgrounds = vec!["#121212".to_owned()];
        let fit = fit_relative(
            &mut search,
            "#aaaaaa",
            "#dddddd",
            SaliencyRequest::new(&backgrounds, 1.52, INACTIVE_LINE_NUMBER_SALIENCY),
        )
        .unwrap();
        assert!((fit.actual_saliency - INACTIVE_LINE_NUMBER_SALIENCY).abs() < 0.03);
    }

    #[test]
    fn relative_fit_preserves_the_hard_floor_and_parent_hierarchy() {
        let backgrounds = vec![
            "#17171e".to_owned(),
            "#25252e".to_owned(),
            "#223249".to_owned(),
        ];
        let parent = "#98989a";
        let fit = fit_relative(
            &mut Search::default(),
            "#727169",
            "#dcd7ba",
            SaliencyRequest::new(&backgrounds, 4.50, 0.38)
                .with_maximum_saliency(0.42)
                .with_contrast_ceiling(parent),
        )
        .unwrap();

        for background in &backgrounds {
            let contrast = contrast_ratio(&fit.output, background).unwrap();
            let parent_contrast = contrast_ratio(parent, background).unwrap();
            assert!(contrast >= 4.50 - 1e-9);
            assert!(contrast <= parent_contrast + 1e-9);
        }
    }
}
