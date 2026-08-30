//! Defines stable semantic syntax roles and nested merge plans.

use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum SemanticRole {
    Base,
    Subdued,
    Predictive,
    Declaration,
    Type,
    Member,
    Control,
    Value,
    String,
    Special,
    Metadata,
    Link,
    DiffChange,
    DiffAdd,
    DiffDelete,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ToneBand {
    Primary,
    Secondary,
    Subdued,
}

impl ToneBand {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Secondary => "secondary",
            Self::Subdued => "subdued",
        }
    }

    pub fn saliency(self) -> f64 {
        match self {
            Self::Primary => 0.90,
            Self::Secondary => 0.75,
            Self::Subdued => 0.55,
        }
    }

    pub fn single_hue_saliency(self) -> f64 {
        match self {
            Self::Primary => 1.00,
            Self::Secondary => 0.65,
            Self::Subdued => 0.40,
        }
    }
}

impl SemanticRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Base => "base",
            Self::Subdued => "subdued",
            Self::Predictive => "predictive",
            Self::Declaration => "declaration",
            Self::Type => "type",
            Self::Member => "member",
            Self::Control => "control",
            Self::Value => "value",
            Self::String => "string",
            Self::Special => "special",
            Self::Metadata => "metadata",
            Self::Link => "link",
            Self::DiffChange => "diff_change",
            Self::DiffAdd => "diff_add",
            Self::DiffDelete => "diff_delete",
        }
    }

    pub fn saliency(self) -> f64 {
        match self {
            Self::Base | Self::DiffChange | Self::DiffAdd | Self::DiffDelete => 1.0,
            _ => self.tone_band().saliency(),
        }
    }

    pub fn tone_band(self) -> ToneBand {
        match self {
            Self::Base
            | Self::Declaration
            | Self::Type
            | Self::Control
            | Self::Special
            | Self::DiffChange
            | Self::DiffAdd
            | Self::DiffDelete => ToneBand::Primary,
            Self::Member | Self::Value | Self::String | Self::Link => ToneBand::Secondary,
            Self::Metadata | Self::Subdued | Self::Predictive => ToneBand::Subdued,
        }
    }
}

pub const ORDINARY_ROLES: [SemanticRole; 9] = [
    SemanticRole::Declaration,
    SemanticRole::Type,
    SemanticRole::Member,
    SemanticRole::Control,
    SemanticRole::Value,
    SemanticRole::String,
    SemanticRole::Special,
    SemanticRole::Metadata,
    SemanticRole::Link,
];

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Family {
    pub anchor: SemanticRole,
    pub roles: Vec<SemanticRole>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MergePlan {
    pub family_count: usize,
    pub families: Vec<Family>,
}

fn anchor_for(role: SemanticRole, family_count: usize) -> SemanticRole {
    match role {
        SemanticRole::Declaration | SemanticRole::Member | SemanticRole::Metadata => role,
        SemanticRole::Type | SemanticRole::Control | SemanticRole::Special if family_count < 4 => {
            SemanticRole::Declaration
        }
        SemanticRole::Control | SemanticRole::Special if family_count < 5 => SemanticRole::Type,
        SemanticRole::Special if family_count < 7 => SemanticRole::Control,
        SemanticRole::Value | SemanticRole::String if family_count < 6 => SemanticRole::Member,
        SemanticRole::String if family_count < 8 => SemanticRole::Value,
        SemanticRole::Link => SemanticRole::Member,
        _ => role,
    }
}

impl MergePlan {
    pub fn from_breadth(authored_breadth: f64) -> Self {
        let family_count = (3.0 + 5.0 * authored_breadth.clamp(0.0, 1.0)).round() as usize;
        Self::with_family_count(family_count)
    }

    pub fn with_family_count(family_count: usize) -> Self {
        let family_count = family_count.clamp(3, 8);
        let mut families = Vec::<Family>::new();
        for role in ORDINARY_ROLES {
            let anchor = anchor_for(role, family_count);
            if let Some(family) = families.iter_mut().find(|family| family.anchor == anchor) {
                family.roles.push(role);
            } else {
                families.push(Family {
                    anchor,
                    roles: vec![role],
                });
            }
        }

        families.sort_by_key(|family| {
            ORDINARY_ROLES
                .iter()
                .position(|role| *role == family.anchor)
                .unwrap()
        });

        Self {
            family_count,
            families,
        }
    }

    pub fn family_for(&self, role: SemanticRole) -> Option<usize> {
        self.families
            .iter()
            .position(|family| family.roles.contains(&role))
    }

    pub fn audit(&self) -> Value {
        let families = self
            .families
            .iter()
            .enumerate()
            .map(|(index, family)| {
                json!({
                    "id": index,
                    "anchor": family.anchor.as_str(),
                    "roles": family.roles.iter().map(|role| role.as_str()).collect::<Vec<_>>(),
                    "tone_band": family.anchor.tone_band().as_str(),
                    "saliency_preference": family.roles.iter().map(|role| role.saliency()).fold(0.0, f64::max),
                })
            })
            .collect::<Vec<_>>();
        let intentional_merges = self
            .families
            .iter()
            .filter(|family| family.roles.len() > 1)
            .map(|family| {
                family
                    .roles
                    .iter()
                    .map(|role| role.as_str())
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        json!({
            "family_count": self.family_count,
            "selection": "palette-native hue-family budget",
            "families": families,
            "intentional_merges": intentional_merges,
            "tone_safe": true,
            "nested": true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn plans_cover_every_ordinary_role_once() {
        for count in 3..=8 {
            let plan = MergePlan::with_family_count(count);
            assert_eq!(plan.families.len(), count);
            let roles = plan
                .families
                .iter()
                .flat_map(|family| family.roles.iter().copied())
                .collect::<Vec<_>>();
            assert_eq!(roles.len(), ORDINARY_ROLES.len());
            assert_eq!(
                roles.into_iter().collect::<BTreeSet<_>>().len(),
                ORDINARY_ROLES.len()
            );
        }
    }

    #[test]
    fn each_plan_only_splits_the_previous_plan() {
        for count in 3..8 {
            let current = MergePlan::with_family_count(count);
            let next = MergePlan::with_family_count(count + 1);
            for family in &next.families {
                assert!(
                    current.families.iter().any(|parent| {
                        family.roles.iter().all(|role| parent.roles.contains(role))
                    })
                );
            }
        }
    }

    #[test]
    fn merges_never_cross_tone_bands() {
        for count in 3..=8 {
            let plan = MergePlan::with_family_count(count);
            for family in plan.families {
                assert!(
                    family
                        .roles
                        .iter()
                        .all(|role| role.tone_band() == family.anchor.tone_band())
                );
            }
        }
    }

    #[test]
    fn ordinary_tone_map_is_independent_of_merge_count() {
        let expected = [
            (ToneBand::Primary, 4),
            (ToneBand::Secondary, 4),
            (ToneBand::Subdued, 1),
        ];
        for count in 3..=8 {
            let plan = MergePlan::with_family_count(count);
            for (band, expected_count) in expected {
                assert_eq!(
                    plan.families
                        .iter()
                        .flat_map(|family| family.roles.iter())
                        .filter(|role| role.tone_band() == band)
                        .count(),
                    expected_count
                );
            }
        }
    }

    #[test]
    fn breadth_selection_is_bounded_and_deterministic() {
        assert_eq!(MergePlan::from_breadth(-1.0).family_count, 3);
        assert_eq!(MergePlan::from_breadth(0.0), MergePlan::from_breadth(0.0));
        assert_eq!(MergePlan::from_breadth(1.0).family_count, 8);
        assert_eq!(MergePlan::from_breadth(2.0).family_count, 8);
    }
}
