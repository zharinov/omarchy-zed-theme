use omarchy_zed_theme::color::{apply_opacity, contrast_ratio, delta_e, gpui_blend};
use omarchy_zed_theme::constants::{
    ADDITIONAL_SYNTAX_FIELDS, BASE_SYNTAX_FIELDS, CANONICAL_COLOR_KEYS, CHROME_FIELDS,
    EDITOR_FIELDS, FOUNDATION_FIELDS, LINK_VC_FIELDS, RUNTIME_STATE_BASE_CONTRAST_STEP,
    RUNTIME_STATE_CONSECUTIVE_CONTRAST, RUNTIME_STATE_CONSECUTIVE_DELTA_E, STATUS_NAMES,
    TERMINAL_FIELDS, VIM_FIELDS,
};
use omarchy_zed_theme::palette::{Provenance, ResolvedPalette, resolve_palette};
use omarchy_zed_theme::publish::atomic_write_file;
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
    if !root.is_dir()
        || std::process::Command::new("omarchy-theme-color")
            .arg("--help")
            .output()
            .is_err()
    {
        return;
    }

    let expected_richness = BTreeMap::from([
        ("catppuccin-latte", 6),
        ("catppuccin", 5),
        ("ethereal", 7),
        ("everforest", 5),
        ("flexoki-light", 5),
        ("gruvbox", 5),
        ("hackerman", 5),
        ("kanagawa", 7),
        ("last-horizon", 4),
        ("lumon", 4),
        ("lupine", 4),
        ("matte-black", 4),
        ("miasma", 5),
        ("nord", 7),
        ("osaka-jade", 4),
        ("retro-82", 4),
        ("ristretto", 5),
        ("rose-pine", 5),
        ("solitude", 0),
        ("tokyo-night", 5),
        ("vantablack", 0),
        ("white", 0),
    ]);
    let syntax_manifest: BTreeSet<_> = BASE_SYNTAX_FIELDS
        .iter()
        .chain(ADDITIONAL_SYNTAX_FIELDS)
        .copied()
        .collect();

    for (name, expected_r) in expected_richness {
        let palette = resolve_palette(&root.join(name).join("colors.toml"), None).unwrap();
        let (document, audit) =
            build_theme(&palette).unwrap_or_else(|error| panic!("{name}: {error}"));

        assert_eq!(
            audit.syntax_richness["R"].as_u64(),
            Some(expected_r),
            "{name}"
        );
        assert!(!audit.diff_metrics.is_empty(), "{name}");
        assert_eq!(audit.interaction_ladders.len(), 3, "{name}");

        let style = style(&document);
        let syntax = style["syntax"].as_object().unwrap();

        assert_eq!(
            syntax.keys().map(String::as_str).collect::<BTreeSet<_>>(),
            syntax_manifest,
            "{name}"
        );
        if matches!(name, "white" | "vantablack" | "solitude") {
            let colors: BTreeSet<_> = syntax
                .values()
                .filter_map(|spec| spec["color"].as_str())
                .collect();
            assert!(
                colors.len() >= 6,
                "{name} collapsed to {} syntax colors",
                colors.len()
            );
        }

        let added = style["version_control.added"].as_str().unwrap();
        let deleted = style["version_control.deleted"].as_str().unwrap();
        let pair_contrast = contrast_ratio(added, deleted).unwrap();
        let pair_delta = delta_e(added, deleted).unwrap();
        assert!(pair_contrast >= 1.05);
        assert!(pair_delta >= 0.075);
    }
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
