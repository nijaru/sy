//! Three-way bidirectional reconciliation semantics for 0.5.
//!
//! Bisync is not a two-way mtime comparison. Every path is classified against
//! a durable common ancestor (the last successfully synchronized semantic
//! value). Endpoint identity can prove the common unchanged case cheaply; when
//! identity differs, the caller supplies a canonical strong value id (normally
//! BLAKE3 for regular-file content plus the policy-selected entry semantics).
//! Timestamps are deliberately absent from this module: they may trigger value
//! discovery, but they are never evidence that two values are equal or that one
//! side is authoritative.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ValueId([u8; 32]);

impl ValueId {
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaselineValue {
    Absent,
    Present(ValueId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SideObservation {
    Absent,
    Present {
        /// True only when the endpoint's durable identity contract proves this
        /// is the same observed value recorded after the prior successful sync.
        baseline_identity_matches: bool,
        /// Canonical semantic value, computed lazily when identity cannot prove
        /// the path unchanged. For regular files this includes a strong content
        /// digest; callers must not synthesize it from size/mtime.
        value: Option<ValueId>,
    },
}

impl SideObservation {
    pub const fn present_unchanged() -> Self {
        Self::Present {
            baseline_identity_matches: true,
            value: None,
        }
    }

    pub const fn present_candidate(value: Option<ValueId>) -> Self {
        Self::Present {
            baseline_identity_matches: false,
            value,
        }
    }

    const fn value(self) -> Option<ValueId> {
        match self {
            Self::Present { value, .. } => value,
            Self::Absent => None,
        }
    }

    const fn is_present(self) -> bool {
        matches!(self, Self::Present { .. })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NeedValues {
    pub left: bool,
    pub right: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictKind {
    CreateCreate,
    ModifyModify,
    ModifyDelete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BisyncDecision {
    NoChange,
    /// Make the right replica semantically equal to the current left value.
    /// A left `Absent` observation therefore means deletion on the right.
    ApplyLeftToRight,
    /// Make the left replica semantically equal to the current right value.
    /// A right `Absent` observation therefore means deletion on the left.
    ApplyRightToLeft,
    /// Both replicas independently reached the same semantic value. No content
    /// transfer is required; the baseline can advance after postcondition checks.
    Converged,
    /// More strong values are required before a safe decision is possible.
    NeedValues(NeedValues),
    /// Both sides changed to different semantic values. Higher layers preserve
    /// both by default unless the user explicitly selected a winner policy.
    Conflict(ConflictKind),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChangeState {
    Unchanged,
    Changed,
    NeedValue,
}

pub fn reconcile_path(
    baseline: BaselineValue,
    left: SideObservation,
    right: SideObservation,
) -> BisyncDecision {
    let left_state = change_state(baseline, left);
    let right_state = change_state(baseline, right);

    let mut need = NeedValues {
        left: left_state == ChangeState::NeedValue,
        right: right_state == ChangeState::NeedValue,
    };
    if need.left || need.right {
        return BisyncDecision::NeedValues(need);
    }

    match (left_state, right_state) {
        (ChangeState::Unchanged, ChangeState::Unchanged) => BisyncDecision::NoChange,
        (ChangeState::Changed, ChangeState::Unchanged) => BisyncDecision::ApplyLeftToRight,
        (ChangeState::Unchanged, ChangeState::Changed) => BisyncDecision::ApplyRightToLeft,
        (ChangeState::Changed, ChangeState::Changed) => {
            if !left.is_present() && !right.is_present() {
                return BisyncDecision::Converged;
            }
            if left.is_present() != right.is_present() {
                return BisyncDecision::Conflict(ConflictKind::ModifyDelete);
            }

            // Both sides contain independently changed values. Their canonical
            // values are needed to eliminate false conflicts.
            if left.value().is_none() {
                need.left = true;
            }
            if right.value().is_none() {
                need.right = true;
            }
            if need.left || need.right {
                return BisyncDecision::NeedValues(need);
            }

            if left.value() == right.value() {
                BisyncDecision::Converged
            } else {
                BisyncDecision::Conflict(match baseline {
                    BaselineValue::Absent => ConflictKind::CreateCreate,
                    BaselineValue::Present(_) => ConflictKind::ModifyModify,
                })
            }
        }
        (ChangeState::NeedValue, _) | (_, ChangeState::NeedValue) => {
            unreachable!("NeedValue is returned before final classification")
        }
    }
}

fn change_state(baseline: BaselineValue, current: SideObservation) -> ChangeState {
    match (baseline, current) {
        (BaselineValue::Absent, SideObservation::Absent) => ChangeState::Unchanged,
        (BaselineValue::Absent, SideObservation::Present { .. }) => ChangeState::Changed,
        (BaselineValue::Present(_), SideObservation::Absent) => ChangeState::Changed,
        (
            BaselineValue::Present(_),
            SideObservation::Present {
                baseline_identity_matches: true,
                ..
            },
        ) => ChangeState::Unchanged,
        (
            BaselineValue::Present(baseline),
            SideObservation::Present {
                baseline_identity_matches: false,
                value: Some(current),
            },
        ) => {
            if current == baseline {
                ChangeState::Unchanged
            } else {
                ChangeState::Changed
            }
        }
        (
            BaselineValue::Present(_),
            SideObservation::Present {
                baseline_identity_matches: false,
                value: None,
            },
        ) => ChangeState::NeedValue,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: ValueId = ValueId::from_bytes([1; 32]);
    const LEFT: ValueId = ValueId::from_bytes([2; 32]);
    const RIGHT: ValueId = ValueId::from_bytes([3; 32]);

    fn candidate(value: ValueId) -> SideObservation {
        SideObservation::present_candidate(Some(value))
    }

    #[test]
    fn stable_identities_make_unchanged_path_free() {
        assert_eq!(
            reconcile_path(
                BaselineValue::Present(BASE),
                SideObservation::present_unchanged(),
                SideObservation::present_unchanged(),
            ),
            BisyncDecision::NoChange
        );
    }

    #[test]
    fn identity_change_requires_value_instead_of_guessing_from_metadata() {
        assert_eq!(
            reconcile_path(
                BaselineValue::Present(BASE),
                SideObservation::present_candidate(None),
                SideObservation::present_unchanged(),
            ),
            BisyncDecision::NeedValues(NeedValues {
                left: true,
                right: false,
            })
        );
    }

    #[test]
    fn touch_only_candidate_collapses_to_unchanged_after_strong_value() {
        assert_eq!(
            reconcile_path(
                BaselineValue::Present(BASE),
                candidate(BASE),
                SideObservation::present_unchanged(),
            ),
            BisyncDecision::NoChange
        );
    }

    #[test]
    fn same_size_or_timestamp_cannot_hide_real_content_change() {
        // Size and mtime are intentionally not inputs to the classifier. Once
        // identity changes, a distinct strong semantic value is authoritative.
        assert_eq!(
            reconcile_path(
                BaselineValue::Present(BASE),
                candidate(LEFT),
                SideObservation::present_unchanged(),
            ),
            BisyncDecision::ApplyLeftToRight
        );
    }

    #[test]
    fn two_independent_edits_to_same_value_are_not_a_conflict() {
        assert_eq!(
            reconcile_path(
                BaselineValue::Present(BASE),
                candidate(LEFT),
                candidate(LEFT),
            ),
            BisyncDecision::Converged
        );
    }

    #[test]
    fn two_different_edits_conflict() {
        assert_eq!(
            reconcile_path(
                BaselineValue::Present(BASE),
                candidate(LEFT),
                candidate(RIGHT),
            ),
            BisyncDecision::Conflict(ConflictKind::ModifyModify)
        );
    }

    #[test]
    fn delete_propagates_only_when_other_side_is_unchanged() {
        assert_eq!(
            reconcile_path(
                BaselineValue::Present(BASE),
                SideObservation::Absent,
                SideObservation::present_unchanged(),
            ),
            BisyncDecision::ApplyLeftToRight
        );
    }

    #[test]
    fn modify_delete_is_a_conflict() {
        assert_eq!(
            reconcile_path(
                BaselineValue::Present(BASE),
                SideObservation::Absent,
                candidate(RIGHT),
            ),
            BisyncDecision::Conflict(ConflictKind::ModifyDelete)
        );
    }

    #[test]
    fn two_deletes_converge() {
        assert_eq!(
            reconcile_path(
                BaselineValue::Present(BASE),
                SideObservation::Absent,
                SideObservation::Absent,
            ),
            BisyncDecision::Converged
        );
    }

    #[test]
    fn one_sided_create_does_not_require_hash_before_propagation() {
        assert_eq!(
            reconcile_path(
                BaselineValue::Absent,
                SideObservation::present_candidate(None),
                SideObservation::Absent,
            ),
            BisyncDecision::ApplyLeftToRight
        );
    }

    #[test]
    fn equal_create_create_converges_after_values_are_known() {
        assert_eq!(
            reconcile_path(
                BaselineValue::Absent,
                candidate(LEFT),
                candidate(LEFT),
            ),
            BisyncDecision::Converged
        );
    }

    #[test]
    fn different_create_create_conflicts() {
        assert_eq!(
            reconcile_path(
                BaselineValue::Absent,
                candidate(LEFT),
                candidate(RIGHT),
            ),
            BisyncDecision::Conflict(ConflictKind::CreateCreate)
        );
    }

    #[test]
    fn create_create_requests_both_values_before_conflicting() {
        assert_eq!(
            reconcile_path(
                BaselineValue::Absent,
                SideObservation::present_candidate(None),
                SideObservation::present_candidate(None),
            ),
            BisyncDecision::NeedValues(NeedValues {
                left: true,
                right: true,
            })
        );
    }
}
