use super::ui_policy::StructurePolicy;
use crate::Result;
use crate::color::{relative_luminance, tone};
use crate::search::MetricBand;

pub(super) struct BorderSurfaces<'a> {
    pub canvas: &'a str,
    pub panel: &'a str,
    pub elevated: &'a str,
    pub chrome: &'a str,
    pub inactive_tab: &'a str,
}

struct Candidate {
    color: String,
    luminance: f64,
    passive: Score,
    normal: Score,
}

struct Score {
    boundary_deficit: f64,
    ceiling_excess: f64,
    target_distance: f64,
}

fn contrast(left: f64, right: f64) -> f64 {
    (left.max(right) + 0.05) / (left.min(right) + 0.05)
}

fn score(luminance: f64, backgrounds: &[f64], boundary: f64, band: MetricBand) -> Score {
    let preferred = band
        .preferred()
        .expect("structural bands have targets")
        .ln();
    let maximum = band.maximum().expect("structural bands have ceilings");

    let mut excess = 0.0_f64;
    let mut preference = 0.0;
    for background in backgrounds {
        let ratio = contrast(luminance, *background);
        let ceiling_excess = (ratio / maximum).ln().max(0.0);
        let target_distance = (ratio.ln() - preferred).abs();

        excess = excess.max(ceiling_excess);
        preference += target_distance;
    }

    let boundary_contrast = contrast(luminance, boundary);
    let boundary_deficit = (band.minimum() / boundary_contrast).ln().max(0.0);

    Score {
        boundary_deficit,
        ceiling_excess: excess,
        target_distance: preference / backgrounds.len() as f64,
    }
}

pub(super) fn derive(
    surfaces: BorderSurfaces<'_>,
    foreground: &str,
    policy: &StructurePolicy,
) -> Result<(String, String)> {
    let canvas = relative_luminance(surfaces.canvas)?;
    let panel = relative_luminance(surfaces.panel)?;
    let elevated = relative_luminance(surfaces.elevated)?;
    let chrome = relative_luminance(surfaces.chrome)?;
    let inactive_tab = relative_luminance(surfaces.inactive_tab)?;

    // Zed shares variant between editor dividers and elevated/aside edges,
    // while normal outlines tabs, docks, and editor file headers.
    let passive_backgrounds = [canvas, panel, elevated];
    let normal_backgrounds = [canvas, panel, chrome, inactive_tab];
    let all_backgrounds = [canvas, panel, elevated, chrome, inactive_tab];

    let lighter = relative_luminance(foreground)? > canvas;
    let edge = all_backgrounds
        .iter()
        .copied()
        .reduce(|a, b| if lighter { a.max(b) } else { a.min(b) })
        .expect("border surfaces are nonempty");

    let mut candidates = Vec::new();
    for step in 0..=256 {
        let color = tone(surfaces.canvas, f64::from(step) / 256.0, 1.0)?;
        let luminance = relative_luminance(&color)?;
        let crosses_surface = if lighter {
            luminance < edge
        } else {
            luminance > edge
        };
        if crosses_surface {
            continue;
        }

        candidates.push(Candidate {
            passive: score(luminance, &passive_backgrounds, elevated, policy.passive),
            normal: score(luminance, &normal_backgrounds, panel, policy.normal),
            color,
            luminance,
        });
    }

    // Direction and the shared tint are construction constraints. Visibility,
    // ceilings, and hierarchy remain ranked targets even at gamut endpoints.
    let mut best: Option<(usize, usize, [f64; 3])> = None;
    for (passive_index, passive) in candidates.iter().enumerate() {
        for (normal_index, normal) in candidates.iter().enumerate() {
            let hierarchy_inverted = if lighter {
                normal.luminance < passive.luminance
            } else {
                normal.luminance > passive.luminance
            };
            if hierarchy_inverted {
                continue;
            }

            let hierarchy_deficit = all_backgrounds
                .iter()
                .map(|background| {
                    let normal_contrast = contrast(normal.luminance, *background);
                    let passive_contrast = contrast(passive.luminance, *background);
                    let hierarchy_step = normal_contrast - passive_contrast;

                    (policy.minimum_hierarchy_step - hierarchy_step).max(0.0)
                })
                .fold(0.0_f64, f64::max);

            let visibility_deficit = passive.passive.boundary_deficit
                + normal.normal.boundary_deficit
                + hierarchy_deficit;
            let ceiling_excess = passive
                .passive
                .ceiling_excess
                .max(normal.normal.ceiling_excess);
            let target_distance = passive.passive.target_distance + normal.normal.target_distance;
            let rank = [visibility_deficit, ceiling_excess, target_distance];
            if best
                .as_ref()
                .is_some_and(|(_, _, previous)| rank >= *previous)
            {
                continue;
            }

            best = Some((passive_index, normal_index, rank));
        }
    }

    let (passive, normal, _) = best.expect("the foreground-side endpoint is always available");
    Ok((
        candidates[passive].color.clone(),
        candidates[normal].color.clone(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::{apply_opacity, contrast_ratio, lab, oklab_to_oklch, render_layers};

    fn policy() -> StructurePolicy {
        StructurePolicy {
            passive: MetricBand::bounded(1.10, 1.20, 1.45),
            normal: MetricBand::bounded(1.16, 1.35, 1.95),
            minimum_hierarchy_step: 0.01,
            active_guide: MetricBand::floor(1.30),
            focus: MetricBand::floor(3.02),
            status_border: MetricBand::floor(1.30),
        }
    }

    #[test]
    fn tinted_borders_stay_on_one_side_including_faded_dividers() {
        for (canvas, panel, elevated, chrome, inactive, foreground) in [
            (
                "#dfe4c4", "#d5daba", "#f9f9f7", "#cdd2b2", "#cdd2b2", "#12211d",
            ),
            (
                "#1e2326", "#242a2d", "#2c3337", "#181b1c", "#30383c", "#d3c6aa",
            ),
            (
                "#202020", "#242424", "#303030", "#181818", "#282828", "#eeeeee",
            ),
        ] {
            let (passive, normal) = derive(
                BorderSurfaces {
                    canvas,
                    panel,
                    elevated,
                    chrome,
                    inactive_tab: inactive,
                },
                foreground,
                &policy(),
            )
            .unwrap();

            let foreground_luminance = relative_luminance(foreground).unwrap();
            let canvas_luminance = relative_luminance(canvas).unwrap();
            let lighter = foreground_luminance > canvas_luminance;
            let weak = relative_luminance(&passive).unwrap();
            let strong = relative_luminance(&normal).unwrap();

            for background in [canvas, panel, elevated, chrome, inactive] {
                let base = relative_luminance(background).unwrap();
                let hierarchy_preserved = if lighter {
                    strong > weak && weak >= base
                } else {
                    strong < weak && weak <= base
                };
                assert!(hierarchy_preserved);

                for border in [&passive, &normal] {
                    let overlay = apply_opacity(border, 0.6).unwrap();
                    let faded = render_layers(background, &[&overlay]).unwrap();
                    let faded_contrast = contrast_ratio(&faded, background).unwrap();
                    let opaque_contrast = contrast_ratio(border, background).unwrap();
                    let faded_luminance = relative_luminance(&faded).unwrap();

                    assert!(faded_contrast <= opaque_contrast);
                    assert_eq!(faded_luminance > base, lighter);
                }
            }

            let source = oklab_to_oklch(lab(canvas).unwrap());
            for border in [&passive, &normal] {
                let actual = oklab_to_oklch(lab(border).unwrap());
                if source[1] < 0.001 {
                    assert!(actual[1] < 0.001);
                    continue;
                }

                let hue_distance = (actual[2] - source[2]).abs();
                let shortest_hue_distance = hue_distance.min(std::f64::consts::TAU - hue_distance);
                assert!(shortest_hue_distance < 0.15);
            }

            assert!(contrast_ratio(&passive, elevated).unwrap() >= 1.10);
            assert!(contrast_ratio(&normal, panel).unwrap() >= 1.16);
        }
    }

    #[test]
    fn incompatible_surfaces_still_have_a_deterministic_endpoint() {
        for foreground in ["#000000", "#ffffff"] {
            let generate = || {
                derive(
                    BorderSurfaces {
                        canvas: "#777777",
                        panel: "#000000",
                        elevated: "#ffffff",
                        chrome: "#222222",
                        inactive_tab: "#dddddd",
                    },
                    foreground,
                    &policy(),
                )
                .unwrap()
            };

            assert_eq!(generate(), (foreground.to_owned(), foreground.to_owned()));
        }
    }
}
