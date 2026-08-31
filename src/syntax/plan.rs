//! Defines stable semantic syntax roles and a nested distinction budget.

use serde_json::{Value, json};
use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum SemanticRole {
    Base,
    Subdued,
    Predictive,
    Callable,
    Type,
    Member,
    Control,
    Value,
    String,
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
            Self::Callable => "callable",
            Self::Type => "type",
            Self::Member => "member",
            Self::Control => "control",
            Self::Value => "value",
            Self::String => "string",
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
            | Self::Callable
            | Self::Type
            | Self::Member
            | Self::Control
            | Self::Value
            | Self::DiffChange
            | Self::DiffAdd
            | Self::DiffDelete => ToneBand::Primary,
            Self::String | Self::Link => ToneBand::Secondary,
            Self::Metadata | Self::Subdued | Self::Predictive => ToneBand::Subdued,
        }
    }
}

pub const ORDINARY_ROLES: [SemanticRole; 8] = [
    SemanticRole::Callable,
    SemanticRole::Type,
    SemanticRole::Member,
    SemanticRole::Control,
    SemanticRole::Value,
    SemanticRole::String,
    SemanticRole::Metadata,
    SemanticRole::Link,
];

pub const MAX_SEMANTIC_BUDGET: usize = 6;

#[derive(Clone, Debug, PartialEq)]
pub struct Family {
    pub name: &'static str,
    pub anchor: SemanticRole,
    pub source_preference: SemanticRole,
    pub roles: Vec<SemanticRole>,
    pub parent: Option<usize>,
    pub fallback_saliency: f64,
    pub parent_saliency_delta: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MergePlan {
    pub budget: usize,
    pub families: Vec<Family>,
}

impl MergePlan {
    pub fn hierarchy(hue_budget: usize) -> Self {
        let hue_budget = hue_budget.clamp(1, 3);
        let mut families = vec![Family {
            name: "data",
            anchor: SemanticRole::String,
            source_preference: SemanticRole::String,
            roles: vec![SemanticRole::String],
            parent: None,
            fallback_saliency: 0.78,
            parent_saliency_delta: 0.0,
        }];
        let symbol = (hue_budget >= 2).then(|| {
            let index = families.len();
            families.push(Family {
                name: "symbol",
                anchor: SemanticRole::Type,
                source_preference: SemanticRole::Callable,
                roles: vec![SemanticRole::Type],
                parent: None,
                fallback_saliency: 0.90,
                parent_saliency_delta: 0.0,
            });
            index
        });
        if hue_budget >= 3 {
            families.push(Family {
                name: "control",
                anchor: SemanticRole::Control,
                source_preference: SemanticRole::Control,
                roles: vec![SemanticRole::Control],
                parent: None,
                fallback_saliency: 0.90,
                parent_saliency_delta: 0.0,
            });
        }
        families.push(Family {
            name: "value",
            anchor: SemanticRole::Value,
            source_preference: SemanticRole::Value,
            roles: vec![SemanticRole::Value],
            parent: Some(0),
            fallback_saliency: 0.90,
            parent_saliency_delta: 0.12,
        });
        if let Some(symbol) = symbol {
            let runtime = families.len();
            families.push(Family {
                name: "runtime_symbol",
                anchor: SemanticRole::Callable,
                source_preference: SemanticRole::Callable,
                roles: vec![SemanticRole::Callable],
                parent: Some(symbol),
                fallback_saliency: 0.78,
                parent_saliency_delta: 0.12,
            });
            families.push(Family {
                name: "member",
                anchor: SemanticRole::Member,
                source_preference: SemanticRole::Member,
                roles: vec![SemanticRole::Member],
                parent: Some(runtime),
                fallback_saliency: 0.68,
                parent_saliency_delta: 0.10,
            });
        }
        Self {
            budget: families.len(),
            families,
        }
    }

    pub fn activate(&self, active: &BTreeSet<usize>) -> Self {
        let value_active =
            self.families.iter().enumerate().any(|(index, family)| {
                active.contains(&index) && family.anchor == SemanticRole::Value
            });
        let callable_active = self.families.iter().enumerate().any(|(index, family)| {
            active.contains(&index) && family.anchor == SemanticRole::Callable
        });
        let member_active = self.families.iter().enumerate().any(|(index, family)| {
            active.contains(&index) && family.anchor == SemanticRole::Member
        });
        let remap = self
            .families
            .iter()
            .enumerate()
            .filter(|(index, _)| active.contains(index))
            .enumerate()
            .map(|(new, (old, _))| (old, new))
            .collect::<std::collections::BTreeMap<_, _>>();
        let families = self
            .families
            .iter()
            .enumerate()
            .filter(|(index, _)| active.contains(index))
            .map(|(_, family)| {
                let mut family = family.clone();
                family.parent = family.parent.and_then(|parent| remap.get(&parent).copied());
                match family.anchor {
                    SemanticRole::String if !value_active => {
                        family.name = "data";
                        family.roles.push(SemanticRole::Value);
                    }
                    SemanticRole::String => family.name = "string",
                    SemanticRole::Type if !callable_active => {
                        family.name = "symbol";
                        family
                            .roles
                            .extend([SemanticRole::Callable, SemanticRole::Member]);
                    }
                    SemanticRole::Type => family.name = "type",
                    SemanticRole::Callable if !member_active => {
                        family.name = "runtime_symbol";
                        family.roles.push(SemanticRole::Member);
                    }
                    SemanticRole::Callable => family.name = "callable",
                    _ => {}
                }
                family
            })
            .collect::<Vec<_>>();
        Self {
            budget: families.len(),
            families,
        }
    }

    pub fn with_budget(budget: usize) -> Self {
        let hierarchy = Self::hierarchy(3);
        let active = (0..budget.min(hierarchy.families.len())).collect();
        hierarchy.activate(&active)
    }

    pub fn family_for(&self, role: SemanticRole) -> Option<usize> {
        self.families
            .iter()
            .position(|family| family.roles.contains(&role))
    }

    pub fn fallback_for(&self, role: SemanticRole) -> SemanticRole {
        if role == SemanticRole::Metadata {
            SemanticRole::Subdued
        } else {
            SemanticRole::Base
        }
    }

    pub fn audit(&self) -> Value {
        json!({
            "budget": self.budget,
            "selection": "fixed semantic refinement forest with independent trunk and branch activation",
            "families": self.families.iter().enumerate().map(|(id, family)| json!({
                "id": id,
                "name": family.name,
                "anchor": family.anchor.as_str(),
                "source_preference": family.source_preference.as_str(),
                "roles": family.roles.iter().map(|role| role.as_str()).collect::<Vec<_>>(),
                "parent": family.parent,
                "fallback_saliency": family.fallback_saliency,
                "parent_saliency_delta": family.parent_saliency_delta,
                "tone_band": family.anchor.tone_band().as_str(),
            })).collect::<Vec<_>>(),
            "inactive_roles": ORDINARY_ROLES.iter().copied().filter(|role| self.family_for(*role).is_none()).map(|role| json!({
                "role": role.as_str(),
                "fallback": self.fallback_for(role).as_str(),
            })).collect::<Vec<_>>(),
            "nested": true,
            "comments_consume_chromatic_budget": false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn plans_cover_each_active_role_once() {
        for budget in 0..=MAX_SEMANTIC_BUDGET {
            let plan = MergePlan::with_budget(budget);
            assert_eq!(plan.families.len(), budget);
            let roles = plan
                .families
                .iter()
                .flat_map(|family| family.roles.iter().copied())
                .collect::<Vec<_>>();
            assert_eq!(
                roles.iter().copied().collect::<BTreeSet<_>>().len(),
                roles.len()
            );
        }
    }

    #[test]
    fn increasing_budget_only_refines_semantic_groups() {
        for budget in 0..MAX_SEMANTIC_BUDGET {
            let current = MergePlan::with_budget(budget);
            let next = MergePlan::with_budget(budget + 1);
            for family in &next.families {
                let parents = family
                    .roles
                    .iter()
                    .map(|role| current.family_for(*role))
                    .collect::<BTreeSet<_>>();
                assert!(parents.len() == 1 || parents == BTreeSet::from([None]));
            }
        }
    }

    #[test]
    fn sparse_plans_leave_routine_syntax_at_base() {
        let plan = MergePlan::with_budget(2);
        for role in [SemanticRole::Control, SemanticRole::Link] {
            assert_eq!(plan.family_for(role), None);
            assert_eq!(plan.fallback_for(role), SemanticRole::Base);
        }
        assert_eq!(
            plan.fallback_for(SemanticRole::Metadata),
            SemanticRole::Subdued
        );
        assert_eq!(plan.family_for(SemanticRole::String), Some(0));
        assert_eq!(plan.family_for(SemanticRole::Value), Some(0));
        assert_eq!(plan.family_for(SemanticRole::Callable), Some(1));
        assert_eq!(plan.family_for(SemanticRole::Type), Some(1));
        assert_eq!(plan.family_for(SemanticRole::Member), Some(1));
    }

    #[test]
    fn data_split_preserves_the_first_family_for_strings() {
        let one = MergePlan::with_budget(1);
        let four = MergePlan::with_budget(4);
        assert_eq!(one.families[0].anchor, SemanticRole::String);
        assert_eq!(four.families[0].anchor, SemanticRole::String);
        assert!(one.families[0].roles.contains(&SemanticRole::Value));
        assert!(!four.families[0].roles.contains(&SemanticRole::Value));
        assert_eq!(four.families[3].roles, vec![SemanticRole::Value]);
    }

    #[test]
    fn symbol_split_preserves_the_second_family_for_types() {
        let two = MergePlan::with_budget(2);
        let five = MergePlan::with_budget(5);
        assert_eq!(two.families[1].anchor, SemanticRole::Type);
        assert_eq!(five.families[1].anchor, SemanticRole::Type);
        assert!(two.families[1].roles.contains(&SemanticRole::Callable));
        assert!(!five.families[1].roles.contains(&SemanticRole::Callable));
        assert_eq!(
            five.families[4].roles,
            vec![SemanticRole::Callable, SemanticRole::Member]
        );
    }

    #[test]
    fn one_failed_branch_does_not_remove_other_trunks_or_branches() {
        let hierarchy = MergePlan::hierarchy(3);
        let plan = hierarchy.activate(&BTreeSet::from([0, 1, 2, 4]));
        assert_eq!(plan.family_for(SemanticRole::Control), Some(2));
        assert_eq!(plan.family_for(SemanticRole::Value), Some(0));
        assert_ne!(
            plan.family_for(SemanticRole::Type),
            plan.family_for(SemanticRole::Callable)
        );
        assert_eq!(
            plan.family_for(SemanticRole::Callable),
            plan.family_for(SemanticRole::Member)
        );
    }
}
