use omarchy_zed_theme::ErrorKind;
use omarchy_zed_theme::color::{contrast_ratio, gamut_map_oklch, lab, oklab_to_oklch};
use omarchy_zed_theme::constants::CANONICAL_COLOR_KEYS;
use omarchy_zed_theme::palette::{Provenance, ResolvedPalette, resolve_palette};
use omarchy_zed_theme::publish::{atomic_write_file, generate_and_publish};
use omarchy_zed_theme::search::Search;
use omarchy_zed_theme::syntax::profile::measure;
use omarchy_zed_theme::syntax::{SyntaxContexts, build_syntax, contrast_floor};
use omarchy_zed_theme::theme::build_theme;
use serde_json::Value;
use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::fs::symlink;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn temporary(name: &str) -> PathBuf {
    let sequence = TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "omarchy-zed-theme-test-{}-{sequence}-{name}",
        std::process::id()
    ))
}

fn style(document: &Value) -> &serde_json::Map<String, Value> {
    document["themes"][0]["style"].as_object().unwrap()
}

fn synthetic_palette() -> ResolvedPalette {
    let value = |key: &str| match key {
        "background" | "color0" => "#1e2326",
        "dark_background" => "#181b1c",
        "darker_background" | "color8" => "#121516",
        "lighter_background" => "#272e33",
        "foreground" | "color7" => "#d3c6aa",
        "dark_foreground" => "#859289",
        "light_foreground" | "color15" => "#e4e1cd",
        "bright_foreground" => "#fdf6e3",
        "accent" | "blue" | "bright_blue" | "color4" | "color12" => "#7fbbb3",
        "selection" | "selection_background" => "#4f5b58",
        "selection_foreground" => "#fdf6e3",
        "muted" => "#9da9a0",
        "cursor" => "#d699b6",
        "red" | "bright_red" | "color1" | "color9" => "#e67e80",
        "yellow" | "bright_yellow" | "color3" | "color11" => "#dbbc7f",
        "orange" | "brown" => "#e69875",
        "green" | "bright_green" | "color2" | "color10" => "#a7c080",
        "cyan" | "bright_cyan" | "color6" | "color14" => "#83c092",
        "magenta" | "bright_magenta" | "color5" | "color13" => "#d699b6",
        _ => unreachable!("missing synthetic color for {key}"),
    };
    ResolvedPalette {
        mode: "dark".into(),
        colors: CANONICAL_COLOR_KEYS
            .iter()
            .map(|key| ((*key).to_owned(), value(key).to_owned()))
            .collect(),
        provenance: CANONICAL_COLOR_KEYS
            .iter()
            .map(|key| ((*key).to_owned(), Provenance::Direct))
            .collect(),
    }
}

fn write_test_resolver(path: &std::path::Path, palette: &ResolvedPalette) {
    let mut output = String::new();
    for (key, value) in &palette.colors {
        output.push_str(&format!("{key}\t{value}\n"));
    }
    output.push_str(&format!("mode\t{}\n", palette.mode));
    fs::write(path.with_extension("output"), output).unwrap();
    fs::write(
        path,
        "#!/usr/bin/env bash\nset -euo pipefail\ncat -- \"${BASH_SOURCE[0]}.output\"\n",
    )
    .unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn malformed_resolved_palettes_are_rejected() {
    let mut missing = synthetic_palette();
    missing.colors.remove("foreground");

    let missing_error = build_theme(&missing).unwrap_err();
    assert_eq!(missing_error.kind(), ErrorKind::InvalidInput);
    assert!(
        missing_error
            .to_string()
            .contains("omitted canonical keys: foreground")
    );

    let mut invalid_mode = synthetic_palette();
    invalid_mode.mode = "sepia".into();

    assert!(
        build_theme(&invalid_mode)
            .unwrap_err()
            .to_string()
            .contains("resolved mode must be 'dark' or 'light'")
    );

    let mut invalid_color = synthetic_palette();
    invalid_color
        .colors
        .insert("foreground".into(), "#€éa".into());

    assert!(build_theme(&invalid_color).is_err());

    let mut missing_provenance = synthetic_palette();
    missing_provenance.provenance.remove("green");

    let profile_error = measure(&missing_provenance).unwrap_err();
    assert_eq!(profile_error.kind(), ErrorKind::InvalidInput);
    assert!(profile_error.to_string().contains("provenance: green"));

    let mut missing_syntax_color = synthetic_palette();
    missing_syntax_color.colors.remove("muted");
    let mut search = Search::default();

    let syntax_error = build_syntax(
        &mut search,
        &missing_syntax_color,
        SyntaxContexts {
            ordinary: &[],
            rendered: &[],
        },
        "#ffffff",
        "#ffffff",
        ["#00ff00", "#ffff00", "#ff0000"],
    )
    .unwrap_err();
    assert_eq!(syntax_error.kind(), ErrorKind::InvalidInput);
    assert!(syntax_error.to_string().contains("canonical keys: muted"));
}

#[test]
fn syntax_inputs_must_be_opaque_colors() {
    let palette = synthetic_palette();
    let invalid = |ordinary: Vec<String>,
                   rendered: Vec<String>,
                   saliency_reference: &str,
                   predictive: &str,
                   diff_sources: [&str; 3]| {
        build_syntax(
            &mut Search::default(),
            &palette,
            SyntaxContexts {
                ordinary: &ordinary,
                rendered: &rendered,
            },
            saliency_reference,
            predictive,
            diff_sources,
        )
        .unwrap_err()
    };
    let opaque_context = || vec!["#101010".to_owned()];
    let opaque_diff = ["#00aa00", "#aaaa00", "#aa0000"];

    for error in [
        invalid(
            opaque_context(),
            opaque_context(),
            "#ffffff80",
            "#ffffff",
            opaque_diff,
        ),
        invalid(
            opaque_context(),
            opaque_context(),
            "#ffffff",
            "#ffffff80",
            opaque_diff,
        ),
        invalid(
            opaque_context(),
            opaque_context(),
            "#ffffff",
            "#ffffff",
            ["#00aa0080", "#aaaa00", "#aa0000"],
        ),
        invalid(
            vec!["#10101080".to_owned()],
            opaque_context(),
            "#ffffff",
            "#ffffff",
            opaque_diff,
        ),
        invalid(
            opaque_context(),
            vec!["#10101080".to_owned()],
            "#ffffff",
            "#ffffff",
            opaque_diff,
        ),
    ] {
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert!(error.to_string().contains("six-digit hex color"));
    }
}

#[test]
fn missing_palette_file_is_an_external_failure() {
    let missing = temporary("missing-palette");
    let error = resolve_palette(&missing, None).unwrap_err();

    assert_eq!(error.kind(), ErrorKind::External);
    assert!(error.to_string().contains(&missing.display().to_string()));
}

#[test]
fn generation_cache_uses_effective_inputs() {
    let root = temporary("generation-cache");
    let colors = root.join("colors.toml");
    let resolver = root.join("resolver");
    let output = root.join("themes");
    fs::create_dir(&root).unwrap();
    fs::write(&colors, "unused by the test resolver\n").unwrap();

    let mut palette = synthetic_palette();
    write_test_resolver(&resolver, &palette);

    let first = generate_and_publish(&colors, Some(&output), Some(&resolver)).unwrap();
    let second = generate_and_publish(&colors, Some(&output), Some(&resolver)).unwrap();

    assert!(!first.cached);
    assert!(second.cached);

    palette.colors.insert("red".into(), "#ff5555".into());
    write_test_resolver(&resolver, &palette);

    let invalidated = generate_and_publish(&colors, Some(&output), Some(&resolver)).unwrap();

    assert!(!invalidated.cached);

    let target = output.join("omarchy.json");
    let generated = fs::read(&target).unwrap();

    fs::write(&target, b"tampered theme\n").unwrap();

    let repaired = generate_and_publish(&colors, Some(&output), Some(&resolver)).unwrap();

    assert!(!repaired.cached);
    assert_eq!(fs::read(&target).unwrap(), generated);

    let cache = output.join(".omarchy-zed-theme.cache");
    let victim = root.join("cache-victim");
    fs::remove_file(&cache).unwrap();
    fs::write(&victim, "keep\n").unwrap();
    symlink(&victim, &cache).unwrap();

    palette.colors.insert("green".into(), "#55ff55".into());
    write_test_resolver(&resolver, &palette);

    let changed = generate_and_publish(&colors, Some(&output), Some(&resolver)).unwrap();

    assert!(!changed.cached);
    assert_eq!(fs::read_to_string(victim).unwrap(), "keep\n");
    assert!(
        fs::symlink_metadata(&cache)
            .unwrap()
            .file_type()
            .is_symlink()
    );

    fs::remove_dir_all(root).unwrap();
}

fn narrow_multicluster_palette() -> ResolvedPalette {
    let mut palette = synthetic_palette();
    for (key, value) in [
        ("background", "#0B0C16"),
        ("color0", "#0B0C16"),
        ("dark_background", "#080910"),
        ("darker_background", "#06060c"),
        ("color8", "#06060c"),
        ("lighter_background", "#151828"),
        ("foreground", "#ddf7ff"),
        ("color7", "#ddf7ff"),
        ("dark_foreground", "#6a6e95"),
        ("light_foreground", "#b5c5db"),
        ("bright_foreground", "#ddf7ff"),
        ("color15", "#ddf7ff"),
        ("accent", "#82FB9C"),
        ("selection", "#1f253a"),
        ("selection_background", "#1f253a"),
        ("selection_foreground", "#ddf7ff"),
        ("muted", "#2d3450"),
        ("cursor", "#82FB9C"),
        ("red", "#50f872"),
        ("color1", "#50f872"),
        ("yellow", "#50f7d4"),
        ("color3", "#50f7d4"),
        ("orange", "#50f7a3"),
        ("green", "#4fe88f"),
        ("color2", "#4fe88f"),
        ("cyan", "#7cf8f7"),
        ("color6", "#7cf8f7"),
        ("blue", "#829dd4"),
        ("color4", "#829dd4"),
        ("magenta", "#86a7df"),
        ("color5", "#86a7df"),
        ("brown", "#287b51"),
        ("bright_red", "#85ff9d"),
        ("color9", "#85ff9d"),
        ("bright_yellow", "#a4ffec"),
        ("color11", "#a4ffec"),
        ("bright_green", "#9cf7c2"),
        ("color10", "#9cf7c2"),
        ("bright_cyan", "#d1fffe"),
        ("color14", "#d1fffe"),
        ("bright_blue", "#c4d2ed"),
        ("color12", "#c4d2ed"),
        ("bright_magenta", "#cddbf4"),
        ("color13", "#cddbf4"),
    ] {
        palette.colors.insert(key.into(), value.into());
    }
    palette
}

#[test]
fn strong_two_cluster_profile_assigns_distinct_semantic_roots() {
    let mut palette = synthetic_palette();
    for (key, value) in [
        ("green", "#ff0000"),
        ("blue", "#00ff00"),
        ("magenta", "#ffff00"),
        ("yellow", "#ff0000"),
        ("red", "#00ff00"),
        ("cyan", "#ffff00"),
        ("orange", "#ff0000"),
        ("accent", "#00ff00"),
    ] {
        palette.colors.insert(key.into(), value.into());
    }

    let document = build_theme(&palette).unwrap();
    let syntax = style(&document)["syntax"].as_object().unwrap();

    assert_ne!(syntax["string"]["color"], syntax["type"]["color"]);
}

#[test]
fn narrow_multicluster_palette_uses_a_fixed_semantic_forest() {
    let document = build_theme(&narrow_multicluster_palette()).unwrap();

    let style = style(&document);
    let syntax = style["syntax"].as_object().unwrap();
    let editor_foreground = style["editor.foreground"].as_str().unwrap();
    let editor_background = style["editor.background"].as_str().unwrap();
    assert_eq!(
        syntax["variable"]["color"].as_str(),
        Some(editor_foreground)
    );
    assert_eq!(
        syntax["comment"]["color"],
        syntax["punctuation.delimiter"]["color"]
    );
    assert_ne!(syntax["comment"]["color"], syntax["string"]["color"]);

    let visible_channels = ["variable", "function", "type", "property", "string"]
        .map(|capture| syntax[capture]["color"].as_str().unwrap())
        .into_iter()
        .collect::<BTreeSet<_>>();
    assert!((3..=7).contains(&visible_channels.len()));
    for capture in ["function", "type", "property", "string"] {
        let color = syntax[capture]["color"].as_str().unwrap();
        assert!(
            contrast_ratio(color, editor_background).unwrap()
                >= contrast_floor(capture).unwrap() - 1e-9,
            "syntax.{capture} lost its ordinary-editor contrast floor"
        );
    }
}

#[test]
fn palette_native_syntax_preserves_branch_sources() {
    let document = build_theme(&synthetic_palette()).unwrap();
    let syntax = style(&document)["syntax"].as_object().unwrap();

    assert_ne!(syntax["string"]["color"], syntax["type"]["color"]);
    assert_ne!(syntax["type"]["color"], syntax["function"]["color"]);
}

#[test]
fn a_scarce_but_perceptible_authored_hue_is_not_discarded() {
    let mut palette = synthetic_palette();
    for key in [
        "green",
        "blue",
        "magenta",
        "yellow",
        "red",
        "cyan",
        "orange",
        "accent",
        "brown",
        "bright_green",
        "bright_blue",
        "bright_magenta",
        "bright_yellow",
        "bright_red",
        "bright_cyan",
    ] {
        palette.provenance.insert(key.into(), Provenance::Derived);
    }
    palette.colors.insert(
        "accent".into(),
        gamut_map_oklch(0.65, 0.03, 0.2).unwrap().opaque_hex(),
    );
    palette
        .provenance
        .insert("accent".into(), Provenance::Direct);

    let document = build_theme(&palette).unwrap();
    let style = style(&document);

    assert_ne!(
        style["syntax"]["string"]["color"],
        style["editor.foreground"]
    );
    let syntax = style["syntax"].as_object().unwrap();
    let semantic_roots = ["string", "type", "keyword"]
        .map(|capture| syntax[capture]["color"].as_str().unwrap())
        .into_iter()
        .collect::<BTreeSet<_>>();
    assert_eq!(semantic_roots.len(), 3);
}

#[test]
fn syntax_profile_does_not_reassign_diff_source_families() {
    let authored = synthetic_palette();
    let mut neutral_profile = authored.clone();
    for provenance in neutral_profile.provenance.values_mut() {
        *provenance = Provenance::Derived;
    }

    let authored_document = build_theme(&authored).unwrap();
    let neutral_document = build_theme(&neutral_profile).unwrap();

    let authored_syntax = style(&authored_document)["syntax"].as_object().unwrap();
    let neutral_syntax = style(&neutral_document)["syntax"].as_object().unwrap();
    for capture in ["diff.plus", "diff.minus", "diff"] {
        assert_eq!(
            authored_syntax[capture]["color"], neutral_syntax[capture]["color"],
            "{capture} changed with profile-only provenance"
        );
    }

    for (capture, source_key) in [
        ("diff.plus", "green"),
        ("diff.minus", "red"),
        ("diff", "yellow"),
    ] {
        let source = oklab_to_oklch(lab(&authored.colors[source_key]).unwrap());
        let output =
            oklab_to_oklch(lab(authored_syntax[capture]["color"].as_str().unwrap()).unwrap());
        let hue_distance = (source[2] - output[2])
            .abs()
            .min(std::f64::consts::TAU - (source[2] - output[2]).abs());

        assert!(
            hue_distance < 0.03,
            "{capture} left the {source_key} hue family"
        );
    }
}

#[test]
fn atomic_writer_rejects_final_symlink() {
    let root = temporary("symlink");
    let themes = root.join("themes");

    fs::create_dir_all(&themes).unwrap();

    let victim = root.join("victim");
    fs::write(&victim, b"keep\n").unwrap();

    let target = themes.join("omarchy.json");
    symlink(&victim, &target).unwrap();

    let error = atomic_write_file(&target, b"replace\n").unwrap_err();

    assert!(error.to_string().contains("symlink"));
    assert_eq!(fs::read(&victim).unwrap(), b"keep\n");

    fs::remove_dir_all(root).unwrap();
}
