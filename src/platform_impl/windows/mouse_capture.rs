//! Scopes the capture notification caused by the backend's own final button release.

use std::cell::Cell;

#[derive(Default)]
pub(super) struct CaptureReleaseScope {
    expected: Cell<bool>,
}

impl CaptureReleaseScope {
    /// Runs only the native release call while its synchronous notification is expected.
    pub(super) fn with_release<R>(&self, release: impl FnOnce() -> R) -> R {
        struct Restore<'scope> {
            expected: &'scope Cell<bool>,
            previous: bool,
        }

        impl Drop for Restore<'_> {
            fn drop(&mut self) {
                self.expected.set(self.previous);
            }
        }

        let _restore = Restore { expected: &self.expected, previous: self.expected.replace(true) };
        release()
    }

    /// Consumes at most one notification; transfer to another owner is never suppressed.
    pub(super) fn is_expected_change(&self, no_new_owner: bool) -> bool {
        self.expected.replace(false) && no_new_owner
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    #[test]
    fn synchronous_self_release_does_not_cancel_before_the_real_button_up() {
        let scope = CaptureReleaseScope::default();
        let events = RefCell::new(Vec::new());
        scope.with_release(|| {
            if !scope.is_expected_change(true) {
                events.borrow_mut().push("capture lost");
            }
        });
        events.borrow_mut().push("button up");
        assert_eq!(events.into_inner(), ["button up"]);
        assert!(!scope.is_expected_change(true));
    }

    #[test]
    fn an_external_capture_loss_is_forwarded_outside_the_release_call() {
        let scope = CaptureReleaseScope::default();
        assert!(!scope.is_expected_change(true));
        assert!(!scope.is_expected_change(false));
        scope.with_release(|| assert!(scope.is_expected_change(true)));
        assert!(!scope.is_expected_change(true));
    }

    #[test]
    fn a_transfer_is_never_suppressed_and_consumes_the_expected_notification() {
        let scope = CaptureReleaseScope::default();
        scope.with_release(|| {
            assert!(!scope.is_expected_change(false));
            assert!(!scope.is_expected_change(true));
        });
    }

    #[test]
    fn a_release_without_a_callback_does_not_suppress_a_later_capture_loss() {
        let scope = CaptureReleaseScope::default();
        assert!(!scope.with_release(|| false));
        assert!(!scope.is_expected_change(true));
        scope.with_release(|| assert!(scope.is_expected_change(true)));
    }

    #[test]
    fn windows_do_not_share_the_expected_release_notification() {
        let source = CaptureReleaseScope::default();
        let other = CaptureReleaseScope::default();
        source.with_release(|| {
            assert!(!other.is_expected_change(true));
            assert!(source.is_expected_change(true));
        });
    }
}
