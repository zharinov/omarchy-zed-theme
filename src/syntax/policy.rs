//! Static capture policy shared by syntax construction and validation.

use super::plan::SemanticRole;
pub const SYNTAX_PRIMARY_FLOOR: f64 = 4.52;
pub const SYNTAX_SEMANTIC_FLOOR: f64 = 3.52;
pub const SYNTAX_SUBDUED_FLOOR: f64 = 3.02;
pub const SYNTAX_ADAPTIVE_OVERLAY_FLOOR: f64 = 2.00;
pub const SYNTAX_SUBDUED_OVERLAY_FLOOR: f64 = 1.30;
const _: () = {
    assert!(SYNTAX_ADAPTIVE_OVERLAY_FLOOR < SYNTAX_SEMANTIC_FLOOR);
    assert!(SYNTAX_SUBDUED_OVERLAY_FLOOR < SYNTAX_SUBDUED_FLOOR);
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CapturePolicy {
    pub capture: &'static str,
    pub role: SemanticRole,
}

use SemanticRole::*;

pub const CAPTURE_POLICIES: [CapturePolicy; 56] = [
    CapturePolicy {
        capture: "attribute",
        role: Member,
    },
    CapturePolicy {
        capture: "boolean",
        role: Value,
    },
    CapturePolicy {
        capture: "comment",
        role: Subdued,
    },
    CapturePolicy {
        capture: "comment.doc",
        role: Subdued,
    },
    CapturePolicy {
        capture: "constant",
        role: Value,
    },
    CapturePolicy {
        capture: "constructor",
        role: Callable,
    },
    CapturePolicy {
        capture: "diff.minus",
        role: DiffDelete,
    },
    CapturePolicy {
        capture: "diff.plus",
        role: DiffAdd,
    },
    CapturePolicy {
        capture: "embedded",
        role: Base,
    },
    CapturePolicy {
        capture: "emphasis",
        role: Base,
    },
    CapturePolicy {
        capture: "emphasis.strong",
        role: Base,
    },
    CapturePolicy {
        capture: "enum",
        role: Type,
    },
    CapturePolicy {
        capture: "function",
        role: Callable,
    },
    CapturePolicy {
        capture: "function.builtin",
        role: Callable,
    },
    CapturePolicy {
        capture: "hint",
        role: Metadata,
    },
    CapturePolicy {
        capture: "keyword",
        role: Control,
    },
    CapturePolicy {
        capture: "label",
        role: Base,
    },
    CapturePolicy {
        capture: "link_text",
        role: Link,
    },
    CapturePolicy {
        capture: "link_uri",
        role: Link,
    },
    CapturePolicy {
        capture: "namespace",
        role: Type,
    },
    CapturePolicy {
        capture: "number",
        role: Value,
    },
    CapturePolicy {
        capture: "operator",
        role: Base,
    },
    CapturePolicy {
        capture: "predictive",
        role: Predictive,
    },
    CapturePolicy {
        capture: "preproc",
        role: Control,
    },
    CapturePolicy {
        capture: "primary",
        role: Base,
    },
    CapturePolicy {
        capture: "property",
        role: Member,
    },
    CapturePolicy {
        capture: "punctuation",
        role: Subdued,
    },
    CapturePolicy {
        capture: "punctuation.bracket",
        role: Base,
    },
    CapturePolicy {
        capture: "punctuation.delimiter",
        role: Subdued,
    },
    CapturePolicy {
        capture: "punctuation.list_marker",
        role: Subdued,
    },
    CapturePolicy {
        capture: "punctuation.markup",
        role: Subdued,
    },
    CapturePolicy {
        capture: "punctuation.special",
        role: Base,
    },
    CapturePolicy {
        capture: "selector",
        role: Member,
    },
    CapturePolicy {
        capture: "selector.pseudo",
        role: Member,
    },
    CapturePolicy {
        capture: "string",
        role: String,
    },
    CapturePolicy {
        capture: "string.escape",
        role: String,
    },
    CapturePolicy {
        capture: "string.regex",
        role: String,
    },
    CapturePolicy {
        capture: "string.special",
        role: String,
    },
    CapturePolicy {
        capture: "string.special.symbol",
        role: String,
    },
    CapturePolicy {
        capture: "tag",
        role: Member,
    },
    CapturePolicy {
        capture: "text.literal",
        role: Value,
    },
    CapturePolicy {
        capture: "title",
        role: Base,
    },
    CapturePolicy {
        capture: "type",
        role: Type,
    },
    CapturePolicy {
        capture: "variable",
        role: Base,
    },
    CapturePolicy {
        capture: "variable.parameter",
        role: Base,
    },
    CapturePolicy {
        capture: "variable.special",
        role: Base,
    },
    CapturePolicy {
        capture: "variant",
        role: Value,
    },
    CapturePolicy {
        capture: "concept",
        role: Type,
    },
    CapturePolicy {
        capture: "diff",
        role: DiffChange,
    },
    CapturePolicy {
        capture: "lifetime",
        role: Type,
    },
    CapturePolicy {
        capture: "markup",
        role: Base,
    },
    CapturePolicy {
        capture: "module",
        role: Type,
    },
    CapturePolicy {
        capture: "storageclass",
        role: Control,
    },
    CapturePolicy {
        capture: "strikethrough",
        role: Subdued,
    },
    CapturePolicy {
        capture: "text",
        role: Base,
    },
    CapturePolicy {
        capture: "warning",
        role: Metadata,
    },
];

pub fn capture_policy(capture: &str) -> Option<&'static CapturePolicy> {
    CAPTURE_POLICIES
        .iter()
        .find(|policy| policy.capture == capture)
}

pub fn contrast_floor(capture: &str) -> Option<f64> {
    capture_policy(capture).map(|policy| match policy.role {
        Base => SYNTAX_PRIMARY_FLOOR,
        Subdued => SYNTAX_SUBDUED_FLOOR,
        _ => SYNTAX_SEMANTIC_FLOOR,
    })
}

pub fn overlay_contrast_floor(capture: &str) -> Option<f64> {
    capture_policy(capture).map(|policy| match policy.role {
        Base | Predictive => SYNTAX_PRIMARY_FLOOR,
        DiffChange | DiffAdd | DiffDelete => SYNTAX_SEMANTIC_FLOOR,
        Subdued | Metadata => SYNTAX_SUBDUED_OVERLAY_FLOOR,
        _ => SYNTAX_ADAPTIVE_OVERLAY_FLOOR,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::{ADDITIONAL_SYNTAX_FIELDS, BASE_SYNTAX_FIELDS};
    use std::collections::BTreeSet;

    #[test]
    fn capture_policy_exactly_covers_the_manifest() {
        let expected = BASE_SYNTAX_FIELDS
            .iter()
            .chain(ADDITIONAL_SYNTAX_FIELDS)
            .copied()
            .collect::<BTreeSet<_>>();
        let actual = CAPTURE_POLICIES
            .iter()
            .map(|policy| policy.capture)
            .collect::<BTreeSet<_>>();
        assert_eq!(actual, expected);
        assert_eq!(CAPTURE_POLICIES.len(), actual.len());
    }

    #[test]
    fn overlay_floors_only_relax_adaptive_tones() {
        assert_eq!(
            overlay_contrast_floor("primary"),
            Some(SYNTAX_PRIMARY_FLOOR)
        );
        assert_eq!(
            overlay_contrast_floor("diff.plus"),
            Some(SYNTAX_SEMANTIC_FLOOR)
        );
        assert_eq!(
            overlay_contrast_floor("function"),
            Some(SYNTAX_ADAPTIVE_OVERLAY_FLOOR)
        );
        assert_eq!(
            overlay_contrast_floor("comment"),
            Some(SYNTAX_SUBDUED_OVERLAY_FLOOR)
        );
        assert_eq!(
            overlay_contrast_floor("warning"),
            Some(SYNTAX_SUBDUED_OVERLAY_FLOOR)
        );
    }

    #[test]
    fn captures_project_onto_the_intended_semantic_partition() {
        for capture in [
            "string",
            "string.escape",
            "string.regex",
            "string.special",
            "string.special.symbol",
        ] {
            assert_eq!(capture_policy(capture).unwrap().role, String, "{capture}");
        }
        for capture in ["boolean", "constant", "number", "text.literal", "variant"] {
            assert_eq!(capture_policy(capture).unwrap().role, Value, "{capture}");
        }
        for capture in ["constructor", "function", "function.builtin"] {
            assert_eq!(capture_policy(capture).unwrap().role, Callable, "{capture}");
        }
        for capture in ["label", "title", "punctuation.special", "variable.special"] {
            assert_eq!(capture_policy(capture).unwrap().role, Base, "{capture}");
        }
        assert_eq!(capture_policy("selector.pseudo").unwrap().role, Member);
        assert_eq!(capture_policy("lifetime").unwrap().role, Type);
    }
}
