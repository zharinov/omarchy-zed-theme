//! Neutral boundaries are fitted after their surfaces and state fills exist.

use super::borders::{Contact, ContactGroup, Purpose, fit, fit_control_stroke};
use super::control_fills::tinted_hover;
use super::ui_policy::StructurePolicy;
use crate::color::{apply_opacity, render_layers, with_alpha};
use crate::search::MetricBand;
use crate::{Error, Result};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

struct Scenes<'a>(&'a Map<String, Value>);

impl Scenes<'_> {
    fn color(&self, role: &str) -> Result<&str> {
        self.0
            .get(role)
            .and_then(Value::as_str)
            .ok_or_else(|| Error::invalid(format!("boundary scene requires generated role {role}")))
    }

    fn fill(&self, host: &str, role: &str) -> Result<String> {
        render_layers(host, &[self.color(role)?])
    }

    fn faded_fill(&self, host: &str, role: &str, opacity: f64) -> Result<String> {
        render_layers(host, &[&apply_opacity(self.color(role)?, opacity)?])
    }
}

// A faded decorative line has a smaller salience budget. This is a policy
// projection, not the compositing equation: Contact performs the actual blend.
fn faded_band(band: MetricBand, opacity: f64) -> MetricBand {
    MetricBand::bounded(
        band.minimum().powf(opacity),
        band.preferred().unwrap_or(band.minimum()).powf(opacity),
        band.maximum().unwrap_or(21.0).powf(opacity),
    )
}

fn faded_outline_band(band: MetricBand, opacity: f64) -> MetricBand {
    // Opacity weakens the stroke, not the existing edge of the filled control.
    // Keep the whole-boundary ceiling while reducing the requested emphasis.
    MetricBand::bounded(
        band.minimum().powf(opacity),
        band.preferred().unwrap_or(band.minimum()).powf(opacity),
        band.maximum().unwrap_or(21.0),
    )
}

fn group(
    label: &'static str,
    purpose: Purpose,
    band: MetricBand,
    contacts: Vec<Contact>,
) -> ContactGroup {
    ContactGroup {
        label,
        purpose,
        band,
        contacts,
    }
}

fn separator(base: &str, opacity: f64) -> Result<Contact> {
    Contact::new(base, base, base, opacity)
}

fn outline(host: &str, fill: &str, opacity: f64) -> Result<Contact> {
    Contact::new(fill, host, fill, opacity)
}

struct Surfaces<'a> {
    canvas: &'a str,
    panel: &'a str,
    elevated: &'a str,
    chrome: &'a str,
    inactive: &'a str,
}

impl<'a> Surfaces<'a> {
    fn new(scenes: &'a Scenes<'a>) -> Result<Self> {
        let canvas = scenes.color("editor.background")?;
        let panel = scenes.color("panel.background")?;
        let elevated = scenes.color("elevated_surface.background")?;
        let chrome = scenes.color("tab_bar.background")?;
        let inactive = scenes.color("tab.inactive_background")?;
        Ok(Self {
            canvas,
            panel,
            elevated,
            chrome,
            inactive,
        })
    }

    fn hosts(&self) -> [&str; 4] {
        [self.canvas, self.panel, self.elevated, self.chrome]
    }
}

struct BoundaryGroups {
    normal: Vec<ContactGroup>,
    variant: Vec<ContactGroup>,
}

impl BoundaryGroups {
    fn workspace(surfaces: &Surfaces<'_>, policy: &StructurePolicy) -> Result<Self> {
        let Surfaces {
            canvas,
            panel,
            elevated,
            chrome,
            inactive,
        } = *surfaces;
        let bases = [canvas, panel, elevated, chrome, inactive];
        let base_lines = bases
            .iter()
            .map(|base| separator(base, 1.0))
            .collect::<Result<Vec<_>>>()?;
        let mut normal = vec![group(
            "workspace and same-surface dividers",
            Purpose::Separator,
            policy.normal,
            base_lines.clone(),
        )];
        let mut variant = vec![group(
            "section and menu dividers",
            Purpose::Separator,
            policy.passive,
            base_lines,
        )];

        let joins = [
            (canvas, panel),
            (canvas, chrome),
            (chrome, inactive),
            (panel, chrome),
        ];
        let joins = joins
            .iter()
            .map(|(outside, inside)| outline(outside, inside, 1.0))
            .collect::<Result<Vec<_>>>()?;
        normal.push(group(
            "dock, tab and header joins",
            Purpose::FilledOutline,
            policy.normal,
            joins.clone(),
        ));
        variant.push(group(
            "toolbar and preview joins",
            Purpose::FilledOutline,
            policy.passive,
            joins,
        ));

        Ok(Self { normal, variant })
    }

    fn add_controls(&mut self, controls: ControlContacts, policy: &StructurePolicy) {
        let ControlContacts {
            popup,
            controls,
            outlined,
            disabled,
            toggle,
            split,
            checkbox_hover,
            selected_tree,
            table,
            callouts,
            banners,
        } = controls;
        self.normal.push(group(
            "inputs, chips, lists and off switches",
            Purpose::FilledOutline,
            policy.normal,
            controls,
        ));
        self.normal.push(group(
            "toggle group ancestor-composited frames",
            Purpose::FilledOutline,
            faded_outline_band(policy.normal, 0.6),
            toggle,
        ));
        self.normal.push(group(
            "split button inset overlay",
            Purpose::FilledOutline,
            faded_outline_band(policy.normal, 0.8),
            split,
        ));
        self.normal.push(group(
            "checkbox hover frames",
            Purpose::FilledOutline,
            faded_outline_band(policy.normal, 0.7),
            checkbox_hover,
        ));
        self.normal.push(group(
            "selected tree frames",
            Purpose::FilledOutline,
            faded_outline_band(policy.normal, 0.4),
            selected_tree,
        ));
        self.normal.push(group(
            "unstriped data table rows",
            Purpose::FilledOutline,
            policy.normal,
            table,
        ));
        self.normal.push(group(
            "callout frames",
            Purpose::FilledOutline,
            policy.normal,
            callouts,
        ));
        self.normal.push(group(
            "info banner frames",
            Purpose::FilledOutline,
            faded_outline_band(policy.normal, 0.5),
            banners,
        ));
        self.variant.push(group(
            "elevated popup frames",
            Purpose::FilledOutline,
            policy.passive,
            popup,
        ));
        self.variant.push(group(
            "outlined button and input frames",
            Purpose::FilledOutline,
            policy.passive,
            outlined,
        ));
        self.variant.push(group(
            "disabled checkbox frames",
            Purpose::FilledOutline,
            policy.passive,
            disabled,
        ));
    }

    fn add_decorations(&mut self, surfaces: &Surfaces<'_>, policy: &StructurePolicy) -> Result<()> {
        let hosts = surfaces.hosts();
        for opacity in [0.5, 0.6, 0.8] {
            let contacts = hosts
                .iter()
                .map(|base| separator(base, opacity))
                .collect::<Result<Vec<_>>>()?;
            self.normal.push(group(
                "faded application cards and tree guides",
                Purpose::Separator,
                faded_band(policy.normal, opacity),
                contacts,
            ));
        }
        for opacity in [0.4, 0.6] {
            let contacts = hosts
                .iter()
                .map(|base| separator(base, opacity))
                .collect::<Result<Vec<_>>>()?;
            self.variant.push(group(
                "faded footers and stable scrollbar tracks",
                Purpose::Separator,
                faded_band(policy.passive, opacity),
                contacts,
            ));
        }

        Ok(())
    }

    fn add_documents(
        &mut self,
        scenes: &Scenes<'_>,
        surfaces: &Surfaces<'_>,
        policy: &StructurePolicy,
    ) -> Result<()> {
        let Surfaces {
            canvas,
            panel,
            elevated,
            ..
        } = *surfaces;
        let mut document_frames = Vec::new();
        for host in [canvas, panel, elevated] {
            for fill in [
                canvas,
                panel,
                scenes.color("title_bar.background")?,
                scenes.color("element.background")?,
            ] {
                document_frames.push(outline(host, fill, 1.0)?);
            }
        }
        self.normal.push(group(
            "Markdown, HTML and Mermaid boundaries",
            Purpose::FilledOutline,
            policy.normal,
            document_frames,
        ));
        self.normal.push(group(
            "REPL table header",
            Purpose::FilledOutline,
            policy.normal,
            vec![outline(canvas, scenes.color("border.focused")?, 1.0)?],
        ));

        Ok(())
    }
}

#[derive(Default)]
struct ControlContacts {
    popup: Vec<Contact>,
    controls: Vec<Contact>,
    outlined: Vec<Contact>,
    disabled: Vec<Contact>,
    toggle: Vec<Contact>,
    split: Vec<Contact>,
    checkbox_hover: Vec<Contact>,
    selected_tree: Vec<Contact>,
    table: Vec<Contact>,
    callouts: Vec<Contact>,
    banners: Vec<Contact>,
}

impl ControlContacts {
    fn new(scenes: &Scenes<'_>, surfaces: &Surfaces<'_>, dark: bool) -> Result<Self> {
        let mut contacts = Self::default();
        for host in surfaces.hosts() {
            contacts.add_inputs(scenes, surfaces, host, dark)?;
            contacts.add_toggles(scenes, host, dark)?;
            contacts.add_content(scenes, host)?;
        }
        Ok(contacts)
    }

    fn add_inputs(
        &mut self,
        scenes: &Scenes<'_>,
        surfaces: &Surfaces<'_>,
        host: &str,
        dark: bool,
    ) -> Result<()> {
        let Surfaces {
            canvas,
            panel,
            elevated,
            ..
        } = *surfaces;
        self.popup.push(outline(host, elevated, 1.0)?);
        self.controls.push(outline(host, canvas, 1.0)?);
        let disabled_fill = scenes.fill(host, "element.disabled")?;
        self.controls.push(outline(host, &disabled_fill, 1.0)?);
        let switch_hover =
            scenes.faded_fill(&disabled_fill, "text", if dark { 0.05 } else { 0.075 })?;
        self.controls.push(outline(host, &switch_hover, 1.0)?);
        let checkbox_disabled = scenes.faded_fill(host, "element.disabled", 0.6)?;
        self.disabled.push(outline(host, &checkbox_disabled, 1.0)?);
        for role in [
            "ghost_element.background",
            "element.background",
            "ghost_element.hover",
            "ghost_element.active",
            "ghost_element.selected",
        ] {
            let fill = scenes.fill(host, role)?;
            self.controls.push(outline(host, &fill, 1.0)?);
        }
        for role in ["ghost_element.background", "element.background"] {
            let fill = scenes.fill(host, role)?;
            self.checkbox_hover.push(outline(host, &fill, 0.7)?);
        }
        for fill in [host, canvas, panel, elevated] {
            self.outlined.push(outline(host, fill, 1.0)?);
        }
        for fill in [host, canvas, panel] {
            self.controls.push(outline(host, fill, 1.0)?);
            self.checkbox_hover.push(outline(host, fill, 0.7)?);
        }
        for role in ["ghost_element.hover", "element.active"] {
            let fill = scenes.fill(host, role)?;
            self.outlined.push(outline(host, &fill, 1.0)?);
        }
        Ok(())
    }

    fn add_toggles(&mut self, scenes: &Scenes<'_>, host: &str, dark: bool) -> Result<()> {
        for role in [
            "ghost_element.background",
            "element.background",
            "ghost_element.hover",
            "ghost_element.active",
            "element.active",
        ] {
            let fill = scenes.fill(host, role)?;
            self.split.push(outline(host, &fill, 0.8)?);
            // ToggleButtonGroup clips its children inside a fill-less frame.
            self.toggle.push(Contact::new(host, host, &fill, 0.6)?);
        }
        let selected = scenes.fill(host, "info.background")?;
        self.toggle.push(Contact::new(host, host, &selected, 0.6)?);
        self.split.push(outline(host, &selected, 0.8)?);
        let selected_hover = tinted_hover(&selected, dark)?;
        self.toggle
            .push(Contact::new(host, host, &selected_hover, 0.6)?);
        self.split.push(outline(host, &selected_hover, 0.8)?);
        Ok(())
    }

    fn add_content(&mut self, scenes: &Scenes<'_>, host: &str) -> Result<()> {
        let tree_fill = scenes.faded_fill(host, "element.active", 0.5)?;
        self.selected_tree.push(outline(host, &tree_fill, 0.4)?);
        let tree_hover = scenes.fill(host, "element.hover")?;
        self.selected_tree.push(outline(host, &tree_hover, 0.4)?);
        // DataTable only paints row borders when striping is disabled.
        let row_hover = scenes.faded_fill(host, "element.hover", 0.6)?;
        self.table.push(outline(host, &row_hover, 1.0)?);
        for (role, opacity) in [
            ("info.background", 0.1),
            ("success", 0.1),
            ("warning.background", 0.2),
            ("error", 0.08),
        ] {
            let fill = scenes.faded_fill(host, role, opacity)?;
            self.callouts.push(outline(host, &fill, 1.0)?);
        }
        let banner = scenes.faded_fill(host, "info.background", 0.5)?;
        self.banners.push(outline(host, &banner, 0.5)?);
        Ok(())
    }
}

fn derive_workspace_roles(
    surfaces: &Surfaces<'_>,
    policy: &StructurePolicy,
    output: &mut BTreeMap<String, String>,
) -> Result<()> {
    let Surfaces {
        canvas,
        panel,
        chrome,
        inactive,
        ..
    } = *surfaces;
    output.insert(
        "border.disabled".into(),
        fit(
            canvas,
            &[group(
                "disabled debugger and loading keymap inputs",
                Purpose::FilledOutline,
                policy.passive,
                vec![outline(panel, canvas, 1.0)?, outline(canvas, canvas, 1.0)?],
            )],
        )?,
    );

    let pane_contacts = [canvas, chrome, inactive]
        .iter()
        .map(|base| separator(base, 1.0))
        .collect::<Result<Vec<_>>>()?;
    output.insert(
        "pane_group.border".into(),
        fit(
            canvas,
            &[group(
                "pane split and centered padding",
                Purpose::Separator,
                policy.normal,
                pane_contacts,
            )],
        )?,
    );
    Ok(())
}

fn derive_scroll_roles(
    scenes: &Scenes<'_>,
    canvas: &str,
    policy: &StructurePolicy,
    output: &mut BTreeMap<String, String>,
) -> Result<()> {
    let track = scenes.fill(canvas, "scrollbar.track.background")?;
    output.insert(
        "scrollbar.track.border".into(),
        fit(
            canvas,
            &[group(
                "editor vertical track edge",
                Purpose::FilledOutline,
                policy.passive,
                vec![outline(canvas, &track, 1.0)?],
            )],
        )?,
    );
    for (prefix, underlays) in [
        ("scrollbar", vec![track.clone()]),
        (
            "minimap",
            vec![
                canvas.to_owned(),
                scenes.fill(canvas, "editor.active_line.background")?,
            ],
        ),
    ] {
        let mut contacts = Vec::new();
        for underlay in &underlays {
            for state in ["background", "hover_background", "active_background"] {
                let fill = scenes.fill(underlay, &format!("{prefix}.thumb.{state}"))?;
                contacts.push(outline(underlay, &fill, 1.0)?);
            }
        }
        output.insert(
            format!("{prefix}.thumb.border"),
            fit(
                canvas,
                &[group(
                    "thumb over rendered content",
                    Purpose::FilledOutline,
                    policy.passive,
                    contacts,
                )],
            )?,
        );
    }
    Ok(())
}

fn derive_guides(
    canvas: &str,
    guide_seed: &str,
    policy: &StructurePolicy,
    output: &mut BTreeMap<String, String>,
) -> Result<()> {
    for (role, band) in [
        ("editor.indent_guide", policy.passive),
        ("editor.indent_guide_active", policy.active_guide),
    ] {
        output.insert(
            role.into(),
            fit(
                canvas,
                &[group(
                    "editor indent guide",
                    Purpose::Separator,
                    band,
                    vec![separator(canvas, 1.0)?],
                )],
            )?,
        );
    }
    output.insert(
        "editor.wrap_guide".into(),
        with_alpha(guide_seed, 13.0 / 255.0)?,
    );
    output.insert(
        "editor.active_wrap_guide".into(),
        with_alpha(guide_seed, 26.0 / 255.0)?,
    );
    Ok(())
}

fn derive_control_border(
    scenes: &Scenes<'_>,
    elevated: &str,
    seed: &str,
    dark: bool,
) -> Result<String> {
    let selected = scenes.fill(elevated, "info.background")?;
    let control_fills = vec![
        scenes.fill(elevated, "ghost_element.background")?,
        scenes.fill(elevated, "ghost_element.hover")?,
        tinted_hover(&selected, dark)?,
        selected,
    ];
    // The frame blends over its host, including beside the selected child.
    fit_control_stroke(seed, elevated, &control_fills)
}

pub(super) fn derive(
    style: &Map<String, Value>,
    policy: &StructurePolicy,
    dark: bool,
) -> Result<BTreeMap<String, String>> {
    let scenes = Scenes(style);
    let surfaces = Surfaces::new(&scenes)?;
    let controls = ControlContacts::new(&scenes, &surfaces, dark)?;

    let mut groups = BoundaryGroups::workspace(&surfaces, policy)?;
    groups.add_controls(controls, policy);
    groups.add_decorations(&surfaces, policy)?;
    groups.add_documents(&scenes, &surfaces, policy)?;

    // Guides retain the quiet scene fit. Only the shared border must also
    // satisfy the translucent control stroke's stronger visibility floor.
    let scene_border = fit(surfaces.canvas, &groups.normal)?;
    let control_border = derive_control_border(&scenes, surfaces.elevated, &scene_border, dark)?;
    let variant_border = fit(surfaces.canvas, &groups.variant)?;
    let mut output = BTreeMap::from([
        ("border".into(), control_border),
        ("border.variant".into(), variant_border),
    ]);

    derive_workspace_roles(&surfaces, policy, &mut output)?;
    derive_scroll_roles(&scenes, surfaces.canvas, policy, &mut output)?;
    derive_guides(surfaces.canvas, &scene_border, policy, &mut output)?;
    Ok(output)
}
