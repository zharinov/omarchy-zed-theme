mod support;

use omarchy_zed_theme::color::{lab, oklab_to_oklch};
use omarchy_zed_theme::palette::Provenance;
use omarchy_zed_theme::theme::build_theme;
use proptest::prelude::*;
use proptest::test_runner::FileFailurePersistence;
use std::collections::BTreeSet;
use std::f64::consts::TAU;
use support::palettes::{
    PaletteMutation, apply_color_mutation, arbitrary_resolved_palettes, generated_palette_specs,
    pathological_palette_sets,
};
use support::theme_assertions::{assert_document_contract, assert_feasible_theme_contract};

fn property_config(cases: u32, regressions: &'static str) -> ProptestConfig {
    ProptestConfig {
        cases,
        failure_persistence: Some(Box::new(FileFailurePersistence::Direct(regressions))),
        ..ProptestConfig::default()
    }
}

fn serialized_theme(palette: &omarchy_zed_theme::palette::ResolvedPalette) -> Vec<u8> {
    serde_json::to_vec(&build_theme(palette).unwrap()).unwrap()
}

proptest! {
    #![proptest_config(property_config(32, "tests/complete-palettes.proptest-regressions"))]

    #[test]
    fn every_complete_palette_builds_a_stable_document(palette in arbitrary_resolved_palettes()) {
        prop_assert!(palette.validate().is_ok());
        let document = build_theme(&palette)
            .map_err(|error| TestCaseError::fail(format!("generated {palette:?}: {error}")))?;
        assert_document_contract("arbitrary complete palette", &palette, &document);
        prop_assert_eq!(
            serde_json::to_vec(&document).unwrap(),
            serialized_theme(&palette),
            "repeated generation changed serialized bytes for {:?}",
            palette
        );
    }
}

proptest! {
    #![proptest_config(property_config(128, "tests/generated-aliases.proptest-regressions"))]

    #[test]
    fn generated_palette_strategy_preserves_alias_groups(spec in generated_palette_specs()) {
        let palette = spec.resolve();
        for (alias, source) in [
            ("accent", "blue"),
            ("cursor", "magenta"),
            ("selection", "selection_background"),
            ("selection_foreground", "bright_foreground"),
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
        ] {
            prop_assert_eq!(
                &palette.colors[alias],
                &palette.colors[source],
                "generated alias {} drifted from {}",
                alias,
                source
            );
        }
    }
}

proptest! {
    #![proptest_config(property_config(3, "tests/pathological-palettes.proptest-regressions"))]

    #[test]
    fn pathological_palettes_build_deterministically(
        (spec, mutations) in pathological_palette_sets()
    ) {
        for mutation in mutations {
            let mut palette = spec.resolve();
            apply_color_mutation(&mut palette, &mutation);
            prop_assert!(palette.validate().is_ok());
            let document = build_theme(&palette).map_err(|error| TestCaseError::fail(format!(
                "generated palette {spec:?}, mutation {mutation:?}: {error}"
            )))?;
            assert_document_contract("pathological palette", &palette, &document);
            prop_assert_eq!(
                serde_json::to_vec(&document).unwrap(),
                serialized_theme(&palette),
                "mutation {:?} changed serialized bytes",
                mutation
            );
        }
    }
}

proptest! {
    #![proptest_config(property_config(12, "tests/feasible-palettes.proptest-regressions"))]

    #[test]
    fn generated_feasible_palettes_satisfy_the_visual_contract(spec in generated_palette_specs()) {
        let palette = spec.resolve();
        prop_assert!(palette.validate().is_ok());
        let document = build_theme(&palette)
            .map_err(|error| TestCaseError::fail(format!("generated palette {spec:?}: {error}")))?;
        assert_feasible_theme_contract("generated feasible palette", &palette, &document);
        prop_assert_eq!(serde_json::to_vec(&document).unwrap(), serialized_theme(&palette));
    }
}

proptest! {
    #![proptest_config(property_config(6, "tests/thread-determinism.proptest-regressions"))]

    #[test]
    fn theme_bytes_are_thread_count_independent(spec in generated_palette_specs()) {
        let palette = spec.resolve();
        let build = |threads| {
            rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .unwrap()
                .install(|| serialized_theme(&palette))
        };
        prop_assert_eq!(build(1), build(8));
    }
}

proptest! {
    #![proptest_config(property_config(6, "tests/derived-provenance.proptest-regressions"))]

    #[test]
    fn palettes_without_authored_syntax_sources_keep_semantic_domains(
        spec in generated_palette_specs()
    ) {
        let mut palette = spec.resolve();
        for provenance in palette.provenance.values_mut() {
            *provenance = Provenance::Derived;
        }
        let document = build_theme(&palette).unwrap();
        assert_document_contract("all-derived palette", &palette, &document);
        let syntax = document["themes"][0]["style"]["syntax"].as_object().unwrap();
        let roots = ["string", "type", "keyword"]
            .map(|capture| syntax[capture]["color"].as_str().unwrap())
            .into_iter()
            .collect::<BTreeSet<_>>();
        prop_assert_eq!(roots.len(), 3);
    }
}

proptest! {
    #![proptest_config(property_config(6, "tests/narrow-hues.proptest-regressions"))]

    #[test]
    fn narrow_authored_palettes_do_not_synthesize_hues(
        spec in generated_palette_specs(),
        hue_degrees in 0_u16..360,
        spread_degrees in 0_u8..=24,
        chroma_step in 0_u8..=12,
    ) {
        let mut palette = spec.resolve();
        apply_color_mutation(
            &mut palette,
            &PaletteMutation::NarrowSemantics {
                hue_degrees,
                spread_degrees,
                chroma_step,
            },
        );
        for provenance in palette.provenance.values_mut() {
            *provenance = Provenance::Direct;
        }
        let document = build_theme(&palette).unwrap();
        let syntax = document["themes"][0]["style"]["syntax"].as_object().unwrap();
        let source_hues = [
            "foreground", "green", "blue", "magenta", "yellow", "red", "cyan", "orange",
            "accent", "brown", "bright_green", "bright_blue", "bright_magenta",
            "bright_yellow", "bright_red", "bright_cyan",
        ]
        .map(|key| oklab_to_oklch(lab(&palette.colors[key]).unwrap()))
        .into_iter()
        .filter(|source| source[1] >= 0.025)
        .map(|source| source[2])
        .collect::<Vec<_>>();
        for capture in [
            "string", "constant", "type", "function", "property", "keyword", "link_text",
        ] {
            let output = oklab_to_oklch(lab(syntax[capture]["color"].as_str().unwrap()).unwrap());
            if output[1] < 0.025 {
                continue;
            }
            let nearest = source_hues
                .iter()
                .map(|hue| (output[2] - hue).abs().min(TAU - (output[2] - hue).abs()))
                .fold(f64::INFINITY, f64::min);
            prop_assert!(nearest <= 0.08, "{capture} synthesized a new hue");
        }
    }
}

proptest! {
    #![proptest_config(property_config(6, "tests/neutral-palettes.proptest-regressions"))]

    #[test]
    fn neutral_authored_palettes_keep_syntax_achromatic(
        spec in generated_palette_specs(),
        lightness_step in 0_u8..=24,
    ) {
        let mut palette = spec.resolve();
        apply_color_mutation(
            &mut palette,
            &PaletteMutation::NeutralSemantics { lightness_step },
        );
        for provenance in palette.provenance.values_mut() {
            *provenance = Provenance::Direct;
        }
        let document = build_theme(&palette).unwrap();
        let syntax = document["themes"][0]["style"]["syntax"].as_object().unwrap();
        for capture in [
            "comment", "string", "constant", "type", "function", "property", "keyword",
        ] {
            let color = syntax[capture]["color"].as_str().unwrap();
            prop_assert!(
                oklab_to_oklch(lab(color).unwrap())[1] <= 0.012 + 1e-9,
                "{capture} introduced chroma into a neutral palette"
            );
        }
    }
}
