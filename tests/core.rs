use omarchy_zed_theme::color::{
    apply_opacity, contrast_ratio, delta_e, gpui_blend, lab, oklab_to_oklch, parse_hex,
    render_layers,
};
use omarchy_zed_theme::constants::{
    ADDITIONAL_SYNTAX_FIELDS, BASE_SYNTAX_FIELDS, CANONICAL_COLOR_KEYS, CHROME_FIELDS,
    DIFF_BORDER_RETENTION_DELTA_E, DIFF_BORDER_RETENTION_RATIO, EDITOR_FIELDS, FOUNDATION_FIELDS,
    LINK_VC_FIELDS, RUNTIME_STATE_BASE_CONTRAST_STEP, RUNTIME_STATE_CONSECUTIVE_CONTRAST,
    RUNTIME_STATE_CONSECUTIVE_DELTA_E, STATUS_NAMES, TERMINAL_FIELDS, VIM_FIELDS,
};
use omarchy_zed_theme::palette::{Provenance, ResolvedPalette, resolve_palette};
use omarchy_zed_theme::publish::atomic_write_file;
use omarchy_zed_theme::saliency::relative_saliency;
use omarchy_zed_theme::syntax::contrast_floor;
use omarchy_zed_theme::theme::build_theme;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
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

fn rgb(value: &str) -> (u8, u8, u8) {
    let color = parse_hex(value).unwrap();
    (
        (color.r * 255.0).round() as u8,
        (color.g * 255.0).round() as u8,
        (color.b * 255.0).round() as u8,
    )
}

fn assert_semantic_hue(name: &str, role: &str, value: &str, target_degrees: f64) {
    let [_, chroma, hue] = oklab_to_oklch(lab(value).unwrap());
    let target = target_degrees.to_radians();
    let difference = (hue - target).abs();
    let distance = difference.min(std::f64::consts::TAU - difference);
    assert!(chroma >= 0.025, "{name}: {role} is effectively achromatic");
    assert!(
        distance <= 45.0_f64.to_radians(),
        "{name}: {role} is outside its conventional hue sector"
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
        extras: BTreeMap::new(),
        resolver_stderr: String::new(),
        provenance: CANONICAL_COLOR_KEYS
            .iter()
            .map(|key| ((*key).to_owned(), Provenance::Direct))
            .collect(),
    }
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

fn renamed_fields(source: &str, structure: &str) -> BTreeSet<String> {
    let start_marker = format!("pub struct {structure} {{");
    let body = source
        .split_once(&start_marker)
        .unwrap_or_else(|| panic!("missing {structure}"))
        .1
        .split_once("\n}")
        .unwrap_or_else(|| panic!("unterminated {structure}"))
        .0;
    let mut fields = BTreeSet::new();
    let mut attributes = Vec::new();
    let mut attribute = String::new();

    for line in body.lines() {
        let line = line.trim();

        if !attribute.is_empty() || line.starts_with("#[") {
            attribute.push_str(line);

            if !line.ends_with(']') {
                continue;
            }

            attributes.push(std::mem::take(&mut attribute));
            continue;
        }

        let Some(declaration) = line.strip_prefix("pub ") else {
            continue;
        };

        let Some((rust_name, _)) = declaration.split_once(':') else {
            continue;
        };

        if attributes
            .iter()
            .any(|attribute| attribute.starts_with("#[deprecated"))
        {
            attributes.clear();
            continue;
        }

        let serde = attributes
            .iter()
            .find(|attribute| attribute.starts_with("#[serde"))
            .and_then(|attribute| {
                attribute
                    .split_once('(')
                    .and_then(|(_, value)| value.strip_suffix(")]"))
            });

        if serde
            .is_some_and(|attribute| serde_items(attribute).any(|item| item == "skip_serializing"))
        {
            attributes.clear();
            continue;
        }

        let serialized_name = serde
            .and_then(|attribute| {
                serde_items(attribute).find_map(|item| {
                    let value = item
                        .strip_prefix("rename")?
                        .trim()
                        .strip_prefix('=')?
                        .trim();
                    value.strip_prefix('"')?.strip_suffix('"')
                })
            })
            .unwrap_or_else(|| rust_name.trim());

        fields.insert(serialized_name.to_owned());
        attributes.clear();
    }

    fields
}

fn serde_items(attribute: &str) -> impl Iterator<Item = &str> {
    attribute.split(',').map(str::trim)
}

#[test]
fn schema_parser_keeps_conditionally_serialized_fields() {
    let fixture = r#"
pub struct Fixture {
    #[serde(rename = "old", skip_serializing)]
    pub old: String,
    #[serde(
        rename = "optional",
        skip_serializing_if = "Option::is_none"
    )]
    pub optional: Option<String>,
    pub implicit: String,
    #[deprecated]
    pub legacy: String,
}
"#;
    assert_eq!(
        renamed_fields(fixture, "Fixture"),
        BTreeSet::from(["implicit".to_owned(), "optional".to_owned()])
    );
}

#[test]
fn current_zed_color_schema_matches_the_manifest_when_source_is_provided() {
    // This opt-in check keeps a Zed checkout out of normal builds and test runs.
    let Some(root) = std::env::var_os("OMARCHY_ZED_THEME_ZED_SOURCE").map(PathBuf::from) else {
        return;
    };
    let source_path = root.join("crates/settings_content/src/theme.rs");
    let source = fs::read_to_string(&source_path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", source_path.display()));

    let expected_colors = FOUNDATION_FIELDS
        .iter()
        .chain(CHROME_FIELDS)
        .chain(EDITOR_FIELDS)
        .chain(TERMINAL_FIELDS)
        .chain(LINK_VC_FIELDS)
        .chain(VIM_FIELDS)
        .map(|field| (*field).to_owned())
        .collect();
    assert_eq!(
        renamed_fields(&source, "ThemeColorsContent"),
        expected_colors
    );

    let expected_status = STATUS_NAMES
        .iter()
        .flat_map(|name| {
            [
                name.to_string(),
                format!("{name}.background"),
                format!("{name}.border"),
            ]
        })
        .collect();
    assert_eq!(
        renamed_fields(&source, "StatusColorsContent"),
        expected_status
    );
}

#[test]
fn theme_bytes_and_audit_are_thread_count_independent() {
    let palette = synthetic_palette();
    let build = |threads| {
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .unwrap()
            .install(|| build_theme(&palette).unwrap())
    };
    let (one_document, one_audit) = build(1);
    let (eight_document, eight_audit) = build(8);
    assert_eq!(
        serde_json::to_vec(&one_document).unwrap(),
        serde_json::to_vec(&eight_document).unwrap()
    );
    assert_eq!(one_audit.detail(), eight_audit.detail());
}

#[test]
fn syntax_profile_does_not_reassign_diff_source_families() {
    let authored = synthetic_palette();
    let mut neutral_profile = authored.clone();
    for provenance in neutral_profile.provenance.values_mut() {
        *provenance = Provenance::Derived;
    }

    let (authored_document, authored_audit) = build_theme(&authored).unwrap();
    let (neutral_document, neutral_audit) = build_theme(&neutral_profile).unwrap();
    assert_ne!(
        authored_audit.syntax_policy["profile"]["target_families"],
        neutral_audit.syntax_policy["profile"]["target_families"]
    );

    let authored_syntax = style(&authored_document)["syntax"].as_object().unwrap();
    let neutral_syntax = style(&neutral_document)["syntax"].as_object().unwrap();
    for capture in ["diff.plus", "diff.minus", "diff"] {
        assert_eq!(
            authored_syntax[capture]["color"], neutral_syntax[capture]["color"],
            "{capture} changed with profile-only provenance"
        );
    }

    let diff_audit = &authored_audit.syntax_policy["diff"];
    assert_eq!(diff_audit["profile_budgeted"].as_bool(), Some(false));
    assert_eq!(diff_audit["added_source_key"].as_str(), Some("green"));
    assert_eq!(diff_audit["deleted_source_key"].as_str(), Some("red"));
    assert_eq!(diff_audit["change_source_key"].as_str(), Some("yellow"));

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
fn narrow_olive_palette_uses_scaffold_and_generates() {
    let (_, audit) = build_theme(&venice_like_palette()).unwrap();
    let allocations = audit.syntax_policy["saliency"]["allocations"]
        .as_array()
        .unwrap();
    assert!(
        allocations
            .iter()
            .filter(|allocation| allocation["seed_kind"] == "authored_hue")
            .count()
            <= 1
    );
    assert!(
        allocations
            .iter()
            .any(|allocation| allocation["seed_kind"] == "dynamic_scaffold")
    );
}

#[test]
fn semantic_tokens_preserve_exact_cross_role_relationships() {
    let (document, audit) = build_theme(&synthetic_palette()).unwrap();
    let style = style(&document);

    for group in [
        &[
            "editor.background",
            "editor.gutter.background",
            "tab.active_background",
            "toolbar.background",
        ][..],
        &[
            "background",
            "status_bar.background",
            "title_bar.background",
        ],
        &["text", "icon"],
        &[
            "text.muted",
            "icon.muted",
            "icon.placeholder",
            "unreachable",
        ],
        &[
            "text.placeholder",
            "text.disabled",
            "icon.disabled",
            "hidden",
            "ignored",
        ],
        &["text.accent", "icon.accent", "link_text.hover"],
        &["element.active", "element.selected"],
        &["ghost_element.active", "ghost_element.selected"],
        &[
            "element.disabled",
            "ghost_element.disabled",
            "title_bar.inactive_background",
        ],
    ] {
        let expected = role(style, group[0]);
        for name in &group[1..] {
            assert_eq!(role(style, name), expected, "{name} left its token group");
        }
    }

    for role_name in [
        "border.transparent",
        "ghost_element.background",
        "scrollbar.track.background",
    ] {
        assert_eq!(role(style, role_name), "#00000000", "{role_name}");
    }

    for (source, aliases) in [
        ("created", &["success"][..]),
        ("deleted", &["error"]),
        ("warning", &["conflict", "modified"]),
        ("info", &["renamed"]),
    ] {
        for suffix in ["", ".background", ".border"] {
            let expected = role(style, &format!("{source}{suffix}"));
            for alias in aliases {
                let alias_role = format!("{alias}{suffix}");
                assert_eq!(role(style, &alias_role), expected, "{alias_role}");
            }
        }
    }

    let syntax = style["syntax"].as_object().unwrap();
    let editor_foreground = role(style, "editor.foreground");
    assert_eq!(syntax["primary"]["color"], editor_foreground);
    assert_eq!(syntax["variable"]["color"], editor_foreground);
    assert_eq!(syntax["predictive"]["color"], role(style, "predictive"));

    let structural_rgb = rgb(role(style, "border"));
    assert_eq!(rgb(role(style, "editor.wrap_guide")), structural_rgb);
    assert_eq!(rgb(role(style, "editor.active_wrap_guide")), structural_rgb);
    assert!(role(style, "editor.wrap_guide").ends_with("0d"));
    assert!(role(style, "editor.active_wrap_guide").ends_with("1a"));
    let canvas = role(style, "editor.background");
    let rendered_wrap = gpui_blend(canvas, role(style, "editor.wrap_guide"))
        .unwrap()
        .opaque_hex();
    let rendered_active = gpui_blend(canvas, role(style, "editor.active_wrap_guide"))
        .unwrap()
        .opaque_hex();
    assert!(
        contrast_ratio(&rendered_active, canvas).unwrap()
            > contrast_ratio(&rendered_wrap, canvas).unwrap()
    );
    assert!(delta_e(&rendered_active, canvas).unwrap() > delta_e(&rendered_wrap, canvas).unwrap());

    let audited_relations = audit
        .fidelity_deviations
        .iter()
        .filter_map(|entry| entry["requested_relation"].as_str())
        .collect::<BTreeSet<_>>();
    assert!(audited_relations.contains("surface and active line share RGB"));
    assert!(audited_relations.contains("content accent and document read share RGB"));
    assert!(audited_relations.contains("cursor and selection share RGB"));
}

#[test]
fn every_syntax_capture_is_readable_on_emitted_diff_overlays() {
    let (document, _) = build_theme(&synthetic_palette()).unwrap();
    let style = style(&document);
    let base = style["editor.background"].as_str().unwrap();
    let added = render_layers(
        base,
        &[style["editor.diff_hunk.added.background"].as_str().unwrap()],
    )
    .unwrap();
    let deleted = render_layers(
        base,
        &[style["editor.diff_hunk.deleted.background"]
            .as_str()
            .unwrap()],
    )
    .unwrap();
    let diff_contexts = vec![
        added.clone(),
        deleted.clone(),
        render_layers(
            base,
            &[style["editor.diff_hunk.added.hollow_background"]
                .as_str()
                .unwrap()],
        )
        .unwrap(),
        render_layers(
            base,
            &[style["editor.diff_hunk.deleted.hollow_background"]
                .as_str()
                .unwrap()],
        )
        .unwrap(),
        render_layers(
            &added,
            &[style["version_control.word_added"].as_str().unwrap()],
        )
        .unwrap(),
        render_layers(
            &deleted,
            &[style["version_control.word_deleted"].as_str().unwrap()],
        )
        .unwrap(),
        render_layers(
            base,
            &[style["version_control.conflict_marker.ours"]
                .as_str()
                .unwrap()],
        )
        .unwrap(),
        render_layers(
            base,
            &[style["version_control.conflict_marker.theirs"]
                .as_str()
                .unwrap()],
        )
        .unwrap(),
    ];

    for (capture, spec) in style["syntax"].as_object().unwrap() {
        let foreground = spec["color"].as_str().unwrap();
        let floor = contrast_floor(capture).unwrap();
        for background in &diff_contexts {
            let actual = contrast_ratio(foreground, background).unwrap();
            assert!(
                actual >= floor - 1e-9,
                "syntax.{capture} reaches only {actual:.3}:1 on rendered diff context; floor is {floor:.2}:1"
            );
        }
    }
}

#[test]
fn renderer_layer_roles_remain_translucent() {
    let (document, _) = build_theme(&synthetic_palette()).unwrap();
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
            parse_hex(style[role].as_str().unwrap()).unwrap().a < 1.0,
            "{role} became opaque"
        );
    }
    assert_eq!(
        parse_hex(style["drop_target.border"].as_str().unwrap())
            .unwrap()
            .a,
        1.0
    );
}

#[test]
fn inline_diff_emphasis_preserves_the_hollow_hunk_border() {
    let (document, _) = build_theme(&synthetic_palette()).unwrap();
    let style = style(&document);
    let base = style["editor.background"].as_str().unwrap();
    for family in ["added", "deleted"] {
        let hollow = style[&format!("editor.diff_hunk.{family}.hollow_background")]
            .as_str()
            .unwrap();
        let border = style[&format!("editor.diff_hunk.{family}.hollow_border")]
            .as_str()
            .unwrap();
        let word = style[&format!("version_control.word_{family}")]
            .as_str()
            .unwrap();
        for highlight in [
            None,
            Some(style["search.match_background"].as_str().unwrap()),
            Some(
                style["editor.document_highlight.read_background"]
                    .as_str()
                    .unwrap(),
            ),
            Some(style["vim.yank.background"].as_str().unwrap()),
        ] {
            let mut without_border_layers = vec![hollow];
            let mut with_border_layers = vec![hollow, border];
            if let Some(highlight) = highlight {
                without_border_layers.push(highlight);
                with_border_layers.push(highlight);
            }
            let before_word = render_layers(base, &without_border_layers).unwrap();
            let border_before_word = render_layers(base, &with_border_layers).unwrap();
            without_border_layers.push(word);
            with_border_layers.push(word);
            let without_border = render_layers(base, &without_border_layers).unwrap();
            let with_border = render_layers(base, &with_border_layers).unwrap();
            let retained = delta_e(&without_border, &with_border).unwrap();
            let retained_ratio = retained
                / delta_e(&before_word, &border_before_word)
                    .unwrap()
                    .max(1e-12);

            assert!(
                retained >= DIFF_BORDER_RETENTION_DELTA_E - 1e-9
                    && retained_ratio >= DIFF_BORDER_RETENTION_RATIO - 1e-9,
                "{family} inline emphasis erased its hunk border: delta E {retained:.3}, retained {:.1}%",
                retained_ratio * 100.0,
            );
        }
    }
}

#[test]
fn runtime_element_active_advances_hover() {
    let (document, _) = build_theme(&synthetic_palette()).unwrap();
    let style = style(&document);
    let base = style["element.background"].as_str().unwrap();
    let hover = gpui_blend(
        base,
        &apply_opacity(style["element.hover"].as_str().unwrap(), 0.6).unwrap(),
    )
    .unwrap()
    .opaque_hex();
    let active = gpui_blend(
        base,
        &apply_opacity(style["element.active"].as_str().unwrap(), 0.5).unwrap(),
    )
    .unwrap()
    .opaque_hex();

    assert!(
        contrast_ratio(&active, base).unwrap()
            >= contrast_ratio(&hover, base).unwrap() + RUNTIME_STATE_BASE_CONTRAST_STEP - 1e-9
    );
    assert!(contrast_ratio(&active, &hover).unwrap() >= RUNTIME_STATE_CONSECUTIVE_CONTRAST - 1e-9);
    assert!(delta_e(&active, &hover).unwrap() >= RUNTIME_STATE_CONSECUTIVE_DELTA_E - 1e-9);
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

#[test]
fn all_builtin_themes_meet_the_rust_contract() {
    let default_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("omarchy/themes");
    let configured_root = std::env::var_os("OMARCHY_THEMES_DIR").map(PathBuf::from);
    let root = configured_root.as_deref().unwrap_or(&default_root);
    let resolver_available = std::process::Command::new("omarchy-theme-color")
        .arg("--help")
        .output()
        .is_ok();
    if configured_root.is_some() {
        assert!(root.is_dir(), "configured Omarchy theme root is missing");
        assert!(
            resolver_available,
            "omarchy-theme-color is required for the configured built-in-theme corpus"
        );
    } else if !root.is_dir() || !resolver_available {
        return;
    }

    let theme_names = [
        "catppuccin-latte",
        "catppuccin",
        "ethereal",
        "everforest",
        "flexoki-light",
        "gruvbox",
        "hackerman",
        "kanagawa",
        "last-horizon",
        "lumon",
        "lupine",
        "matte-black",
        "miasma",
        "nord",
        "osaka-jade",
        "retro-82",
        "ristretto",
        "rose-pine",
        "solitude",
        "tokyo-night",
        "vantablack",
        "white",
    ];

    let syntax_manifest: BTreeSet<_> = BASE_SYNTAX_FIELDS
        .iter()
        .chain(ADDITIONAL_SYNTAX_FIELDS)
        .copied()
        .collect();

    let mut profile_summary = BTreeMap::new();

    for name in theme_names {
        let palette = resolve_palette(&root.join(name).join("colors.toml"), None).unwrap();
        let (document, audit) =
            build_theme(&palette).unwrap_or_else(|error| panic!("{name}: {error}"));

        let profile = &audit.syntax_policy["profile"];
        profile_summary.insert(
            name,
            (
                profile["scores"]["authored_breadth"].as_f64().unwrap(),
                profile["scores"]["authored_intensity"].as_f64().unwrap(),
                profile["baseline_kind"].as_str().unwrap().to_owned(),
                profile["target_families"].as_u64().unwrap(),
            ),
        );
        let family_count = audit.syntax_policy["merge_plan"]["family_count"]
            .as_u64()
            .unwrap();
        let requested_family_count = profile["target_families"].as_u64().unwrap();
        assert!(family_count <= requested_family_count, "{name}");
        assert_eq!(
            audit.syntax_policy["merge_plan"]["requested_family_count"].as_u64(),
            Some(requested_family_count),
            "{name}"
        );
        assert!((4..=8).contains(&family_count), "{name}");
        let family_colors = audit.syntax_policy["saliency"]["ordinary_pair_metrics"]["colors"]
            .as_array()
            .unwrap();
        assert_eq!(family_colors.len(), family_count as usize, "{name}");
        assert_eq!(
            family_colors
                .iter()
                .map(|color| color.as_str().unwrap())
                .collect::<BTreeSet<_>>()
                .len(),
            family_count as usize,
            "{name}: accidental family collision"
        );
        assert!(!audit.diff_metrics.is_empty(), "{name}");
        assert_eq!(audit.interaction_ladders.len(), 3, "{name}");

        let style = style(&document);
        let syntax = style["syntax"].as_object().unwrap();
        let editor_backgrounds = vec![style["editor.background"].as_str().unwrap().to_owned()];
        let editor_foreground = style["editor.foreground"].as_str().unwrap();
        let inactive_saliency = relative_saliency(
            style["editor.line_number"].as_str().unwrap(),
            editor_foreground,
            &editor_backgrounds,
        )
        .unwrap();
        let hover_saliency = relative_saliency(
            style["editor.hover_line_number"].as_str().unwrap(),
            editor_foreground,
            &editor_backgrounds,
        )
        .unwrap();
        let active_saliency = relative_saliency(
            style["editor.active_line_number"].as_str().unwrap(),
            editor_foreground,
            &editor_backgrounds,
        )
        .unwrap();
        assert!(inactive_saliency + 0.10 <= hover_saliency, "{name}");
        assert!(inactive_saliency + 0.20 <= active_saliency, "{name}");
        assert!(hover_saliency <= active_saliency + 0.03, "{name}");

        let line_audit = audit
            .saliency
            .iter()
            .find(|entry| entry["role"] == "editor.line_number")
            .unwrap();
        let active_line_audit = audit
            .saliency
            .iter()
            .find(|entry| entry["role"] == "editor.active_line_number")
            .unwrap();
        assert_eq!(line_audit["preferred_saliency"].as_f64(), Some(0.394));
        assert!(
            (line_audit["actual_saliency"].as_f64().unwrap() - 0.394).abs() <= 0.03,
            "{name}"
        );
        assert!(
            (active_line_audit["actual_saliency"].as_f64().unwrap() - 1.0).abs() <= 0.03,
            "{name}"
        );
        assert_eq!(
            audit.syntax_policy["saliency"]["measured_order_verified"].as_bool(),
            Some(true),
            "{name}"
        );

        let primary_saliency = relative_saliency(
            syntax["primary"]["color"].as_str().unwrap(),
            editor_foreground,
            &editor_backgrounds,
        )
        .unwrap();
        let subdued_saliency = relative_saliency(
            syntax["comment"]["color"].as_str().unwrap(),
            editor_foreground,
            &editor_backgrounds,
        )
        .unwrap();
        assert!(subdued_saliency + 0.03 <= primary_saliency, "{name}");

        for (role, value, hue) in [
            (
                "syntax add",
                syntax["diff.plus"]["color"].as_str().unwrap(),
                145.0,
            ),
            (
                "syntax remove",
                syntax["diff.minus"]["color"].as_str().unwrap(),
                25.0,
            ),
            (
                "syntax change",
                syntax["diff"]["color"].as_str().unwrap(),
                85.0,
            ),
            (
                "added diff background",
                style["editor.diff_hunk.added.background"].as_str().unwrap(),
                145.0,
            ),
            (
                "removed diff background",
                style["editor.diff_hunk.deleted.background"]
                    .as_str()
                    .unwrap(),
                25.0,
            ),
            (
                "added diff border",
                style["editor.diff_hunk.added.hollow_border"]
                    .as_str()
                    .unwrap(),
                145.0,
            ),
            (
                "removed diff border",
                style["editor.diff_hunk.deleted.hollow_border"]
                    .as_str()
                    .unwrap(),
                25.0,
            ),
            (
                "version-control added",
                style["version_control.added"].as_str().unwrap(),
                145.0,
            ),
            (
                "version-control deleted",
                style["version_control.deleted"].as_str().unwrap(),
                25.0,
            ),
            (
                "version-control modified",
                style["version_control.modified"].as_str().unwrap(),
                85.0,
            ),
            ("created status", style["created"].as_str().unwrap(), 145.0),
            ("deleted status", style["deleted"].as_str().unwrap(), 25.0),
            ("modified status", style["modified"].as_str().unwrap(), 85.0),
        ] {
            assert_semantic_hue(name, role, value, hue);
        }

        assert_eq!(
            syntax.keys().map(String::as_str).collect::<BTreeSet<_>>(),
            syntax_manifest,
            "{name}"
        );
        if matches!(name, "white" | "vantablack" | "solitude") {
            assert_eq!(profile["baseline_kind"].as_str(), Some("neutral"), "{name}");
            assert_eq!(family_count, 4, "{name}");
            assert!(
                profile["chroma_envelope"]["maximum_ordinary_chroma"]
                    .as_f64()
                    .unwrap()
                    <= 0.055 + 1e-9,
                "{name} inflated the neutral chroma envelope"
            );
            assert!(
                audit.syntax_policy["saliency"]["allocations"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|allocation| allocation["seed_kind"] == "dynamic_scaffold"),
                "{name} did not use the dynamic scaffold"
            );
            let colors: BTreeSet<_> = syntax
                .values()
                .filter_map(|spec| spec["color"].as_str())
                .collect();
            assert!(
                colors.len() >= 7,
                "{name} collapsed to {} syntax colors",
                colors.len()
            );
        }

        assert!(
            audit.syntax_policy["saliency"]["allocations"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|allocation| allocation["seed_kind"] == "dynamic_scaffold")
                .all(|allocation| allocation["output_chroma"].as_f64().unwrap() <= 0.055 + 1e-9),
            "{name} synthesized an ordinary high-chroma family"
        );

        let added = style["version_control.added"].as_str().unwrap();
        let deleted = style["version_control.deleted"].as_str().unwrap();
        let pair_contrast = contrast_ratio(added, deleted).unwrap();
        let pair_delta = delta_e(added, deleted).unwrap();
        assert!(pair_contrast >= 1.05);
        assert!(pair_delta >= 0.075);
    }

    let matte = &profile_summary["matte-black"];
    let nord = &profile_summary["nord"];
    let tokyo = &profile_summary["tokyo-night"];
    let catppuccin = &profile_summary["catppuccin"];
    assert_eq!(matte.2, "accent_led");
    assert_eq!(nord.2, "palette_native");
    assert!(matte.0 < nord.0, "matte-black should be narrower than nord");
    assert!(
        matte.1 > nord.1,
        "matte-black should be more intense than nord"
    );
    assert!(tokyo.0 > nord.0, "tokyo-night should be broader than nord");
    assert!(
        catppuccin.0 > nord.0,
        "catppuccin should be broader than nord"
    );
}

#[test]
fn pinned_external_corpus_meets_the_rust_contract_when_available() {
    // The opt-in root contains local checkouts at the commits in external-corpus.tsv.
    let Some(root) = std::env::var_os("OMARCHY_ZED_THEME_EXTERNAL_CORPUS").map(PathBuf::from)
    else {
        return;
    };
    let manifest = include_str!("external-corpus.tsv");
    let mut tested = 0;

    for line in manifest.lines().filter(|line| !line.starts_with('#')) {
        let columns: Vec<_> = line.split('\t').collect();
        assert_eq!(columns.len(), 4, "bad corpus row: {line}");
        let [name, _, commit, palette_path] = columns.as_slice() else {
            unreachable!()
        };

        let checkout = root.join(name);
        let actual_commit = std::process::Command::new("git")
            .args(["-C"])
            .arg(&checkout)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap_or_else(|error| panic!("{name}: cannot inspect commit: {error}"));

        assert!(
            actual_commit.status.success(),
            "{name}: git rev-parse failed"
        );
        assert_eq!(
            String::from_utf8_lossy(&actual_commit.stdout).trim(),
            *commit,
            "{name}: corpus checkout is not at the pinned commit"
        );

        let palette = resolve_palette(&checkout.join(palette_path), None)
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        let (document, audit) =
            build_theme(&palette).unwrap_or_else(|error| panic!("{name}: {error}"));
        let syntax = style(&document)["syntax"].as_object().unwrap();
        assert_eq!(syntax.len(), 56, "{name}");
        assert!(
            audit.syntax_collapses.is_empty(),
            "{name}: unexpected syntax collapse"
        );
        assert!(!audit.diff_metrics.is_empty(), "{name}");
        assert_eq!(audit.interaction_ladders.len(), 3, "{name}");

        tested += 1;
    }

    assert_eq!(tested, 16);
}
