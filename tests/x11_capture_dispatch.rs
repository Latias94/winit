//! Exercise ordinary grab dispatch against the production pointer-capture state without X11.

use std::num::NonZeroU64;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

#[path = "../src/platform_impl/linux/x11/pointer_facts.rs"]
mod pointer_facts;

use pointer_facts::{
    button_state_transition, ExplicitCaptureCompletion, ExplicitCapturePhase,
    ExplicitCaptureStatus, ImplicitPointerCaptures, NativePointerRoute, PointerGrabMutationGuard,
    PointerPressConnectionDomain,
};

const DEVICE: u16 = 2;
const OTHER_DEVICE: u16 = 3;
const OWNER: u32 = 100;

fn validated_primary(
    captures: &mut ImplicitPointerCaptures,
    device: u16,
    sequence: u64,
) -> (Arc<PointerPressConnectionDomain>, NonZeroU64) {
    assert_eq!(
        captures.after_button_event(
            device,
            OWNER,
            true,
            true,
            button_state_transition(&[0], 1, true),
        ),
        NativePointerRoute::Window(OWNER),
    );
    let candidate = captures.primary_press_candidate(device, OWNER).unwrap();
    captures.complete_primary_press(candidate, NonZeroU64::new(sequence));
    captures.primary_press(device, OWNER).unwrap()
}

#[test]
fn ordinary_grab_rejects_unsettled_explicit_phases_without_spending_press_eligibility() {
    let mut state = ImplicitPointerCaptures::default();
    let (domain, press) = validated_primary(&mut state, DEVICE, 101);
    let (other_domain, other_press) = validated_primary(&mut state, OTHER_DEVICE, 102);
    let candidate = state.begin_explicit_capture(DEVICE, OWNER, &domain, press).unwrap();
    let captures = Mutex::new(state);
    let depth = AtomicU32::new(0);

    let assert_rejected = |phase, status| {
        assert!(PointerGrabMutationGuard::try_new(&captures, &depth).is_none());
        assert_eq!(depth.load(Ordering::Relaxed), 0);
        let state = captures.try_lock().expect("rejection must release the capture mutex");
        assert_eq!(state.explicit_capture_phase(DEVICE), phase);
        assert_eq!(state.explicit_capture_status(DEVICE, OWNER, &domain, press), status);
        assert!(state.validate_primary_press(DEVICE, OWNER, &domain, press));
        assert_eq!(
            state.has_explicit_capture(DEVICE, OWNER, &domain, press),
            phase == ExplicitCapturePhase::Active,
        );
        assert!(state.validate_primary_press(OTHER_DEVICE, OWNER, &other_domain, other_press));
        assert_eq!(
            state.explicit_capture_status(OTHER_DEVICE, OWNER, &other_domain, other_press),
            ExplicitCaptureStatus::Pending,
        );
    };

    assert_rejected(ExplicitCapturePhase::NativeRequest, ExplicitCaptureStatus::Pending);
    assert_eq!(
        captures.lock().unwrap().complete_explicit_capture(candidate, Some(true)),
        ExplicitCaptureCompletion::AwaitingFence,
    );
    assert_rejected(ExplicitCapturePhase::AwaitingFence, ExplicitCaptureStatus::Pending);
    assert!(captures.lock().unwrap().begin_sync(depth.load(Ordering::Relaxed)));
    assert_rejected(ExplicitCapturePhase::AwaitingFence, ExplicitCaptureStatus::Pending);
    captures.lock().unwrap().finish_sync(true);
    assert_rejected(ExplicitCapturePhase::AwaitingFence, ExplicitCaptureStatus::Pending);
    captures.lock().unwrap().queue_drained(depth.load(Ordering::Relaxed));
    assert_rejected(ExplicitCapturePhase::Active, ExplicitCaptureStatus::Acquired);

    let release =
        captures.lock().unwrap().release_explicit_capture(DEVICE, OWNER, &domain, press).unwrap();
    assert_rejected(ExplicitCapturePhase::Retiring, ExplicitCaptureStatus::Unavailable);

    let (next_other_domain, next_other_press) = {
        let mut state = captures.lock().unwrap();
        let other =
            state.begin_explicit_capture(OTHER_DEVICE, OWNER, &other_domain, other_press).expect(
                "rejected ordinary grabs must preserve the unrelated press's acquisition attempt",
            );
        assert_eq!(
            state.complete_explicit_capture(other, Some(false)),
            ExplicitCaptureCompletion::Failed,
        );
        assert_eq!(
            state.after_button_event(
                OTHER_DEVICE,
                OWNER,
                true,
                false,
                button_state_transition(&[0b10], 1, false),
            ),
            NativePointerRoute::None,
        );
        let (domain, press) = validated_primary(&mut state, OTHER_DEVICE, 103);
        assert_eq!(
            state.explicit_capture_status(OTHER_DEVICE, OWNER, &domain, press),
            ExplicitCaptureStatus::Pending,
        );
        (domain, press)
    };

    // Only release dispatch closes the shared fence and revokes other acquisition attempts.
    assert!(captures.lock().unwrap().begin_explicit_capture_release(&release));
    assert!(PointerGrabMutationGuard::try_new(&captures, &depth).is_none());
    assert_eq!(depth.load(Ordering::Relaxed), 0);

    let mut state = captures.lock().unwrap();
    assert_eq!(state.explicit_capture_phase(DEVICE), ExplicitCapturePhase::Retiring);
    assert!(!state.validate_primary_press(DEVICE, OWNER, &domain, press));
    assert!(!state.validate_primary_press(
        OTHER_DEVICE,
        OWNER,
        &next_other_domain,
        next_other_press
    ));
    assert!(state.complete_explicit_capture_release(release, true));
    state.queue_drained(depth.load(Ordering::Relaxed));
    assert!(!state.validate_primary_press(DEVICE, OWNER, &domain, press));
    assert!(!state.validate_primary_press(
        OTHER_DEVICE,
        OWNER,
        &next_other_domain,
        next_other_press
    ));
    assert!(state.begin_sync(depth.load(Ordering::Relaxed)));
    state.finish_sync(true);
    assert!(!state.validate_primary_press(DEVICE, OWNER, &domain, press));
    state.queue_drained(depth.load(Ordering::Relaxed));

    // The released device loses its old implicit owner. Unrelated devices keep their Primary
    // identity, but none can renew an explicit attempt revoked by the shared mutation fence.
    assert!(!state.validate_primary_press(DEVICE, OWNER, &domain, press));
    assert!(state.validate_primary_press(
        OTHER_DEVICE,
        OWNER,
        &next_other_domain,
        next_other_press
    ));
    assert_eq!(
        state.explicit_capture_status(DEVICE, OWNER, &domain, press),
        ExplicitCaptureStatus::Unavailable,
    );
    assert_eq!(
        state.explicit_capture_status(OTHER_DEVICE, OWNER, &next_other_domain, next_other_press),
        ExplicitCaptureStatus::Unavailable,
    );
    assert!(state.begin_explicit_capture(DEVICE, OWNER, &domain, press).is_none());
    assert!(state
        .begin_explicit_capture(OTHER_DEVICE, OWNER, &next_other_domain, next_other_press)
        .is_none());
    assert_eq!(
        state.after_button_event(
            OTHER_DEVICE,
            OWNER,
            true,
            false,
            button_state_transition(&[0b10], 1, false),
        ),
        NativePointerRoute::None,
    );
    let (fresh_domain, fresh_press) = validated_primary(&mut state, OTHER_DEVICE, 104);
    let other = state
        .begin_explicit_capture(OTHER_DEVICE, OWNER, &fresh_domain, fresh_press)
        .expect("only a fresh Primary edge can rearm acquisition after release dispatch");
    assert_eq!(
        state.complete_explicit_capture(other, Some(false)),
        ExplicitCaptureCompletion::Failed
    );
}

#[test]
fn ordinary_grab_waits_for_late_native_results_and_confirmed_cleanup() {
    for native_result in [Some(true), None] {
        let mut state = ImplicitPointerCaptures::default();
        let (domain, press) = validated_primary(&mut state, DEVICE, 201);
        let candidate = state.begin_explicit_capture(DEVICE, OWNER, &domain, press).unwrap();
        assert!(state.release_explicit_capture(DEVICE, OWNER, &domain, press).is_none());
        assert_eq!(state.explicit_capture_phase(DEVICE), ExplicitCapturePhase::Retiring);
        assert!(state.claim_explicit_capture_releases().is_empty());
        let captures = Mutex::new(state);
        let depth = AtomicU32::new(0);

        assert!(PointerGrabMutationGuard::try_new(&captures, &depth).is_none());
        let release = {
            let mut state = captures.lock().unwrap();
            assert_eq!(
                state.complete_explicit_capture(candidate, native_result),
                ExplicitCaptureCompletion::Retiring,
            );
            let mut releases = state.claim_explicit_capture_releases();
            assert_eq!(releases.len(), 1);
            releases.pop().unwrap()
        };
        assert!(PointerGrabMutationGuard::try_new(&captures, &depth).is_none());
        {
            let mut state = captures.lock().unwrap();
            assert!(state.validate_primary_press(DEVICE, OWNER, &domain, press));
            assert!(state.begin_explicit_capture_release(&release));
            assert!(!state.validate_primary_press(DEVICE, OWNER, &domain, press));
            // Reentrant service cannot cover an unlocked release dispatch, even on failure.
            assert!(!state.begin_sync(depth.load(Ordering::Relaxed)));
            state.finish_sync(true);
            assert!(!state.complete_explicit_capture_release(release, false));
            state.queue_drained(depth.load(Ordering::Relaxed));
            assert_eq!(state.explicit_capture_phase(DEVICE), ExplicitCapturePhase::Retiring);
            assert!(!state.validate_primary_press(DEVICE, OWNER, &domain, press));
        }
        assert!(PointerGrabMutationGuard::try_new(&captures, &depth).is_none());
        assert_eq!(depth.load(Ordering::Relaxed), 0);

        {
            let mut state = captures.lock().unwrap();
            let mut releases = state.claim_explicit_capture_releases();
            assert_eq!(releases.len(), 1);
            let retry = releases.pop().unwrap();
            assert!(state.begin_explicit_capture_release(&retry));
            assert!(!state.begin_sync(depth.load(Ordering::Relaxed)));
            state.finish_sync(true);
            assert!(state.complete_explicit_capture_release(retry, true));
            assert!(state.claim_explicit_capture_releases().is_empty());
            state.queue_drained(depth.load(Ordering::Relaxed));
            assert!(!state.validate_primary_press(DEVICE, OWNER, &domain, press));
            assert!(state.begin_sync(depth.load(Ordering::Relaxed)));
            state.finish_sync(true);
            assert!(!state.validate_primary_press(DEVICE, OWNER, &domain, press));
            state.queue_drained(depth.load(Ordering::Relaxed));
            assert!(!state.validate_primary_press(DEVICE, OWNER, &domain, press));
            assert!(state.begin_explicit_capture(DEVICE, OWNER, &domain, press).is_none());
        }

        let grab = PointerGrabMutationGuard::try_new(&captures, &depth)
            .expect("only confirmed cleanup permits ordinary takeover");
        assert_eq!(depth.load(Ordering::Relaxed), 1);
        drop(grab);
        assert_eq!(depth.load(Ordering::Relaxed), 0);
        let mut state = captures.lock().unwrap();
        assert!(state.begin_sync(depth.load(Ordering::Relaxed)));
        state.finish_sync(true);
        state.queue_drained(depth.load(Ordering::Relaxed));
        assert!(state.primary_press(DEVICE, OWNER).is_none());
        assert!(!state.validate_primary_press(DEVICE, OWNER, &domain, press));
        assert!(state.begin_explicit_capture(DEVICE, OWNER, &domain, press).is_none());
    }
}

#[test]
fn ordinary_grab_keeps_the_mutex_free_and_excludes_competing_dispatch() {
    let mut state = ImplicitPointerCaptures::default();
    let (domain, press) = validated_primary(&mut state, DEVICE, 301);
    let (other_domain, other_press) = validated_primary(&mut state, OTHER_DEVICE, 302);
    let captures = Mutex::new(state);
    // The surrounding native callback owns one depth independently of ordinary dispatch.
    let depth = AtomicU32::new(1);
    let grab = PointerGrabMutationGuard::try_new(&captures, &depth).unwrap();
    assert_eq!(depth.load(Ordering::Relaxed), 2);
    {
        let mut state =
            captures.try_lock().expect("native dispatch must not hold the capture mutex");
        assert!(!state.validate_primary_press(DEVICE, OWNER, &domain, press));
        assert!(!state.validate_primary_press(OTHER_DEVICE, OWNER, &other_domain, other_press));
        assert!(state.begin_explicit_capture(DEVICE, OWNER, &domain, press).is_none());
        assert!(state
            .begin_explicit_capture(OTHER_DEVICE, OWNER, &other_domain, other_press)
            .is_none());
        assert!(state.release_explicit_capture(DEVICE, OWNER, &domain, press).is_none());
        assert!(state.claim_explicit_capture_releases().is_empty());
        assert!(!state.begin_sync(depth.load(Ordering::Relaxed)));
        // Even an older zero-depth sample cannot bypass the live ordinary reservation.
        assert!(!state.begin_sync(0));
        state.finish_sync(true);
        state.queue_drained(0);
        assert_eq!(
            state.after_button_event(
                OTHER_DEVICE + 1,
                OWNER,
                true,
                true,
                button_state_transition(&[0], 1, true),
            ),
            NativePointerRoute::Unknown,
        );
        assert!(state.primary_press_candidate(OTHER_DEVICE + 1, OWNER).is_none());
    }
    assert!(PointerGrabMutationGuard::try_new(&captures, &depth).is_none());
    assert_eq!(depth.load(Ordering::Relaxed), 2);
    drop(grab);
    assert_eq!(depth.load(Ordering::Relaxed), 1);

    {
        let mut state = captures.try_lock().unwrap();
        assert!(!state.begin_sync(depth.load(Ordering::Relaxed)));
        state.queue_drained(depth.load(Ordering::Relaxed));
        assert!(state.primary_press(DEVICE, OWNER).is_none());
    }
    assert_eq!(depth.fetch_sub(1, Ordering::Relaxed), 1);
    let mut state = captures.lock().unwrap();
    assert!(state.begin_sync(depth.load(Ordering::Relaxed)));
    state.finish_sync(true);
    state.queue_drained(depth.load(Ordering::Relaxed));
    assert!(!state.validate_primary_press(DEVICE, OWNER, &domain, press));
    assert!(!state.validate_primary_press(OTHER_DEVICE, OWNER, &other_domain, other_press));
    assert!(state.primary_press_candidate(OTHER_DEVICE + 1, OWNER).is_none());
}

#[test]
fn ordinary_grab_unwind_restores_the_slot_but_requires_new_sync_and_fifo_drain() {
    let mut state = ImplicitPointerCaptures::default();
    let (domain, press) = validated_primary(&mut state, DEVICE, 401);
    let captures = Mutex::new(state);
    let depth = AtomicU32::new(0);
    let result = std::panic::catch_unwind(|| {
        let _grab = PointerGrabMutationGuard::try_new(&captures, &depth).unwrap();
        assert_eq!(depth.load(Ordering::Relaxed), 1);
        assert!(captures.try_lock().is_ok());
        panic!("ordinary native dispatch unwound");
    });
    assert!(result.is_err());
    assert_eq!(depth.load(Ordering::Relaxed), 0);
    assert!(!captures.is_poisoned());

    let assert_fresh_press_blocked = |state: &mut ImplicitPointerCaptures| {
        assert_eq!(
            state.after_button_event(
                DEVICE,
                OWNER,
                true,
                false,
                button_state_transition(&[0b10], 1, false),
            ),
            NativePointerRoute::None,
        );
        assert_eq!(
            state.after_button_event(
                DEVICE,
                OWNER,
                true,
                true,
                button_state_transition(&[0], 1, true),
            ),
            NativePointerRoute::Unknown,
        );
        assert!(state.primary_press_candidate(DEVICE, OWNER).is_none());
        assert!(state.primary_press(DEVICE, OWNER).is_none());
    };

    let mut state = captures.try_lock().unwrap();
    state.queue_drained(depth.load(Ordering::Relaxed));
    assert_fresh_press_blocked(&mut state);
    assert!(state.begin_sync(depth.load(Ordering::Relaxed)));
    state.queue_drained(depth.load(Ordering::Relaxed));
    assert_fresh_press_blocked(&mut state);
    state.finish_sync(true);
    assert_fresh_press_blocked(&mut state);
    // Only the normal event loop's FIFO-empty proof admits a subsequent new press.
    state.queue_drained(depth.load(Ordering::Relaxed));
    assert!(state.primary_press_candidate(DEVICE, OWNER).is_none());
    assert!(state.primary_press(DEVICE, OWNER).is_none());
    state.after_button_event(
        DEVICE,
        OWNER,
        true,
        false,
        button_state_transition(&[0b10], 1, false),
    );
    let (new_domain, new_press) = validated_primary(&mut state, DEVICE, 402);
    assert!(!state.validate_primary_press(DEVICE, OWNER, &domain, press));
    assert!(state.begin_explicit_capture(DEVICE, OWNER, &domain, press).is_none());
    let candidate = state.begin_explicit_capture(DEVICE, OWNER, &new_domain, new_press).unwrap();
    assert_eq!(
        state.complete_explicit_capture(candidate, Some(false)),
        ExplicitCaptureCompletion::Failed,
    );
    drop(state);

    let next = PointerGrabMutationGuard::try_new(&captures, &depth)
        .expect("unwind must release the ordinary dispatch slot");
    assert_eq!(depth.load(Ordering::Relaxed), 1);
    drop(next);
    assert_eq!(depth.load(Ordering::Relaxed), 0);
}

#[test]
fn ordinary_grab_drop_rejects_old_sync_completion_on_either_side_of_dispatch_return() {
    for complete_before_drop in [true, false] {
        let mut state = ImplicitPointerCaptures::default();
        state.invalidate(None);
        assert!(state.begin_sync(0));
        let captures = Mutex::new(state);
        let depth = AtomicU32::new(0);
        let grab = PointerGrabMutationGuard::try_new(&captures, &depth).unwrap();
        if complete_before_drop {
            let mut state = captures.try_lock().unwrap();
            state.finish_sync(true);
            state.queue_drained(depth.load(Ordering::Relaxed));
            assert!(!state.begin_sync(depth.load(Ordering::Relaxed)));
        }
        drop(grab);
        assert_eq!(depth.load(Ordering::Relaxed), 0);
        let mut state = captures.lock().unwrap();
        if !complete_before_drop {
            state.finish_sync(true);
        }
        state.queue_drained(depth.load(Ordering::Relaxed));
        assert_eq!(
            state.after_button_event(
                DEVICE,
                OWNER,
                true,
                true,
                button_state_transition(&[0], 1, true),
            ),
            NativePointerRoute::Unknown,
        );
        assert!(state.primary_press_candidate(DEVICE, OWNER).is_none());
        assert!(state.begin_sync(depth.load(Ordering::Relaxed)));
        state.finish_sync(true);
        state.queue_drained(depth.load(Ordering::Relaxed));
        state.after_button_event(
            DEVICE,
            OWNER,
            true,
            false,
            button_state_transition(&[0b10], 1, false),
        );
        let (domain, press) = validated_primary(&mut state, DEVICE, 501);
        let candidate = state.begin_explicit_capture(DEVICE, OWNER, &domain, press).unwrap();
        assert_eq!(
            state.complete_explicit_capture(candidate, Some(false)),
            ExplicitCaptureCompletion::Failed,
        );
    }
}
