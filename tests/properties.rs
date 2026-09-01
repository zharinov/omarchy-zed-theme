mod support;

use omarchy_zed_theme::ErrorKind;
use omarchy_zed_theme::theme::build_theme;
use proptest::prelude::*;
use proptest::test_runner::FileFailurePersistence;
use support::palettes::{
    apply_color_mutation, light_high_chroma_boundary_palette, production_palette_specs,
    structurally_valid_palette_specs,
};

fn property_config(cases: u32, regressions: &'static str) -> ProptestConfig {
    ProptestConfig {
        cases,
        failure_persistence: Some(Box::new(FileFailurePersistence::Direct(regressions))),
        ..ProptestConfig::default()
    }
}

proptest! {
    #![proptest_config(property_config(12, "tests/adversarial-palettes.proptest-regressions"))]

    #[test]
    fn structurally_valid_palettes_fail_honestly(
        (spec, mutation) in structurally_valid_palette_specs()
    ) {
        let mut palette = spec.resolve();
        apply_color_mutation(&mut palette, &mutation);
        prop_assert!(palette.validate().is_ok());
        if let Err(error) = build_theme(&palette) {
            prop_assert_eq!(
                error.kind(),
                ErrorKind::Infeasible,
                "generated palette {:?}, mutation {:?}: {}",
                spec,
                mutation,
                error,
            );
        }
    }
}

#[test]
fn minimized_light_high_chroma_boundary_builds() {
    build_theme(&light_high_chroma_boundary_palette()).unwrap();
}

proptest! {
    #![proptest_config(property_config(8, "tests/production-palettes.proptest-regressions"))]

    #[test]
    fn production_shaped_palettes_build(spec in production_palette_specs()) {
        let palette = spec.resolve();
        prop_assert!(palette.validate().is_ok());
        let document = build_theme(&palette)
            .map_err(|error| TestCaseError::fail(format!("generated palette {spec:?}: {error}")))?;
        let repeated = build_theme(&palette)
            .map_err(|error| TestCaseError::fail(format!("repeated palette {spec:?}: {error}")))?;
        prop_assert_eq!(document, repeated, "generated palette: {:?}", spec);
    }
}
