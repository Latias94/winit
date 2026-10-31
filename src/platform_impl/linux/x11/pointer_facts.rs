//! Pure XI2 pointer-route state. Keeping Xlib/XCB calls outside this module lets the same state
//! transitions run as ordinary unit tests on non-X11 development hosts.

use std::collections::{HashMap, HashSet};
use std::os::raw::c_int;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

/// Bound hierarchy probing so a malformed or concurrently changing X11 window tree fails closed.
const MAX_POINTER_HIERARCHY_DEPTH: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum NativePointerRoute {
    Unknown,
    None,
    Window(u32),
    Foreign,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImplicitCaptureOwner {
    Window(u32),
    Unknown,
}

impl ImplicitCaptureOwner {
    fn route(self) -> NativePointerRoute {
        match self {
            Self::Window(window) => NativePointerRoute::Window(window),
            Self::Unknown => NativePointerRoute::Unknown,
        }
    }
}

#[derive(Debug, Default)]
pub(super) struct ImplicitPointerCaptures {
    owners: HashMap<u16, ImplicitCaptureOwner>,
    fence: CaptureMutationFence,
}

/// Bracket request dispatch without holding either a Display lock or the capture mutex.
pub(super) struct PointerCaptureMutationGuard<'a> {
    captures: &'a Mutex<ImplicitPointerCaptures>,
    native_event_depth: &'a AtomicU32,
}

impl<'a> PointerCaptureMutationGuard<'a> {
    pub(super) fn new(
        captures: &'a Mutex<ImplicitPointerCaptures>,
        native_event_depth: &'a AtomicU32,
        window: Option<u32>,
    ) -> Self {
        let mut state = captures.lock().unwrap_or_else(|error| error.into_inner());
        native_event_depth.fetch_add(1, Ordering::Relaxed);
        state.invalidate(window);
        Self { captures, native_event_depth }
    }
}

impl Drop for PointerCaptureMutationGuard<'_> {
    fn drop(&mut self) {
        let mut state = self.captures.lock().unwrap_or_else(|error| error.into_inner());
        // A sync started before dispatch may already have returned. Require a new round trip
        // after dispatch, including when dispatch unwinds, before this depth can reach zero.
        state.close_fence();
        self.native_event_depth.fetch_sub(1, Ordering::Relaxed);
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
enum CaptureMutationFence {
    #[default]
    Open,
    NeedsSync,
    Syncing,
    Draining,
    Failed,
}

impl ImplicitPointerCaptures {
    // Keep invalidated devices until an observed all-up state. A later first-press mask alone
    // must not resurrect the owner of a stream whose continuity was lost.
    pub(super) fn invalidate(&mut self, window: Option<u32>) {
        for owner in self.owners.values_mut() {
            if window.is_none()
                || window.is_some_and(|window| *owner == ImplicitCaptureOwner::Window(window))
            {
                *owner = ImplicitCaptureOwner::Unknown;
            }
        }
        self.close_fence();
    }

    fn close_fence(&mut self) {
        if self.fence != CaptureMutationFence::Failed {
            self.fence = CaptureMutationFence::NeedsSync;
        }
    }

    // The caller samples the shared depth while holding the same mutex as mutation begin/Drop.
    pub(super) fn begin_sync(&mut self, native_event_depth: u32) -> bool {
        if native_event_depth != 0 || self.fence != CaptureMutationFence::NeedsSync {
            return false;
        }
        self.fence = CaptureMutationFence::Syncing;
        true
    }

    pub(super) fn finish_sync(&mut self, succeeded: bool) {
        if !succeeded {
            for owner in self.owners.values_mut() {
                *owner = ImplicitCaptureOwner::Unknown;
            }
            self.fence = CaptureMutationFence::Failed;
        } else if self.fence == CaptureMutationFence::Syncing {
            // A mutation during XSync changes Syncing back to NeedsSync. Its request must be
            // covered by another round trip, not by this older completion.
            self.fence = CaptureMutationFence::Draining;
        }
    }

    pub(super) fn queue_drained(&mut self, native_event_depth: u32) {
        if native_event_depth == 0 && self.fence == CaptureMutationFence::Draining {
            self.fence = CaptureMutationFence::Open;
        }
    }

    pub(super) fn after_button_event(
        &mut self,
        device_id: u16,
        delivery_window: u32,
        delivery_is_winit: bool,
        pressed: bool,
        transition: Option<ButtonStateTransition>,
    ) -> NativePointerRoute {
        let Some(transition) = transition else {
            self.owners.insert(device_id, ImplicitCaptureOwner::Unknown);
            return NativePointerRoute::Unknown;
        };

        if !transition.any_after {
            self.owners.remove(&device_id);
            return NativePointerRoute::None;
        }

        if pressed && !transition.any_before {
            let owner = if delivery_is_winit
                && self.fence == CaptureMutationFence::Open
                && !self.owners.contains_key(&device_id)
            {
                ImplicitCaptureOwner::Window(delivery_window)
            } else {
                ImplicitCaptureOwner::Unknown
            };
            self.owners.insert(device_id, owner);
            return owner.route();
        }

        self.owners.entry(device_id).or_insert(ImplicitCaptureOwner::Unknown).route()
    }

    pub(super) fn for_button_state(
        &mut self,
        device_id: u16,
        any_pressed: Option<bool>,
    ) -> NativePointerRoute {
        match any_pressed {
            Some(false) => {
                self.owners.remove(&device_id);
                NativePointerRoute::None
            },
            Some(true) => {
                self.owners.entry(device_id).or_insert(ImplicitCaptureOwner::Unknown).route()
            },
            None => {
                self.owners.insert(device_id, ImplicitCaptureOwner::Unknown);
                NativePointerRoute::Unknown
            },
        }
    }

    pub(super) fn reset_device(&mut self, device_id: u16) -> Option<u32> {
        let owner = self.owners.get_mut(&device_id)?;
        let window = match *owner {
            ImplicitCaptureOwner::Window(window) => window,
            ImplicitCaptureOwner::Unknown => return None,
        };
        *owner = ImplicitCaptureOwner::Unknown;
        self.close_fence();
        Some(window)
    }

    pub(super) fn remove_device(&mut self, device_id: u16) {
        self.owners.remove(&device_id);
    }

    pub(super) fn reset_window(&mut self, window: u32) -> Vec<u16> {
        let devices = self
            .owners
            .iter()
            .filter_map(|(device_id, owner)| {
                (*owner == ImplicitCaptureOwner::Window(window)).then_some(*device_id)
            })
            .collect::<Vec<_>>();
        for device_id in &devices {
            self.owners.insert(*device_id, ImplicitCaptureOwner::Unknown);
        }
        self.close_fence();
        devices
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ButtonStateTransition {
    pub(super) any_before: bool,
    pub(super) any_after: bool,
}

pub(super) fn button_state_transition(
    mask: &[u8],
    detail: c_int,
    pressed: bool,
) -> Option<ButtonStateTransition> {
    let detail = usize::try_from(detail).ok().filter(|detail| *detail > 0)?;
    let button_count = mask.len().checked_mul(8)?;
    if detail >= button_count {
        return None;
    }
    let mut any_before = false;
    let mut any_after = false;

    for button in 0..button_count {
        let byte = mask[button / 8];
        let pressed_before = byte & (1_u8 << (button % 8)) != 0;
        let pressed_after = if button == detail { pressed } else { pressed_before };
        any_before |= pressed_before;
        any_after |= pressed_after;
    }

    Some(ButtonStateTransition { any_before, any_after })
}

pub(super) fn classify_pointer_hierarchy(
    first_child: u32,
    mut is_winit_window: impl FnMut(u32) -> bool,
    mut child_at_pointer: impl FnMut(u32) -> Result<u32, ()>,
) -> NativePointerRoute {
    if first_child == 0 {
        return NativePointerRoute::None;
    }

    let mut visited = HashSet::new();
    let mut window = first_child;
    for _ in 0..MAX_POINTER_HIERARCHY_DEPTH {
        if !visited.insert(window) {
            return NativePointerRoute::Unknown;
        }
        if is_winit_window(window) {
            return NativePointerRoute::Window(window);
        }

        match child_at_pointer(window) {
            Ok(child) if child != 0 => window = child,
            Ok(_) => return NativePointerRoute::Foreign,
            Err(()) => return NativePointerRoute::Unknown,
        }
    }

    NativePointerRoute::Unknown
}

pub(super) fn fp1616_matches_event(value: i32, event_value: f64) -> bool {
    event_value.is_finite() && f64::from(value) == event_value * 65_536.0
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEVICE: u16 = 2;
    const OWNER: u32 = 100;
    const OTHER_WINDOW: u32 = 200;

    fn transition(any_before: bool, any_after: bool) -> Option<ButtonStateTransition> {
        Some(ButtonStateTransition { any_before, any_after })
    }

    #[test]
    fn button_transition_uses_post_event_state() {
        assert_eq!(button_state_transition(&[0], 1, true), transition(false, true));
        assert_eq!(button_state_transition(&[0b0000_0010], 1, false), transition(true, false));
        assert_eq!(button_state_transition(&[0b0000_1010], 1, false), transition(true, true));
        assert_eq!(button_state_transition(&[], 1, true), None);
        assert_eq!(button_state_transition(&[0], 0, true), None);
    }

    #[test]
    fn implicit_capture_stays_with_first_delivery_until_last_release() {
        let mut captures = ImplicitPointerCaptures::default();

        assert_eq!(
            captures.after_button_event(DEVICE, OWNER, true, true, transition(false, true)),
            NativePointerRoute::Window(OWNER)
        );
        assert_eq!(
            captures.for_button_state(DEVICE, Some(true)),
            NativePointerRoute::Window(OWNER)
        );
        assert_eq!(
            captures.after_button_event(DEVICE, OWNER, true, true, transition(true, true)),
            NativePointerRoute::Window(OWNER)
        );
        assert_eq!(
            captures.after_button_event(DEVICE, OWNER, true, false, transition(true, true)),
            NativePointerRoute::Window(OWNER)
        );
        assert_eq!(
            captures.after_button_event(DEVICE, OWNER, true, false, transition(true, false)),
            NativePointerRoute::None
        );
    }

    #[test]
    fn capture_without_observed_press_fails_closed_until_buttons_are_up() {
        let mut captures = ImplicitPointerCaptures::default();

        assert_eq!(captures.for_button_state(DEVICE, Some(true)), NativePointerRoute::Unknown);
        assert_eq!(captures.for_button_state(DEVICE, None), NativePointerRoute::Unknown);
        assert_eq!(captures.for_button_state(DEVICE, Some(false)), NativePointerRoute::None);
    }

    #[test]
    fn reentrant_mutation_cannot_remint_the_current_press() {
        let mut captures = ImplicitPointerCaptures::default();
        assert_eq!(
            captures.after_button_event(DEVICE, OWNER, true, true, transition(false, true)),
            NativePointerRoute::Window(OWNER)
        );

        // The native button edge is recorded before ModifiersChanged. Its callback ungrabs;
        // the subsequent MouseInput must read Unknown instead of recording the edge again.
        captures.invalidate(None);
        assert_eq!(captures.for_button_state(DEVICE, Some(true)), NativePointerRoute::Unknown);
        assert!(captures.begin_sync(0));
        captures.finish_sync(true);
        captures.queue_drained(0);
        assert_eq!(
            captures.after_button_event(DEVICE, OWNER, true, true, transition(false, true)),
            NativePointerRoute::Unknown
        );
        assert_eq!(captures.for_button_state(DEVICE, Some(false)), NativePointerRoute::None);
        assert_eq!(
            captures.after_button_event(DEVICE, OWNER, true, true, transition(false, true)),
            NativePointerRoute::Window(OWNER)
        );
    }

    #[test]
    fn queued_presses_cannot_bind_before_sync_and_normal_dispatch_drain() {
        let mut captures = ImplicitPointerCaptures::default();
        captures.invalidate(None);
        assert_eq!(
            captures.after_button_event(DEVICE, OWNER, true, true, transition(false, true)),
            NativePointerRoute::Unknown
        );
        assert_eq!(
            captures.after_button_event(DEVICE, OWNER, true, false, transition(true, false)),
            NativePointerRoute::None
        );
        assert!(captures.begin_sync(0));
        assert_eq!(
            captures.after_button_event(DEVICE, OWNER, true, true, transition(false, true)),
            NativePointerRoute::Unknown
        );
        assert_eq!(captures.for_button_state(DEVICE, Some(false)), NativePointerRoute::None);
        captures.finish_sync(true);

        // XSync has queued older events, including an all-up followed by another first press.
        assert_eq!(
            captures.after_button_event(DEVICE, OWNER, true, true, transition(false, true)),
            NativePointerRoute::Unknown
        );
        captures.queue_drained(0);
        assert_eq!(captures.for_button_state(DEVICE, Some(true)), NativePointerRoute::Unknown);
        assert_eq!(captures.for_button_state(DEVICE, Some(false)), NativePointerRoute::None);
        assert_eq!(
            captures.after_button_event(DEVICE, OWNER, true, true, transition(false, true)),
            NativePointerRoute::Window(OWNER)
        );
    }

    #[test]
    fn invalidated_multibutton_stream_needs_the_last_release() {
        let mut captures = ImplicitPointerCaptures::default();
        let other_device = DEVICE + 1;
        captures.after_button_event(
            other_device,
            OTHER_WINDOW,
            true,
            true,
            transition(false, true),
        );
        captures.after_button_event(DEVICE, OWNER, true, true, transition(false, true));
        captures.after_button_event(DEVICE, OWNER, true, true, transition(true, true));
        captures.invalidate(Some(OWNER));
        assert_eq!(
            captures.for_button_state(other_device, Some(true)),
            NativePointerRoute::Window(OTHER_WINDOW)
        );
        assert!(captures.begin_sync(0));
        captures.finish_sync(true);
        captures.queue_drained(0);

        assert_eq!(
            captures.after_button_event(DEVICE, OWNER, true, false, transition(true, true)),
            NativePointerRoute::Unknown
        );
        assert_eq!(
            captures.after_button_event(DEVICE, OWNER, true, true, transition(true, true)),
            NativePointerRoute::Unknown
        );
        assert_eq!(
            captures.after_button_event(DEVICE, OWNER, true, false, transition(true, false)),
            NativePointerRoute::None
        );
        assert_eq!(
            captures.after_button_event(DEVICE, OWNER, true, true, transition(false, true)),
            NativePointerRoute::Window(OWNER)
        );
    }

    #[test]
    fn source_unmap_and_remap_cannot_restore_a_held_capture() {
        let mut captures = ImplicitPointerCaptures::default();
        captures.after_button_event(DEVICE, OWNER, true, true, transition(false, true));
        assert_eq!(captures.reset_window(OWNER), vec![DEVICE]);
        assert!(captures.begin_sync(0));
        captures.finish_sync(true);
        captures.queue_drained(0);

        // MapNotify deliberately has no capture transition. Neither a down mask nor a repeated
        // first-press mask on the remapped source restores the old identity.
        assert_eq!(captures.for_button_state(DEVICE, Some(true)), NativePointerRoute::Unknown);
        assert_eq!(
            captures.after_button_event(DEVICE, OWNER, true, true, transition(false, true)),
            NativePointerRoute::Unknown
        );
    }

    #[test]
    fn mutation_during_sync_requires_another_round_trip() {
        let mut captures = ImplicitPointerCaptures::default();
        captures.invalidate(None);
        assert!(captures.begin_sync(0));
        captures.invalidate(Some(OWNER));
        captures.finish_sync(true);
        captures.queue_drained(0);
        assert_eq!(
            captures.after_button_event(DEVICE, OWNER, true, true, transition(false, true)),
            NativePointerRoute::Unknown
        );
        assert!(captures.begin_sync(0));
        captures.finish_sync(true);
        captures.queue_drained(0);
        assert_eq!(captures.for_button_state(DEVICE, Some(false)), NativePointerRoute::None);
        assert_eq!(
            captures.after_button_event(DEVICE, OWNER, true, true, transition(false, true)),
            NativePointerRoute::Window(OWNER)
        );
    }

    #[test]
    fn mutation_guard_blocks_sync_until_request_dispatch_returns() {
        let captures = Mutex::new(ImplicitPointerCaptures::default());
        let depth = AtomicU32::new(0);
        let mutation = PointerCaptureMutationGuard::new(&captures, &depth, None);
        assert_eq!(depth.load(Ordering::Relaxed), 1);

        // Dispatch holds neither mutex. An event-loop poll can acquire the owner but cannot
        // start a fence round trip while the request has not finished dispatching.
        {
            let mut state = captures.try_lock().expect("mutation must release the capture mutex");
            assert!(!state.begin_sync(depth.load(Ordering::Relaxed)));
            state.queue_drained(depth.load(Ordering::Relaxed));
            assert_eq!(state.fence, CaptureMutationFence::NeedsSync);
            assert_eq!(
                state.after_button_event(DEVICE, OWNER, true, true, transition(false, true)),
                NativePointerRoute::Unknown
            );
        }

        drop(mutation);
        assert_eq!(depth.load(Ordering::Relaxed), 0);
        let mut state = captures.lock().unwrap();
        assert!(state.begin_sync(depth.load(Ordering::Relaxed)));
        state.finish_sync(true);
        state.queue_drained(depth.load(Ordering::Relaxed));
        assert_eq!(state.for_button_state(DEVICE, Some(true)), NativePointerRoute::Unknown);
    }

    #[test]
    fn mutation_overlapping_sync_rejects_completion_on_either_side_of_drop() {
        for complete_before_drop in [true, false] {
            let captures = Mutex::new(ImplicitPointerCaptures::default());
            let depth = AtomicU32::new(0);
            {
                let mut state = captures.lock().unwrap();
                state.invalidate(None);
                assert!(state.begin_sync(depth.load(Ordering::Relaxed)));
            }

            // The event loop released the mutex to run XSync. Another thread begins sending
            // a mutation; either XSync or the mutation's dispatch can return first.
            let mutation = PointerCaptureMutationGuard::new(&captures, &depth, Some(OWNER));
            assert!(!captures.lock().unwrap().begin_sync(depth.load(Ordering::Relaxed)));
            if complete_before_drop {
                captures.lock().unwrap().finish_sync(true);
                drop(mutation);
            } else {
                drop(mutation);
                captures.lock().unwrap().finish_sync(true);
            }

            let mut state = captures.lock().unwrap();
            state.queue_drained(depth.load(Ordering::Relaxed));
            assert_eq!(state.fence, CaptureMutationFence::NeedsSync);
            assert_eq!(
                state.after_button_event(DEVICE, OWNER, true, true, transition(false, true)),
                NativePointerRoute::Unknown
            );
            assert!(state.begin_sync(depth.load(Ordering::Relaxed)));
            state.finish_sync(true);
            state.queue_drained(depth.load(Ordering::Relaxed));
            assert_eq!(state.for_button_state(DEVICE, Some(false)), NativePointerRoute::None);
            assert_eq!(
                state.after_button_event(DEVICE, OWNER, true, true, transition(false, true)),
                NativePointerRoute::Window(OWNER)
            );
        }
    }

    #[test]
    fn stale_empty_queue_cannot_reopen_across_nested_mutation_or_native_callback() {
        let captures = Mutex::new(ImplicitPointerCaptures::default());
        let depth = AtomicU32::new(0);
        {
            let mut state = captures.lock().unwrap();
            state.invalidate(None);
            assert!(state.begin_sync(depth.load(Ordering::Relaxed)));
            state.finish_sync(true);
        }

        // An empty queue has been sampled without the owner mutex. A native callback and
        // nested mutations intervene before the final locked depth/fence check.
        depth.fetch_add(1, Ordering::Relaxed);
        {
            let mut state = captures.lock().unwrap();
            state.queue_drained(depth.load(Ordering::Relaxed));
            assert_eq!(state.fence, CaptureMutationFence::Draining);
        }
        let outer = PointerCaptureMutationGuard::new(&captures, &depth, Some(OWNER));
        let inner = PointerCaptureMutationGuard::new(&captures, &depth, None);
        assert_eq!(depth.load(Ordering::Relaxed), 3);
        drop(outer);
        assert_eq!(depth.load(Ordering::Relaxed), 2);
        drop(inner);
        assert_eq!(depth.load(Ordering::Relaxed), 1);
        {
            let mut state = captures.lock().unwrap();
            state.queue_drained(depth.load(Ordering::Relaxed));
            assert_eq!(state.fence, CaptureMutationFence::NeedsSync);
            assert!(!state.begin_sync(depth.load(Ordering::Relaxed)));
        }
        depth.fetch_sub(1, Ordering::Relaxed);
        let mut state = captures.lock().unwrap();
        state.queue_drained(depth.load(Ordering::Relaxed));
        assert_eq!(state.fence, CaptureMutationFence::NeedsSync);
        assert!(state.begin_sync(depth.load(Ordering::Relaxed)));
        state.finish_sync(true);
        state.queue_drained(depth.load(Ordering::Relaxed));
        assert_eq!(state.fence, CaptureMutationFence::Open);
    }

    #[test]
    fn mutation_dispatch_unwind_closes_fence_before_releasing_depth() {
        let captures = Mutex::new(ImplicitPointerCaptures::default());
        let depth = AtomicU32::new(0);
        let result = std::panic::catch_unwind(|| {
            let _mutation = PointerCaptureMutationGuard::new(&captures, &depth, None);
            panic!("request dispatch unwound");
        });
        assert!(result.is_err());
        assert_eq!(depth.load(Ordering::Relaxed), 0);
        assert!(!captures.is_poisoned());
        let mut state = captures.lock().unwrap();
        state.queue_drained(depth.load(Ordering::Relaxed));
        assert_eq!(state.fence, CaptureMutationFence::NeedsSync);
        assert!(state.begin_sync(depth.load(Ordering::Relaxed)));
        state.finish_sync(false);
        drop(state);

        // A later guard, including its exit path, must not replace a failed fence with NeedsSync.
        drop(PointerCaptureMutationGuard::new(&captures, &depth, None));
        let mut state = captures.lock().unwrap();
        state.queue_drained(depth.load(Ordering::Relaxed));
        assert_eq!(state.fence, CaptureMutationFence::Failed);
        assert!(!state.begin_sync(depth.load(Ordering::Relaxed)));
    }

    #[test]
    fn failed_fence_stays_closed_after_drain_all_up_and_later_mutations() {
        let mut captures = ImplicitPointerCaptures::default();
        let other_device = DEVICE + 1;
        captures.after_button_event(
            other_device,
            OTHER_WINDOW,
            true,
            true,
            transition(false, true),
        );
        captures.invalidate(Some(OWNER));
        assert!(captures.begin_sync(0));
        captures.finish_sync(false);
        assert_eq!(
            captures.for_button_state(other_device, Some(true)),
            NativePointerRoute::Unknown
        );
        captures.queue_drained(0);
        captures.invalidate(None);
        assert!(!captures.begin_sync(0));
        captures.finish_sync(true);
        captures.queue_drained(0);
        assert_eq!(captures.for_button_state(DEVICE, Some(false)), NativePointerRoute::None);
        assert_eq!(
            captures.after_button_event(DEVICE, OWNER, true, true, transition(false, true)),
            NativePointerRoute::Unknown
        );
    }

    #[test]
    fn missing_button_mask_revokes_identity_until_observed_all_up() {
        let mut captures = ImplicitPointerCaptures::default();
        captures.after_button_event(DEVICE, OWNER, true, true, transition(false, true));
        assert_eq!(captures.for_button_state(DEVICE, None), NativePointerRoute::Unknown);
        assert_eq!(captures.for_button_state(DEVICE, Some(true)), NativePointerRoute::Unknown);
        assert_eq!(
            captures.after_button_event(DEVICE, OWNER, true, true, transition(false, true)),
            NativePointerRoute::Unknown
        );
        assert_eq!(captures.for_button_state(DEVICE, Some(false)), NativePointerRoute::None);
        assert_eq!(
            captures.after_button_event(DEVICE, OWNER, true, true, transition(false, true)),
            NativePointerRoute::Window(OWNER)
        );
    }

    #[test]
    fn resetting_window_capture_is_scoped_and_idempotent() {
        let mut captures = ImplicitPointerCaptures::default();
        let other_device = DEVICE + 1;
        captures.owners.insert(DEVICE, ImplicitCaptureOwner::Window(OWNER));
        captures.owners.insert(other_device, ImplicitCaptureOwner::Window(OTHER_WINDOW));

        assert_eq!(captures.reset_window(OWNER), vec![DEVICE]);
        assert!(captures.reset_window(OWNER).is_empty());
        assert_eq!(captures.for_button_state(DEVICE, Some(true)), NativePointerRoute::Unknown);
        assert_eq!(
            captures.for_button_state(other_device, Some(true)),
            NativePointerRoute::Window(OTHER_WINDOW)
        );
        assert_eq!(captures.reset_device(other_device), Some(OTHER_WINDOW));
        assert_eq!(captures.reset_device(other_device), None);
        assert_eq!(
            captures.for_button_state(other_device, Some(true)),
            NativePointerRoute::Unknown
        );
        captures.remove_device(other_device);
        assert!(!captures.owners.contains_key(&other_device));
    }

    #[test]
    fn hierarchy_distinguishes_desktop_foreign_and_winit_windows() {
        assert_eq!(
            classify_pointer_hierarchy(0, |_| false, |_| unreachable!()),
            NativePointerRoute::None
        );
        assert_eq!(
            classify_pointer_hierarchy(OWNER, |window| window == OWNER, |_| unreachable!()),
            NativePointerRoute::Window(OWNER)
        );
        assert_eq!(
            classify_pointer_hierarchy(
                50,
                |window| window == OWNER,
                |window| Ok(if window == 50 { OWNER } else { 0 }),
            ),
            NativePointerRoute::Window(OWNER)
        );
        assert_eq!(
            classify_pointer_hierarchy(50, |_| false, |_| Ok(0)),
            NativePointerRoute::Foreign
        );
        assert_eq!(
            classify_pointer_hierarchy(50, |_| false, |_| Err(())),
            NativePointerRoute::Unknown
        );
        assert_eq!(classify_pointer_hierarchy(50, |_| false, Ok), NativePointerRoute::Unknown);
    }

    #[test]
    fn hover_route_does_not_change_capture_owner() {
        let mut captures = ImplicitPointerCaptures::default();
        captures.owners.insert(DEVICE, ImplicitCaptureOwner::Window(OWNER));

        for (first_child, expected) in [
            (0, NativePointerRoute::None),
            (OTHER_WINDOW, NativePointerRoute::Window(OTHER_WINDOW)),
            (300, NativePointerRoute::Foreign),
            (400, NativePointerRoute::Unknown),
        ] {
            let hover = classify_pointer_hierarchy(
                first_child,
                |window| window == OWNER || window == OTHER_WINDOW,
                |window| if window == 400 { Err(()) } else { Ok(0) },
            );
            assert_eq!(hover, expected);
            assert_eq!(
                captures.for_button_state(DEVICE, Some(true)),
                NativePointerRoute::Window(OWNER)
            );
        }
    }

    #[test]
    fn pointer_query_coordinates_must_match_the_native_event() {
        assert!(fp1616_matches_event(12 * 65_536 + 32_768, 12.5));
        assert!(fp1616_matches_event(-(12 * 65_536 + 32_768), -12.5));
        assert!(!fp1616_matches_event(12 * 65_536, 12.5));
        assert!(!fp1616_matches_event(0, f64::NAN));
    }
}
