use omarchy_zed_theme::color::{
    apply_opacity, contrast_ratio, delta_e, lab, oklab_to_oklch, parse_hex, render_layers,
};
use omarchy_zed_theme::constants::{
    CHROME_FIELDS, DARK_DIFF_BORDER_OPACITY, DARK_DIFF_HOLLOW_OPACITY, DARK_DIFF_LINE_OPACITY,
    DARK_DIFF_WORD_OPACITY, DIFF_NORMAL_FLOOR_DELTA_E, EDITOR_FIELDS, FOUNDATION_FIELDS,
    HARD_TEXT_CONTRAST, LIGHT_DIFF_BORDER_OPACITY, LIGHT_DIFF_HOLLOW_OPACITY,
    LIGHT_DIFF_LINE_OPACITY, LIGHT_DIFF_WORD_OPACITY, LINK_VC_FIELDS, SCHEMA_URL,
    SEMANTIC_PAIR_CONTRACT, STATUS_NAMES, SYNTAX_DIFF_CONTRACT, TERMINAL_FIELDS, THEME_NAME,
    VIM_FIELDS,
};
use omarchy_zed_theme::palette::ResolvedPalette;
use omarchy_zed_theme::search::cvd_distance;
use omarchy_zed_theme::syntax::policy::CAPTURE_POLICIES;
use serde_json::{Map, Value};
use std::collections::BTreeSet;

fn style(document: &Value) -> &Map<String, Value> {
    document["themes"][0]["style"]
        .as_object()
        .expect("generated style must be an object")
}

fn role<'a>(style: &'a Map<String, Value>, name: &str) -> &'a str {
    style[name]
        .as_str()
        .unwrap_or_else(|| panic!("generated role {name} must be a color"))
}

fn assert_metric_between(label: &str, metric: f64, minimum: f64, maximum: f64) {
    assert!(
        (minimum - 1e-9..=maximum + 1e-9).contains(&metric),
        "{label}: expected {minimum:.3}..={maximum:.3}, got {metric:.4}"
    );
}

pub fn assert_document_contract(label: &str, palette: &ResolvedPalette, document: &Value) {
    let root = document
        .as_object()
        .expect("generated document must be an object");
    assert_eq!(
        root.keys().map(String::as_str).collect::<BTreeSet<_>>(),
        BTreeSet::from(["$schema", "author", "name", "themes"]),
        "{label}: root manifest drifted"
    );
    let themes = root["themes"]
        .as_array()
        .expect("generated themes must be an array");

    assert_eq!(themes.len(), 1, "{label}: expected one generated theme");
    assert_eq!(root["$schema"].as_str(), Some(SCHEMA_URL));
    assert_eq!(root["name"].as_str(), Some(THEME_NAME));
    assert_eq!(root["author"].as_str(), Some("APS"));
    let theme = themes[0]
        .as_object()
        .expect("generated theme must be an object");

    assert_eq!(
        theme.keys().map(String::as_str).collect::<BTreeSet<_>>(),
        BTreeSet::from(["appearance", "name", "style"]),
        "{label}: theme manifest drifted"
    );
    assert_eq!(theme["name"].as_str(), Some(THEME_NAME));
    assert_eq!(
        theme["appearance"].as_str(),
        Some(palette.mode.as_str()),
        "{label}: appearance does not follow the palette mode"
    );

    let style = style(document);
    let expected_roles = FOUNDATION_FIELDS
        .iter()
        .chain(CHROME_FIELDS)
        .chain(EDITOR_FIELDS)
        .chain(TERMINAL_FIELDS)
        .chain(LINK_VC_FIELDS)
        .chain(VIM_FIELDS)
        .map(|name| (*name).to_owned())
        .chain(STATUS_NAMES.iter().flat_map(|name| {
            [
                (*name).to_owned(),
                format!("{name}.background"),
                format!("{name}.border"),
            ]
        }))
        .chain(["background.appearance", "accents", "players", "syntax"].map(str::to_owned))
        .collect::<BTreeSet<_>>();

    assert_eq!(
        style.keys().cloned().collect::<BTreeSet<_>>(),
        expected_roles,
        "{label}: emitted style role set drifted"
    );
    for (name, value) in style {
        if matches!(name.as_str(), "accents" | "players" | "syntax") {
            continue;
        }
        if name == "background.appearance" {
            assert_eq!(value.as_str(), Some("opaque"));
            continue;
        }
        let color = value
            .as_str()
            .unwrap_or_else(|| panic!("{label}: {name} must be a color string"));
        parse_hex(color).unwrap_or_else(|error| panic!("{label}: invalid {name}: {error}"));
    }

    let accents = style["accents"]
        .as_array()
        .expect("generated accents must be an array");
    assert_eq!(accents.len(), 12, "{label}: expected twelve accents");
    for color in accents {
        parse_hex(color.as_str().expect("accent must be a color")).unwrap();
    }

    let players = style["players"]
        .as_array()
        .expect("generated players must be an array");
    assert_eq!(players.len(), 8, "{label}: expected eight players");
    for player in players {
        let player = player.as_object().expect("player must be an object");
        assert_eq!(
            player.keys().map(String::as_str).collect::<BTreeSet<_>>(),
            BTreeSet::from(["background", "cursor", "selection"]),
            "{label}: player manifest drifted"
        );
        for color in player.values() {
            parse_hex(color.as_str().expect("player role must be a color")).unwrap();
        }
    }

    let syntax = style["syntax"]
        .as_object()
        .expect("generated syntax must be an object");
    assert_eq!(
        syntax.keys().map(String::as_str).collect::<BTreeSet<_>>(),
        CAPTURE_POLICIES
            .iter()
            .map(|policy| policy.capture)
            .collect(),
        "{label}: syntax capture manifest drifted"
    );
    for (capture, entry) in syntax {
        let color = entry["color"]
            .as_str()
            .unwrap_or_else(|| panic!("{label}: syntax.{capture} has no color"));
        parse_hex(color)
            .unwrap_or_else(|error| panic!("{label}: syntax.{capture} has invalid color: {error}"));
    }

    for aliases in [
        &["created", "success"][..],
        &["deleted", "error"][..],
        &["conflict", "modified", "warning"][..],
        &["info", "renamed"][..],
    ] {
        for suffix in ["", ".background", ".border"] {
            let expected = &style[&format!("{}{suffix}", aliases[0])];
            for alias in &aliases[1..] {
                assert_eq!(
                    &style[&format!("{alias}{suffix}")],
                    expected,
                    "{label}: status alias {alias}{suffix} drifted"
                );
            }
        }
    }
}

pub fn assert_feasible_theme_contract(label: &str, palette: &ResolvedPalette, document: &Value) {
    assert_document_contract(label, palette, document);
    assert_ui_contract(label, palette, document);
    assert_diff_contract(label, palette, document);
    assert_overlay_roles_are_translucent(label, document);
}

fn assert_diff_contract(label: &str, palette: &ResolvedPalette, document: &Value) {
    let style = style(document);
    let base = role(style, "editor.background");
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
        let word_role = format!("version_control.word_{family}");
        for (role_name, expected) in [
            (&line_role, expected_opacities[0]),
            (&hollow_role, expected_opacities[1]),
            (&border_role, expected_opacities[2]),
            (&word_role, expected_opacities[3]),
        ] {
            let actual = parse_hex(role(style, role_name)).unwrap().alpha();
            assert_eq!(
                (actual * 255.0).round(),
                (expected * 255.0).round(),
                "{label}: {role_name} opacity drifted"
            );
        }

        for hunk_role in [&line_role, &hollow_role] {
            let hunk = render_layers(base, &[role(style, hunk_role)]).unwrap();
            for highlight in [
                "search.match_background",
                "search.active_match_background",
                "editor.document_highlight.read_background",
                "editor.document_highlight.write_background",
                "editor.document_highlight.bracket_background",
                "vim.yank.background",
            ] {
                let underlay = render_layers(&hunk, &[role(style, highlight)]).unwrap();
                let word = render_layers(&underlay, &[role(style, &word_role)]).unwrap();
                assert_ne!(underlay, word, "{label}: {word_role} disappeared");
                assert!(
                    contrast_ratio(role(style, status), &word).unwrap()
                        >= HARD_TEXT_CONTRAST - 1e-9,
                    "{label}: {status} is unreadable over {word_role}"
                );
            }
        }
    }

    let added = render_layers(base, &[role(style, "editor.diff_hunk.added.background")]).unwrap();
    let deleted =
        render_layers(base, &[role(style, "editor.diff_hunk.deleted.background")]).unwrap();
    assert!(
        delta_e(&added, &deleted).unwrap() >= DIFF_NORMAL_FLOOR_DELTA_E - 1e-9,
        "{label}: rendered add/delete fills collapsed"
    );

    let vcs_added = role(style, "version_control.added");
    let vcs_deleted = role(style, "version_control.deleted");

    assert!(delta_e(vcs_added, vcs_deleted).unwrap() >= SEMANTIC_PAIR_CONTRACT.normal_delta_e);
    assert!(cvd_distance(vcs_added, vcs_deleted).unwrap() >= SEMANTIC_PAIR_CONTRACT.cvd_delta_e);

    let syntax_added = style["syntax"]["diff.plus"]["color"].as_str().unwrap();
    let syntax_deleted = style["syntax"]["diff.minus"]["color"].as_str().unwrap();
    assert!(delta_e(syntax_added, syntax_deleted).unwrap() >= SYNTAX_DIFF_CONTRACT.normal_delta_e);
    assert!(
        cvd_distance(syntax_added, syntax_deleted).unwrap() >= SYNTAX_DIFF_CONTRACT.cvd_delta_e
    );
}

fn assert_overlay_roles_are_translucent(label: &str, document: &Value) {
    let style = style(document);
    for name in [
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
            parse_hex(role(style, name)).unwrap().alpha() < 1.0,
            "{label}: {name} became opaque"
        );
    }
    assert_eq!(
        parse_hex(role(style, "drop_target.border"))
            .unwrap()
            .alpha(),
        1.0,
        "{label}: drop target border became translucent"
    );
}

fn assert_ui_contract(label: &str, palette: &ResolvedPalette, document: &Value) {
    let style = style(document);
    let element_base = role(style, "element.background");
    let rendered_element = |state| render_layers(element_base, &[role(style, state)]).unwrap();
    let element_hover = rendered_element("element.hover");
    let element_active = rendered_element("element.active");
    let element_selected = rendered_element("element.selected");

    for (name, rendered, minimum, maximum) in [
        ("hover", &element_hover, 1.10, 1.50),
        ("active", &element_active, 1.18, 1.75),
        ("selected", &element_selected, 1.18, 1.90),
    ] {
        assert_metric_between(
            &format!("{label} element {name}"),
            contrast_ratio(rendered, element_base).unwrap(),
            minimum,
            maximum,
        );
    }
    assert!(delta_e(&element_hover, &element_active).unwrap() >= 0.012 - 1e-9);
    assert!(delta_e(&element_active, &element_selected).unwrap() >= 0.012 - 1e-9);

    let interaction_background_roles = [
        "background",
        "surface.background",
        "elevated_surface.background",
        "title_bar.background",
        "panel.overlay_background",
        "tab.inactive_background",
    ];
    for base_role in interaction_background_roles {
        let base = role(style, base_role);
        for state in ["element.hover", "element.active", "element.selected"] {
            let rendered = render_layers(base, &[role(style, state)]).unwrap();
            assert!(
                contrast_ratio(role(style, "text"), &rendered).unwrap()
                    >= HARD_TEXT_CONTRAST - 1e-9,
                "{label}: primary text is unreadable on {state} over {base_role}"
            );
        }
        let rendered = [
            "ghost_element.hover",
            "ghost_element.active",
            "ghost_element.selected",
        ]
        .map(|state| render_layers(base, &[role(style, state)]).unwrap());
        let contrasts: [f64; 3] =
            std::array::from_fn(|index| contrast_ratio(&rendered[index], base).unwrap());

        for (index, (minimum, maximum)) in [(1.10, 1.50), (1.18, 1.75), (1.18, 1.90)]
            .into_iter()
            .enumerate()
        {
            assert_metric_between(
                &format!("{label} ghost state {index} on {base_role}"),
                contrasts[index],
                minimum,
                maximum,
            );
        }
        assert!(delta_e(&rendered[0], &rendered[1]).unwrap() >= 0.012 - 1e-9);
        assert!(delta_e(&rendered[1], &rendered[2]).unwrap() >= 0.012 - 1e-9);
        for (index, rendered) in rendered.iter().enumerate() {
            assert!(
                contrast_ratio(role(style, "text"), rendered).unwrap() >= HARD_TEXT_CONTRAST - 1e-9,
                "{label}: primary text is unreadable on ghost state {index} over {base_role}"
            );
        }
    }

    for background_role in [
        "background",
        "surface.background",
        "elevated_surface.background",
        "title_bar.background",
    ] {
        let background = role(style, background_role);
        let text_contrasts = [
            ("text", 4.50),
            ("text.muted", 4.50),
            ("text.placeholder", 4.50),
            ("text.disabled", 3.00),
        ]
        .map(|(name, floor)| {
            let contrast = contrast_ratio(role(style, name), background).unwrap();
            assert!(
                contrast >= floor - 1e-9,
                "{label}: {name} is unreadable on {background_role}"
            );
            contrast
        });
        assert!(
            text_contrasts
                .windows(2)
                .all(|pair| pair[0] + 0.02 >= pair[1]),
            "{label}: text hierarchy is inverted on {background_role}: {text_contrasts:?}"
        );

        let icon_contrasts = [
            ("icon", 3.00),
            ("icon.muted", 3.00),
            ("icon.placeholder", 2.25),
            ("icon.disabled", 1.50),
        ]
        .map(|(name, floor)| {
            let contrast = contrast_ratio(role(style, name), background).unwrap();
            assert!(
                contrast >= floor - 1e-9,
                "{label}: {name} is unreadable on {background_role}"
            );
            contrast
        });
        assert!(
            icon_contrasts
                .windows(2)
                .all(|pair| pair[0] + 0.02 >= pair[1]),
            "{label}: icon hierarchy is inverted on {background_role}: {icon_contrasts:?}"
        );
    }

    let structure_background_roles = [
        "background",
        "surface.background",
        "elevated_surface.background",
        "title_bar.background",
    ];
    for background_role in structure_background_roles {
        let background = role(style, background_role);
        let border = contrast_ratio(role(style, "border"), background).unwrap();
        let variant = contrast_ratio(role(style, "border.variant"), background).unwrap();

        assert_metric_between(
            &format!("{label} border on {background_role}"),
            border,
            1.15,
            2.00,
        );
        assert_metric_between(
            &format!("{label} border variant on {background_role}"),
            variant,
            1.09,
            1.70,
        );
        assert!(
            border >= variant + 0.005 - 1e-9,
            "{label}: border hierarchy collapsed on {background_role}: border {border:.4}, variant {variant:.4}"
        );
    }

    let background = role(style, "background");
    let border = contrast_ratio(role(style, "border"), background).unwrap();
    let focused = contrast_ratio(role(style, "border.focused"), background).unwrap();

    assert_metric_between(&format!("{label} focused border"), focused, 3.00, 4.60);
    assert!(focused > border);

    let accent_lch = oklab_to_oklch(lab(&palette.colors["accent"]).unwrap());
    if accent_lch[1] >= 0.035 {
        let info_lch = oklab_to_oklch(lab(role(style, "info.background")).unwrap());
        let hue_difference = (accent_lch[2] - info_lch[2]).abs();
        let hue_distance = hue_difference.min(std::f64::consts::TAU - hue_difference);
        assert!(
            info_lch[1] >= 0.030 - 1e-9,
            "{label}: accent status background lost its tint"
        );
        assert!(
            hue_distance <= 0.08 + 1e-9,
            "{label}: accent status background changed hue"
        );
    }

    let panel = role(style, "panel.background");
    let panel_overlay = role(style, "panel.overlay_background");
    let panel_hover = role(style, "panel.overlay_hover");
    assert_metric_between(
        &format!("{label} panel overlay"),
        contrast_ratio(panel_overlay, panel).unwrap(),
        1.01,
        1.35,
    );
    assert_metric_between(
        &format!("{label} panel hover"),
        contrast_ratio(panel_hover, panel).unwrap(),
        1.10,
        1.50,
    );
    assert!(delta_e(panel_overlay, panel_hover).unwrap() >= 0.012 - 1e-9);

    let panel_guides = [
        "panel.indent_guide",
        "panel.indent_guide_hover",
        "panel.indent_guide_active",
    ]
    .map(|name| contrast_ratio(role(style, name), panel).unwrap());

    assert!(panel_guides[0] < panel_guides[1] && panel_guides[1] < panel_guides[2]);

    let active_tab = role(style, "tab.active_background");
    let inactive_tab = role(style, "tab.inactive_background");
    let tab_contrast = contrast_ratio(active_tab, inactive_tab).unwrap();
    let tab_distance = delta_e(active_tab, inactive_tab).unwrap();
    assert!(
        tab_distance >= 0.020 - 1e-9
            && (tab_contrast >= 1.07 - 1e-9 || tab_distance >= 0.040 - 1e-9)
    );

    for prefix in ["scrollbar.thumb", "minimap.thumb"] {
        for base_role in ["background", "surface.background", "editor.background"] {
            let base = role(style, base_role);
            let rendered = [
                format!("{prefix}.background"),
                format!("{prefix}.hover_background"),
                format!("{prefix}.active_background"),
            ]
            .map(|name| render_layers(base, &[role(style, &name)]).unwrap());
            let contrasts: [f64; 3] =
                std::array::from_fn(|index| contrast_ratio(&rendered[index], base).unwrap());

            assert!(contrasts[0] + 1e-9 < contrasts[1]);
            assert!(contrasts[1] + 1e-9 < contrasts[2]);
            assert_metric_between(
                &format!("{label} {prefix} idle on {base_role}"),
                contrasts[0],
                1.40,
                2.65,
            );
            assert!(delta_e(&rendered[0], &rendered[1]).unwrap() >= 0.012 - 1e-9);
            assert!(delta_e(&rendered[1], &rendered[2]).unwrap() >= 0.012 - 1e-9);
        }
    }

    let canvas = role(style, "editor.background");
    let guide = contrast_ratio(role(style, "editor.indent_guide"), canvas).unwrap();
    let active_guide = contrast_ratio(role(style, "editor.indent_guide_active"), canvas).unwrap();

    assert!(active_guide > guide);

    for status in [
        "created",
        "deleted",
        "warning",
        "info",
        "predictive",
        "hint",
        "hidden",
        "ignored",
        "unreachable",
    ] {
        let status_background = role(style, &format!("{status}.background"));
        assert!(
            contrast_ratio(role(style, status), status_background).unwrap()
                >= HARD_TEXT_CONTRAST - 1e-9
        );
        assert_metric_between(
            &format!("{label} {status} background"),
            contrast_ratio(status_background, role(style, "surface.background")).unwrap(),
            1.18,
            1.90,
        );
    }

    let terminal_background = role(style, "terminal.background");
    let selection = role(style, "element.selection_background");
    let unfocused_selection = apply_opacity(selection, 0.5).unwrap();
    let terminal_backgrounds = [
        terminal_background.to_owned(),
        render_layers(terminal_background, &[selection]).unwrap(),
        render_layers(terminal_background, &[&unfocused_selection]).unwrap(),
    ];
    for family in std::iter::once(None).chain(
        [
            "black", "red", "green", "yellow", "blue", "magenta", "cyan", "white",
        ]
        .into_iter()
        .map(Some),
    ) {
        let triplet = match family {
            None => [
                "terminal.dim_foreground".to_owned(),
                "terminal.foreground".to_owned(),
                "terminal.bright_foreground".to_owned(),
            ],
            Some(family) => [
                format!("terminal.ansi.dim_{family}"),
                format!("terminal.ansi.{family}"),
                format!("terminal.ansi.bright_{family}"),
            ],
        };
        for terminal_background in &terminal_backgrounds {
            let contrasts: [f64; 3] = std::array::from_fn(|index| {
                contrast_ratio(role(style, &triplet[index]), terminal_background).unwrap()
            });
            assert!(contrasts[0] <= contrasts[1] + 1e-9);
            assert!(contrasts[1] <= contrasts[2] + 1e-9);
            assert!(contrasts[0] >= HARD_TEXT_CONTRAST - 1e-9);
        }
    }
}
