pub const SCHEMA_URL: &str = "https://zed.dev/schema/themes/v0.2.0.json";
pub const THEME_NAME: &str = "Omarchy";

// This 384-step grid intentionally oversamples the 8-bit output: the tested
// 256-step alternative skipped feasible boundary colors after gamut mapping.
pub const CANDIDATE_LIGHTNESS_STEPS: u16 = 384;
pub const CANDIDATE_CHROMA_STEPS: u8 = 16;

// Search targets leave quantization headroom above the corresponding hard floors.
// Validation uses the hard floors and remains the final authority.
pub const TEXT_CONTRAST: f64 = 4.52;
pub const CONTROL_CONTRAST: f64 = 3.02;
pub const PASSIVE_CONTRAST: f64 = 1.52;
pub const HARD_TEXT_CONTRAST: f64 = 4.50;
pub const HARD_CONTROL_CONTRAST: f64 = 3.00;
pub const HARD_PASSIVE_CONTRAST: f64 = 1.50;
pub const TERMINAL_NORMAL_PREFERRED: f64 = 5.52;
pub const TERMINAL_BRIGHT_PREFERRED: f64 = 7.02;
pub const STATE_HOVER_CONTRAST: f64 = 1.20;
pub const STATE_ACTIVE_CONTRAST: f64 = 1.30;
pub const STATE_SELECTED_CONTRAST: f64 = 1.40;
pub const STATE_CONSECUTIVE_CONTRAST: f64 = 1.05;
pub const STATE_CONSECUTIVE_DELTA_E: f64 = 0.025;
pub const STATE_BASE_CONTRAST_STEP: f64 = 0.10;
pub const RUNTIME_STATE_BASE_CONTRAST_STEP: f64 = 0.001;
pub const RUNTIME_STATE_CONSECUTIVE_CONTRAST: f64 = 1.01;
pub const RUNTIME_STATE_CONSECUTIVE_DELTA_E: f64 = 0.005;
pub const UI_STATE_TEXT_CONTRAST: f64 = TEXT_CONTRAST * 1.75 * 1.05;
pub const LAYER_HOVER_CONTRAST: f64 = 1.35;
pub const LAYER_ACTIVE_CONTRAST: f64 = 1.55;
pub const LAYER_SELECTED_CONTRAST: f64 = 1.75;
pub const TAB_STATE_CONTRAST: f64 = 1.08;
pub const SEARCH_MATCH_CONTRAST: f64 = 1.20;
pub const SEARCH_ACTIVE_CONTRAST: f64 = 1.40;
pub const EDITOR_OVERLAY_TEXT_CONTRAST: f64 = TEXT_CONTRAST * FOCUSED_SELECTION_CONTRAST;
pub const EDITOR_BASE_TEXT_CONTRAST: f64 = EDITOR_OVERLAY_TEXT_CONTRAST * DIFF_FILL_CONTRAST * 1.08;
pub const EDITOR_CANVAS_TEXT_CONTRAST: f64 = EDITOR_BASE_TEXT_CONTRAST * STATE_SELECTED_CONTRAST;
pub const FOCUSED_SELECTION_CONTRAST: f64 = 1.20;
pub const FOCUSED_SELECTION_DELTA_E: f64 = 0.040;
pub const PLAYER_SELECTION_DELTA_E: f64 = 0.001;
pub const OVERLAY_MAX_ALPHA: u8 = 250;
pub const PREFERRED_HIGHLIGHT_MAX_ALPHA: u8 = 166;
pub const WORD_OVERLAY_MAX_ALPHA: u8 = 198;
pub const LIGHT_DIFF_HUNK_FILLED_OPACITY: f64 = 0.16;
pub const LIGHT_DIFF_HUNK_HOLLOW_BACKGROUND_OPACITY: f64 = 0.08;
pub const LIGHT_DIFF_HUNK_HOLLOW_BORDER_OPACITY: f64 = 0.48;
pub const DARK_DIFF_HUNK_FILLED_OPACITY: f64 = 0.12;
pub const DARK_DIFF_HUNK_HOLLOW_BACKGROUND_OPACITY: f64 = 0.06;
pub const DARK_DIFF_HUNK_HOLLOW_BORDER_OPACITY: f64 = 0.36;
pub const DIFF_FILL_CONTRAST: f64 = 1.30;
pub const DIFF_HOLLOW_CONTRAST: f64 = 1.20;
pub const WORD_DIFF_CONTRAST: f64 = 1.10;
pub const WORD_TEXT_CONTRAST: f64 = EDITOR_OVERLAY_TEXT_CONTRAST;
pub const PRESENTATION_WORD_DIFF_CONTRAST: f64 = 1.05;
pub const DIFF_PAIR_CONTRAST: f64 = 1.01;
pub const DIFF_LUMINANCE_SEPARATION_CONTRAST: f64 = 1.12;
pub const DIFF_NORMAL_FLOOR_DELTA_E: f64 = 0.030;
pub const DIFF_CVD_FLOOR_DELTA_E: f64 = 0.030;
pub const DIFF_NORMAL_DELTA_E: f64 = 0.075;
pub const DIFF_CVD_DELTA_E: f64 = 0.035;
pub const DIFF_BORDER_RETENTION_DELTA_E: f64 = 0.005;
pub const DIFF_BORDER_RETENTION_RATIO: f64 = 0.15;
pub const THUMB_HOVER_CONTRAST: f64 = 3.52;
pub const THUMB_ACTIVE_CONTRAST: f64 = 4.02;
pub const STATE_HOVER_DELTA_E: f64 = 0.040;
pub const STATE_ACTIVE_DELTA_E: f64 = 0.060;
pub const STATE_SELECTED_DELTA_E: f64 = 0.080;
pub const ACCENT_NORMAL_DELTA_E: f64 = 0.035;
pub const ACCENT_CVD_DELTA_E: f64 = 0.020;

#[derive(Clone, Copy)]
pub struct PairContract {
    pub contrast: f64,
    pub normal_delta_e: f64,
    pub cvd_delta_e: f64,
    pub separation_alternative: Option<(f64, f64, f64)>,
}

pub const SEMANTIC_PAIR_CONTRACT: PairContract = PairContract {
    contrast: 1.05,
    normal_delta_e: 0.075,
    cvd_delta_e: 0.040,
    separation_alternative: Some((1.35, 0.10, 0.050)),
};

pub const SYNTAX_DIFF_CONTRACT: PairContract = PairContract {
    contrast: 1.10,
    normal_delta_e: 0.080,
    cvd_delta_e: 0.040,
    separation_alternative: None,
};

pub const FOUNDATION_FIELDS: &[&str] = &[
    "border",
    "border.variant",
    "border.focused",
    "border.selected",
    "border.transparent",
    "border.disabled",
    "elevated_surface.background",
    "surface.background",
    "background",
    "element.background",
    "element.hover",
    "element.active",
    "element.selected",
    "element.disabled",
    "element.selection_background",
    "drop_target.background",
    "drop_target.border",
    "ghost_element.background",
    "ghost_element.hover",
    "ghost_element.active",
    "ghost_element.selected",
    "ghost_element.disabled",
    "text",
    "text.muted",
    "text.placeholder",
    "text.disabled",
    "text.accent",
    "icon",
    "icon.muted",
    "icon.disabled",
    "icon.placeholder",
    "icon.accent",
    "debugger.accent",
];

pub const CHROME_FIELDS: &[&str] = &[
    "status_bar.background",
    "title_bar.background",
    "title_bar.inactive_background",
    "toolbar.background",
    "tab_bar.background",
    "tab.inactive_background",
    "tab.active_background",
    "search.match_background",
    "search.active_match_background",
    "panel.background",
    "panel.focused_border",
    "panel.indent_guide",
    "panel.indent_guide_hover",
    "panel.indent_guide_active",
    "panel.overlay_background",
    "panel.overlay_hover",
    "pane.focused_border",
    "pane_group.border",
    "scrollbar.thumb.background",
    "scrollbar.thumb.hover_background",
    "scrollbar.thumb.active_background",
    "scrollbar.thumb.border",
    "scrollbar.track.background",
    "scrollbar.track.border",
    "minimap.thumb.background",
    "minimap.thumb.hover_background",
    "minimap.thumb.active_background",
    "minimap.thumb.border",
];

pub const EDITOR_FIELDS: &[&str] = &[
    "editor.foreground",
    "editor.background",
    "editor.gutter.background",
    "editor.subheader.background",
    "editor.active_line.background",
    "editor.highlighted_line.background",
    "editor.debugger_active_line.background",
    "editor.line_number",
    "editor.active_line_number",
    "editor.hover_line_number",
    "editor.invisible",
    "editor.wrap_guide",
    "editor.active_wrap_guide",
    "editor.indent_guide",
    "editor.indent_guide_active",
    "editor.document_highlight.read_background",
    "editor.document_highlight.write_background",
    "editor.document_highlight.bracket_background",
    "editor.diff_hunk.added.background",
    "editor.diff_hunk.added.hollow_background",
    "editor.diff_hunk.added.hollow_border",
    "editor.diff_hunk.deleted.background",
    "editor.diff_hunk.deleted.hollow_background",
    "editor.diff_hunk.deleted.hollow_border",
];

pub const TERMINAL_FIELDS: &[&str] = &[
    "terminal.background",
    "terminal.foreground",
    "terminal.ansi.background",
    "terminal.bright_foreground",
    "terminal.dim_foreground",
    "terminal.ansi.black",
    "terminal.ansi.bright_black",
    "terminal.ansi.dim_black",
    "terminal.ansi.red",
    "terminal.ansi.bright_red",
    "terminal.ansi.dim_red",
    "terminal.ansi.green",
    "terminal.ansi.bright_green",
    "terminal.ansi.dim_green",
    "terminal.ansi.yellow",
    "terminal.ansi.bright_yellow",
    "terminal.ansi.dim_yellow",
    "terminal.ansi.blue",
    "terminal.ansi.bright_blue",
    "terminal.ansi.dim_blue",
    "terminal.ansi.magenta",
    "terminal.ansi.bright_magenta",
    "terminal.ansi.dim_magenta",
    "terminal.ansi.cyan",
    "terminal.ansi.bright_cyan",
    "terminal.ansi.dim_cyan",
    "terminal.ansi.white",
    "terminal.ansi.bright_white",
    "terminal.ansi.dim_white",
];

pub const LINK_VC_FIELDS: &[&str] = &[
    "link_text.hover",
    "version_control.added",
    "version_control.deleted",
    "version_control.modified",
    "version_control.renamed",
    "version_control.conflict",
    "version_control.ignored",
    "version_control.word_added",
    "version_control.word_deleted",
    "version_control.conflict_marker.ours",
    "version_control.conflict_marker.theirs",
];

pub const VIM_FIELDS: &[&str] = &[
    "vim.normal.background",
    "vim.insert.background",
    "vim.replace.background",
    "vim.visual.background",
    "vim.visual_line.background",
    "vim.visual_block.background",
    "vim.yank.background",
    "vim.helix_jump_label.foreground",
    "vim.helix_normal.background",
    "vim.helix_select.background",
    "vim.normal.foreground",
    "vim.insert.foreground",
    "vim.replace.foreground",
    "vim.visual.foreground",
    "vim.visual_line.foreground",
    "vim.visual_block.foreground",
    "vim.helix_normal.foreground",
    "vim.helix_select.foreground",
];

pub const STATUS_NAMES: &[&str] = &[
    "conflict",
    "created",
    "deleted",
    "error",
    "hidden",
    "hint",
    "ignored",
    "info",
    "modified",
    "predictive",
    "renamed",
    "success",
    "unreachable",
    "warning",
];

pub const BASE_SYNTAX_FIELDS: &[&str] = &[
    "attribute",
    "boolean",
    "comment",
    "comment.doc",
    "constant",
    "constructor",
    "diff.minus",
    "diff.plus",
    "embedded",
    "emphasis",
    "emphasis.strong",
    "enum",
    "function",
    "function.builtin",
    "hint",
    "keyword",
    "label",
    "link_text",
    "link_uri",
    "namespace",
    "number",
    "operator",
    "predictive",
    "preproc",
    "primary",
    "property",
    "punctuation",
    "punctuation.bracket",
    "punctuation.delimiter",
    "punctuation.list_marker",
    "punctuation.markup",
    "punctuation.special",
    "selector",
    "selector.pseudo",
    "string",
    "string.escape",
    "string.regex",
    "string.special",
    "string.special.symbol",
    "tag",
    "text.literal",
    "title",
    "type",
    "variable",
    "variable.parameter",
    "variable.special",
    "variant",
];

pub const ADDITIONAL_SYNTAX_FIELDS: &[&str] = &[
    "concept",
    "diff",
    "lifetime",
    "markup",
    "module",
    "storageclass",
    "strikethrough",
    "text",
    "warning",
];

pub const CANONICAL_COLOR_KEYS: &[&str] = &[
    "accent",
    "selection",
    "muted",
    "background",
    "dark_background",
    "darker_background",
    "lighter_background",
    "foreground",
    "dark_foreground",
    "light_foreground",
    "bright_foreground",
    "red",
    "yellow",
    "orange",
    "green",
    "cyan",
    "blue",
    "magenta",
    "brown",
    "bright_red",
    "bright_yellow",
    "bright_green",
    "bright_cyan",
    "bright_blue",
    "bright_magenta",
    "color0",
    "color1",
    "color2",
    "color3",
    "color4",
    "color5",
    "color6",
    "color7",
    "color8",
    "color9",
    "color10",
    "color11",
    "color12",
    "color13",
    "color14",
    "color15",
    "cursor",
    "selection_background",
    "selection_foreground",
];

pub const COLOR_ALIASES: &[(&str, &str)] = &[
    ("bg", "background"),
    ("dark_bg", "dark_background"),
    ("darker_bg", "darker_background"),
    ("lighter_bg", "lighter_background"),
    ("fg", "foreground"),
    ("dark_fg", "dark_foreground"),
    ("light_fg", "light_foreground"),
    ("bright_fg", "bright_foreground"),
    ("purple", "magenta"),
    ("bright_purple", "bright_magenta"),
];
