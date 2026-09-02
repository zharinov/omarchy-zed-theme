mod support;

use omarchy_zed_theme::theme::build_theme;
use proptest::prelude::*;
use proptest::test_runner::FileFailurePersistence;
use support::palettes::{
    apply_color_mutation, arbitrary_resolved_palettes, pathological_palette_specs,
    production_palette_specs,
};

fn property_config(cases: u32, regressions: &'static str) -> ProptestConfig {
    ProptestConfig {
        cases,
        failure_persistence: Some(Box::new(FileFailurePersistence::Direct(regressions))),
        ..ProptestConfig::default()
    }
}

proptest! {
    #![proptest_config(property_config(8, "tests/adversarial-palettes.proptest-regressions"))]

    #[test]
    fn arbitrary_complete_palettes_always_build(palette in arbitrary_resolved_palettes()) {
        prop_assert!(palette.validate().is_ok());
        let document = build_theme(&palette)
            .map_err(|error| TestCaseError::fail(format!(
                "generated complete palette {palette:?}: {error}"
            )))?;
        let repeated = build_theme(&palette)
            .map_err(|error| TestCaseError::fail(format!(
                "repeated complete palette {palette:?}: {error}"
            )))?;
        prop_assert_eq!(&document, &repeated);
        prop_assert_eq!(document["themes"][0]["appearance"].as_str(), Some(palette.mode.as_str()));
        prop_assert_eq!(document["themes"][0]["style"]["players"].as_array().map(Vec::len), Some(8));
        prop_assert_eq!(
            document["themes"][0]["style"]["syntax"].as_object().map(serde_json::Map::len),
            Some(omarchy_zed_theme::syntax::policy::CAPTURE_POLICIES.len())
        );
    }
}

proptest! {
    #![proptest_config(property_config(3, "tests/pathological-palettes.proptest-regressions"))]

    #[test]
    fn every_pathological_mutation_class_always_builds(
        (spec, mutations) in pathological_palette_specs()
    ) {
        for mutation in mutations {
            let mut palette = spec.resolve();
            apply_color_mutation(&mut palette, &mutation);
            prop_assert!(palette.validate().is_ok());
            build_theme(&palette).map_err(|error| TestCaseError::fail(format!(
                "generated palette {spec:?}, mutation {mutation:?}: {error}"
            )))?;
        }
    }
}

proptest! {
    #![proptest_config(property_config(4, "tests/production-palettes.proptest-regressions"))]

    #[test]
    fn production_shaped_palettes_build(spec in production_palette_specs()) {
        let palette = spec.resolve();
        prop_assert!(palette.validate().is_ok());
        let document = build_theme(&palette)
            .map_err(|error| TestCaseError::fail(format!("generated palette {spec:?}: {error}")))?;
        prop_assert_eq!(document["themes"][0]["appearance"].as_str(), Some(palette.mode.as_str()));
    }
}
