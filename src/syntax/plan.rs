//! Defines stable semantic syntax roles and their refinement hierarchy.

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

#[derive(Clone, Debug, PartialEq)]
pub struct Family {
    pub anchor: SemanticRole,
    pub source_preference: SemanticRole,
    pub roles: Vec<SemanticRole>,
    pub parent: Option<usize>,
    pub fallback_saliency: f64,
    pub parent_saliency_delta: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MergePlan {
    pub families: Vec<Family>,
}

impl MergePlan {
    pub fn hierarchy() -> Self {
        let families = vec![
            Family {
                anchor: SemanticRole::String,
                source_preference: SemanticRole::String,
                roles: vec![SemanticRole::String, SemanticRole::Link],
                parent: None,
                fallback_saliency: 0.76,
                parent_saliency_delta: 0.0,
            },
            Family {
                anchor: SemanticRole::Type,
                source_preference: SemanticRole::Callable,
                roles: vec![SemanticRole::Type],
                parent: None,
                fallback_saliency: 0.92,
                parent_saliency_delta: 0.0,
            },
            Family {
                anchor: SemanticRole::Control,
                source_preference: SemanticRole::Control,
                roles: vec![SemanticRole::Control],
                parent: None,
                fallback_saliency: 0.84,
                parent_saliency_delta: 0.0,
            },
            Family {
                anchor: SemanticRole::Value,
                source_preference: SemanticRole::Value,
                roles: vec![SemanticRole::Value],
                parent: Some(0),
                fallback_saliency: 0.88,
                parent_saliency_delta: 0.12,
            },
            Family {
                anchor: SemanticRole::Callable,
                source_preference: SemanticRole::Callable,
                roles: vec![SemanticRole::Callable],
                parent: Some(1),
                fallback_saliency: 0.80,
                parent_saliency_delta: -0.12,
            },
            Family {
                anchor: SemanticRole::Member,
                source_preference: SemanticRole::Member,
                roles: vec![SemanticRole::Member],
                parent: Some(1),
                fallback_saliency: 0.70,
                parent_saliency_delta: -0.22,
            },
        ];
        Self { families }
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
                        family.roles.push(SemanticRole::Value);
                    }
                    SemanticRole::Type => {
                        if !callable_active {
                            family.roles.push(SemanticRole::Callable);
                        }
                        if !member_active {
                            family.roles.push(SemanticRole::Member);
                        }
                    }
                    _ => {}
                }
                family
            })
            .collect::<Vec<_>>();
        Self { families }
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn fixed_hierarchy_covers_every_ordinary_role_once() {
        let plan = MergePlan::hierarchy();
        let roles = plan
            .families
            .iter()
            .flat_map(|family| family.roles.iter().copied())
            .collect::<Vec<_>>();
        assert_eq!(
            roles.iter().copied().collect::<BTreeSet<_>>().len(),
            roles.len()
        );
        let expected = ORDINARY_ROLES
            .into_iter()
            .filter(|role| *role != SemanticRole::Metadata)
            .collect();
        assert_eq!(roles.iter().copied().collect::<BTreeSet<_>>(), expected);
    }

    #[test]
    fn inactive_branches_merge_into_their_semantic_roots() {
        let plan = MergePlan::hierarchy().activate(&BTreeSet::from([0, 1, 2]));
        assert_eq!(plan.family_for(SemanticRole::Control), Some(2));
        assert_eq!(plan.family_for(SemanticRole::Link), Some(0));
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
    fn one_failed_branch_does_not_remove_other_trunks_or_branches() {
        let hierarchy = MergePlan::hierarchy();
        let plan = hierarchy.activate(&BTreeSet::from([0, 1, 2, 4]));
        assert_eq!(plan.family_for(SemanticRole::Control), Some(2));
        assert_eq!(plan.family_for(SemanticRole::Value), Some(0));
        assert_ne!(
            plan.family_for(SemanticRole::Type),
            plan.family_for(SemanticRole::Callable)
        );
        assert_eq!(
            plan.family_for(SemanticRole::Type),
            plan.family_for(SemanticRole::Member)
        );
    }
}
