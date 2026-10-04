use crate::hid::ProfileId;

/// Own only automatic transitions. Repeated focus/telemetry updates must never
/// undo a physical Mode/Cycle key press. A failed HID transaction is not committed.
#[derive(Clone, Debug)]
pub struct LinkState {
    pub target: Option<ProfileId>,
    pub baseline: ProfileId,
}

pub struct Transition {
    pub selection: Option<Option<ProfileId>>,
    pub activate: Option<ProfileId>,
    pub next: LinkState,
}

impl LinkState {
    pub fn new(current: ProfileId, fallback: u8) -> Self {
        Self {
            target: None,
            baseline: if current.namespace == 0 {
                current
            } else {
                ProfileId {
                    namespace: 0,
                    index: fallback,
                }
            },
        }
    }

    pub fn plan(&self, current: ProfileId, desired: Option<ProfileId>) -> Option<Transition> {
        if self.target == desired {
            return None;
        }
        let mut next = self.clone();
        // An onboard profile different from our own target is a user's selection.
        if current.namespace == 0 && self.target != Some(current) {
            next.baseline = current;
        }
        next.target = desired;
        let selection = if desired.is_some_and(|id| id.namespace == 1) {
            Some(desired)
        } else if self.target.is_some_and(|id| id.namespace == 1) {
            Some(None)
        } else {
            None
        };
        let activate = match desired {
            Some(id) => (current != id).then_some(id),
            None if self.target == Some(current) || current.namespace == 1 => Some(next.baseline),
            None => None,
        };
        Some(Transition {
            selection,
            activate,
            next,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const P2: ProfileId = ProfileId {
        namespace: 0,
        index: 1,
    };
    const P3: ProfileId = ProfileId {
        namespace: 0,
        index: 2,
    };
    const GAME: ProfileId = ProfileId {
        namespace: 1,
        index: 0,
    };

    #[test]
    fn linked_profile_restores_actual_onboard_selection_not_p1() {
        let state = LinkState::new(P2, 0);
        let enter = state.plan(P2, Some(GAME)).unwrap();
        assert_eq!(enter.selection, Some(Some(GAME)));
        assert_eq!(enter.activate, Some(GAME));
        let leave = enter.next.plan(GAME, None).unwrap();
        assert_eq!(leave.selection, Some(None));
        assert_eq!(leave.activate, Some(P2));
    }

    #[test]
    fn physical_mode_cycle_override_survives_same_app_and_exit() {
        let active = LinkState::new(P2, 0).plan(P2, Some(GAME)).unwrap().next;
        assert!(active.plan(P3, Some(GAME)).is_none());
        let leave = active.plan(P3, None).unwrap();
        assert_eq!(leave.activate, None);
        assert_eq!(leave.next.baseline, P3);
        assert_eq!(leave.selection, Some(None));
    }

    #[test]
    fn failed_transition_does_not_lose_return_profile() {
        let state = LinkState::new(P2, 0);
        let _failed = state.plan(P2, Some(GAME)).unwrap();
        // Firmware activated, but readback failed. Retry from uncommitted state.
        let retry = state.plan(GAME, Some(GAME)).unwrap();
        assert_eq!(retry.next.baseline, P2);
        assert_eq!(retry.next.plan(GAME, None).unwrap().activate, Some(P2));
    }
}
