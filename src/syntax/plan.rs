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
            Self::Declaration => 0.95,
            Self::Control => 0.88,
            Self::Type => 0.82,
            Self::Special => 0.74,
            Self::Value => 0.65,
            Self::String => 0.55,
            Self::Link => 0.50,
            Self::Member => 0.45,
            Self::Metadata => 0.35,
            Self::Base | Self::DiffChange | Self::DiffAdd | Self::DiffDelete => 1.0,
            Self::Subdued | Self::Predictive => 0.20,
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
        SemanticRole::Member => SemanticRole::Declaration,
        SemanticRole::String if family_count < 5 => SemanticRole::Value,
        SemanticRole::Link if family_count < 6 => SemanticRole::Declaration,
        SemanticRole::Metadata if family_count < 7 => SemanticRole::Type,
        SemanticRole::Special if family_count < 8 => SemanticRole::Control,
        _ => role,
    }
}

impl MergePlan {
    pub fn from_breadth(authored_breadth: f64) -> Self {
        let family_count = (4.0 + 4.0 * authored_breadth.clamp(0.0, 1.0)).round() as usize;
        Self::with_family_count(family_count)
    }

    pub fn with_family_count(family_count: usize) -> Self {
        let family_count = family_count.clamp(4, 8);
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
            "selection": "round(4 + 4 * authored_breadth)",
            "families": families,
            "intentional_merges": intentional_merges,
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
        for count in 4..=8 {
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
        for count in 4..8 {
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
    fn breadth_selection_is_bounded_and_deterministic() {
        assert_eq!(MergePlan::from_breadth(-1.0).family_count, 4);
        assert_eq!(MergePlan::from_breadth(0.0), MergePlan::from_breadth(0.0));
        assert_eq!(MergePlan::from_breadth(1.0).family_count, 8);
        assert_eq!(MergePlan::from_breadth(2.0).family_count, 8);
    }
}
