use omarchy_zed_theme::ErrorKind;
use omarchy_zed_theme::color::{
    contrast_ratio, delta_e, gamut_map_oklch, lab, oklab_to_oklch, parse_hex, render_layers,
};
use omarchy_zed_theme::constants::{
    CANONICAL_COLOR_KEYS, DARK_DIFF_BORDER_OPACITY, DARK_DIFF_HOLLOW_OPACITY,
    DARK_DIFF_LINE_OPACITY, DARK_DIFF_WORD_OPACITY, DIFF_NORMAL_FLOOR_DELTA_E, HARD_TEXT_CONTRAST,
    LIGHT_DIFF_BORDER_OPACITY, LIGHT_DIFF_HOLLOW_OPACITY, LIGHT_DIFF_LINE_OPACITY,
    LIGHT_DIFF_WORD_OPACITY, SEMANTIC_PAIR_CONTRACT, SYNTAX_DIFF_CONTRACT,
};
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

fn role<'a>(style: &'a serde_json::Map<String, Value>, name: &str) -> &'a str {
    style[name].as_str().unwrap()
}

fn assert_rendered_diff_edge(name: &str, document: &Value) {
    let style = style(document);
    let base = role(style, "editor.background");
    let added = render_layers(base, &[role(style, "editor.diff_hunk.added.background")]).unwrap();
    let deleted =
        render_layers(base, &[role(style, "editor.diff_hunk.deleted.background")]).unwrap();
    let distance = delta_e(&added, &deleted).unwrap();
    assert!(
        distance >= DIFF_NORMAL_FLOOR_DELTA_E - 1e-9,
        "{name}: touching add/delete fills have delta E {distance:.4}"
    );
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

fn venice_like_palette() -> ResolvedPalette {
    let mut palette = synthetic_palette();
    palette.mode = "light".into();
    for (key, value) in [
        ("background", "#f5f2e9"),
        ("dark_background", "#b8b6af"),
        ("darker_background", "#7a7974"),
        ("lighter_background", "#f6f3eb"),
        ("foreground", "#492924"),
        ("dark_foreground", "#361e1b"),
        ("light_foreground", "#644945"),
        ("bright_foreground", "#765e5b"),
        ("accent", "#72684b"),
        ("selection", "#c3c0b8"),
        ("selection_background", "#c3c0b8"),
        ("selection_foreground", "#492924"),
        ("muted", "#85837d"),
        ("cursor", "#72684b"),
        ("red", "#706548"),
        ("yellow", "#6f6645"),
        ("orange", "#706547"),
        ("green", "#6f6644"),
        ("cyan", "#726947"),
        ("blue", "#72684b"),
        ("magenta", "#796e51"),
        ("brown", "#3e3827"),
        ("bright_red", "#948962"),
        ("bright_yellow", "#938a5e"),
        ("bright_green", "#938a5c"),
        ("bright_cyan", "#968d60"),
        ("bright_blue", "#968c65"),
        ("bright_magenta", "#9e926c"),
    ] {
        palette.colors.insert(key.into(), value.into());
    }

    for (key, value) in [
        ("color1", "#706548"),
        ("color2", "#6f6644"),
        ("color3", "#6f6645"),
        ("color4", "#72684b"),
        ("color5", "#796e51"),
        ("color6", "#726947"),
        ("color9", "#948962"),
        ("color10", "#938a5c"),
        ("color11", "#938a5e"),
        ("color12", "#968c65"),
        ("color13", "#9e926c"),
        ("color14", "#968d60"),
    ] {
        palette.colors.insert(key.into(), value.into());
    }
    palette
}

fn parse_palette_fixture(value: &Value) -> (String, ResolvedPalette) {
    let object = value
        .as_object()
        .expect("palette fixture must be an object");
    let name = object["name"]
        .as_str()
        .expect("palette fixture must have a name")
        .to_owned();
    let mode = object["mode"]
        .as_str()
        .expect("palette fixture must have a mode")
        .to_owned();
    let colors = object["colors"]
        .as_object()
        .expect("palette fixture must have colors")
        .iter()
        .map(|(key, value)| {
            (
                key.clone(),
                value
                    .as_str()
                    .unwrap_or_else(|| panic!("{name}: {key} must be a string"))
                    .to_owned(),
            )
        })
        .collect();
    let provenance = object["provenance"]
        .as_object()
        .expect("palette fixture must have provenance")
        .iter()
        .map(|(key, value)| {
            let provenance = match value.as_str() {
                Some("direct") => Provenance::Direct,
                Some("alias") => Provenance::Alias,
                Some("derived") => Provenance::Derived,
                other => panic!("{name}: invalid provenance for {key}: {other:?}"),
            };
            (key.clone(), provenance)
        })
        .collect();

    (
        name,
        ResolvedPalette {
            mode,
            colors,
            provenance,
        },
    )
}

#[test]
fn dark_and_light_fixture_palettes_generate_valid_themes() {
    let fixtures: Value =
        serde_json::from_str(include_str!("fixtures/resolved-palettes.json")).unwrap();
    assert_eq!(fixtures["version"].as_u64(), Some(1));
    for name in ["matte-black", "white"] {
        let fixture = fixtures["palettes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|fixture| fixture["name"].as_str() == Some(name))
            .unwrap_or_else(|| panic!("missing fixture palette {name}"));
        let (name, palette) = parse_palette_fixture(fixture);
        let document = build_theme(&palette).unwrap_or_else(|error| panic!("{name}: {error}"));
        assert_rendered_diff_edge(&name, &document);
    }
}

#[test]
fn installed_theme_corpus_generates_when_configured() {
    let Some(root) = std::env::var_os("OMARCHY_THEMES_DIR").map(PathBuf::from) else {
        return;
    };
    let mut themes = fs::read_dir(&root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.join("colors.toml").is_file())
        .collect::<Vec<_>>();
    themes.sort();
    assert!(!themes.is_empty());
    for theme in themes {
        let name = theme.file_name().unwrap().to_string_lossy();
        let palette = resolve_palette(&theme.join("colors.toml"), None)
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        let document = build_theme(&palette).unwrap_or_else(|error| panic!("{name}: {error}"));
        assert_rendered_diff_edge(&name, &document);
    }
}

#[test]
fn pinned_external_corpus_generates_when_configured() {
    let Some(root) = std::env::var_os("OMARCHY_ZED_EXTERNAL_CORPUS").map(PathBuf::from) else {
        return;
    };
    let mut tested = 0;
    for line in include_str!("external-corpus.tsv")
        .lines()
        .filter(|line| !line.starts_with('#'))
    {
        let columns = line.split('\t').collect::<Vec<_>>();
        let [name, _, commit, palette_path] = columns.as_slice() else {
            panic!("invalid external corpus row: {line}");
        };
        let checkout = root.join(name);
        let output = std::process::Command::new("git")
            .args(["-C"])
            .arg(&checkout)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap_or_else(|error| panic!("{name}: cannot inspect revision: {error}"));
        assert!(output.status.success(), "{name}: git rev-parse failed");
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), *commit);
        let palette = resolve_palette(&checkout.join(palette_path), None)
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        let document = build_theme(&palette).unwrap_or_else(|error| panic!("{name}: {error}"));
        assert_rendered_diff_edge(name, &document);
        tested += 1;
    }
    assert_eq!(tested, 16);
}

#[test]
fn rendered_diff_hierarchy_survives_dark_and_light_palettes() {
    let fixtures: Value =
        serde_json::from_str(include_str!("fixtures/resolved-palettes.json")).unwrap();
    for name in ["matte-black", "white"] {
        let fixture = fixtures["palettes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|fixture| fixture["name"] == name)
            .unwrap();
        let (_, palette) = parse_palette_fixture(fixture);
        let document = build_theme(&palette).unwrap();
        let style = style(&document);
        let base = style["editor.background"].as_str().unwrap();
        let expected_opacities = if palette.mode == "light" {
            [
                LIGHT_DIFF_LINE_OPACITY,
                LIGHT_DIFF_HOLLOW_OPACITY,
                LIGHT_DIFF_BORDER_OPACITY,
                LIGHT_DIFF_WORD_OPACITY,
            ]
        } else {
            [
                DARK_DIFF_LINE_OPACITY,
                DARK_DIFF_HOLLOW_OPACITY,
                DARK_DIFF_BORDER_OPACITY,
                DARK_DIFF_WORD_OPACITY,
            ]
        };
        for (family, status) in [("added", "created"), ("deleted", "deleted")] {
            let line_role = format!("editor.diff_hunk.{family}.background");
            let hollow_role = format!("editor.diff_hunk.{family}.hollow_background");
            let border_role = format!("editor.diff_hunk.{family}.hollow_border");
            let word_role = format!(
                "version_control.word_{}",
                if family == "added" {
                    "added"
                } else {
                    "deleted"
                }
            );
            for (role, expected) in [
                (&line_role, expected_opacities[0]),
                (&hollow_role, expected_opacities[1]),
                (&border_role, expected_opacities[2]),
                (&word_role, expected_opacities[3]),
            ] {
                let actual = parse_hex(style[role].as_str().unwrap()).unwrap().alpha();
                assert_eq!((actual * 255.0).round(), (expected * 255.0).round());
            }

            for hunk_role in [&line_role, &hollow_role] {
                let hunk = render_layers(base, &[style[hunk_role].as_str().unwrap()]).unwrap();
                for highlight in [
                    "search.match_background",
                    "search.active_match_background",
                    "editor.document_highlight.read_background",
                    "editor.document_highlight.write_background",
                    "editor.document_highlight.bracket_background",
                    "vim.yank.background",
                ] {
                    let underlay =
                        render_layers(&hunk, &[style[highlight].as_str().unwrap()]).unwrap();
                    let word =
                        render_layers(&underlay, &[style[&word_role].as_str().unwrap()]).unwrap();
                    assert!(delta_e(&underlay, &word).unwrap() > 0.0);
                    assert!(
                        contrast_ratio(style[status].as_str().unwrap(), &word).unwrap()
                            >= HARD_TEXT_CONTRAST - 1e-9
                    );
                }
            }
        }

        let vcs_added = style["version_control.added"].as_str().unwrap();
        let vcs_deleted = style["version_control.deleted"].as_str().unwrap();
        assert!(delta_e(vcs_added, vcs_deleted).unwrap() >= SEMANTIC_PAIR_CONTRACT.normal_delta_e);
        assert!(
            omarchy_zed_theme::search::cvd_distance(vcs_added, vcs_deleted).unwrap()
                >= SEMANTIC_PAIR_CONTRACT.cvd_delta_e
        );
        let syntax_added = style["syntax"]["diff.plus"]["color"].as_str().unwrap();
        let syntax_deleted = style["syntax"]["diff.minus"]["color"].as_str().unwrap();
        assert!(
            delta_e(syntax_added, syntax_deleted).unwrap() >= SYNTAX_DIFF_CONTRACT.normal_delta_e
        );
        assert!(
            omarchy_zed_theme::search::cvd_distance(syntax_added, syntax_deleted).unwrap()
                >= SYNTAX_DIFF_CONTRACT.cvd_delta_e
        );
        assert_eq!(style["success"], style["created"]);
        assert_eq!(style["error"], style["deleted"]);
    }
}

#[test]
fn neutral_profile_expresses_fixed_semantic_domains_as_tones() {
    let fixtures: Value =
        serde_json::from_str(include_str!("fixtures/resolved-palettes.json")).unwrap();
    let name = "white";
    let fixture = fixtures["palettes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|fixture| fixture["name"] == name)
        .unwrap();
    let (_, palette) = parse_palette_fixture(fixture);
    let document = build_theme(&palette).unwrap();
    let style = style(&document);
    let syntax = style["syntax"].as_object().unwrap();
    let editor_foreground = role(style, "editor.foreground");
    let semantic_roots = ["string", "type", "keyword"]
        .map(|capture| syntax[capture]["color"].as_str().unwrap())
        .into_iter()
        .collect::<BTreeSet<_>>();
    assert_eq!(semantic_roots.len(), 3);
    assert!(!semantic_roots.contains(editor_foreground));
    for capture in [
        "comment", "string", "constant", "type", "function", "property", "keyword",
    ] {
        let color = syntax[capture]["color"].as_str().unwrap();
        assert!(
            oklab_to_oklch(lab(color).unwrap())[1] <= 0.005 + 1e-9,
            "{name}: {capture}"
        );
    }
}

#[test]
fn palette_without_authored_syntax_sources_keeps_fixed_semantic_domains() {
    let fixtures: Value =
        serde_json::from_str(include_str!("fixtures/resolved-palettes.json")).unwrap();
    let fixture = fixtures["palettes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|fixture| fixture["name"] == "white")
        .unwrap();
    let (_, mut palette) = parse_palette_fixture(fixture);
    for provenance in palette.provenance.values_mut() {
        *provenance = Provenance::Derived;
    }

    let document = build_theme(&palette).unwrap();
    let syntax = style(&document)["syntax"].as_object().unwrap();
    let semantic_roots = ["string", "type", "keyword"]
        .map(|capture| syntax[capture]["color"].as_str().unwrap())
        .into_iter()
        .collect::<BTreeSet<_>>();
    assert_eq!(semantic_roots.len(), 3);
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
fn theme_bytes_are_thread_count_independent() {
    let palette = synthetic_palette();
    let build = |threads| {
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .unwrap()
            .install(|| build_theme(&palette).unwrap())
    };
    let one_document = build(1);
    let eight_document = build(8);
    assert_eq!(
        serde_json::to_vec(&one_document).unwrap(),
        serde_json::to_vec(&eight_document).unwrap()
    );
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
fn narrow_olive_palette_never_synthesizes_hues() {
    let palette = venice_like_palette();
    let document = build_theme(&palette).unwrap();
    let syntax = style(&document)["syntax"].as_object().unwrap();
    let captures = [
        "string",
        "constant",
        "type",
        "function",
        "property",
        "keyword",
        "link_text",
    ];
    let colors = captures.map(|capture| syntax[capture]["color"].as_str().unwrap());
    assert!(colors.iter().copied().collect::<BTreeSet<_>>().len() >= 4);

    let authored_hues = [
        "foreground",
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
    ]
    .map(|key| oklab_to_oklch(lab(&palette.colors[key]).unwrap()))
    .into_iter()
    .filter(|source| source[1] >= 0.025)
    .map(|source| source[2])
    .collect::<Vec<_>>();
    for (capture, color) in captures.into_iter().zip(colors) {
        let output = oklab_to_oklch(lab(color).unwrap());
        if output[1] < 0.025 {
            continue;
        }
        let nearest = authored_hues
            .iter()
            .map(|hue| {
                (output[2] - hue)
                    .abs()
                    .min(std::f64::consts::TAU - (output[2] - hue).abs())
            })
            .fold(f64::INFINITY, f64::min);
        assert!(nearest <= 0.08, "{capture} synthesized a new hue");
    }
}

#[test]
fn renderer_layer_roles_remain_translucent() {
    let document = build_theme(&synthetic_palette()).unwrap();
    let style = style(&document);
    for role in [
        "search.match_background",
        "search.active_match_background",
        "editor.document_highlight.read_background",
        "editor.document_highlight.write_background",
        "editor.document_highlight.bracket_background",
        "editor.diff_hunk.added.background",
        "editor.diff_hunk.added.hollow_background",
        "editor.diff_hunk.added.hollow_border",
        "editor.diff_hunk.deleted.background",
        "editor.diff_hunk.deleted.hollow_background",
        "editor.diff_hunk.deleted.hollow_border",
        "version_control.word_added",
        "version_control.word_deleted",
        "version_control.conflict_marker.ours",
        "version_control.conflict_marker.theirs",
        "vim.yank.background",
        "drop_target.background",
        "scrollbar.thumb.background",
        "scrollbar.thumb.hover_background",
        "scrollbar.thumb.active_background",
        "minimap.thumb.background",
        "minimap.thumb.hover_background",
        "minimap.thumb.active_background",
    ] {
        assert!(
            parse_hex(style[role].as_str().unwrap()).unwrap().alpha() < 1.0,
            "{role} became opaque"
        );
    }
    assert_eq!(
        parse_hex(style["drop_target.border"].as_str().unwrap())
            .unwrap()
            .alpha(),
        1.0
    );
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
