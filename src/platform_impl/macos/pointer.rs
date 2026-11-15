//! Original AppKit press identities, independent of later global button samples.

use std::num::NonZeroU64;
use std::sync::Arc;

use crate::event::{PointerPressConnectionDomain, PointerPressToken};

/// Delivers a replacement press before its position can drive the previous gesture.
///
/// The real down replaces the old identity without clearing other held buttons
/// or claiming that native capture was released.
pub(super) fn dispatch_primary_down(
    pressed_buttons: usize,
    synchronize_position: impl FnOnce(),
    press: impl FnOnce(),
) {
    if pressed_buttons & 1 != 0 {
        press();
        synchronize_position();
    } else {
        synchronize_position();
        press();
    }
}

#[derive(Debug)]
pub(super) struct PrimaryPressState {
    domain: Arc<PointerPressConnectionDomain>,
    next: Option<NonZeroU64>,
    active: Option<PointerPressToken>,
}

impl Default for PrimaryPressState {
    fn default() -> Self {
        Self {
            domain: Arc::new(PointerPressConnectionDomain),
            next: NonZeroU64::new(1),
            active: None,
        }
    }
}

impl PrimaryPressState {
    pub(super) fn press(&mut self) -> Option<PointerPressToken> {
        self.active = None;
        let sequence = self.next?;
        self.next = sequence.get().checked_add(1).and_then(NonZeroU64::new);
        let token = PointerPressToken::from_macos(Arc::clone(&self.domain), sequence);
        self.active = Some(token.clone());
        Some(token)
    }

    pub(super) fn release(&mut self) -> Option<PointerPressToken> {
        self.active.take()
    }

    pub(super) fn matches(&self, expected: &PointerPressToken) -> bool {
        self.active.as_ref() == Some(expected)
    }

    pub(super) fn cancel_released(&mut self, expected: &PointerPressToken, buttons: usize) -> bool {
        if buttons != 0 || !self.matches(expected) {
            return false;
        }
        self.active = None;
        true
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};

    use super::*;

    #[test]
    fn first_primary_down_synchronizes_position_before_press_with_other_buttons_held() {
        for held in [0, 2, 4, 2 | 4] {
            let buttons = Cell::new(held);
            let events = RefCell::new(Vec::new());
            dispatch_primary_down(
                held,
                || events.borrow_mut().push(("position", buttons.get())),
                || {
                    buttons.set(buttons.get() | 1);
                    events.borrow_mut().push(("press", buttons.get()));
                },
            );
            assert_eq!(events.into_inner(), [("position", held), ("press", held | 1)]);
            assert_eq!(buttons.get(), held | 1);
        }
    }

    #[test]
    fn replacement_primary_down_installs_its_identity_before_position_without_clearing_buttons() {
        for held in [1, 1 | 2, 1 | 4, 1 | 2 | 4] {
            let state = RefCell::new(PrimaryPressState::default());
            let old = state.borrow_mut().press().unwrap();
            let buttons = Cell::new(held);
            let events = RefCell::new(Vec::new());
            dispatch_primary_down(
                held,
                || {
                    let state = state.borrow();
                    assert!(!state.matches(&old), "motion must not retain the previous press");
                    events.borrow_mut().push(("position", state.active.clone(), buttons.get()));
                },
                || {
                    let current = state.borrow_mut().press().unwrap();
                    buttons.set(buttons.get() | 1);
                    events.borrow_mut().push(("press", Some(current), buttons.get()));
                },
            );
            let current = state.borrow().active.clone().unwrap();
            assert_ne!(old, current);
            assert_eq!(
                events.into_inner(),
                [("press", Some(current.clone()), held), ("position", Some(current), held)]
            );
            assert_eq!(buttons.get(), held, "replacement must preserve every other held button");
        }
    }

    #[test]
    fn replacement_primary_down_orders_press_first_even_without_a_native_identity() {
        let state = RefCell::new(PrimaryPressState { next: None, ..Default::default() });
        let events = RefCell::new(Vec::new());
        dispatch_primary_down(
            1,
            || events.borrow_mut().push("position"),
            || {
                assert_eq!(state.borrow_mut().press(), None);
                events.borrow_mut().push("press");
            },
        );
        assert_eq!(events.into_inner(), ["press", "position"]);
    }

    #[test]
    fn release_and_repress_never_revive_an_original_identity() {
        let mut state = PrimaryPressState::default();
        let old = state.press().unwrap();
        assert!(state.matches(&old));
        assert_eq!(state.release(), Some(old.clone()));
        let current = state.press().unwrap();
        assert!(!state.matches(&old));
        assert!(state.matches(&current));
        assert_ne!(old, current);
    }

    #[test]
    fn a_new_down_replaces_a_press_with_a_missing_release() {
        let mut state = PrimaryPressState::default();
        let old = state.press().unwrap();
        let current = state.press().unwrap();
        assert!(!state.matches(&old));
        assert!(state.matches(&current));
    }

    #[test]
    fn window_domains_and_reset_invalidate_retained_tokens() {
        let mut first = PrimaryPressState::default();
        let mut second = PrimaryPressState::default();
        let old = first.press().unwrap();
        assert_ne!(old, second.press().unwrap());
        assert!(!second.matches(&old));
        first.release();
        assert!(!first.matches(&old));
        assert_eq!(first.release(), None);
        assert_ne!(old, first.press().unwrap());
    }

    #[test]
    fn exhausted_sequences_do_not_wrap_or_retain_the_last_press() {
        let mut state = PrimaryPressState { next: NonZeroU64::new(u64::MAX), ..Default::default() };
        let last = state.press().unwrap();
        assert!(state.matches(&last));
        assert_eq!(state.press(), None);
        assert!(!state.matches(&last));
    }

    #[test]
    fn negative_sample_preserves_other_buttons_and_cancels_only_the_matching_old_press() {
        let mut state = PrimaryPressState::default();
        let old = state.press().unwrap();
        assert!(!state.cancel_released(&old, 2));
        assert!(state.matches(&old));
        assert!(state.cancel_released(&old, 0));
        assert!(!state.cancel_released(&old, 0));
        let current = state.press().unwrap();
        assert!(!state.cancel_released(&old, 0));
        assert!(state.matches(&current));
    }
}
