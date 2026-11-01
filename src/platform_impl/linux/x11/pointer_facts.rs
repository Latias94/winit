//! Pure XI2 pointer-route state. Keeping Xlib/XCB calls outside this module lets the same state
//! transitions run as ordinary unit tests on non-X11 development hosts.
//!
//! Native grab and ungrab dispatch stays outside this module. Each reserved acquisition must report
//! its result, even after revocation. Retiring identities keep late successes and failed cleanup in
//! the same device row until confirmed release; neither all-up nor device removal erases that duty.

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
    // A cleanup-only row has no implicit stream or live Primary qualification.
    None,
    Window(u32),
    Unknown,
}

impl ImplicitCaptureOwner {
    fn route(self) -> NativePointerRoute {
        match self {
            Self::None => NativePointerRoute::None,
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
struct ExplicitCaptureIdentity {
    device: u16,
    window: u32,
    domain: Arc<PointerPressConnectionDomain>,
    press: NonZeroU64,
}

impl ExplicitCaptureIdentity {
    fn matches_press(
        &self,
        device: u16,
        window: u32,
        domain: &Arc<PointerPressConnectionDomain>,
        press: NonZeroU64,
    ) -> bool {
        self.device == device
            && self.window == window
            && Arc::ptr_eq(&self.domain, domain)
            && self.press == press
    }
}

#[derive(Debug)]
#[must_use = "Every reserved acquisition must report its native result, including cancellation."]
pub(super) struct ExplicitCaptureCandidate(Arc<ExplicitCaptureIdentity>);

#[derive(Debug)]
#[must_use = "A capture sample must finish its exact native refresh, including cancellation."]
pub(super) struct ExplicitCaptureSample(Arc<ExplicitCaptureIdentity>);

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ExplicitCapturePhase {
    Unacquired,
    NativeRequest,
    AwaitingFence,
    Active,
    Sampling,
    Retiring,
    Revoked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ExplicitCaptureStatus {
    Pending,
    Acquired,
    Unavailable,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum ExplicitCaptureCompletion {
    AwaitingFence,
    Retiring,
    Failed,
    Stale,
}

#[derive(Debug)]
struct ExplicitCaptureReleaseIdentity;

#[derive(Debug)]
#[must_use = "A claimed release must report completion or cancellation before it can be retried."]
pub(super) struct ExplicitCaptureRelease {
    identity: Arc<ExplicitCaptureIdentity>,
    claim: Arc<ExplicitCaptureReleaseIdentity>,
}

impl ExplicitCaptureRelease {
    #[allow(dead_code)]
    pub(super) fn device(&self) -> u16 {
        self.identity.device
    }
}

#[derive(Debug)]
enum ExplicitCaptureFence {
    NeedsSync,
    Syncing,
    Draining,
}

#[derive(Debug)]
enum ExplicitCaptureAcquisition {
    NativeRequest,
    AwaitingFence(ExplicitCaptureFence),
}

#[derive(Debug)]
enum ExplicitCaptureRetirement {
    NativeRequest,
    NeedsRelease,
    ReleaseQueued(Arc<ExplicitCaptureReleaseIdentity>),
    Releasing(Arc<ExplicitCaptureReleaseIdentity>),
}

#[derive(Debug)]
enum ExplicitCaptureState {
    Unacquired,
    Acquiring { identity: Arc<ExplicitCaptureIdentity>, stage: ExplicitCaptureAcquisition },
    Active(Arc<ExplicitCaptureIdentity>),
    Sampling { identity: Arc<ExplicitCaptureIdentity>, interval_clear: bool },
    Retiring { identity: Arc<ExplicitCaptureIdentity>, stage: ExplicitCaptureRetirement },
    Revoked,
}

#[derive(Debug)]
struct ImplicitPointerCapture {
    owner: ImplicitCaptureOwner,
    primary: Option<PrimaryPressState>,
    // A successor Primary may become eligible while an older native obligation still retires.
    acquisition_available: bool,
    explicit: ExplicitCaptureState,
}

impl ImplicitPointerCapture {
    fn new(owner: ImplicitCaptureOwner) -> Self {
        Self {
            owner,
            primary: None,
            acquisition_available: false,
            explicit: ExplicitCaptureState::Unacquired,
        }
    }

    fn invalidate(&mut self) {
        self.clear_primary();
        if self.owner != ImplicitCaptureOwner::None {
            self.owner = ImplicitCaptureOwner::Unknown;
        }
    }

    fn replace_owner(&mut self, owner: ImplicitCaptureOwner) {
        self.clear_primary();
        self.owner = owner;
    }

    fn clear_primary(&mut self) {
        self.revoke_explicit_capture();
        self.primary = None;
    }

    fn revoke_explicit_capture(&mut self) {
        self.acquisition_available = false;
        self.retire_explicit_capture();
    }

    fn retire_explicit_capture(&mut self) {
        self.explicit = match std::mem::replace(&mut self.explicit, ExplicitCaptureState::Revoked) {
            ExplicitCaptureState::Acquiring { identity, stage } => {
                let stage = match stage {
                    ExplicitCaptureAcquisition::NativeRequest => {
                        ExplicitCaptureRetirement::NativeRequest
                    },
                    ExplicitCaptureAcquisition::AwaitingFence(_) => {
                        ExplicitCaptureRetirement::NeedsRelease
                    },
                };
                ExplicitCaptureState::Retiring { identity, stage }
            },
            ExplicitCaptureState::Active(identity) => ExplicitCaptureState::Retiring {
                identity,
                stage: ExplicitCaptureRetirement::NeedsRelease,
            },
            ExplicitCaptureState::Sampling { identity, .. } => ExplicitCaptureState::Retiring {
                identity,
                stage: ExplicitCaptureRetirement::NativeRequest,
            },
            retiring @ ExplicitCaptureState::Retiring { .. } => retiring,
            ExplicitCaptureState::Unacquired | ExplicitCaptureState::Revoked => {
                ExplicitCaptureState::Revoked
            },
        };
    }

    fn has_native_obligation(&self) -> bool {
        matches!(
            self.explicit,
            ExplicitCaptureState::Acquiring { .. }
                | ExplicitCaptureState::Active(_)
                | ExplicitCaptureState::Sampling { .. }
                | ExplicitCaptureState::Retiring { .. }
        )
    }

    fn release_in_flight(&self) -> bool {
        matches!(self.explicit, ExplicitCaptureState::Retiring {
            stage: ExplicitCaptureRetirement::Releasing(_),
            ..
        })
    }

    fn clear_native_obligation(&mut self) {
        self.explicit = if self.acquisition_available {
            ExplicitCaptureState::Unacquired
        } else {
            ExplicitCaptureState::Revoked
        };
    }

    fn claim_explicit_capture_release(&mut self) -> Option<ExplicitCaptureRelease> {
        let ExplicitCaptureState::Retiring {
            identity,
            stage: stage @ ExplicitCaptureRetirement::NeedsRelease,
        } = &mut self.explicit
        else {
            return None;
        };
        let claim = Arc::new(ExplicitCaptureReleaseIdentity);
        *stage = ExplicitCaptureRetirement::ReleaseQueued(Arc::clone(&claim));
        Some(ExplicitCaptureRelease { identity: Arc::clone(identity), claim })
    }

    fn begin_explicit_capture_sync(&mut self) -> bool {
        let ExplicitCaptureState::Acquiring {
            stage:
                stage @ ExplicitCaptureAcquisition::AwaitingFence(ExplicitCaptureFence::NeedsSync),
            ..
        } = &mut self.explicit
        else {
            return false;
        };
        *stage = ExplicitCaptureAcquisition::AwaitingFence(ExplicitCaptureFence::Syncing);
        true
    }

    fn finish_explicit_capture_sync(&mut self) {
        if let ExplicitCaptureState::Acquiring {
            stage: stage @ ExplicitCaptureAcquisition::AwaitingFence(ExplicitCaptureFence::Syncing),
            ..
        } = &mut self.explicit
        {
            *stage = ExplicitCaptureAcquisition::AwaitingFence(ExplicitCaptureFence::Draining);
        }
    }

    fn explicit_capture_awaits_sync(&self) -> bool {
        matches!(
            self.explicit,
            ExplicitCaptureState::Sampling { .. }
                | ExplicitCaptureState::Acquiring {
                    stage: ExplicitCaptureAcquisition::NativeRequest
                        | ExplicitCaptureAcquisition::AwaitingFence(
                            ExplicitCaptureFence::NeedsSync | ExplicitCaptureFence::Syncing,
                        ),
                    ..
                }
                | ExplicitCaptureState::Retiring {
                    stage: ExplicitCaptureRetirement::NativeRequest
                        | ExplicitCaptureRetirement::Releasing(_),
                    ..
                }
        )
    }

    fn finish_explicit_capture_fence(&mut self) {
        let ExplicitCaptureState::Acquiring {
            identity,
            stage: ExplicitCaptureAcquisition::AwaitingFence(ExplicitCaptureFence::Draining),
        } = &self.explicit
        else {
            return;
        };
        self.explicit = if self.owner == ImplicitCaptureOwner::Window(identity.window)
            && matches!(self.primary.as_ref(), Some(PrimaryPressState::Active(press))
                if *press == identity.press)
        {
            ExplicitCaptureState::Active(Arc::clone(identity))
        } else {
            ExplicitCaptureState::Retiring {
                identity: Arc::clone(identity),
                stage: ExplicitCaptureRetirement::NeedsRelease,
            }
        };
    }
}

#[derive(Debug, Default)]
pub(super) struct ImplicitPointerCaptures {
    owners: HashMap<u16, ImplicitPointerCapture>,
    fence: CaptureMutationFence,
    // Serialize round trips even when acquisition leaves the mutation fence Open.
    sync_in_flight: bool,
    grab_mutation_in_flight: bool,
    domain: Arc<PointerPressConnectionDomain>,
}

/// Bracket request dispatch without holding either a Display lock or the capture mutex.
pub(super) struct PointerCaptureMutationGuard<'a> {
    captures: &'a Mutex<ImplicitPointerCaptures>,
    native_event_depth: &'a AtomicU32,
}

/// Serialize one ordinary grab operation without holding the capture mutex across native calls.
pub(super) struct PointerGrabMutationGuard<'a> {
    captures: &'a Mutex<ImplicitPointerCaptures>,
    native_event_depth: &'a AtomicU32,
}

impl<'a> PointerGrabMutationGuard<'a> {
    pub(super) fn try_new(
        captures: &'a Mutex<ImplicitPointerCaptures>,
        native_event_depth: &'a AtomicU32,
    ) -> Option<Self> {
        let mut state = captures.lock().unwrap_or_else(|error| error.into_inner());
        if state.grab_mutation_in_flight
            || state.owners.values().any(ImplicitPointerCapture::has_native_obligation)
        {
            return None;
        }
        state.grab_mutation_in_flight = true;
        native_event_depth.fetch_add(1, Ordering::Relaxed);
        state.invalidate(None);
        Some(Self { captures, native_event_depth })
    }
}

impl Drop for PointerGrabMutationGuard<'_> {
    fn drop(&mut self) {
        let mut state = self.captures.lock().unwrap_or_else(|error| error.into_inner());
        state.close_fence();
        state.grab_mutation_in_flight = false;
        self.native_event_depth.fetch_sub(1, Ordering::Relaxed);
    }
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
    /// A nested native expansion invalidates any older in-flight query even
    /// when its selected events have all been consumed before the final check.
    pub(super) fn invalidate_capture_sample_intervals(&mut self) {
        for capture in self.owners.values_mut() {
            if let ExplicitCaptureState::Sampling { interval_clear, .. } = &mut capture.explicit {
                *interval_clear = false;
            }
        }
    }

    fn end_device_stream(&mut self, device: u16) {
        if let Some(capture) = self.owners.get_mut(&device) {
            capture.replace_owner(ImplicitCaptureOwner::None);
        }
        self.prune_finished_device(device);
    }

    fn prune_finished_device(&mut self, device: u16) {
        if self.owners.get(&device).is_some_and(|capture| {
            capture.owner == ImplicitCaptureOwner::None && !capture.has_native_obligation()
        }) {
            self.owners.remove(&device);
        }
    }

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
        // Every ordinary mutation revokes leases before any later sync/drain can reopen admission.
        for capture in self.owners.values_mut() {
            capture.revoke_explicit_capture();
        }
        if self.fence != CaptureMutationFence::Failed {
            self.fence = CaptureMutationFence::NeedsSync;
        }
    }

    // The caller samples the shared depth while holding the same mutex as mutation begin/Drop.
    pub(super) fn begin_sync(&mut self, native_event_depth: u32) -> bool {
        if native_event_depth != 0
            || self.sync_in_flight
            || self.grab_mutation_in_flight
            || self.owners.values().any(ImplicitPointerCapture::release_in_flight)
        {
            return false;
        }
        let mutation = self.fence == CaptureMutationFence::NeedsSync;
        if !mutation && self.fence != CaptureMutationFence::Open {
            return false;
        }
        let mut acquisition = false;
        for capture in self.owners.values_mut() {
            acquisition |= capture.begin_explicit_capture_sync();
        }
        if !mutation && !acquisition {
            return false;
        }
        if mutation {
            self.fence = CaptureMutationFence::Syncing;
        }
        self.sync_in_flight = true;
        true
    }

    pub(super) fn finish_sync(&mut self, succeeded: bool) {
        let was_in_flight = self.sync_in_flight;
        self.sync_in_flight = false;
        if !succeeded {
            for capture in self.owners.values_mut() {
                capture.invalidate();
            }
            self.fence = CaptureMutationFence::Failed;
        } else if was_in_flight {
            if self.fence == CaptureMutationFence::Syncing {
                // A mutation during XSync changes Syncing back to NeedsSync. Its request must be
                // covered by another round trip, not by this older completion.
                self.fence = CaptureMutationFence::Draining;
            }
            for capture in self.owners.values_mut() {
                capture.finish_explicit_capture_sync();
            }
        }
    }

    pub(super) fn queue_drained(&mut self, native_event_depth: u32) {
        // Only the normal event-loop FIFO empty check supplies this proof; sync dispatches no
        // events.
        if native_event_depth != 0
            || self.sync_in_flight
            || self.grab_mutation_in_flight
            || self.owners.values().any(ImplicitPointerCapture::release_in_flight)
        {
            return;
        }
        if self.fence == CaptureMutationFence::Draining {
            self.fence = CaptureMutationFence::Open;
        }
        if self.fence != CaptureMutationFence::Open
            || self.owners.values().any(ImplicitPointerCapture::explicit_capture_awaits_sync)
        {
            return;
        }
        // A native completion after an unlocked empty-queue sample still requires its own sync.
        // Keep the original press valid while only its explicit lease awaits this FIFO proof.
        for capture in self.owners.values_mut() {
            capture.finish_explicit_capture_fence();
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
                .entry(device_id)
                .or_insert_with(|| ImplicitPointerCapture::new(ImplicitCaptureOwner::Unknown))
                .replace_owner(ImplicitCaptureOwner::Unknown);
            return NativePointerRoute::Unknown;
        };

        if !transition.any_after {
            self.end_device_stream(device_id);
            return NativePointerRoute::None;
        }

        if pressed && !transition.any_before {
            let owner = if delivery_is_winit
                && self.fence == CaptureMutationFence::Open
                && self
                    .owners
                    .get(&device_id)
                    .map_or(true, |capture| capture.owner == ImplicitCaptureOwner::None)
            {
                ImplicitCaptureOwner::Window(delivery_window)
            } else {
                ImplicitCaptureOwner::Unknown
            };
            self.owners
                .entry(device_id)
                .or_insert_with(|| ImplicitPointerCapture::new(owner))
                .replace_owner(owner);
        }

        let capture = self
            .owners
            .entry(device_id)
            .or_insert_with(|| ImplicitPointerCapture::new(ImplicitCaptureOwner::Unknown));
        if !transition.primary_after {
            // Primary ends independently of the implicit grab, which lasts until all buttons are
            // up.
            capture.clear_primary();
        } else if pressed && !transition.primary_before {
            capture.revoke_explicit_capture();
            capture.primary = (delivery_is_winit
                && self.fence == CaptureMutationFence::Open
                && capture.owner == ImplicitCaptureOwner::Window(delivery_window)
                && capture.primary.is_none())
            .then(|| PrimaryPressState::Pending(Arc::new(PrimaryPressCandidateIdentity)));
            if capture.primary.is_some() {
                // A new Primary is eligible, but an older native obligation still blocks reserve.
                capture.acquisition_available = true;
                if !capture.has_native_obligation() {
                    capture.explicit = ExplicitCaptureState::Unacquired;
                }
            }
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
                self.end_device_stream(device_id);
                NativePointerRoute::None
            },
            Some(buttons) => {
                let capture = self
                    .owners
                    .entry(device_id)
                    .or_insert_with(|| ImplicitPointerCapture::new(ImplicitCaptureOwner::Unknown));
                if capture.owner == ImplicitCaptureOwner::None {
                    // Retained cleanup rows do not make a down-mask observation a new press.
                    capture.owner = ImplicitCaptureOwner::Unknown;
                }
                if !buttons.primary_pressed {
                    // A motion/enter mask can reveal a lost Primary release while another
                    // button still holds the implicit grab. Observations never mint a press.
                    capture.clear_primary();
                }
                capture.owner.route()
            },
            None => {
                self.owners
                    .entry(device_id)
                    .or_insert_with(|| ImplicitPointerCapture::new(ImplicitCaptureOwner::Unknown))
                    .replace_owner(ImplicitCaptureOwner::Unknown);
                NativePointerRoute::Unknown
            },
        }
    }

    pub(super) fn reset_device(&mut self, device_id: u16) -> Option<u32> {
        let capture = self.owners.get_mut(&device_id)?;
        let window = match capture.owner {
            ImplicitCaptureOwner::Window(window) => window,
            ImplicitCaptureOwner::None | ImplicitCaptureOwner::Unknown => return None,
        };
        capture.invalidate();
        self.close_fence();
        Some(window)
    }

    pub(super) fn remove_device(&mut self, device_id: u16) {
        self.end_device_stream(device_id);
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

impl ImplicitPointerCaptures {
    /// Read the device phase without granting authority or reserving another attempt.
    #[cfg(test)]
    pub(super) fn explicit_capture_phase(&self, device: u16) -> ExplicitCapturePhase {
        let Some(capture) = self.owners.get(&device) else {
            return ExplicitCapturePhase::Unacquired;
        };
        match &capture.explicit {
            ExplicitCaptureState::Unacquired => ExplicitCapturePhase::Unacquired,
            ExplicitCaptureState::Acquiring {
                stage: ExplicitCaptureAcquisition::NativeRequest,
                ..
            } => ExplicitCapturePhase::NativeRequest,
            ExplicitCaptureState::Acquiring {
                stage: ExplicitCaptureAcquisition::AwaitingFence(_),
                ..
            } => ExplicitCapturePhase::AwaitingFence,
            ExplicitCaptureState::Active(_) => ExplicitCapturePhase::Active,
            ExplicitCaptureState::Sampling { .. } => ExplicitCapturePhase::Sampling,
            ExplicitCaptureState::Retiring { .. } => ExplicitCapturePhase::Retiring,
            ExplicitCaptureState::Revoked => ExplicitCapturePhase::Revoked,
        }
    }

    /// Poll the exact operation effect, not a fresh native capture observation.
    /// Pending includes an eligible successor waiting for an older cleanup to finish.
    pub(super) fn explicit_capture_status(
        &self,
        device: u16,
        window: u32,
        domain: &Arc<PointerPressConnectionDomain>,
        press: NonZeroU64,
    ) -> ExplicitCaptureStatus {
        if !self.validate_primary_press(device, window, domain, press) {
            return ExplicitCaptureStatus::Unavailable;
        }
        if self.has_explicit_capture(device, window, domain, press) {
            return ExplicitCaptureStatus::Acquired;
        }
        let Some(capture) = self.owners.get(&device) else {
            return ExplicitCaptureStatus::Unavailable;
        };
        if capture.acquisition_available
            || matches!(&capture.explicit, ExplicitCaptureState::Acquiring { identity, .. }
                | ExplicitCaptureState::Sampling { identity, .. }
                if identity.matches_press(device, window, domain, press))
        {
            ExplicitCaptureStatus::Pending
        } else {
            ExplicitCaptureStatus::Unavailable
        }
    }

    /// Reserve the only acquisition attempt allowed for this original validated press.
    pub(super) fn begin_explicit_capture(
        &mut self,
        device: u16,
        window: u32,
        domain: &Arc<PointerPressConnectionDomain>,
        press: NonZeroU64,
    ) -> Option<ExplicitCaptureCandidate> {
        if self.grab_mutation_in_flight
            || !self.validate_primary_press(device, window, domain, press)
        {
            return None;
        }
        let capture = self.owners.get_mut(&device)?;
        if !capture.acquisition_available || capture.has_native_obligation() {
            return None;
        }
        capture.acquisition_available = false;
        let identity =
            Arc::new(ExplicitCaptureIdentity { device, window, domain: Arc::clone(domain), press });
        capture.explicit = ExplicitCaptureState::Acquiring {
            identity: Arc::clone(&identity),
            stage: ExplicitCaptureAcquisition::NativeRequest,
        };
        Some(ExplicitCaptureCandidate(identity))
    }

    /// Report every native result after dispatch ends, including cancellation and late success.
    /// Some(false) confirms no grab; None means a grab may exist and must be cleaned up.
    /// Acceptance and FIFO completion describe the operation effect, not capture continuity.
    pub(super) fn complete_explicit_capture(
        &mut self,
        candidate: ExplicitCaptureCandidate,
        result: Option<bool>,
    ) -> ExplicitCaptureCompletion {
        let identity = candidate.0;
        let current_press = self.validate_primary_press(
            identity.device,
            identity.window,
            &identity.domain,
            identity.press,
        );
        let Some(capture) = self.owners.get_mut(&identity.device) else {
            return ExplicitCaptureCompletion::Stale;
        };
        let pending_authority = matches!(&capture.explicit,
            ExplicitCaptureState::Acquiring {
                identity: current,
                stage: ExplicitCaptureAcquisition::NativeRequest,
            } if Arc::ptr_eq(current, &identity));
        let retiring = matches!(&capture.explicit,
            ExplicitCaptureState::Retiring {
                identity: current,
                stage: ExplicitCaptureRetirement::NativeRequest,
            } if Arc::ptr_eq(current, &identity));
        if !pending_authority && !retiring {
            return ExplicitCaptureCompletion::Stale;
        }
        if result == Some(false) {
            capture.clear_native_obligation();
            self.prune_finished_device(identity.device);
            return ExplicitCaptureCompletion::Failed;
        }
        if retiring || !current_press || result.is_none() {
            capture.explicit = ExplicitCaptureState::Retiring {
                identity,
                stage: ExplicitCaptureRetirement::NeedsRelease,
            };
            return ExplicitCaptureCompletion::Retiring;
        }

        // Native success is not a lease. Require a round trip started after this completion,
        // followed by the existing depth-checked FIFO drain. This per-row fence leaves the
        // original Primary identity valid; ordinary mutations still revoke the explicit attempt.
        capture.explicit = ExplicitCaptureState::Acquiring {
            identity,
            stage: ExplicitCaptureAcquisition::AwaitingFence(ExplicitCaptureFence::NeedsSync),
        };
        ExplicitCaptureCompletion::AwaitingFence
    }

    /// Tests settled effect ownership; fresh route observations still require a sample.
    pub(super) fn has_explicit_capture(
        &self,
        device: u16,
        window: u32,
        domain: &Arc<PointerPressConnectionDomain>,
        press: NonZeroU64,
    ) -> bool {
        if !self.validate_primary_press(device, window, domain, press) {
            return false;
        }
        self.owners.get(&device).is_some_and(|capture| {
            matches!(&capture.explicit, ExplicitCaptureState::Active(identity)
                if identity.matches_press(device, window, domain, press))
        })
    }

    /// Pins the existing active duty across a source-directed native refresh.
    pub(super) fn begin_explicit_capture_sample(
        &mut self,
        device: u16,
        window: u32,
        domain: &Arc<PointerPressConnectionDomain>,
        press: NonZeroU64,
    ) -> Option<ExplicitCaptureSample> {
        if self.grab_mutation_in_flight
            || !self.validate_primary_press(device, window, domain, press)
        {
            return None;
        }
        let capture = self.owners.get_mut(&device)?;
        let ExplicitCaptureState::Active(identity) = &capture.explicit else {
            return None;
        };
        if !identity.matches_press(device, window, domain, press) {
            return None;
        }
        let identity = Arc::clone(identity);
        capture.explicit = ExplicitCaptureState::Sampling {
            identity: Arc::clone(&identity),
            interval_clear: true,
        };
        Some(ExplicitCaptureSample(identity))
    }

    /// A refused refresh never releases the original grab. An uncertain result
    /// retains cleanup; retirement during dispatch cannot restore active proof.
    /// None rejects the entire query after interval invalidation or uncertainty.
    pub(super) fn complete_explicit_capture_sample(
        &mut self,
        sample: ExplicitCaptureSample,
        result: Option<bool>,
    ) -> Option<bool> {
        let identity = sample.0;
        let current_press = self.validate_primary_press(
            identity.device,
            identity.window,
            &identity.domain,
            identity.press,
        );
        let capture = self.owners.get_mut(&identity.device)?;
        let interval_clear = match &capture.explicit {
            ExplicitCaptureState::Sampling { identity: current, interval_clear }
                if Arc::ptr_eq(current, &identity) =>
            {
                Some(*interval_clear)
            },
            _ => None,
        };
        let retiring = matches!(&capture.explicit, ExplicitCaptureState::Retiring {
            identity: current, stage: ExplicitCaptureRetirement::NativeRequest,
        } if Arc::ptr_eq(current, &identity));
        if interval_clear.is_none() && !retiring {
            return None;
        }
        if retiring || !current_press || result.is_none() {
            capture.explicit = ExplicitCaptureState::Retiring {
                identity,
                stage: ExplicitCaptureRetirement::NeedsRelease,
            };
            return None;
        }
        capture.explicit = ExplicitCaptureState::Active(identity);
        interval_clear.filter(|clear| *clear).and(result)
    }

    /// Retire the exact original lease, even after Primary revocation, and claim known cleanup.
    /// None can mean its native result is still pending or another release already owns the duty.
    pub(super) fn release_explicit_capture(
        &mut self,
        device: u16,
        window: u32,
        domain: &Arc<PointerPressConnectionDomain>,
        press: NonZeroU64,
    ) -> Option<ExplicitCaptureRelease> {
        let capture = self.owners.get_mut(&device)?;
        let identity = match &capture.explicit {
            ExplicitCaptureState::Acquiring { identity, .. }
            | ExplicitCaptureState::Active(identity)
            | ExplicitCaptureState::Sampling { identity, .. }
            | ExplicitCaptureState::Retiring { identity, .. } => identity,
            ExplicitCaptureState::Unacquired | ExplicitCaptureState::Revoked => return None,
        };
        if !identity.matches_press(device, window, domain, press) {
            return None;
        }
        // Retirement only claims cleanup. Actual ungrab dispatch closes the shared FIFO fence
        // and spends any successor which arrived while the old native lease could still route it.
        capture.retire_explicit_capture();
        capture.claim_explicit_capture_release()
    }

    pub(super) fn has_explicit_capture_obligation(
        &self,
        device: u16,
        window: u32,
        domain: &Arc<PointerPressConnectionDomain>,
        press: NonZeroU64,
    ) -> bool {
        self.owners.get(&device).is_some_and(|capture| match &capture.explicit {
            ExplicitCaptureState::Acquiring { identity, .. }
            | ExplicitCaptureState::Active(identity)
            | ExplicitCaptureState::Sampling { identity, .. }
            | ExplicitCaptureState::Retiring { identity, .. } => {
                identity.matches_press(device, window, domain, press)
            },
            ExplicitCaptureState::Unacquired | ExplicitCaptureState::Revoked => false,
        })
    }

    pub(super) fn retire_explicit_capture_for_window(&mut self, window: u32) {
        for capture in self.owners.values_mut() {
            let matches = match &capture.explicit {
                ExplicitCaptureState::Acquiring { identity, .. }
                | ExplicitCaptureState::Active(identity)
                | ExplicitCaptureState::Sampling { identity, .. }
                | ExplicitCaptureState::Retiring { identity, .. } => identity.window == window,
                ExplicitCaptureState::Unacquired | ExplicitCaptureState::Revoked => false,
            };
            if matches {
                capture.revoke_explicit_capture();
            }
        }
    }

    /// Claim automatic cleanup after releases, mutations, removal, or late native success.
    /// This only collects work; native dispatch stays outside the capture mutex and sync.
    pub(super) fn claim_explicit_capture_releases(&mut self) -> Vec<ExplicitCaptureRelease> {
        if self.grab_mutation_in_flight {
            return Vec::new();
        }
        self.owners
            .values_mut()
            .filter_map(ImplicitPointerCapture::claim_explicit_capture_release)
            .collect()
    }

    /// Validate immediately before native dispatch, pinning this exact duty until completion.
    /// A queued stale release must never dispatch an ungrab against a successor lease.
    /// Success keeps the row pinned across unlocked dispatch until its result is reported.
    pub(super) fn begin_explicit_capture_release(
        &mut self,
        release: &ExplicitCaptureRelease,
    ) -> bool {
        if self.grab_mutation_in_flight || !Arc::ptr_eq(&self.domain, &release.identity.domain) {
            return false;
        }
        let Some(capture) = self.owners.get_mut(&release.identity.device) else {
            return false;
        };
        let ExplicitCaptureState::Retiring { identity, stage } = &mut capture.explicit else {
            return false;
        };
        let ExplicitCaptureRetirement::ReleaseQueued(claim) = stage else {
            return false;
        };
        if !Arc::ptr_eq(identity, &release.identity) || !Arc::ptr_eq(claim, &release.claim) {
            return false;
        }
        *stage = ExplicitCaptureRetirement::Releasing(Arc::clone(&release.claim));
        // Explicit capture replaced the original automatic grab. Ungrab cannot restore it,
        // even while Primary or Secondary is still held; only all-up can end that uncertainty.
        capture.invalidate();
        // Requests queued while the old explicit grab was active must not mint a new
        // implicit owner after native cleanup. Keep them quarantined through normal FIFO.
        self.close_fence();
        true
    }

    /// True means this exact duty was cleared after confirmed native release success.
    /// Failed dispatch/release, or queued cancellation, leaves it ready for a fresh claim.
    pub(super) fn complete_explicit_capture_release(
        &mut self,
        release: ExplicitCaptureRelease,
        succeeded: bool,
    ) -> bool {
        if !Arc::ptr_eq(&self.domain, &release.identity.domain) {
            return false;
        }
        let Some(capture) = self.owners.get_mut(&release.identity.device) else {
            return false;
        };
        let ExplicitCaptureState::Retiring { identity, stage } = &mut capture.explicit else {
            return false;
        };
        let (claim, dispatched) = match stage {
            ExplicitCaptureRetirement::ReleaseQueued(claim) => (claim, false),
            ExplicitCaptureRetirement::Releasing(claim) => (claim, true),
            ExplicitCaptureRetirement::NativeRequest | ExplicitCaptureRetirement::NeedsRelease => {
                return false;
            },
        };
        if !Arc::ptr_eq(identity, &release.identity)
            || !Arc::ptr_eq(claim, &release.claim)
            || (succeeded && !dispatched)
        {
            return false;
        }
        if !succeeded {
            *stage = ExplicitCaptureRetirement::NeedsRelease;
            if dispatched {
                self.close_fence();
            }
            return false;
        }
        capture.clear_native_obligation();
        self.prune_finished_device(release.identity.device);
        // An older sync may have returned while release dispatch was unlocked.
        self.close_fence();
        true
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

    fn staged_explicit_capture(
        captures: &mut ImplicitPointerCaptures,
        stage: u8,
    ) -> (Arc<PointerPressConnectionDomain>, NonZeroU64, ExplicitCaptureCandidate) {
        let (domain, press) = validated_primary(captures, DEVICE, OWNER, 101);
        let candidate = captures.begin_explicit_capture(DEVICE, OWNER, &domain, press).unwrap();
        let stale = ExplicitCaptureCandidate(Arc::clone(&candidate.0));
        // Stages cover the native request, NeedsSync, Syncing, Draining, and the active lease.
        if stage >= 1 {
            assert_eq!(
                captures.complete_explicit_capture(candidate, Some(true)),
                ExplicitCaptureCompletion::AwaitingFence,
            );
        }
        if stage >= 2 {
            assert!(captures.begin_sync(0));
        }
        if stage >= 3 {
            captures.finish_sync(true);
        }
        if stage >= 4 {
            captures.queue_drained(0);
        }
        assert!(captures.validate_primary_press(DEVICE, OWNER, &domain, press));
        assert_eq!(captures.has_explicit_capture(DEVICE, OWNER, &domain, press), stage == 4);
        (domain, press, stale)
    }

    #[test]
    fn explicit_capture_requires_the_exact_validated_original_press() {
        let mut captures = ImplicitPointerCaptures::default();
        let pending = begin_primary_validation(&mut captures, DEVICE, OWNER);
        let domain = Arc::clone(&captures.domain);
        let press = NonZeroU64::new(103).unwrap();
        let other_press = NonZeroU64::new(104).unwrap();
        let other_domain = Arc::new(PointerPressConnectionDomain);
        assert!(captures.begin_explicit_capture(DEVICE, OWNER, &domain, press).is_none());
        captures.complete_primary_press(pending, Some(press));
        for (device, window, expected_domain, expected_press) in [
            (DEVICE + 1, OWNER, &domain, press),
            (DEVICE, OTHER_WINDOW, &domain, press),
            (DEVICE, OWNER, &other_domain, press),
            (DEVICE, OWNER, &domain, other_press),
        ] {
            assert!(captures
                .begin_explicit_capture(device, window, expected_domain, expected_press)
                .is_none());
        }

        let candidate = captures.begin_explicit_capture(DEVICE, OWNER, &domain, press).unwrap();
        assert!(captures.begin_explicit_capture(DEVICE, OWNER, &domain, press).is_none());
        assert_eq!(
            captures.complete_explicit_capture(candidate, Some(true)),
            ExplicitCaptureCompletion::AwaitingFence,
        );
        assert!(captures.begin_sync(0));
        captures.finish_sync(true);
        captures.queue_drained(0);
        assert!(captures.has_explicit_capture(DEVICE, OWNER, &domain, press));
        assert!(!captures.has_explicit_capture(DEVICE + 1, OWNER, &domain, press));
        assert!(!captures.has_explicit_capture(DEVICE, OTHER_WINDOW, &domain, press));
        assert!(!captures.has_explicit_capture(DEVICE, OWNER, &other_domain, press));
        assert!(!captures.has_explicit_capture(DEVICE, OWNER, &domain, other_press));
        assert!(captures.begin_explicit_capture(DEVICE, OWNER, &domain, press).is_none());
    }

    #[test]
    fn explicit_capture_sampling_pins_only_the_exact_active_original_duty() {
        let mut captures = ImplicitPointerCaptures::default();
        let (domain, press, _) = staged_explicit_capture(&mut captures, 4);
        let other_domain = Arc::new(PointerPressConnectionDomain);
        for (device, window, expected_domain, expected_press) in [
            (DEVICE + 1, OWNER, &domain, press),
            (DEVICE, OTHER_WINDOW, &domain, press),
            (DEVICE, OWNER, &other_domain, press),
            (DEVICE, OWNER, &domain, NonZeroU64::new(102).unwrap()),
        ] {
            assert!(captures
                .begin_explicit_capture_sample(device, window, expected_domain, expected_press)
                .is_none());
        }
        let sample = captures.begin_explicit_capture_sample(DEVICE, OWNER, &domain, press).unwrap();
        assert_eq!(captures.explicit_capture_phase(DEVICE), ExplicitCapturePhase::Sampling);
        assert!(captures.begin_explicit_capture_sample(DEVICE, OWNER, &domain, press).is_none());
        assert!(!captures.has_explicit_capture(DEVICE, OWNER, &domain, press));
        assert!(captures.has_explicit_capture_obligation(DEVICE, OWNER, &domain, press));
        assert_eq!(captures.complete_explicit_capture_sample(sample, Some(true)), Some(true));
        assert!(captures.has_explicit_capture(DEVICE, OWNER, &domain, press));
    }

    #[test]
    fn refused_capture_sample_keeps_the_original_duty_and_allows_a_fresh_sample() {
        let mut captures = ImplicitPointerCaptures::default();
        let (domain, press, _) = staged_explicit_capture(&mut captures, 4);
        let sample = captures.begin_explicit_capture_sample(DEVICE, OWNER, &domain, press).unwrap();
        assert_eq!(captures.complete_explicit_capture_sample(sample, Some(false)), Some(false));
        assert!(captures.has_explicit_capture(DEVICE, OWNER, &domain, press));
        assert!(captures.claim_explicit_capture_releases().is_empty());
        let fresh = captures.begin_explicit_capture_sample(DEVICE, OWNER, &domain, press).unwrap();
        assert_eq!(captures.complete_explicit_capture_sample(fresh, Some(true)), Some(true));
        assert!(captures.has_explicit_capture(DEVICE, OWNER, &domain, press));
    }

    #[test]
    fn completed_native_expansion_rejects_the_old_sample_even_after_queue_drain() {
        let mut captures = ImplicitPointerCaptures::default();
        let (domain, press, _) = staged_explicit_capture(&mut captures, 4);
        let sample = captures.begin_explicit_capture_sample(DEVICE, OWNER, &domain, press).unwrap();
        // Production native-entry guards mark the existing row before expansion.
        // Returning to depth zero and an empty queue cannot erase this history.
        captures.invalidate_capture_sample_intervals();
        captures.queue_drained(0);
        assert!(captures.validate_primary_press(DEVICE, OWNER, &domain, press));
        assert_eq!(captures.complete_explicit_capture_sample(sample, Some(true)), None);
        assert!(captures.has_explicit_capture(DEVICE, OWNER, &domain, press));
        let fresh = captures.begin_explicit_capture_sample(DEVICE, OWNER, &domain, press).unwrap();
        assert_eq!(captures.complete_explicit_capture_sample(fresh, Some(true)), Some(true));
    }

    #[test]
    fn capture_sample_retirement_waits_for_the_native_result_and_never_restores_authority() {
        for retirement in 0..5 {
            for result in [Some(true), Some(false), None] {
                let mut captures = ImplicitPointerCaptures::default();
                let (domain, press, _) = staged_explicit_capture(&mut captures, 4);
                let sample =
                    captures.begin_explicit_capture_sample(DEVICE, OWNER, &domain, press).unwrap();
                match retirement {
                    0 => captures.invalidate(Some(OWNER)),
                    1 => {
                        captures.for_button_state(DEVICE, current_button_state(&[0b1000]));
                    },
                    2 => captures.remove_device(DEVICE),
                    3 => {
                        captures.reset_window(OWNER);
                    },
                    4 => assert!(captures
                        .release_explicit_capture(DEVICE, OWNER, &domain, press)
                        .is_none()),
                    _ => unreachable!(),
                }
                assert!(captures.claim_explicit_capture_releases().is_empty());
                captures.queue_drained(0);
                assert!(!captures.has_explicit_capture(DEVICE, OWNER, &domain, press));
                assert_eq!(captures.complete_explicit_capture_sample(sample, result), None);
                assert_eq!(captures.explicit_capture_phase(DEVICE), ExplicitCapturePhase::Retiring);
                let [release]: [_; 1] = captures
                    .claim_explicit_capture_releases()
                    .try_into()
                    .expect("normal polling must automatically claim the retired duty");
                assert!(captures.claim_explicit_capture_releases().is_empty());
                assert!(captures.begin_explicit_capture_release(&release));
                assert!(captures.complete_explicit_capture_release(release, true));
                assert!(!captures.has_explicit_capture_obligation(DEVICE, OWNER, &domain, press));
            }
        }
    }

    #[test]
    fn indeterminate_capture_sample_retains_cleanup_without_new_acquisition() {
        let mut captures = ImplicitPointerCaptures::default();
        let (domain, press, _) = staged_explicit_capture(&mut captures, 4);
        let sample = captures.begin_explicit_capture_sample(DEVICE, OWNER, &domain, press).unwrap();
        assert_eq!(captures.complete_explicit_capture_sample(sample, None), None);
        assert_eq!(captures.explicit_capture_phase(DEVICE), ExplicitCapturePhase::Retiring);
        assert!(captures.begin_explicit_capture(DEVICE, OWNER, &domain, press).is_none());
        let release = captures.release_explicit_capture(DEVICE, OWNER, &domain, press).unwrap();
        assert!(captures.begin_explicit_capture_release(&release));
        assert!(captures.complete_explicit_capture_release(release, true));
        assert!(!captures.has_explicit_capture_obligation(DEVICE, OWNER, &domain, press));
    }

    #[test]
    fn late_capture_sample_cannot_authorize_a_successor_primary_or_erase_old_cleanup() {
        for result in [Some(true), Some(false), None] {
            let mut captures = ImplicitPointerCaptures::default();
            let (domain, press, _) = staged_explicit_capture(&mut captures, 4);
            let sample =
                captures.begin_explicit_capture_sample(DEVICE, OWNER, &domain, press).unwrap();
            captures.remove_device(DEVICE);
            captures.for_button_state(DEVICE, current_button_state(&[0]));
            let (next_domain, next_press) =
                validated_primary(&mut captures, DEVICE, OTHER_WINDOW, 202);
            assert!(captures
                .begin_explicit_capture(DEVICE, OTHER_WINDOW, &next_domain, next_press)
                .is_none());
            assert_eq!(captures.complete_explicit_capture_sample(sample, result), None);
            assert!(captures.has_explicit_capture_obligation(DEVICE, OWNER, &domain, press));
            assert!(!captures.has_explicit_capture(DEVICE, OTHER_WINDOW, &next_domain, next_press));
            let release = captures.release_explicit_capture(DEVICE, OWNER, &domain, press).unwrap();
            let late = ExplicitCaptureRelease {
                identity: Arc::clone(&release.identity),
                claim: Arc::clone(&release.claim),
            };
            assert!(captures.begin_explicit_capture_release(&release));
            assert!(!captures.complete_explicit_capture_release(release, false));
            let retry = captures.release_explicit_capture(DEVICE, OWNER, &domain, press).unwrap();
            assert!(!captures.begin_explicit_capture_release(&late));
            assert!(!captures.complete_explicit_capture_release(late, true));
            assert!(captures.begin_explicit_capture_release(&retry));
            assert!(captures.complete_explicit_capture_release(retry, true));
            assert!(!captures.validate_primary_press(
                DEVICE,
                OTHER_WINDOW,
                &next_domain,
                next_press
            ));
            assert!(captures.release_explicit_capture(DEVICE, OWNER, &domain, press).is_none());
        }
    }

    #[test]
    fn capture_sampling_blocks_ordinary_takeover_through_exact_cleanup() {
        let mut state = ImplicitPointerCaptures::default();
        let (domain, press, _) = staged_explicit_capture(&mut state, 4);
        let sample = state.begin_explicit_capture_sample(DEVICE, OWNER, &domain, press).unwrap();
        let captures = Mutex::new(state);
        let depth = AtomicU32::new(0);
        assert!(PointerGrabMutationGuard::try_new(&captures, &depth).is_none());
        assert!(captures
            .lock()
            .unwrap()
            .release_explicit_capture(DEVICE, OWNER, &domain, press)
            .is_none());
        assert!(PointerGrabMutationGuard::try_new(&captures, &depth).is_none());
        assert_eq!(
            captures.lock().unwrap().complete_explicit_capture_sample(sample, Some(true)),
            None
        );
        assert!(PointerGrabMutationGuard::try_new(&captures, &depth).is_none());
        let release = captures
            .lock()
            .unwrap()
            .release_explicit_capture(DEVICE, OWNER, &domain, press)
            .unwrap();
        assert!(captures.lock().unwrap().begin_explicit_capture_release(&release));
        assert!(captures.lock().unwrap().complete_explicit_capture_release(release, true));
        assert!(PointerGrabMutationGuard::try_new(&captures, &depth).is_some());
    }

    #[test]
    fn explicit_capture_waits_for_sync_and_normal_fifo_drain() {
        let mut captures = ImplicitPointerCaptures::default();
        let (domain, press, stale) = staged_explicit_capture(&mut captures, 1);
        captures.queue_drained(0);
        assert!(!captures.has_explicit_capture(DEVICE, OWNER, &domain, press));
        assert!(!captures.begin_sync(1));
        assert!(captures.begin_sync(0));
        assert!(!captures.begin_sync(0));
        captures.queue_drained(0);
        assert!(!captures.has_explicit_capture(DEVICE, OWNER, &domain, press));
        assert!(captures.validate_primary_press(DEVICE, OWNER, &domain, press));
        captures.finish_sync(true);
        assert!(!captures.has_explicit_capture(DEVICE, OWNER, &domain, press));
        assert!(matches!(captures.owners[&DEVICE].explicit, ExplicitCaptureState::Acquiring {
            stage: ExplicitCaptureAcquisition::AwaitingFence(ExplicitCaptureFence::Draining),
            ..
        }));
        assert_eq!(captures.primary_press(DEVICE, OWNER).unwrap().1, press);
        captures.queue_drained(1);
        assert!(!captures.has_explicit_capture(DEVICE, OWNER, &domain, press));
        assert_eq!(
            captures.complete_explicit_capture(stale, Some(true)),
            ExplicitCaptureCompletion::Stale,
        );
        captures.queue_drained(0);
        assert!(captures.has_explicit_capture(DEVICE, OWNER, &domain, press));
        assert!(captures.validate_primary_press(DEVICE, OWNER, &domain, press));
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
            assert!(captures.has_explicit_capture(DEVICE, OWNER, &domain, press));
        }
    }

    #[test]
    fn explicit_capture_failure_is_terminal_for_the_original_press() {
        let mut captures = ImplicitPointerCaptures::default();
        let (domain, press, candidate) = staged_explicit_capture(&mut captures, 0);
        let duplicate = ExplicitCaptureCandidate(Arc::clone(&candidate.0));
        assert_eq!(
            captures.complete_explicit_capture(candidate, Some(false)),
            ExplicitCaptureCompletion::Failed,
        );
        assert_eq!(
            captures.complete_explicit_capture(duplicate, Some(true)),
            ExplicitCaptureCompletion::Stale,
        );
        assert!(captures.validate_primary_press(DEVICE, OWNER, &domain, press));
        assert!(!captures.has_explicit_capture(DEVICE, OWNER, &domain, press));
        assert!(captures.begin_explicit_capture(DEVICE, OWNER, &domain, press).is_none());
        assert!(!captures.begin_sync(0));
        captures.queue_drained(0);
        captures.after_button_event(
            DEVICE,
            OWNER,
            true,
            true,
            button_state_transition(&[0b0000_0010], 1, true),
        );
        assert!(captures.begin_explicit_capture(DEVICE, OWNER, &domain, press).is_none());
        assert!(captures.validate_primary_press(DEVICE, OWNER, &domain, press));

        captures.for_button_state(DEVICE, current_button_state(&[0]));
        let (fresh_domain, fresh_press) = validated_primary(&mut captures, DEVICE, OWNER, 105);
        assert!(captures
            .begin_explicit_capture(DEVICE, OWNER, &fresh_domain, fresh_press)
            .is_some());
    }

    #[test]
    fn explicit_status_polling_is_read_only_and_never_rearms_a_failed_press() {
        let mut captures = ImplicitPointerCaptures::default();
        let (domain, press) = validated_primary(&mut captures, DEVICE, OWNER, 115);
        assert_eq!(captures.explicit_capture_phase(DEVICE), ExplicitCapturePhase::Unacquired);
        for _ in 0..2 {
            assert_eq!(
                captures.explicit_capture_status(DEVICE, OWNER, &domain, press),
                ExplicitCaptureStatus::Pending,
            );
            assert_eq!(captures.explicit_capture_phase(DEVICE), ExplicitCapturePhase::Unacquired);
        }
        let candidate = captures.begin_explicit_capture(DEVICE, OWNER, &domain, press).unwrap();
        assert_eq!(captures.explicit_capture_phase(DEVICE), ExplicitCapturePhase::NativeRequest);
        assert_eq!(
            captures.complete_explicit_capture(candidate, Some(false)),
            ExplicitCaptureCompletion::Failed,
        );
        for _ in 0..2 {
            assert_eq!(captures.explicit_capture_phase(DEVICE), ExplicitCapturePhase::Revoked);
            assert_eq!(
                captures.explicit_capture_status(DEVICE, OWNER, &domain, press),
                ExplicitCaptureStatus::Unavailable,
            );
            assert!(captures.begin_explicit_capture(DEVICE, OWNER, &domain, press).is_none());
        }
        captures.remove_device(DEVICE);
        assert_eq!(captures.explicit_capture_phase(DEVICE), ExplicitCapturePhase::Unacquired);
        assert_eq!(
            captures.explicit_capture_status(DEVICE, OWNER, &domain, press),
            ExplicitCaptureStatus::Unavailable,
        );
    }

    #[test]
    fn explicit_status_distinguishes_pending_fenced_effect_and_retirement_exactly() {
        for stage in 0..5 {
            let mut captures = ImplicitPointerCaptures::default();
            let (domain, press, _) = staged_explicit_capture(&mut captures, stage);
            let phase = match stage {
                0 => ExplicitCapturePhase::NativeRequest,
                1..=3 => ExplicitCapturePhase::AwaitingFence,
                4 => ExplicitCapturePhase::Active,
                _ => unreachable!(),
            };
            assert_eq!(captures.explicit_capture_phase(DEVICE), phase);
            assert_eq!(
                captures.explicit_capture_status(DEVICE, OWNER, &domain, press),
                if stage == 4 {
                    ExplicitCaptureStatus::Acquired
                } else {
                    ExplicitCaptureStatus::Pending
                },
            );
            let foreign_domain = Arc::new(PointerPressConnectionDomain);
            for (device, window, expected_domain, expected_press) in [
                (DEVICE + 1, OWNER, &domain, press),
                (DEVICE, OTHER_WINDOW, &domain, press),
                (DEVICE, OWNER, &foreign_domain, press),
                (DEVICE, OWNER, &domain, NonZeroU64::new(116).unwrap()),
            ] {
                assert_eq!(
                    captures.explicit_capture_status(
                        device,
                        window,
                        expected_domain,
                        expected_press
                    ),
                    ExplicitCaptureStatus::Unavailable,
                );
            }
            captures.for_button_state(DEVICE, current_button_state(&[0b0000_1000]));
            assert_eq!(captures.explicit_capture_phase(DEVICE), ExplicitCapturePhase::Retiring);
            assert_eq!(
                captures.explicit_capture_status(DEVICE, OWNER, &domain, press),
                ExplicitCaptureStatus::Unavailable,
            );
        }
    }

    #[test]
    fn indeterminate_native_result_only_retires_and_blocks_successor_until_exact_cleanup() {
        for removed in [false, true] {
            let mut captures = ImplicitPointerCaptures::default();
            let (domain, old_press, candidate) = staged_explicit_capture(&mut captures, 0);
            let duplicate = ExplicitCaptureCandidate(Arc::clone(&candidate.0));
            if removed {
                captures.remove_device(DEVICE);
            }
            assert_eq!(
                captures.complete_explicit_capture(candidate, None),
                ExplicitCaptureCompletion::Retiring,
            );
            captures.queue_drained(0);
            assert!(!captures.begin_sync(0));
            assert_eq!(captures.explicit_capture_phase(DEVICE), ExplicitCapturePhase::Retiring);
            assert_eq!(
                captures.explicit_capture_status(DEVICE, OWNER, &domain, old_press),
                ExplicitCaptureStatus::Unavailable,
            );
            assert_eq!(
                captures.complete_explicit_capture(duplicate, Some(false)),
                ExplicitCaptureCompletion::Stale,
            );
            assert!(captures.owners[&DEVICE].has_native_obligation());
            captures.for_button_state(DEVICE, current_button_state(&[0]));
            assert!(captures.owners[&DEVICE].primary.is_none());
            let (_, next_press) = validated_primary(&mut captures, DEVICE, OTHER_WINDOW, 117);
            assert_eq!(
                captures.explicit_capture_status(DEVICE, OTHER_WINDOW, &domain, next_press),
                ExplicitCaptureStatus::Pending,
            );
            assert!(captures
                .begin_explicit_capture(DEVICE, OTHER_WINDOW, &domain, next_press)
                .is_none());
            let release =
                captures.release_explicit_capture(DEVICE, OWNER, &domain, old_press).unwrap();
            assert!(captures.begin_explicit_capture_release(&release));
            assert!(captures.complete_explicit_capture_release(release, true));
            assert_eq!(captures.explicit_capture_phase(DEVICE), ExplicitCapturePhase::Revoked);
            assert!(!captures.validate_primary_press(DEVICE, OTHER_WINDOW, &domain, next_press));
            assert!(captures.begin_sync(0));
            captures.finish_sync(true);
            captures.queue_drained(0);
            assert!(captures
                .begin_explicit_capture(DEVICE, OTHER_WINDOW, &domain, next_press)
                .is_none());
            captures.for_button_state(DEVICE, current_button_state(&[0]));
            let (_, next_press) = validated_primary(&mut captures, DEVICE, OTHER_WINDOW, 118);
            assert!(captures
                .begin_explicit_capture(DEVICE, OTHER_WINDOW, &domain, next_press)
                .is_some());
        }
    }

    #[test]
    fn primary_release_revokes_every_explicit_capture_phase_with_secondary_still_down() {
        for stage in 0..5 {
            for observed_release in [false, true] {
                let mut captures = ImplicitPointerCaptures::default();
                let (domain, press, stale) = staged_explicit_capture(&mut captures, stage);
                let route = if observed_release {
                    captures.for_button_state(DEVICE, current_button_state(&[0b0000_1000]))
                } else {
                    captures.after_button_event(
                        DEVICE,
                        OWNER,
                        true,
                        false,
                        button_state_transition(&[0b0000_1010], 1, false),
                    )
                };
                assert_eq!(route, NativePointerRoute::Window(OWNER));
                assert!(matches!(
                    captures.owners[&DEVICE].explicit,
                    ExplicitCaptureState::Retiring { .. }
                ));
                assert_eq!(
                    captures.complete_explicit_capture(stale, Some(true)),
                    if stage == 0 {
                        ExplicitCaptureCompletion::Retiring
                    } else {
                        ExplicitCaptureCompletion::Stale
                    },
                );
                if captures.sync_in_flight {
                    captures.finish_sync(true);
                }
                captures.queue_drained(0);
                assert!(!captures.has_explicit_capture(DEVICE, OWNER, &domain, press));
                assert!(!captures.validate_primary_press(DEVICE, OWNER, &domain, press));
                assert_eq!(
                    captures.for_button_state(DEVICE, current_button_state(&[0b0000_1010])),
                    NativePointerRoute::Window(OWNER)
                );
                assert!(captures.begin_explicit_capture(DEVICE, OWNER, &domain, press).is_none());

                // The same implicit owner can carry a later, distinct Primary edge.
                captures.after_button_event(
                    DEVICE,
                    OWNER,
                    true,
                    true,
                    button_state_transition(&[0b0000_1000], 1, true),
                );
                let fresh = captures.primary_press_candidate(DEVICE, OWNER).unwrap();
                let fresh_press = NonZeroU64::new(106).unwrap();
                captures.complete_primary_press(fresh, Some(fresh_press));
                assert!(captures
                    .begin_explicit_capture(DEVICE, OWNER, &domain, fresh_press)
                    .is_none());
                assert!(captures.validate_primary_press(DEVICE, OWNER, &domain, fresh_press));
                let release =
                    captures.release_explicit_capture(DEVICE, OWNER, &domain, press).unwrap();
                assert!(captures.begin_explicit_capture_release(&release));
                assert!(captures.complete_explicit_capture_release(release, true));
                assert!(!captures.validate_primary_press(DEVICE, OWNER, &domain, fresh_press));
                assert!(captures.begin_sync(0));
                captures.finish_sync(true);
                captures.queue_drained(0);
                assert!(captures
                    .begin_explicit_capture(DEVICE, OWNER, &domain, fresh_press)
                    .is_none());
                // Explicit ungrab loses even the implicit owner. Secondary keeps that stream
                // uncertain until all-up; another Primary edge alone cannot restore it.
                captures.after_button_event(
                    DEVICE,
                    OWNER,
                    true,
                    false,
                    button_state_transition(&[0b0000_1010], 1, false),
                );
                captures.after_button_event(
                    DEVICE,
                    OWNER,
                    true,
                    true,
                    button_state_transition(&[0b0000_1000], 1, true),
                );
                assert!(captures.primary_press_candidate(DEVICE, OWNER).is_none());
                assert_eq!(
                    captures.for_button_state(DEVICE, current_button_state(&[0b1010])),
                    NativePointerRoute::Unknown,
                );
                captures.for_button_state(DEVICE, current_button_state(&[0]));
                let (_, fresh_press) = validated_primary(&mut captures, DEVICE, OWNER, 119);
                assert!(captures
                    .begin_explicit_capture(DEVICE, OWNER, &domain, fresh_press)
                    .is_some());
            }
        }
    }

    #[test]
    fn original_press_releases_every_explicit_capture_phase_exactly() {
        for stage in 0..5 {
            let mut captures = ImplicitPointerCaptures::default();
            let (domain, press, stale) = staged_explicit_capture(&mut captures, stage);
            let other_domain = Arc::new(PointerPressConnectionDomain);
            assert!(captures.release_explicit_capture(DEVICE + 1, OWNER, &domain, press).is_none());
            assert!(captures
                .release_explicit_capture(DEVICE, OTHER_WINDOW, &domain, press)
                .is_none());
            assert!(captures
                .release_explicit_capture(DEVICE, OWNER, &other_domain, press)
                .is_none());
            assert!(captures
                .release_explicit_capture(DEVICE, OWNER, &domain, NonZeroU64::new(107).unwrap())
                .is_none());
            let release = captures.release_explicit_capture(DEVICE, OWNER, &domain, press);
            let release = if stage == 0 {
                assert!(release.is_none(), "a pending native result cannot be ungrabbed early");
                assert_eq!(
                    captures.complete_explicit_capture(stale, Some(true)),
                    ExplicitCaptureCompletion::Retiring,
                );
                captures.release_explicit_capture(DEVICE, OWNER, &domain, press).unwrap()
            } else {
                assert_eq!(
                    captures.complete_explicit_capture(stale, Some(true)),
                    ExplicitCaptureCompletion::Stale,
                );
                release.unwrap()
            };
            assert!(captures.release_explicit_capture(DEVICE, OWNER, &domain, press).is_none());
            if captures.sync_in_flight {
                captures.finish_sync(true);
            }
            captures.queue_drained(0);
            assert!(captures.validate_primary_press(DEVICE, OWNER, &domain, press));
            assert!(!captures.has_explicit_capture(DEVICE, OWNER, &domain, press));
            assert!(captures.begin_explicit_capture(DEVICE, OWNER, &domain, press).is_none());
            assert!(captures.begin_explicit_capture_release(&release));
            assert!(!captures.begin_explicit_capture_release(&release));
            assert!(captures.complete_explicit_capture_release(release, true));
            assert!(!captures.validate_primary_press(DEVICE, OWNER, &domain, press));
            assert!(captures.begin_explicit_capture(DEVICE, OWNER, &domain, press).is_none());
            assert!(captures.begin_sync(0));
            captures.finish_sync(true);
            captures.queue_drained(0);
            assert!(!captures.validate_primary_press(DEVICE, OWNER, &domain, press));
            assert!(captures.begin_explicit_capture(DEVICE, OWNER, &domain, press).is_none());
        }
    }

    #[test]
    fn mutation_revokes_every_explicit_capture_phase_before_reopening() {
        for stage in 0..5 {
            for mutation in 0..6 {
                let mut captures = ImplicitPointerCaptures::default();
                let (domain, press, stale) = staged_explicit_capture(&mut captures, stage);
                match mutation {
                    0 => captures.invalidate(None),
                    1 => captures.invalidate(Some(OWNER)),
                    2 => captures.invalidate(Some(OTHER_WINDOW)),
                    3 => assert_eq!(captures.reset_device(DEVICE), Some(OWNER)),
                    4 => assert_eq!(captures.reset_window(OWNER), [DEVICE]),
                    5 => captures.close_fence(),
                    _ => unreachable!(),
                }
                assert!(matches!(
                    captures.owners[&DEVICE].explicit,
                    ExplicitCaptureState::Retiring { .. }
                ));
                assert_eq!(
                    captures.complete_explicit_capture(stale, Some(true)),
                    if stage == 0 {
                        ExplicitCaptureCompletion::Retiring
                    } else {
                        ExplicitCaptureCompletion::Stale
                    },
                );
                if captures.sync_in_flight {
                    assert!(!captures.begin_sync(0));
                    captures.finish_sync(true);
                }
                captures.queue_drained(0);
                assert!(!captures.has_explicit_capture(DEVICE, OWNER, &domain, press));
                assert!(captures.begin_sync(0));
                captures.finish_sync(true);
                captures.queue_drained(0);
                assert!(!captures.has_explicit_capture(DEVICE, OWNER, &domain, press));
                assert!(captures.begin_explicit_capture(DEVICE, OWNER, &domain, press).is_none());
                assert_eq!(
                    captures.validate_primary_press(DEVICE, OWNER, &domain, press),
                    mutation == 2 || mutation == 5,
                );
            }
        }
    }

    #[test]
    fn replacement_owner_records_primary_but_waits_for_old_native_cleanup() {
        for stage in [0, 4] {
            for replacement in 0..4 {
                let mut captures = ImplicitPointerCaptures::default();
                let (domain, press, predecessor) = staged_explicit_capture(&mut captures, stage);
                let duplicate = ExplicitCaptureCandidate(Arc::clone(&predecessor.0));
                match replacement {
                    0 => captures.remove_device(DEVICE),
                    1 => {
                        captures.for_button_state(DEVICE, None);
                    },
                    2 => {
                        captures.after_button_event(DEVICE, OWNER, true, true, None);
                    },
                    3 => {
                        captures.after_button_event(
                            DEVICE,
                            OTHER_WINDOW,
                            true,
                            true,
                            button_state_transition(&[0], 1, true),
                        );
                    },
                    _ => unreachable!(),
                }
                assert!(!captures.has_explicit_capture(DEVICE, OWNER, &domain, press));
                captures.for_button_state(DEVICE, current_button_state(&[0]));
                let (fresh_domain, fresh_press) =
                    validated_primary(&mut captures, DEVICE, OTHER_WINDOW, 108);
                assert!(captures
                    .begin_explicit_capture(DEVICE, OTHER_WINDOW, &fresh_domain, fresh_press)
                    .is_none());
                assert!(captures.validate_primary_press(
                    DEVICE,
                    OTHER_WINDOW,
                    &fresh_domain,
                    fresh_press
                ));
                assert_eq!(
                    captures.complete_explicit_capture(predecessor, Some(true)),
                    if stage == 0 {
                        ExplicitCaptureCompletion::Retiring
                    } else {
                        ExplicitCaptureCompletion::Stale
                    },
                );
                let release =
                    captures.release_explicit_capture(DEVICE, OWNER, &domain, press).unwrap();
                assert!(captures.begin_explicit_capture_release(&release));
                assert!(captures.complete_explicit_capture_release(release, true));
                assert!(!captures.validate_primary_press(
                    DEVICE,
                    OTHER_WINDOW,
                    &fresh_domain,
                    fresh_press
                ));
                assert!(captures.begin_sync(0));
                captures.finish_sync(true);
                captures.queue_drained(0);
                assert!(captures
                    .begin_explicit_capture(DEVICE, OTHER_WINDOW, &fresh_domain, fresh_press)
                    .is_none());
                captures.for_button_state(DEVICE, current_button_state(&[0]));
                let (fresh_domain, fresh_press) =
                    validated_primary(&mut captures, DEVICE, OTHER_WINDOW, 120);
                let successor = captures
                    .begin_explicit_capture(DEVICE, OTHER_WINDOW, &fresh_domain, fresh_press)
                    .unwrap();
                assert!(!Arc::ptr_eq(&duplicate.0, &successor.0));
                assert_eq!(
                    captures.complete_explicit_capture(duplicate, Some(true)),
                    ExplicitCaptureCompletion::Stale,
                );
                assert_eq!(
                    captures.complete_explicit_capture(successor, Some(true)),
                    ExplicitCaptureCompletion::AwaitingFence,
                );
                assert!(captures.begin_sync(0));
                captures.finish_sync(true);
                captures.queue_drained(0);
                assert!(captures.has_explicit_capture(
                    DEVICE,
                    OTHER_WINDOW,
                    &fresh_domain,
                    fresh_press,
                ));
                assert!(captures.release_explicit_capture(DEVICE, OWNER, &domain, press).is_none());
            }
        }
    }

    #[test]
    fn ended_or_removed_stream_keeps_obligations_without_a_live_primary() {
        for stage in 0..5 {
            for end in 0..3 {
                let mut captures = ImplicitPointerCaptures::default();
                let (domain, press, candidate) = staged_explicit_capture(&mut captures, stage);
                match end {
                    0 => assert_eq!(
                        captures.for_button_state(DEVICE, current_button_state(&[0])),
                        NativePointerRoute::None,
                    ),
                    1 => assert_eq!(
                        captures.after_button_event(
                            DEVICE,
                            OWNER,
                            true,
                            false,
                            button_state_transition(&[0b0000_0010], 1, false),
                        ),
                        NativePointerRoute::None,
                    ),
                    2 => captures.remove_device(DEVICE),
                    _ => unreachable!(),
                }
                assert_eq!(captures.owners[&DEVICE].owner, ImplicitCaptureOwner::None);
                assert!(captures.owners[&DEVICE].primary.is_none());
                assert!(captures.owners[&DEVICE].has_native_obligation());
                assert!(!captures.validate_primary_press(DEVICE, OWNER, &domain, press));
                assert_eq!(captures.reset_device(DEVICE), None);
                assert!(captures.reset_window(OWNER).is_empty());
                assert_eq!(captures.owners[&DEVICE].owner, ImplicitCaptureOwner::None);
                if stage == 0 {
                    assert_eq!(
                        captures.complete_explicit_capture(candidate, Some(false)),
                        ExplicitCaptureCompletion::Failed,
                    );
                } else {
                    let release =
                        captures.release_explicit_capture(DEVICE, OWNER, &domain, press).unwrap();
                    assert!(captures.begin_explicit_capture_release(&release));
                    assert!(captures.complete_explicit_capture_release(release, true));
                }
                assert!(!captures.owners.contains_key(&DEVICE));
                assert!(captures.claim_explicit_capture_releases().is_empty());
            }
        }
    }

    #[test]
    fn late_native_failure_unblocks_successor_without_an_ungrab() {
        let mut captures = ImplicitPointerCaptures::default();
        let (_, _, predecessor) = staged_explicit_capture(&mut captures, 0);
        captures.remove_device(DEVICE);
        let (domain, press) = validated_primary(&mut captures, DEVICE, OTHER_WINDOW, 113);
        assert!(captures.begin_explicit_capture(DEVICE, OTHER_WINDOW, &domain, press).is_none());
        assert_eq!(
            captures.complete_explicit_capture(predecessor, Some(false)),
            ExplicitCaptureCompletion::Failed,
        );
        assert!(captures.claim_explicit_capture_releases().is_empty());
        assert!(captures.validate_primary_press(DEVICE, OTHER_WINDOW, &domain, press));
        assert!(captures.begin_explicit_capture(DEVICE, OTHER_WINDOW, &domain, press).is_some());
    }

    #[test]
    fn failed_cleanup_retains_duty_and_stale_claim_cannot_ungrab_successor() {
        let mut captures = ImplicitPointerCaptures::default();
        let (domain, old_press, _) = staged_explicit_capture(&mut captures, 4);
        captures.for_button_state(DEVICE, current_button_state(&[0]));
        let (_, next_press) = validated_primary(&mut captures, DEVICE, OTHER_WINDOW, 114);
        let release = captures.release_explicit_capture(DEVICE, OWNER, &domain, old_press).unwrap();
        let stale = ExplicitCaptureRelease {
            identity: Arc::clone(&release.identity),
            claim: Arc::clone(&release.claim),
        };
        let undispatched = ExplicitCaptureRelease {
            identity: Arc::clone(&release.identity),
            claim: Arc::clone(&release.claim),
        };
        assert!(!captures.complete_explicit_capture_release(undispatched, true));
        assert!(captures.begin_explicit_capture_release(&release));
        assert!(captures
            .begin_explicit_capture(DEVICE, OTHER_WINDOW, &domain, next_press)
            .is_none());
        assert!(!captures.complete_explicit_capture_release(release, false));
        assert!(captures.owners[&DEVICE].has_native_obligation());
        assert!(captures
            .begin_explicit_capture(DEVICE, OTHER_WINDOW, &domain, next_press)
            .is_none());
        assert!(!captures.begin_explicit_capture_release(&stale));

        let retry = captures.release_explicit_capture(DEVICE, OWNER, &domain, old_press).unwrap();
        assert!(!Arc::ptr_eq(&stale.claim, &retry.claim));
        assert!(!captures.begin_explicit_capture_release(&stale));
        let queued_old_release = ExplicitCaptureRelease {
            identity: Arc::clone(&retry.identity),
            claim: Arc::clone(&retry.claim),
        };
        assert!(captures.begin_explicit_capture_release(&retry));
        assert!(captures
            .begin_explicit_capture(DEVICE, OTHER_WINDOW, &domain, next_press)
            .is_none());
        assert!(!captures.complete_explicit_capture_release(stale, true));
        assert!(captures.complete_explicit_capture_release(retry, true));
        assert!(!captures.validate_primary_press(DEVICE, OTHER_WINDOW, &domain, next_press));
        assert!(captures.begin_sync(0));
        captures.finish_sync(true);
        captures.queue_drained(0);
        assert!(captures
            .begin_explicit_capture(DEVICE, OTHER_WINDOW, &domain, next_press)
            .is_none());
        captures.for_button_state(DEVICE, current_button_state(&[0]));
        let (_, next_press) = validated_primary(&mut captures, DEVICE, OTHER_WINDOW, 121);
        let successor =
            captures.begin_explicit_capture(DEVICE, OTHER_WINDOW, &domain, next_press).unwrap();
        assert_eq!(
            captures.complete_explicit_capture(successor, Some(true)),
            ExplicitCaptureCompletion::AwaitingFence,
        );
        assert!(captures.begin_sync(0));
        captures.finish_sync(true);
        captures.queue_drained(0);
        assert!(!captures.begin_explicit_capture_release(&queued_old_release));
        assert!(!captures.complete_explicit_capture_release(queued_old_release, true));
        assert!(captures.has_explicit_capture(DEVICE, OTHER_WINDOW, &domain, next_press));
    }

    #[test]
    fn cleanup_dispatch_remains_pinned_across_mutation_reset_and_removal() {
        for revoke in 0..5 {
            let mut captures = ImplicitPointerCaptures::default();
            let (domain, press, _) = staged_explicit_capture(&mut captures, 4);
            let release = captures.release_explicit_capture(DEVICE, OWNER, &domain, press).unwrap();
            assert!(captures.begin_explicit_capture_release(&release));
            match revoke {
                0 => captures.invalidate(None),
                1 => assert_eq!(captures.reset_device(DEVICE), None),
                2 => assert!(captures.reset_window(OWNER).is_empty()),
                3 => captures.remove_device(DEVICE),
                4 => {
                    captures.for_button_state(DEVICE, current_button_state(&[0]));
                },
                _ => unreachable!(),
            }
            assert!(captures.owners[&DEVICE].primary.is_none());
            assert!(captures.claim_explicit_capture_releases().is_empty());
            assert!(!captures.begin_explicit_capture_release(&release));
            assert!(!captures.complete_explicit_capture_release(release, false));
            let mut retries = captures.claim_explicit_capture_releases();
            assert_eq!(retries.len(), 1);
            let retry = retries.pop().unwrap();
            assert!(captures.begin_explicit_capture_release(&retry));
            assert!(captures.complete_explicit_capture_release(retry, true));
        }
    }

    #[test]
    fn automatic_cleanup_claim_is_exclusive_cancellable_and_connection_bound() {
        let mut captures = ImplicitPointerCaptures::default();
        let (..) = staged_explicit_capture(&mut captures, 4);
        captures.invalidate(None);
        let mut releases = captures.claim_explicit_capture_releases();
        assert_eq!(releases.len(), 1);
        let release = releases.pop().unwrap();
        assert_eq!(release.device(), DEVICE);
        assert!(captures.claim_explicit_capture_releases().is_empty());
        let foreign = ExplicitCaptureRelease {
            identity: Arc::clone(&release.identity),
            claim: Arc::clone(&release.claim),
        };
        let mut other_connection = ImplicitPointerCaptures::default();
        assert!(!other_connection.begin_explicit_capture_release(&foreign));
        assert!(!other_connection.complete_explicit_capture_release(foreign, true));
        assert!(!captures.complete_explicit_capture_release(release, false));
        let mut retries = captures.claim_explicit_capture_releases();
        assert_eq!(retries.len(), 1);
        let retry = retries.pop().unwrap();
        assert!(captures.begin_explicit_capture_release(&retry));
        assert!(captures.complete_explicit_capture_release(retry, true));
        assert!(captures.claim_explicit_capture_releases().is_empty());
    }

    #[test]
    fn acquisition_completed_during_sync_requires_a_later_round_trip() {
        let mut captures = ImplicitPointerCaptures::default();
        let (first_domain, first_press, _) = staged_explicit_capture(&mut captures, 2);
        let second_device = DEVICE + 1;
        let (second_domain, second_press) =
            validated_primary(&mut captures, second_device, OTHER_WINDOW, 109);
        let second = captures
            .begin_explicit_capture(second_device, OTHER_WINDOW, &second_domain, second_press)
            .unwrap();
        assert_eq!(
            captures.complete_explicit_capture(second, Some(true)),
            ExplicitCaptureCompletion::AwaitingFence,
        );
        assert!(!captures.begin_sync(0));
        captures.finish_sync(true);
        captures.queue_drained(0);
        assert!(!captures.has_explicit_capture(DEVICE, OWNER, &first_domain, first_press));
        assert!(!captures.has_explicit_capture(
            second_device,
            OTHER_WINDOW,
            &second_domain,
            second_press,
        ));
        assert!(captures.validate_primary_press(DEVICE, OWNER, &first_domain, first_press));
        assert!(captures.validate_primary_press(
            second_device,
            OTHER_WINDOW,
            &second_domain,
            second_press,
        ));
        assert!(captures.begin_sync(0));
        captures.finish_sync(true);
        captures.queue_drained(0);
        assert!(captures.has_explicit_capture(DEVICE, OWNER, &first_domain, first_press));
        assert!(captures.has_explicit_capture(
            second_device,
            OTHER_WINDOW,
            &second_domain,
            second_press,
        ));
    }

    #[test]
    fn unfinished_native_request_cannot_borrow_either_of_two_syncs() {
        let mut captures = ImplicitPointerCaptures::default();
        let (domain, first_press, _) = staged_explicit_capture(&mut captures, 2);
        let (_, pending_press) = validated_primary(&mut captures, DEVICE + 1, OWNER, 110);
        let pending =
            captures.begin_explicit_capture(DEVICE + 1, OWNER, &domain, pending_press).unwrap();
        captures.finish_sync(true);
        captures.queue_drained(0);
        assert!(!captures.has_explicit_capture(DEVICE, OWNER, &domain, first_press));
        assert!(!captures.begin_sync(0));

        let (_, third_press) = validated_primary(&mut captures, DEVICE + 2, OTHER_WINDOW, 111);
        let third = captures
            .begin_explicit_capture(DEVICE + 2, OTHER_WINDOW, &domain, third_press)
            .unwrap();
        assert_eq!(
            captures.complete_explicit_capture(third, Some(true)),
            ExplicitCaptureCompletion::AwaitingFence,
        );
        assert!(captures.begin_sync(0));
        captures.finish_sync(true);
        captures.queue_drained(0);
        assert!(matches!(
            captures.owners[&(DEVICE + 1)].explicit,
            ExplicitCaptureState::Acquiring {
                stage: ExplicitCaptureAcquisition::NativeRequest,
                ..
            }
        ));
        assert!(!captures.has_explicit_capture(DEVICE + 1, OWNER, &domain, pending_press));
        assert!(!captures.has_explicit_capture(DEVICE + 2, OTHER_WINDOW, &domain, third_press));

        assert_eq!(
            captures.complete_explicit_capture(pending, Some(true)),
            ExplicitCaptureCompletion::AwaitingFence,
        );
        captures.queue_drained(0);
        assert!(!captures.has_explicit_capture(DEVICE + 1, OWNER, &domain, pending_press));
        assert!(captures.begin_sync(0));
        captures.finish_sync(true);
        captures.queue_drained(0);
        assert!(captures.has_explicit_capture(DEVICE, OWNER, &domain, first_press));
        assert!(captures.has_explicit_capture(DEVICE + 1, OWNER, &domain, pending_press));
        assert!(captures.has_explicit_capture(DEVICE + 2, OTHER_WINDOW, &domain, third_press));
    }

    #[test]
    fn mutation_during_sync_revokes_an_uncovered_native_request_before_global_reopen() {
        let mut captures = ImplicitPointerCaptures::default();
        let (domain, first_press, _) = staged_explicit_capture(&mut captures, 2);
        let (_, pending_press) = validated_primary(&mut captures, DEVICE + 1, OWNER, 112);
        let pending =
            captures.begin_explicit_capture(DEVICE + 1, OWNER, &domain, pending_press).unwrap();
        captures.invalidate(Some(OTHER_WINDOW));
        assert!(!captures.begin_sync(0));
        captures.finish_sync(true);
        captures.queue_drained(0);
        assert_eq!(captures.fence, CaptureMutationFence::NeedsSync);
        assert!(captures.begin_sync(0));
        captures.finish_sync(true);
        captures.queue_drained(0);
        assert_eq!(captures.fence, CaptureMutationFence::Open);
        assert_eq!(
            captures.complete_explicit_capture(pending, Some(true)),
            ExplicitCaptureCompletion::Retiring,
        );
        assert!(!captures.has_explicit_capture(DEVICE, OWNER, &domain, first_press));
        assert!(!captures.has_explicit_capture(DEVICE + 1, OWNER, &domain, pending_press));
        assert!(captures.begin_explicit_capture(DEVICE, OWNER, &domain, first_press).is_none());
        assert!(captures
            .begin_explicit_capture(DEVICE + 1, OWNER, &domain, pending_press)
            .is_none());
        assert!(captures.validate_primary_press(DEVICE, OWNER, &domain, first_press));
        assert!(captures.validate_primary_press(DEVICE + 1, OWNER, &domain, pending_press));
    }

    #[test]
    fn explicit_candidates_cannot_complete_in_a_replacement_connection() {
        let mut first = ImplicitPointerCaptures::default();
        let mut replacement = ImplicitPointerCaptures::default();
        let (_, _, predecessor) = staged_explicit_capture(&mut first, 0);
        let (domain, press, successor) = staged_explicit_capture(&mut replacement, 0);
        assert_eq!(predecessor.0.press, successor.0.press);
        assert!(!Arc::ptr_eq(&predecessor.0, &successor.0));
        assert_eq!(
            replacement.complete_explicit_capture(predecessor, Some(true)),
            ExplicitCaptureCompletion::Stale,
        );
        assert_eq!(
            replacement.complete_explicit_capture(successor, Some(true)),
            ExplicitCaptureCompletion::AwaitingFence,
        );
        assert!(replacement.begin_sync(0));
        replacement.finish_sync(true);
        replacement.queue_drained(0);
        assert!(replacement.has_explicit_capture(DEVICE, OWNER, &domain, press));
    }

    #[test]
    fn failed_acquisition_fence_never_activates_or_retries() {
        let mut captures = ImplicitPointerCaptures::default();
        let (domain, press, stale) = staged_explicit_capture(&mut captures, 2);
        captures.finish_sync(false);
        assert_eq!(
            captures.complete_explicit_capture(stale, Some(true)),
            ExplicitCaptureCompletion::Stale,
        );
        captures.finish_sync(true);
        captures.queue_drained(0);
        captures.invalidate(None);
        assert!(!captures.begin_sync(0));
        assert!(!captures.has_explicit_capture(DEVICE, OWNER, &domain, press));
        assert!(!captures.validate_primary_press(DEVICE, OWNER, &domain, press));
        assert!(captures.begin_explicit_capture(DEVICE, OWNER, &domain, press).is_none());
        let mut releases = captures.claim_explicit_capture_releases();
        assert_eq!(releases.len(), 1);
        let release = releases.pop().unwrap();
        assert!(captures.begin_explicit_capture_release(&release));
        assert!(captures.complete_explicit_capture_release(release, true));
        assert_eq!(captures.fence, CaptureMutationFence::Failed);
        assert!(!captures.begin_sync(0));
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
    #[test]
    fn explicit_release_requires_a_new_fifo_boundary_before_queued_primary_can_qualify() {
        let mut captures = ImplicitPointerCaptures::default();
        let (domain, press, _) = staged_explicit_capture(&mut captures, 4);
        captures.after_button_event(
            DEVICE,
            OWNER,
            true,
            false,
            button_state_transition(&[0b10], 1, false),
        );
        let release = captures.release_explicit_capture(DEVICE, OWNER, &domain, press).unwrap();
        assert!(captures.begin_explicit_capture_release(&release));
        assert!(captures.complete_explicit_capture_release(release, true));
        // This press was queued while the predecessor still owned native routing.
        captures.after_button_event(
            DEVICE,
            OWNER,
            true,
            true,
            button_state_transition(&[0], 1, true),
        );
        assert!(captures.primary_press_candidate(DEVICE, OWNER).is_none());
        captures.queue_drained(0);
        assert!(captures.primary_press_candidate(DEVICE, OWNER).is_none());
        assert!(captures.begin_sync(0));
        captures.finish_sync(true);
        captures.queue_drained(0);
        assert!(captures.primary_press_candidate(DEVICE, OWNER).is_none());
        captures.for_button_state(DEVICE, current_button_state(&[0]));
        let (new_domain, new_press) = validated_primary(&mut captures, DEVICE, OWNER, 999);
        assert!(captures.begin_explicit_capture(DEVICE, OWNER, &new_domain, new_press).is_some());
    }

    #[test]
    fn explicit_release_dispatch_cannot_reopen_through_reentrant_sync_and_drain() {
        for succeeded in [false, true] {
            let mut captures = ImplicitPointerCaptures::default();
            let (domain, press, _) = staged_explicit_capture(&mut captures, 4);
            captures.for_button_state(DEVICE, current_button_state(&[0]));
            let release = captures.release_explicit_capture(DEVICE, OWNER, &domain, press).unwrap();
            assert!(captures.begin_explicit_capture_release(&release));
            assert!(!captures.begin_sync(0), "unlocked release dispatch cannot be covered early");
            captures.finish_sync(true);
            captures.queue_drained(0);
            assert_eq!(
                captures.after_button_event(
                    DEVICE,
                    OWNER,
                    true,
                    true,
                    button_state_transition(&[0], 1, true),
                ),
                NativePointerRoute::Unknown,
            );
            assert!(captures.primary_press_candidate(DEVICE, OWNER).is_none());
            assert_eq!(captures.complete_explicit_capture_release(release, succeeded), succeeded);
            assert!(captures.begin_sync(0));
            captures.finish_sync(true);
            captures.queue_drained(0);
            assert!(captures.primary_press_candidate(DEVICE, OWNER).is_none());
            if !succeeded {
                assert!(captures.owners[&DEVICE].has_native_obligation());
            }
        }
    }

    #[test]
    fn explicit_ungrab_revokes_the_original_implicit_owner_until_all_up() {
        let mut captures = ImplicitPointerCaptures::default();
        let (domain, press, _) = staged_explicit_capture(&mut captures, 4);
        let release = captures.release_explicit_capture(DEVICE, OWNER, &domain, press).unwrap();
        assert!(captures.begin_explicit_capture_release(&release));
        assert!(captures.complete_explicit_capture_release(release, true));
        assert!(captures.begin_sync(0));
        captures.finish_sync(true);
        captures.queue_drained(0);
        assert!(!captures.validate_primary_press(DEVICE, OWNER, &domain, press));
        assert_eq!(
            captures.for_button_state(DEVICE, current_button_state(&[0b10])),
            NativePointerRoute::Unknown,
            "explicit ungrab cannot restore the earlier automatic grab",
        );
        captures.for_button_state(DEVICE, current_button_state(&[0]));
        let (domain, press) = validated_primary(&mut captures, DEVICE, OTHER_WINDOW, 1000);
        assert!(captures.begin_explicit_capture(DEVICE, OTHER_WINDOW, &domain, press).is_some());
    }
}
