//! Semantic colors that are shared by multiple Zed roles.
//!
//! The generator solves these tokens before serialization. Exact aliases are
//! therefore represented once here instead of being rediscovered through
//! independent searches or checked only after JSON emission.

use crate::color::parse_hex;
#[cfg(test)]
use crate::color::with_alpha;
use crate::{Error, Result};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct OpaqueColor(String);

impl OpaqueColor {
    pub(crate) fn new(value: String) -> Result<Self> {
        if parse_hex(&value)?.a < 1.0 {
            return Err(Error::invalid(format!(
                "opaque token received translucent color {value}"
            )));
        }
        Ok(Self(value))
    }

    pub(crate) fn to_hex(&self) -> String {
        self.0.clone()
    }

    #[cfg(test)]
    pub(crate) fn with_alpha(&self, alpha: u8) -> Result<OverlayColor> {
        OverlayColor::new(with_alpha(&self.0, f64::from(alpha) / 255.0)?)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct OverlayColor(String);

impl OverlayColor {
    pub(crate) fn new(value: String) -> Result<Self> {
        if parse_hex(&value)?.a >= 1.0 {
            return Err(Error::invalid(format!(
                "overlay token received opaque color {value}"
            )));
        }
        Ok(Self(value))
    }

    pub(crate) fn transparent() -> Self {
        Self("#00000000".into())
    }

    #[cfg(test)]
    pub(crate) fn hex(&self) -> &str {
        &self.0
    }

    pub(crate) fn to_hex(&self) -> String {
        self.0.clone()
    }
}

#[derive(Clone, Debug)]
pub(crate) struct SurfaceTokens {
    pub(crate) editor_canvas: OpaqueColor,
    pub(crate) app_frame: OpaqueColor,
    pub(crate) elevated: OpaqueColor,
    pub(crate) secondary: OpaqueColor,
    pub(crate) inactive_control: OpaqueColor,
    pub(crate) editor_highlighted_line: OverlayColor,
}

#[derive(Clone, Debug)]
pub(crate) struct ContentTokens {
    pub(crate) primary: OpaqueColor,
    pub(crate) muted: OpaqueColor,
    pub(crate) placeholder: OpaqueColor,
    pub(crate) disabled: OpaqueColor,
    pub(crate) icon_muted: OpaqueColor,
    pub(crate) icon_placeholder: OpaqueColor,
    pub(crate) icon_disabled: OpaqueColor,
    pub(crate) accent: OpaqueColor,
    pub(crate) editor_primary: OpaqueColor,
}

#[derive(Clone, Debug)]
pub(crate) struct InteractionTokens {
    pub(crate) element_hover: OverlayColor,
    pub(crate) element_active: OverlayColor,
    pub(crate) element_selected: OverlayColor,
    pub(crate) ghost_hover: OverlayColor,
    pub(crate) ghost_active: OverlayColor,
    pub(crate) ghost_selected: OverlayColor,
}

#[derive(Clone, Debug)]
pub(crate) struct StatusChannel {
    pub(crate) foreground: OpaqueColor,
    pub(crate) background: OpaqueColor,
    pub(crate) border: OpaqueColor,
}

#[derive(Clone, Debug)]
pub(crate) struct StatusTokens {
    pub(crate) positive: StatusChannel,
    pub(crate) negative: StatusChannel,
    pub(crate) warning: StatusChannel,
    pub(crate) informational: StatusChannel,
    pub(crate) predictive: StatusChannel,
    pub(crate) hint: StatusChannel,
    pub(crate) hidden: StatusChannel,
    pub(crate) ignored: StatusChannel,
    pub(crate) unreachable: StatusChannel,
}

#[derive(Clone, Debug)]
pub(crate) struct DerivedTokens {
    pub(crate) editor_active_line: OverlayColor,
    pub(crate) wrap_guide: OverlayColor,
    pub(crate) active_wrap_guide: OverlayColor,
    pub(crate) document_read: OverlayColor,
}

#[derive(Clone, Debug)]
pub(crate) struct ThemeTokens {
    pub(crate) surfaces: SurfaceTokens,
    pub(crate) content: ContentTokens,
    pub(crate) interactions: InteractionTokens,
    pub(crate) statuses: StatusTokens,
    pub(crate) derived: DerivedTokens,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RoleColor {
    role: String,
    value: String,
}

impl RoleColor {
    fn new(role: impl Into<String>, value: String) -> Self {
        Self {
            role: role.into(),
            value,
        }
    }

    fn opaque(role: impl Into<String>, color: &OpaqueColor) -> Self {
        Self::new(role, color.to_hex())
    }

    fn overlay(role: impl Into<String>, color: &OverlayColor) -> Self {
        Self::new(role, color.to_hex())
    }

    pub(crate) fn opaque_value(role: impl Into<String>, value: String) -> Result<Self> {
        let color = OpaqueColor::new(value)?;
        Ok(Self::new(role, color.0))
    }

    pub(crate) fn overlay_value(role: impl Into<String>, value: String) -> Result<Self> {
        let color = OverlayColor::new(value)?;
        Ok(Self::new(role, color.0))
    }

    pub(crate) fn into_parts(self) -> (String, String) {
        (self.role, self.value)
    }
}

fn push_opaque(roles: &mut Vec<RoleColor>, names: &[&'static str], color: &OpaqueColor) {
    roles.extend(names.iter().map(|role| RoleColor::opaque(*role, color)));
}

fn push_overlay(roles: &mut Vec<RoleColor>, names: &[&'static str], color: &OverlayColor) {
    roles.extend(names.iter().map(|role| RoleColor::overlay(*role, color)));
}

impl ThemeTokens {
    pub(crate) fn zed_roles(&self) -> Vec<RoleColor> {
        let mut roles = Vec::new();
        push_opaque(
            &mut roles,
            &[
                "editor.background",
                "editor.gutter.background",
                "tab.active_background",
                "toolbar.background",
            ],
            &self.surfaces.editor_canvas,
        );
        push_opaque(
            &mut roles,
            &[
                "background",
                "status_bar.background",
                "title_bar.background",
            ],
            &self.surfaces.app_frame,
        );
        push_opaque(
            &mut roles,
            &["elevated_surface.background"],
            &self.surfaces.elevated,
        );
        push_opaque(
            &mut roles,
            &["surface.background"],
            &self.surfaces.secondary,
        );
        push_opaque(
            &mut roles,
            &[
                "element.disabled",
                "ghost_element.disabled",
                "title_bar.inactive_background",
            ],
            &self.surfaces.inactive_control,
        );
        roles.push(RoleColor::overlay(
            "editor.highlighted_line.background",
            &self.surfaces.editor_highlighted_line,
        ));

        push_opaque(&mut roles, &["text", "icon"], &self.content.primary);
        push_opaque(&mut roles, &["text.muted"], &self.content.muted);
        push_opaque(&mut roles, &["text.placeholder"], &self.content.placeholder);
        push_opaque(&mut roles, &["text.disabled"], &self.content.disabled);
        push_opaque(&mut roles, &["icon.muted"], &self.content.icon_muted);
        push_opaque(
            &mut roles,
            &["icon.placeholder"],
            &self.content.icon_placeholder,
        );
        push_opaque(&mut roles, &["icon.disabled"], &self.content.icon_disabled);
        push_opaque(
            &mut roles,
            &["text.accent", "icon.accent", "link_text.hover"],
            &self.content.accent,
        );
        push_opaque(
            &mut roles,
            &["editor.foreground"],
            &self.content.editor_primary,
        );
        push_overlay(
            &mut roles,
            &["element.hover"],
            &self.interactions.element_hover,
        );
        push_overlay(
            &mut roles,
            &["element.active"],
            &self.interactions.element_active,
        );
        push_overlay(
            &mut roles,
            &["element.selected"],
            &self.interactions.element_selected,
        );
        push_overlay(
            &mut roles,
            &["ghost_element.hover"],
            &self.interactions.ghost_hover,
        );
        push_overlay(
            &mut roles,
            &["ghost_element.active"],
            &self.interactions.ghost_active,
        );
        push_overlay(
            &mut roles,
            &["ghost_element.selected"],
            &self.interactions.ghost_selected,
        );
        push_overlay(
            &mut roles,
            &[
                "border.transparent",
                "ghost_element.background",
                "scrollbar.track.background",
            ],
            &OverlayColor::transparent(),
        );

        push_overlay(
            &mut roles,
            &["editor.active_line.background"],
            &self.derived.editor_active_line,
        );
        push_overlay(&mut roles, &["editor.wrap_guide"], &self.derived.wrap_guide);
        push_overlay(
            &mut roles,
            &["editor.active_wrap_guide"],
            &self.derived.active_wrap_guide,
        );
        push_overlay(
            &mut roles,
            &["editor.document_highlight.read_background"],
            &self.derived.document_read,
        );

        roles
    }
}

impl StatusTokens {
    pub(crate) fn zed_roles(&self) -> Vec<RoleColor> {
        let mut roles = Vec::new();
        for (names, channel) in [
            (&["created", "success"][..], &self.positive),
            (&["deleted", "error"][..], &self.negative),
            (&["conflict", "modified", "warning"][..], &self.warning),
            (&["info", "renamed"][..], &self.informational),
            (&["predictive"][..], &self.predictive),
            (&["hint"][..], &self.hint),
            (&["hidden"][..], &self.hidden),
            (&["ignored"][..], &self.ignored),
            (&["unreachable"][..], &self.unreachable),
        ] {
            for name in names {
                roles.extend([
                    RoleColor::opaque(*name, &channel.foreground),
                    RoleColor::opaque(format!("{name}.background"), &channel.background),
                    RoleColor::opaque(format!("{name}.border"), &channel.border),
                ]);
            }
        }

        roles
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opaque_and_overlay_tokens_reject_the_wrong_color_kind() {
        assert!(OpaqueColor::new("#112233".into()).is_ok());
        assert!(OpaqueColor::new("#11223380".into()).is_err());
        assert!(OverlayColor::new("#11223380".into()).is_ok());
        assert!(OverlayColor::new("#112233".into()).is_err());
        assert!(RoleColor::opaque_value("opaque", "#11223380".into()).is_err());
        assert!(RoleColor::overlay_value("overlay", "#112233".into()).is_err());
    }

    #[test]
    fn alpha_projection_retains_rgb() {
        let source = OpaqueColor::new("#123456".into()).unwrap();
        assert_eq!(source.with_alpha(0x3d).unwrap().hex(), "#1234563d");
    }

    #[test]
    fn transparent_token_is_canonical() {
        assert_eq!(OverlayColor::transparent().hex(), "#00000000");
    }
}
