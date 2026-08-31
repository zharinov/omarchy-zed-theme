//! Shared contrast-relative saliency policy for editor foreground roles.

use crate::color::contrast_ratio;
use crate::search::{FitBounds, Search};
use crate::{Error, Result};

pub const INACTIVE_LINE_NUMBER_SALIENCY: f64 = 0.394;
pub const HOVER_LINE_NUMBER_SALIENCY: f64 = 0.70;
pub const PRIMARY_SALIENCY: f64 = 1.0;

#[derive(Clone, Debug)]
pub struct SaliencyFit {
    pub output: String,
    pub reference_contrast: f64,
    pub preferred_contrast: f64,
    pub actual_contrast: f64,
    pub preferred_saliency: f64,
    pub actual_saliency: f64,
}

pub struct SaliencyRequest<'a> {
    pub backgrounds: &'a [String],
    pub hard_floor: f64,
    pub preferred_saliency: f64,
    pub avoid: &'a [String],
    pub bounds: FitBounds,
}

impl<'a> SaliencyRequest<'a> {
    pub fn new(backgrounds: &'a [String], hard_floor: f64, preferred_saliency: f64) -> Self {
        Self {
            backgrounds,
            hard_floor,
            preferred_saliency,
            avoid: &[],
            bounds: FitBounds::default(),
        }
    }

    pub fn with_avoid(mut self, avoid: &'a [String]) -> Self {
        self.avoid = avoid;
        self
    }

    pub fn with_bounds(mut self, bounds: FitBounds) -> Self {
        self.bounds = bounds;
        self
    }
}

fn geometric_contrast(color: &str, backgrounds: &[String]) -> Result<f64> {
    if backgrounds.is_empty() {
        return Err(Error(
            "relative saliency requires at least one background".into(),
        ));
    }

    let mean_log = backgrounds
        .iter()
        .map(|background| contrast_ratio(color, background).map(f64::ln))
        .sum::<Result<f64>>()?
        / backgrounds.len() as f64;
    Ok(mean_log.exp())
}

pub fn relative_saliency(color: &str, reference: &str, backgrounds: &[String]) -> Result<f64> {
    let reference_contrast = geometric_contrast(reference, backgrounds)?;
    let actual_contrast = geometric_contrast(color, backgrounds)?;
    Ok(actual_contrast.ln() / reference_contrast.ln().max(1e-12))
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
        avoid,
        mut bounds,
    } = request;

    let reference_contrast = geometric_contrast(reference, backgrounds)?;
    let preferred_saliency = preferred_saliency.clamp(0.0, 1.0);
    let preferred_contrast = (reference_contrast.ln() * preferred_saliency)
        .exp()
        .max(hard_floor);
    bounds.preferred_contrast = Some(preferred_contrast);

    let output = search.fit_color_bounded(seed, backgrounds, hard_floor, avoid, bounds)?;
    let actual_contrast = geometric_contrast(&output, backgrounds)?;
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
        assert!((fit.preferred_saliency - INACTIVE_LINE_NUMBER_SALIENCY).abs() < 1e-12);
        assert!((fit.actual_saliency - INACTIVE_LINE_NUMBER_SALIENCY).abs() < 0.03);
    }
}
