use crate::color::{ColorMetrics, Rgb24, Rgba, parse_hex, tone, validate_opaque_hex};
use crate::search::MetricBand;
use crate::{Error, Result};
use std::collections::BTreeSet;

#[derive(Clone, Copy)]
pub(super) struct Contact {
    underlay: ColorMetrics,
    outside: ColorMetrics,
    inside: ColorMetrics,
    opacity: u8,
}

impl Contact {
    pub(super) fn new(underlay: &str, outside: &str, inside: &str, opacity: f64) -> Result<Self> {
        if !opacity.is_finite() || !(0.0..=1.0).contains(&opacity) {
            return Err(Error::invalid("border contact opacity must be in 0..=1"));
        }

        for value in [underlay, outside, inside] {
            validate_opaque_hex(value, "rendered border contact")?;
        }

        Ok(Self {
            underlay: ColorMetrics::from_hex(underlay)?,
            outside: ColorMetrics::from_hex(outside)?,
            inside: ColorMetrics::from_hex(inside)?,
            opacity: (opacity * 255.0 + 0.5).floor() as u8,
        })
    }

    fn key(self) -> (Rgb24, Rgb24, Rgb24, u8) {
        (
            self.underlay.rgb24(),
            self.outside.rgb24(),
            self.inside.rgb24(),
            self.opacity,
        )
    }
}

#[derive(Clone, Copy)]
pub(super) enum Purpose {
    Separator,
    FilledOutline,
}

pub(super) struct ContactGroup {
    pub label: &'static str,
    pub purpose: Purpose,
    pub band: MetricBand,
    pub contacts: Vec<Contact>,
}

/// Preserve the existing border as far as possible while keeping the actual
/// translucent control stroke visible. Filled edges cannot replace this line.
pub(super) fn fit_control_stroke(seed: &str, host: &str, fills: &[String]) -> Result<String> {
    let seed = parse_hex(seed)?;
    let host = ColorMetrics::from_hex(host)?;
    let fills = fills
        .iter()
        .map(|fill| ColorMetrics::from_hex(fill))
        .collect::<Result<Vec<_>>>()?;

    let mut best: Option<(String, [f64; 2])> = None;
    let mut seen = BTreeSet::new();

    for endpoint in [0.0, 1.0] {
        for step in 0..=1024 {
            let amount = f64::from(step) / 1024.0;
            let channel = |value: f64| {
                let interpolated = value * (1.0 - amount) + endpoint * amount;
                (interpolated * 255.0).round_ties_even() / 255.0
            };
            let candidate = Rgba {
                r: channel(seed.r),
                g: channel(seed.g),
                b: channel(seed.b),
                a: 1.0,
            };
            let rgb = Rgb24::from_rgba(candidate);
            if !seen.insert(rgb) {
                continue;
            }

            let stroke = ColorMetrics::blend_rgb24(host, rgb, 153).metrics();
            let strength = fills.iter().fold(stroke.contrast(host), |minimum, fill| {
                minimum.min(stroke.contrast(*fill))
            });

            let distance = (candidate.r - seed.r).powi(2)
                + (candidate.g - seed.g).powi(2)
                + (candidate.b - seed.b).powi(2);
            // If no candidate meets the floor, retain the strongest available
            // stroke instead of rejecting an otherwise usable palette.
            let visibility_deficit = (1.5 - strength).max(0.0);
            let score = [visibility_deficit, distance];
            let color = rgb.hex();
            if best.as_ref().is_none_or(|(previous_color, previous)| {
                score < *previous || (score == *previous && color < *previous_color)
            }) {
                best = Some((color, score));
            }
        }
    }
    Ok(best
        .expect("stroke candidates include the seed and endpoints")
        .0)
}

struct PreparedContact {
    rendering: usize,
    outside: ColorMetrics,
    inside: ColorMetrics,
    native_contrast: f64,
    native_distance: f64,
}

struct PreparedGroup {
    purpose: Purpose,
    minimum: f64,
    preferred: f64,
    maximum: Option<f64>,
    contacts: Vec<PreparedContact>,
}

struct PreparedScenes {
    groups: Vec<PreparedGroup>,
    renderings: Vec<(ColorMetrics, u8)>,
}

fn prepare(groups: &[ContactGroup]) -> Result<PreparedScenes> {
    let mut renderings: Vec<(ColorMetrics, u8)> = Vec::new();
    let mut prepared = Vec::new();
    for group in groups {
        let band = group.band;
        let values = [Some(band.minimum()), band.preferred(), band.maximum()];
        let invalid_value = values
            .into_iter()
            .flatten()
            .any(|value| !value.is_finite() || value < 1.0);
        let preferred = band.preferred().unwrap_or(band.minimum());
        let invalid_order =
            preferred < band.minimum() || band.maximum().is_some_and(|maximum| maximum < preferred);
        if invalid_value || invalid_order || group.contacts.is_empty() {
            return Err(Error::invalid(format!(
                "border contact group {:?} requires contacts and an ordered finite contrast band",
                group.label
            )));
        }

        let mut seen = BTreeSet::new();
        let mut contacts = Vec::new();
        for contact in &group.contacts {
            if !seen.insert(contact.key()) {
                continue;
            }

            let rendering = renderings
                .iter()
                .position(|(underlay, opacity)| {
                    underlay.rgb24() == contact.underlay.rgb24() && *opacity == contact.opacity
                })
                .unwrap_or_else(|| {
                    renderings.push((contact.underlay, contact.opacity));
                    renderings.len() - 1
                });
            contacts.push(PreparedContact {
                rendering,
                outside: contact.outside,
                inside: contact.inside,
                native_contrast: contact.outside.contrast(contact.inside).ln(),
                native_distance: contact.outside.delta_e(contact.inside),
            });
        }

        prepared.push(PreparedGroup {
            purpose: group.purpose,
            minimum: band.minimum().ln(),
            preferred: preferred.ln(),
            maximum: band.maximum().map(f64::ln),
            contacts,
        });
    }

    Ok(PreparedScenes {
        groups: prepared,
        renderings,
    })
}

fn rank(
    candidate: ColorMetrics,
    seed: ColorMetrics,
    groups: &[PreparedGroup],
    rendered: &[ColorMetrics],
) -> [f64; 4] {
    let mut worst_violation = 0.0_f64;
    let mut mean_violation = 0.0;
    let mut continuity = 0.0;
    let mut preference = 0.0;
    for group in groups {
        let mut group_violation = 0.0;
        let mut group_continuity = 0.0;
        let mut group_preference = 0.0;
        for contact in &group.contacts {
            let stroke = rendered[contact.rendering];
            let outside_contrast = stroke.contrast(contact.outside).ln();
            let inside_contrast = stroke.contrast(contact.inside).ln();
            let strength = match group.purpose {
                Purpose::Separator => outside_contrast.max(inside_contrast),
                Purpose::FilledOutline => outside_contrast.max(contact.native_contrast),
            };
            // Contrast one and a black/white endpoint still have finite scores.
            // A small positive scale also handles a caller-provided floor of one.
            let deficit = (group.minimum - strength).max(0.0) / group.minimum.max(0.01);
            let excess = group.maximum.map_or(0.0, |maximum| {
                let ceiling = match group.purpose {
                    Purpose::Separator => maximum,
                    Purpose::FilledOutline => maximum.max(contact.native_contrast),
                };
                let stroke_strength = outside_contrast.max(inside_contrast);
                (stroke_strength - ceiling).max(0.0) / ceiling.max(0.01)
            });
            let violation = deficit.max(excess);
            worst_violation = worst_violation.max(violation);
            group_violation += violation;

            if matches!(group.purpose, Purpose::FilledOutline) {
                let fill_distance = stroke.delta_e(contact.inside);
                let detour = (stroke.delta_e(contact.outside) + fill_distance
                    - contact.native_distance)
                    .max(0.0);
                let scale = contact.native_distance.max(0.04);
                group_continuity += detour / scale + (fill_distance / scale).min(1.0);
            }
            group_preference += (strength - group.preferred).abs() / group.preferred.max(0.01);
        }

        let count = group.contacts.len() as f64;
        group_violation /= count;
        group_continuity /= count;
        group_preference /= count;
        mean_violation += group_violation;
        continuity += group_continuity;
        preference += group_preference;
    }

    // Each category has one vote regardless of how many call sites paint it.
    let count = groups.len().max(1) as f64;
    [
        worst_violation,
        mean_violation / count,
        (continuity + preference) / count,
        candidate.delta_e(seed),
    ]
}

pub(super) fn fit(seed: &str, groups: &[ContactGroup]) -> Result<String> {
    validate_opaque_hex(seed, "border seed")?;
    let seed = ColorMetrics::from_hex(seed)?;
    let seed_color = seed.rgb24().hex();
    let PreparedScenes { groups, renderings } = prepare(groups)?;
    let mut colors = vec![seed_color.clone(), "#000000".into(), "#ffffff".into()];
    for step in 0..=256 {
        colors.push(tone(&seed_color, f64::from(step) / 256.0, 1.0)?);
    }

    let mut seen = BTreeSet::new();
    let mut best: Option<(String, [f64; 4])> = None;
    let mut rendered = Vec::with_capacity(renderings.len());
    for color in colors {
        if !seen.insert(color.clone()) {
            continue;
        }

        let candidate = ColorMetrics::from_hex(&color)?;
        rendered.clear();
        for (underlay, opacity) in &renderings {
            rendered
                .push(ColorMetrics::blend_rgb24(*underlay, candidate.rgb24(), *opacity).metrics());
        }
        let score = rank(candidate, seed, &groups, &rendered);
        debug_assert!(score.iter().all(|value| value.is_finite()));
        if best
            .as_ref()
            .is_some_and(|(_, previous)| score >= *previous)
        {
            continue;
        }

        best = Some((color, score));
    }

    // The seed and endpoints are never excluded by an aesthetic preference.
    Ok(best.expect("border candidates always include the seed").0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::{apply_opacity, contrast_ratio, render_layers};
    use proptest::prelude::*;

    fn group(purpose: Purpose, contacts: Vec<Contact>) -> ContactGroup {
        ContactGroup {
            label: "test boundary",
            purpose,
            band: MetricBand::bounded(1.16, 1.35, 1.95),
            contacts,
        }
    }

    fn hex(rgb: [u8; 3]) -> String {
        let [r, g, b] = rgb;
        format!("#{r:02x}{g:02x}{b:02x}")
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(32))]

        #[test]
        fn control_stroke_preserves_or_improves_seed_visibility(
            seed in any::<[u8; 3]>(),
            host in any::<[u8; 3]>(),
            fill in any::<[u8; 3]>(),
        ) {
            let (seed, host, fill) = (hex(seed), hex(host), hex(fill));
            let strength = |color: &str| {
                let overlay = apply_opacity(color, 0.6).unwrap();
                let rendered = render_layers(&host, &[&overlay]).unwrap();
                contrast_ratio(&rendered, &host).unwrap()
                    .min(contrast_ratio(&rendered, &fill).unwrap())
            };
            let seed_strength = strength(&seed);
            let output = fit_control_stroke(&seed, &host, std::slice::from_ref(&fill)).unwrap();

            if seed_strength >= 1.5 {
                prop_assert_eq!(output, seed);
            } else {
                prop_assert!(strength(&output) + 1e-12 >= seed_strength);
            }
        }

        #[test]
        fn control_stroke_is_valid_deterministic_and_independent_of_fill_order(
            seed in any::<[u8; 3]>(),
            host in any::<[u8; 3]>(),
            fills in prop::collection::vec(any::<[u8; 3]>(), 0..8),
        ) {
            let (seed, host) = (hex(seed), hex(host));
            let mut fills = fills.into_iter().map(hex).collect::<Vec<_>>();
            let output = fit_control_stroke(&seed, &host, &fills).unwrap();
            prop_assert!(validate_opaque_hex(&output, "control stroke").is_ok());
            prop_assert_eq!(&output, &fit_control_stroke(&seed, &host, &fills).unwrap());

            fills.reverse();
            fills.extend(fills.clone());
            prop_assert_eq!(output, fit_control_stroke(&seed, &host, &fills).unwrap());
        }
    }

    #[test]
    fn stroke_composites_over_its_underlay_before_comparing_neighbors() {
        let contact = Contact::new("#e8e8e3", "#ffffff", "#d08f79", 0.6).unwrap();
        let groups = [group(Purpose::FilledOutline, vec![contact])];
        let PreparedScenes {
            groups: prepared,
            renderings,
        } = prepare(&groups).unwrap();
        let candidate = ColorMetrics::from_hex("#9c9c95").unwrap();
        let (underlay, opacity) = renderings[0];
        let rendered = ColorMetrics::blend_rgb24(underlay, candidate.rgb24(), opacity).metrics();
        let overlay = apply_opacity("#9c9c95", 0.6).unwrap();
        let expected = render_layers("#e8e8e3", &[&overlay]).unwrap();
        assert_eq!(rendered.rgb24().hex(), expected);
        assert_eq!(prepared[0].contacts[0].outside.rgb24().hex(), "#ffffff");
        assert_ne!(rendered.rgb24(), candidate.rgb24());
        let wrong_underlay =
            ColorMetrics::blend_rgb24(contact.inside, candidate.rgb24(), opacity).metrics();
        assert_ne!(rendered.rgb24(), wrong_underlay.rgb24());
    }

    #[test]
    fn visible_filled_edge_allows_the_stroke_to_match_the_fill() {
        let fill = "#b0b0b0";
        let contact = Contact::new("#ffffff", "#ffffff", fill, 1.0).unwrap();
        let output = fit(fill, &[group(Purpose::FilledOutline, vec![contact])]).unwrap();
        assert_eq!(output, fill);
        assert_eq!(contrast_ratio(&output, fill).unwrap(), 1.0);
    }

    #[test]
    fn same_surface_separator_retains_a_visible_quiet_edge() {
        let base = "#dddddd";
        let contact = Contact::new(base, base, base, 0.6).unwrap();
        let output = fit(base, &[group(Purpose::Separator, vec![contact])]).unwrap();
        let overlay = apply_opacity(&output, 0.6).unwrap();
        let rendered = render_layers(base, &[&overlay]).unwrap();
        let contrast = contrast_ratio(&rendered, base).unwrap();
        assert!((1.16..=1.95).contains(&contrast));
    }

    #[test]
    fn incompatible_neutral_and_mixed_contacts_remain_deterministic() {
        for seed in ["#000000", "#ffffff", "#777777", "#dfe4c4", "#1e2326"] {
            let contacts = ["#000000", "#ffffff", "#777777", "#ff8000"]
                .into_iter()
                .map(|base| Contact::new(base, base, base, 0.6).unwrap())
                .collect();
            let groups = [group(Purpose::Separator, contacts)];
            let first = fit(seed, &groups).unwrap();
            assert_eq!(first, fit(seed, &groups).unwrap());
            validate_opaque_hex(&first, "result").unwrap();
        }
    }

    #[test]
    fn duplicate_contacts_do_not_change_the_group_weight() {
        let light = Contact::new("#eeeeee", "#eeeeee", "#eeeeee", 1.0).unwrap();
        let dark = Contact::new("#333333", "#333333", "#333333", 0.6).unwrap();
        let ordinary = [group(Purpose::Separator, vec![light, dark])];
        let duplicated = [group(Purpose::Separator, vec![light, dark, light, light])];
        assert_eq!(
            fit("#777777", &ordinary).unwrap(),
            fit("#777777", &duplicated).unwrap()
        );
    }

    #[test]
    fn zero_opacity_and_absent_contacts_have_total_fallbacks() {
        let contact = Contact::new("#ffffff", "#ffffff", "#ffffff", 0.0).unwrap();
        let groups = [group(Purpose::Separator, vec![contact])];
        assert_eq!(fit("#8a7654", &groups).unwrap(), "#8a7654");
        assert_eq!(fit("#8a7654", &[]).unwrap(), "#8a7654");
    }
}
