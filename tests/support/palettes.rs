use omarchy_zed_theme::color::gamut_map_oklch;
use omarchy_zed_theme::constants::CANONICAL_COLOR_KEYS;
use omarchy_zed_theme::palette::{Provenance, ResolvedPalette};
use proptest::prelude::*;
use std::collections::BTreeMap;
use std::f64::consts::TAU;

#[derive(Clone, Debug)]
pub struct PaletteSpec {
    dark: bool,
    background_step: u8,
    foreground_step: u8,
    hue_degrees: u16,
    chroma_step: u8,
    semantic_variation: [[i8; 3]; 8],
    provenance_mask: u64,
}

#[derive(Clone, Debug)]
pub enum PaletteMutation {
    Random(BTreeMap<usize, [u8; 3]>),
    CollapseAll([u8; 3]),
    CollapseSurfaces([u8; 3]),
    ForegroundMatchesBackground,
    CollapseSemantics([u8; 3]),
    SelectionMatchesBackground,
    SwapEndpoints,
}

pub fn production_palette_specs() -> impl Strategy<Value = PaletteSpec> {
    (
        any::<bool>(),
        0_u8..=24,
        0_u8..=24,
        0_u16..360,
        0_u8..=8,
        any::<[[i8; 3]; 8]>(),
        prop_oneof![
            2 => Just(0),
            2 => Just(u64::MAX),
            1 => (0..CANONICAL_COLOR_KEYS.len()).prop_map(|index| u64::MAX ^ (1 << index)),
            5 => any::<u64>(),
        ],
    )
        .prop_map(
            |(
                dark,
                background_step,
                foreground_step,
                hue_degrees,
                chroma_step,
                semantic_variation,
                provenance_mask,
            )| PaletteSpec {
                dark,
                background_step,
                foreground_step,
                hue_degrees,
                chroma_step,
                semantic_variation,
                provenance_mask,
            },
        )
}

pub fn arbitrary_resolved_palettes() -> impl Strategy<Value = ResolvedPalette> {
    let length = CANONICAL_COLOR_KEYS.len();
    (
        any::<bool>(),
        prop::collection::vec(edge_rgb(), length..=length),
        prop::collection::vec(
            prop_oneof![
                Just(Provenance::Direct),
                Just(Provenance::Alias),
                Just(Provenance::Derived),
            ],
            length..=length,
        ),
    )
        .prop_map(|(dark, values, provenance)| ResolvedPalette {
            mode: if dark { "dark" } else { "light" }.into(),
            colors: CANONICAL_COLOR_KEYS
                .iter()
                .copied()
                .zip(values.into_iter().map(hex))
                .map(|(key, value)| (key.to_owned(), value))
                .collect(),
            provenance: CANONICAL_COLOR_KEYS
                .iter()
                .copied()
                .zip(provenance)
                .map(|(key, provenance)| (key.to_owned(), provenance))
                .collect(),
        })
}

pub fn pathological_palette_specs() -> impl Strategy<Value = (PaletteSpec, Vec<PaletteMutation>)> {
    (
        production_palette_specs(),
        prop::collection::btree_map(
            0..CANONICAL_COLOR_KEYS.len(),
            edge_rgb(),
            1..=CANONICAL_COLOR_KEYS.len(),
        ),
        prop::array::uniform3(edge_rgb()),
    )
        .prop_map(|(spec, random, [all, surfaces, semantics])| {
            (
                spec,
                vec![
                    PaletteMutation::Random(random),
                    PaletteMutation::CollapseAll(all),
                    PaletteMutation::CollapseSurfaces(surfaces),
                    PaletteMutation::ForegroundMatchesBackground,
                    PaletteMutation::CollapseSemantics(semantics),
                    PaletteMutation::SelectionMatchesBackground,
                    PaletteMutation::SwapEndpoints,
                ],
            )
        })
}

impl PaletteSpec {
    pub fn resolve(&self) -> ResolvedPalette {
        let background = if self.dark {
            0.12 + f64::from(self.background_step) / 400.0
        } else {
            0.94 - f64::from(self.background_step) / 600.0
        };
        let foreground = if self.dark {
            0.86 + f64::from(self.foreground_step) / 400.0
        } else {
            0.18 - f64::from(self.foreground_step) / 600.0
        };
        let direction = if self.dark { 1.0 } else { -1.0 };
        let accent_lightness = if self.dark { 0.72 } else { 0.48 };
        let chroma = 0.09 + f64::from(self.chroma_step) / 100.0;
        let hue = f64::from(self.hue_degrees) * TAU / 360.0;

        let neutral = |lightness| color(lightness, 0.008, hue);
        let semantic_color =
            |index: usize, offset: f64, lightness_offset: f64, chroma_scale: f64| {
                let [hue_step, chroma_step, lightness_step] = self.semantic_variation[index];
                let hue_jitter = f64::from(hue_step) / 127.0 * TAU / 18.0;
                let chroma_jitter = f64::from(chroma_step) / 127.0 * 0.03;
                let lightness_jitter = f64::from(lightness_step) / 127.0 * 0.05;
                color(
                    (accent_lightness + lightness_jitter + lightness_offset).clamp(0.0, 1.0),
                    ((chroma + chroma_jitter) * chroma_scale).max(0.04),
                    hue + offset + hue_jitter,
                )
            };
        let pigment = |index, offset| semantic_color(index, offset, 0.0, 1.0);
        let bright_pigment = |index, offset| semantic_color(index, offset, direction * 0.08, 0.92);

        let values = BTreeMap::from([
            ("background", neutral(background)),
            (
                "dark_background",
                neutral((background - 0.035).clamp(0.0, 1.0)),
            ),
            (
                "darker_background",
                neutral((background - 0.070).clamp(0.0, 1.0)),
            ),
            (
                "lighter_background",
                neutral((background + 0.055).clamp(0.0, 1.0)),
            ),
            ("foreground", neutral(foreground)),
            (
                "dark_foreground",
                neutral((foreground - direction * 0.18).clamp(0.0, 1.0)),
            ),
            (
                "light_foreground",
                neutral((foreground + direction * 0.055).clamp(0.0, 1.0)),
            ),
            (
                "bright_foreground",
                neutral((foreground + direction * 0.10).clamp(0.0, 1.0)),
            ),
            (
                "muted",
                neutral((foreground - direction * 0.28).clamp(0.0, 1.0)),
            ),
            ("red", pigment(0, 0.20)),
            ("orange", pigment(1, 0.75)),
            ("brown", pigment(2, 0.90)),
            ("yellow", pigment(3, 1.35)),
            ("green", pigment(4, 2.35)),
            ("cyan", pigment(5, 3.15)),
            ("blue", pigment(6, 4.15)),
            ("magenta", pigment(7, 5.25)),
            ("bright_red", bright_pigment(0, 0.20)),
            ("bright_yellow", bright_pigment(3, 1.35)),
            ("bright_green", bright_pigment(4, 2.35)),
            ("bright_cyan", bright_pigment(5, 3.15)),
            ("bright_blue", bright_pigment(6, 4.15)),
            ("bright_magenta", bright_pigment(7, 5.25)),
        ]);

        let selection = color(
            (background + direction * 0.16).clamp(0.0, 1.0),
            chroma * 0.45,
            hue + 4.15,
        );
        let aliases = BTreeMap::from([
            ("accent", "blue"),
            ("selection", "selection_background"),
            ("cursor", "magenta"),
            ("color0", "background"),
            ("color1", "red"),
            ("color2", "green"),
            ("color3", "yellow"),
            ("color4", "blue"),
            ("color5", "magenta"),
            ("color6", "cyan"),
            ("color7", "foreground"),
            ("color8", "darker_background"),
            ("color9", "bright_red"),
            ("color10", "bright_green"),
            ("color11", "bright_yellow"),
            ("color12", "bright_blue"),
            ("color13", "bright_magenta"),
            ("color14", "bright_cyan"),
            ("color15", "bright_foreground"),
        ]);

        let mut colors = BTreeMap::new();
        colors.insert("selection".into(), selection.clone());
        colors.insert("selection_background".into(), selection);
        colors.insert(
            "selection_foreground".into(),
            values["bright_foreground"].clone(),
        );
        for key in CANONICAL_COLOR_KEYS {
            if colors.contains_key(*key) {
                continue;
            }
            let source = aliases.get(key).copied().unwrap_or(key);
            colors.insert((*key).to_owned(), values[source].clone());
        }

        let provenance = CANONICAL_COLOR_KEYS
            .iter()
            .enumerate()
            .map(|(index, key)| {
                let source = if aliases.contains_key(key) {
                    Provenance::Alias
                } else if self.provenance_mask & (1 << index) != 0 {
                    Provenance::Derived
                } else {
                    Provenance::Direct
                };
                ((*key).to_owned(), source)
            })
            .collect();

        ResolvedPalette {
            mode: if self.dark { "dark" } else { "light" }.into(),
            colors,
            provenance,
        }
    }
}

pub fn apply_color_mutation(palette: &mut ResolvedPalette, mutation: &PaletteMutation) {
    let set = |palette: &mut ResolvedPalette, key: &str, value: String| {
        palette.colors.insert(key.to_owned(), value);
    };
    match mutation {
        PaletteMutation::Random(values) => {
            for (index, value) in values {
                set(palette, CANONICAL_COLOR_KEYS[*index], hex(*value));
            }
        }
        PaletteMutation::CollapseAll(value) => {
            for key in CANONICAL_COLOR_KEYS {
                set(palette, key, hex(*value));
            }
        }
        PaletteMutation::CollapseSurfaces(value) => {
            for key in [
                "background",
                "dark_background",
                "darker_background",
                "lighter_background",
            ] {
                set(palette, key, hex(*value));
            }
        }
        PaletteMutation::ForegroundMatchesBackground => {
            set(palette, "foreground", palette.colors["background"].clone());
        }
        PaletteMutation::CollapseSemantics(value) => {
            for key in [
                "red", "orange", "brown", "yellow", "green", "cyan", "blue", "magenta",
            ] {
                set(palette, key, hex(*value));
            }
        }
        PaletteMutation::SelectionMatchesBackground => {
            let background = palette.colors["background"].clone();
            set(palette, "selection", background.clone());
            set(palette, "selection_background", background);
        }
        PaletteMutation::SwapEndpoints => {
            let background = palette.colors["background"].clone();
            let foreground = palette.colors["foreground"].clone();
            set(palette, "background", foreground);
            set(palette, "foreground", background);
        }
    }
}

fn edge_rgb() -> impl Strategy<Value = [u8; 3]> {
    prop_oneof![
        2 => Just([0, 0, 0]),
        2 => Just([255, 255, 255]),
        2 => any::<u8>().prop_map(|value| [value, value, value]),
        4 => any::<[u8; 3]>(),
    ]
}

fn hex([red, green, blue]: [u8; 3]) -> String {
    format!("#{red:02x}{green:02x}{blue:02x}")
}

fn color(lightness: f64, chroma: f64, hue: f64) -> String {
    gamut_map_oklch(lightness, chroma, hue)
        .expect("generated OKLCH components are valid")
        .opaque_hex()
}
