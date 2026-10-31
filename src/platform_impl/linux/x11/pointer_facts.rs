//! Pure XI2 pointer-route state. Keeping Xlib/XCB calls outside this module lets the same state
//! transitions run as ordinary unit tests on non-X11 development hosts.

use std::collections::{HashMap, HashSet};
use std::num::NonZeroU64;
use std::os::raw::c_int;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

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

/// One connection lifetime, kept alive by every public token minted by this owner.
#[derive(Debug, Default)]
pub(crate) struct PointerPressConnectionDomain;

#[derive(Debug)]
struct PrimaryPressCandidateIdentity;

/// A pending validation owns this allocation across the unlocked XCB request/reply.
#[derive(Debug)]
pub(super) struct PrimaryPressCandidate {
    device: u16,
    window: u32,
    identity: Arc<PrimaryPressCandidateIdentity>,
}

#[derive(Debug)]
enum PrimaryPressState {
    Pending(Arc<PrimaryPressCandidateIdentity>),
    Active(NonZeroU64),
}

#[derive(Debug)]
struct ImplicitPointerCapture {
    owner: ImplicitCaptureOwner,
    primary: Option<PrimaryPressState>,
}

impl ImplicitPointerCapture {
    fn new(owner: ImplicitCaptureOwner) -> Self {
        Self { owner, primary: None }
    }

    fn invalidate(&mut self) {
        self.owner = ImplicitCaptureOwner::Unknown;
        self.primary = None;
    }
}

#[derive(Debug, Default)]
pub(super) struct ImplicitPointerCaptures {
    owners: HashMap<u16, ImplicitPointerCapture>,
    fence: CaptureMutationFence,
    domain: Arc<PointerPressConnectionDomain>,
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
        for capture in self.owners.values_mut() {
            if window.is_none()
                || window
                    .is_some_and(|window| capture.owner == ImplicitCaptureOwner::Window(window))
            {
                capture.invalidate();
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
            for capture in self.owners.values_mut() {
                capture.invalidate();
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
            self.owners
                .insert(device_id, ImplicitPointerCapture::new(ImplicitCaptureOwner::Unknown));
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
            self.owners.insert(device_id, ImplicitPointerCapture::new(owner));
        }

        let capture = self
            .owners
            .entry(device_id)
            .or_insert_with(|| ImplicitPointerCapture::new(ImplicitCaptureOwner::Unknown));
        if !transition.primary_after {
            // Primary ends independently of the implicit grab, which lasts until all buttons are
            // up.
            capture.primary = None;
        } else if pressed && !transition.primary_before {
            capture.primary = (delivery_is_winit
                && self.fence == CaptureMutationFence::Open
                && capture.owner == ImplicitCaptureOwner::Window(delivery_window)
                && capture.primary.is_none())
            .then(|| PrimaryPressState::Pending(Arc::new(PrimaryPressCandidateIdentity)));
        }
        capture.owner.route()
    }

    pub(super) fn primary_press_candidate(
        &self,
        device: u16,
        window: u32,
    ) -> Option<PrimaryPressCandidate> {
        let capture = self.owners.get(&device)?;
        if self.fence != CaptureMutationFence::Open
            || capture.owner != ImplicitCaptureOwner::Window(window)
        {
            return None;
        }
        let PrimaryPressState::Pending(identity) = capture.primary.as_ref()? else {
            return None;
        };
        Some(PrimaryPressCandidate { device, window, identity: Arc::clone(identity) })
    }

    pub(super) fn complete_primary_press(
        &mut self,
        candidate: PrimaryPressCandidate,
        sequence: Option<NonZeroU64>,
    ) {
        let Some(capture) = self.owners.get_mut(&candidate.device) else {
            return;
        };
        if !matches!(capture.primary.as_ref(), Some(PrimaryPressState::Pending(identity))
            if Arc::ptr_eq(identity, &candidate.identity))
        {
            return;
        }
        // Consume this exact pending validation even on failure. A repeated press cannot retry it.
        capture.primary = None;
        if self.fence == CaptureMutationFence::Open
            && capture.owner == ImplicitCaptureOwner::Window(candidate.window)
        {
            capture.primary = sequence.map(PrimaryPressState::Active);
        }
    }

    pub(super) fn primary_press(
        &self,
        device: u16,
        window: u32,
    ) -> Option<(Arc<PointerPressConnectionDomain>, NonZeroU64)> {
        let capture = self.owners.get(&device)?;
        if self.fence != CaptureMutationFence::Open
            || capture.owner != ImplicitCaptureOwner::Window(window)
        {
            return None;
        }
        let PrimaryPressState::Active(sequence) = capture.primary.as_ref()? else {
            return None;
        };
        Some((Arc::clone(&self.domain), *sequence))
    }

    pub(super) fn validate_primary_press(
        &self,
        device: u16,
        window: u32,
        domain: &Arc<PointerPressConnectionDomain>,
        sequence: NonZeroU64,
    ) -> bool {
        self.fence == CaptureMutationFence::Open
            && Arc::ptr_eq(&self.domain, domain)
            && self.owners.get(&device).is_some_and(|capture| {
                capture.owner == ImplicitCaptureOwner::Window(window)
                    && matches!(capture.primary.as_ref(), Some(PrimaryPressState::Active(current))
                        if *current == sequence)
            })
    }

    pub(super) fn for_button_state(
        &mut self,
        device_id: u16,
        buttons: Option<PointerButtonState>,
    ) -> NativePointerRoute {
        match buttons {
            Some(PointerButtonState { any_pressed: false, .. }) => {
                self.owners.remove(&device_id);
                NativePointerRoute::None
            },
            Some(buttons) => {
                let capture = self
                    .owners
                    .entry(device_id)
                    .or_insert_with(|| ImplicitPointerCapture::new(ImplicitCaptureOwner::Unknown));
                if !buttons.primary_pressed {
                    // A motion/enter mask can reveal a lost Primary release while another
                    // button still holds the implicit grab. Observations never mint a press.
                    capture.primary = None;
                }
                capture.owner.route()
            },
            None => {
                self.owners
                    .insert(device_id, ImplicitPointerCapture::new(ImplicitCaptureOwner::Unknown));
                NativePointerRoute::Unknown
            },
        }
    }

    pub(super) fn reset_device(&mut self, device_id: u16) -> Option<u32> {
        let capture = self.owners.get_mut(&device_id)?;
        let window = match capture.owner {
            ImplicitCaptureOwner::Window(window) => window,
            ImplicitCaptureOwner::Unknown => return None,
        };
        capture.invalidate();
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
            .filter_map(|(device_id, capture)| {
                (capture.owner == ImplicitCaptureOwner::Window(window)).then_some(*device_id)
            })
            .collect::<Vec<_>>();
        for device_id in &devices {
            if let Some(capture) = self.owners.get_mut(device_id) {
                capture.invalidate();
            }
        }
        self.close_fence();
        devices
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ButtonStateTransition {
    pub(super) any_before: bool,
    pub(super) any_after: bool,
    pub(super) primary_before: bool,
    pub(super) primary_after: bool,
}

impl ButtonStateTransition {
    pub(super) fn after(self) -> PointerButtonState {
        PointerButtonState { any_pressed: self.any_after, primary_pressed: self.primary_after }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PointerButtonState {
    pub(super) any_pressed: bool,
    pub(super) primary_pressed: bool,
}

pub(super) fn current_button_state(mask: &[u8]) -> Option<PointerButtonState> {
    let first = *mask.first()?;
    Some(PointerButtonState {
        any_pressed: mask.iter().any(|byte| *byte != 0),
        primary_pressed: first & (1 << 1) != 0,
    })
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

    let primary_before = mask[0] & (1 << 1) != 0;
    let primary_after = if detail == 1 { pressed } else { primary_before };
    Some(ButtonStateTransition { any_before, any_after, primary_before, primary_after })
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
        Some(ButtonStateTransition {
            any_before,
            any_after,
            primary_before: any_before,
            primary_after: any_after,
        })
    }

    fn begin_primary_validation(
        captures: &mut ImplicitPointerCaptures,
        device: u16,
        window: u32,
    ) -> PrimaryPressCandidate {
        assert_eq!(
            captures.after_button_event(
                device,
                window,
                true,
                true,
                button_state_transition(&[0], 1, true),
            ),
            NativePointerRoute::Window(window)
        );
        captures.primary_press_candidate(device, window).expect("the first Primary edge is pending")
    }

    fn validated_primary(
        captures: &mut ImplicitPointerCaptures,
        device: u16,
        window: u32,
        sequence: u64,
    ) -> (Arc<PointerPressConnectionDomain>, NonZeroU64) {
        let candidate = begin_primary_validation(captures, device, window);
        captures.complete_primary_press(candidate, NonZeroU64::new(sequence));
        captures.primary_press(device, window).expect("the window validation completed")
    }

    #[test]
    fn current_primary_up_revokes_identity_while_secondary_motion_keeps_capture() {
        let mut captures = ImplicitPointerCaptures::default();
        let (domain, sequence) = validated_primary(&mut captures, DEVICE, OWNER, 11);
        // No Primary release edge was delivered. The next native motion/enter mask still
        // observes Secondary down, so it must revoke only the Primary qualification.
        assert_eq!(
            captures.for_button_state(DEVICE, current_button_state(&[0b0000_1000])),
            NativePointerRoute::Window(OWNER)
        );
        assert!(!captures.validate_primary_press(DEVICE, OWNER, &domain, sequence));
        assert!(captures.primary_press(DEVICE, OWNER).is_none());
        assert!(captures.primary_press_candidate(DEVICE, OWNER).is_none());
        assert_eq!(
            captures.for_button_state(DEVICE, current_button_state(&[0b0000_1010])),
            NativePointerRoute::Window(OWNER)
        );
        assert!(captures.primary_press(DEVICE, OWNER).is_none(), "motion cannot mint a new press");
        assert!(captures.primary_press_candidate(DEVICE, OWNER).is_none());
        assert_eq!(
            captures.for_button_state(DEVICE, current_button_state(&[])),
            NativePointerRoute::Unknown
        );
        assert!(!captures.validate_primary_press(DEVICE, OWNER, &domain, sequence));

        captures.for_button_state(DEVICE, current_button_state(&[0]));
        let pending = begin_primary_validation(&mut captures, DEVICE, OWNER);
        assert_eq!(
            captures.for_button_state(DEVICE, current_button_state(&[0b0000_1000])),
            NativePointerRoute::Window(OWNER)
        );
        captures.complete_primary_press(pending, NonZeroU64::new(12));
        assert!(
            captures.primary_press(DEVICE, OWNER).is_none(),
            "a released candidate cannot complete"
        );
    }

    #[test]
    fn non_primary_edges_never_reissue_primary_identity() {
        let mut captures = ImplicitPointerCaptures::default();
        let (domain, sequence) = validated_primary(&mut captures, DEVICE, OWNER, 13);
        for (mask, pressed) in [(0b0000_0010, true), (0b0000_1010, false)] {
            assert_eq!(
                captures.after_button_event(
                    DEVICE,
                    OWNER,
                    true,
                    pressed,
                    button_state_transition(&[mask], 3, pressed),
                ),
                NativePointerRoute::Window(OWNER)
            );
            assert!(captures.validate_primary_press(DEVICE, OWNER, &domain, sequence));
            assert!(captures.primary_press_candidate(DEVICE, OWNER).is_none());
        }
    }

    #[test]
    fn primary_release_revokes_identity_before_secondary_releases_capture() {
        let mut captures = ImplicitPointerCaptures::default();
        let (domain, sequence) = validated_primary(&mut captures, DEVICE, OWNER, 17);
        assert!(captures.validate_primary_press(DEVICE, OWNER, &domain, sequence));
        assert!(!captures.validate_primary_press(DEVICE + 1, OWNER, &domain, sequence));
        assert!(!captures.validate_primary_press(DEVICE, OTHER_WINDOW, &domain, sequence));
        assert_eq!(
            captures.after_button_event(
                DEVICE,
                OWNER,
                true,
                true,
                button_state_transition(&[0b0000_0010], 3, true),
            ),
            NativePointerRoute::Window(OWNER)
        );
        assert!(captures.validate_primary_press(DEVICE, OWNER, &domain, sequence));
        assert_eq!(
            captures.after_button_event(
                DEVICE,
                OWNER,
                true,
                false,
                button_state_transition(&[0b0000_1010], 1, false),
            ),
            NativePointerRoute::Window(OWNER)
        );
        assert!(captures.primary_press(DEVICE, OWNER).is_none());
        assert!(!captures.validate_primary_press(DEVICE, OWNER, &domain, sequence));
        assert_eq!(
            captures.for_button_state(DEVICE, current_button_state(&[0b0000_0010])),
            NativePointerRoute::Window(OWNER)
        );

        // The remaining implicit grab can carry a distinct, genuinely new Primary press.
        captures.after_button_event(
            DEVICE,
            OWNER,
            true,
            true,
            button_state_transition(&[0b0000_1000], 1, true),
        );
        let fresh = captures.primary_press_candidate(DEVICE, OWNER).unwrap();
        captures.complete_primary_press(fresh, NonZeroU64::new(18));
        let (fresh_domain, fresh_sequence) = captures.primary_press(DEVICE, OWNER).unwrap();
        assert!(Arc::ptr_eq(&domain, &fresh_domain));
        assert_ne!(sequence, fresh_sequence);
        assert!(!captures.validate_primary_press(DEVICE, OWNER, &domain, sequence));
        assert!(captures.validate_primary_press(DEVICE, OWNER, &fresh_domain, fresh_sequence));
        assert_eq!(
            captures.after_button_event(
                DEVICE,
                OWNER,
                true,
                false,
                button_state_transition(&[0b0000_1010], 3, false),
            ),
            NativePointerRoute::Window(OWNER)
        );
        assert!(captures.validate_primary_press(DEVICE, OWNER, &fresh_domain, fresh_sequence));
        assert_eq!(
            captures.after_button_event(
                DEVICE,
                OWNER,
                true,
                false,
                button_state_transition(&[0b0000_0010], 1, false),
            ),
            NativePointerRoute::None
        );
    }

    #[test]
    fn repeated_primary_edges_never_replace_or_retry_window_validation() {
        let mut captures = ImplicitPointerCaptures::default();
        let candidate = begin_primary_validation(&mut captures, DEVICE, OWNER);
        let duplicate = captures.primary_press_candidate(DEVICE, OWNER).unwrap();
        assert!(captures.primary_press(DEVICE, OWNER).is_none());
        captures.after_button_event(
            DEVICE,
            OWNER,
            true,
            true,
            button_state_transition(&[0b0000_0010], 1, true),
        );
        assert!(matches!(
            captures.owners[&DEVICE].primary.as_ref(),
            Some(PrimaryPressState::Pending(_))
        ));
        captures.complete_primary_press(candidate, NonZeroU64::new(31));
        let (domain, sequence) = captures.primary_press(DEVICE, OWNER).unwrap();
        captures.complete_primary_press(duplicate, NonZeroU64::new(32));
        assert!(captures.primary_press_candidate(DEVICE, OWNER).is_none());
        assert!(captures.validate_primary_press(DEVICE, OWNER, &domain, sequence));
        assert_eq!(sequence.get(), 31);

        captures.for_button_state(DEVICE, current_button_state(&[0]));
        let failed = begin_primary_validation(&mut captures, DEVICE, OWNER);
        captures.complete_primary_press(failed, None);
        captures.after_button_event(
            DEVICE,
            OWNER,
            true,
            true,
            button_state_transition(&[0b0000_0010], 1, true),
        );
        assert!(captures.primary_press_candidate(DEVICE, OWNER).is_none());
        assert!(captures.primary_press(DEVICE, OWNER).is_none());
        assert_eq!(
            captures.for_button_state(DEVICE, current_button_state(&[0b0000_0010])),
            NativePointerRoute::Window(OWNER)
        );
    }

    #[test]
    fn revoked_pending_validation_cannot_bind_a_reused_window_or_device() {
        for revoke in 0..5 {
            let mut captures = ImplicitPointerCaptures::default();
            let predecessor = begin_primary_validation(&mut captures, DEVICE, OWNER);
            match revoke {
                0 => captures.invalidate(Some(OWNER)),
                1 => assert_eq!(captures.reset_window(OWNER), [DEVICE]),
                2 => assert_eq!(captures.reset_device(DEVICE), Some(OWNER)),
                3 => captures.remove_device(DEVICE),
                4 => {
                    assert_eq!(
                        captures.for_button_state(DEVICE, None),
                        NativePointerRoute::Unknown
                    );
                },
                _ => unreachable!(),
            }
            if captures.begin_sync(0) {
                captures.finish_sync(true);
                captures.queue_drained(0);
            }
            captures.for_button_state(DEVICE, current_button_state(&[0]));
            let successor = begin_primary_validation(&mut captures, DEVICE, OWNER);
            captures.complete_primary_press(predecessor, NonZeroU64::new(51));
            assert!(captures.primary_press(DEVICE, OWNER).is_none());
            captures.complete_primary_press(successor, NonZeroU64::new(52));
            let (domain, sequence) = captures.primary_press(DEVICE, OWNER).unwrap();
            assert_eq!(sequence.get(), 52);
            assert!(captures.validate_primary_press(DEVICE, OWNER, &domain, sequence));
        }
    }

    #[test]
    fn reentrant_mutation_revokes_an_already_validated_primary() {
        let mut captures = ImplicitPointerCaptures::default();
        let (domain, sequence) = validated_primary(&mut captures, DEVICE, OWNER, 61);
        // This is the gap between recording the edge and returning from ModifiersChanged.
        captures.invalidate(None);
        assert!(!captures.validate_primary_press(DEVICE, OWNER, &domain, sequence));
        assert!(captures.primary_press(DEVICE, OWNER).is_none());
        assert!(captures.begin_sync(0));
        captures.finish_sync(true);
        captures.queue_drained(0);
        assert!(!captures.validate_primary_press(DEVICE, OWNER, &domain, sequence));
        assert!(captures.primary_press_candidate(DEVICE, OWNER).is_none());
    }

    #[test]
    fn validation_completing_with_a_closed_fence_is_consumed_without_issuing_identity() {
        let mut captures = ImplicitPointerCaptures::default();
        let candidate = begin_primary_validation(&mut captures, DEVICE, OWNER);
        // An unrelated mutation leaves this owner row intact but closes candidate admission.
        captures.invalidate(Some(OTHER_WINDOW));
        captures.complete_primary_press(candidate, NonZeroU64::new(71));
        assert!(captures.begin_sync(0));
        captures.finish_sync(true);
        captures.queue_drained(0);
        assert!(captures.primary_press(DEVICE, OWNER).is_none());
        assert!(captures.primary_press_candidate(DEVICE, OWNER).is_none());
        assert_eq!(
            captures.for_button_state(DEVICE, current_button_state(&[0b0000_0010])),
            NativePointerRoute::Window(OWNER)
        );
    }

    #[test]
    fn connection_domains_and_new_requests_separate_reused_numeric_identifiers() {
        let mut first_connection = ImplicitPointerCaptures::default();
        let mut replacement_connection = ImplicitPointerCaptures::default();
        let (old_domain, old_sequence) =
            validated_primary(&mut first_connection, DEVICE, OWNER, 81);
        let (new_domain, new_sequence) =
            validated_primary(&mut replacement_connection, DEVICE, OWNER, 81);
        assert_eq!(old_sequence, new_sequence);
        assert!(!Arc::ptr_eq(&old_domain, &new_domain));
        assert!(!replacement_connection.validate_primary_press(
            DEVICE,
            OWNER,
            &old_domain,
            old_sequence
        ));
        assert!(replacement_connection.validate_primary_press(
            DEVICE,
            OWNER,
            &new_domain,
            new_sequence
        ));

        first_connection.remove_device(DEVICE);
        let (reused_domain, reused_sequence) =
            validated_primary(&mut first_connection, DEVICE, OWNER, 82);
        assert!(Arc::ptr_eq(&old_domain, &reused_domain));
        assert_ne!(old_sequence, reused_sequence);
        assert!(!first_connection.validate_primary_press(DEVICE, OWNER, &old_domain, old_sequence));
        assert!(first_connection.validate_primary_press(
            DEVICE,
            OWNER,
            &reused_domain,
            reused_sequence
        ));
    }

    #[test]
    fn failed_sync_and_unknown_button_transition_revoke_validated_primary() {
        for unknown_transition in [false, true] {
            let mut captures = ImplicitPointerCaptures::default();
            let (domain, sequence) = validated_primary(&mut captures, DEVICE, OWNER, 91);
            if unknown_transition {
                captures.after_button_event(DEVICE, OWNER, true, true, None);
            } else {
                captures.invalidate(Some(OTHER_WINDOW));
                assert!(captures.begin_sync(0));
                captures.finish_sync(false);
            }
            assert!(captures.primary_press(DEVICE, OWNER).is_none());
            assert!(!captures.validate_primary_press(DEVICE, OWNER, &domain, sequence));
            assert_eq!(
                captures.for_button_state(DEVICE, current_button_state(&[0b0000_0010])),
                NativePointerRoute::Unknown
            );
        }
    }

    #[test]
    fn button_transition_uses_post_event_state() {
        assert_eq!(button_state_transition(&[0], 1, true), transition(false, true));
        assert_eq!(button_state_transition(&[0b0000_0010], 1, false), transition(true, false));
        assert_eq!(
            button_state_transition(&[0b0000_1010], 1, false),
            Some(ButtonStateTransition {
                any_before: true,
                any_after: true,
                primary_before: true,
                primary_after: false,
            })
        );
        assert_eq!(button_state_transition(&[], 1, true), None);
        assert_eq!(button_state_transition(&[0], 0, true), None);
        assert_eq!(
            Some(button_state_transition(&[0b0000_1010], 1, false).unwrap().after()),
            current_button_state(&[0b0000_1000])
        );
    }

    #[test]
    fn implicit_capture_stays_with_first_delivery_until_last_release() {
        let mut captures = ImplicitPointerCaptures::default();

        assert_eq!(
            captures.after_button_event(DEVICE, OWNER, true, true, transition(false, true)),
            NativePointerRoute::Window(OWNER)
        );
        assert_eq!(
            captures.for_button_state(DEVICE, current_button_state(&[0b0000_0010])),
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

        assert_eq!(
            captures.for_button_state(DEVICE, current_button_state(&[0b0000_0010])),
            NativePointerRoute::Unknown
        );
        assert_eq!(captures.for_button_state(DEVICE, None), NativePointerRoute::Unknown);
        assert_eq!(
            captures.for_button_state(DEVICE, current_button_state(&[0])),
            NativePointerRoute::None
        );
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
        assert_eq!(
            captures.for_button_state(DEVICE, current_button_state(&[0b0000_0010])),
            NativePointerRoute::Unknown
        );
        assert!(captures.begin_sync(0));
        captures.finish_sync(true);
        captures.queue_drained(0);
        assert_eq!(
            captures.after_button_event(DEVICE, OWNER, true, true, transition(false, true)),
            NativePointerRoute::Unknown
        );
        assert_eq!(
            captures.for_button_state(DEVICE, current_button_state(&[0])),
            NativePointerRoute::None
        );
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
        assert_eq!(
            captures.for_button_state(DEVICE, current_button_state(&[0])),
            NativePointerRoute::None
        );
        captures.finish_sync(true);

        // XSync has queued older events, including an all-up followed by another first press.
        assert_eq!(
            captures.after_button_event(DEVICE, OWNER, true, true, transition(false, true)),
            NativePointerRoute::Unknown
        );
        captures.queue_drained(0);
        assert_eq!(
            captures.for_button_state(DEVICE, current_button_state(&[0b0000_0010])),
            NativePointerRoute::Unknown
        );
        assert_eq!(
            captures.for_button_state(DEVICE, current_button_state(&[0])),
            NativePointerRoute::None
        );
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
            captures.for_button_state(other_device, current_button_state(&[0b0000_0010])),
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
        assert_eq!(
            captures.for_button_state(DEVICE, current_button_state(&[0b0000_0010])),
            NativePointerRoute::Unknown
        );
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
        assert_eq!(
            captures.for_button_state(DEVICE, current_button_state(&[0])),
            NativePointerRoute::None
        );
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
        assert_eq!(
            state.for_button_state(DEVICE, current_button_state(&[0b0000_0010])),
            NativePointerRoute::Unknown
        );
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
            assert_eq!(
                state.for_button_state(DEVICE, current_button_state(&[0])),
                NativePointerRoute::None
            );
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
            captures.for_button_state(other_device, current_button_state(&[0b0000_0010])),
            NativePointerRoute::Unknown
        );
        captures.queue_drained(0);
        captures.invalidate(None);
        assert!(!captures.begin_sync(0));
        captures.finish_sync(true);
        captures.queue_drained(0);
        assert_eq!(
            captures.for_button_state(DEVICE, current_button_state(&[0])),
            NativePointerRoute::None
        );
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
        assert_eq!(
            captures.for_button_state(DEVICE, current_button_state(&[0b0000_0010])),
            NativePointerRoute::Unknown
        );
        assert_eq!(
            captures.after_button_event(DEVICE, OWNER, true, true, transition(false, true)),
            NativePointerRoute::Unknown
        );
        assert_eq!(
            captures.for_button_state(DEVICE, current_button_state(&[0])),
            NativePointerRoute::None
        );
        assert_eq!(
            captures.after_button_event(DEVICE, OWNER, true, true, transition(false, true)),
            NativePointerRoute::Window(OWNER)
        );
    }

    #[test]
    fn resetting_window_capture_is_scoped_and_idempotent() {
        let mut captures = ImplicitPointerCaptures::default();
        let other_device = DEVICE + 1;
        captures
            .owners
            .insert(DEVICE, ImplicitPointerCapture::new(ImplicitCaptureOwner::Window(OWNER)));
        captures.owners.insert(
            other_device,
            ImplicitPointerCapture::new(ImplicitCaptureOwner::Window(OTHER_WINDOW)),
        );

        assert_eq!(captures.reset_window(OWNER), vec![DEVICE]);
        assert!(captures.reset_window(OWNER).is_empty());
        assert_eq!(
            captures.for_button_state(DEVICE, current_button_state(&[0b0000_0010])),
            NativePointerRoute::Unknown
        );
        assert_eq!(
            captures.for_button_state(other_device, current_button_state(&[0b0000_0010])),
            NativePointerRoute::Window(OTHER_WINDOW)
        );
        assert_eq!(captures.reset_device(other_device), Some(OTHER_WINDOW));
        assert_eq!(captures.reset_device(other_device), None);
        assert_eq!(
            captures.for_button_state(other_device, current_button_state(&[0b0000_0010])),
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
        captures
            .owners
            .insert(DEVICE, ImplicitPointerCapture::new(ImplicitCaptureOwner::Window(OWNER)));

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
                captures.for_button_state(DEVICE, current_button_state(&[0b0000_0010])),
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
