//! Explicit source-directed capture effects on the existing per-device owner.

use x11rb::protocol::xinput::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{self, ConnectionExt as _};

use super::pointer_facts::{
    ExplicitCaptureCandidate, ExplicitCaptureCompletion, ExplicitCaptureStatus,
};
use super::{ActiveEventLoop, WindowId, XConnection};
use crate::event::{DeviceId, PointerPressToken};
use crate::platform::x11::{X11PointerCaptureAcquisition, X11PointerCaptureRelease};

pub(super) fn explicit_capture_event_mask() -> xinput::XIEventMask {
    xinput::XIEventMask::BUTTON_PRESS
        | xinput::XIEventMask::BUTTON_RELEASE
        | xinput::XIEventMask::MOTION
        | xinput::XIEventMask::ENTER
        | xinput::XIEventMask::LEAVE
}

struct CaptureDispatch<'a> {
    connection: &'a XConnection,
    candidate: Option<ExplicitCaptureCandidate>,
    result: Option<bool>,
}

impl CaptureDispatch<'_> {
    fn finish(mut self, result: Option<bool>) -> ExplicitCaptureCompletion {
        self.connection.pointer_captures().complete_explicit_capture(
            self.candidate.take().expect("capture dispatch completes its own candidate once"),
            result,
        )
    }
}

impl Drop for CaptureDispatch<'_> {
    fn drop(&mut self) {
        if let Some(candidate) = self.candidate.take() {
            self.connection.pointer_captures().complete_explicit_capture(candidate, self.result);
        }
    }
}

impl ActiveEventLoop {
    pub(crate) fn acquire_pointer_capture(
        &self,
        expected_window: crate::window::WindowId,
        device_id: DeviceId,
        expected_press: &PointerPressToken,
    ) -> X11PointerCaptureAcquisition {
        let device = match device_id.0 {
            crate::platform_impl::DeviceId::X(device) => device,
            #[cfg(wayland_platform)]
            crate::platform_impl::DeviceId::Wayland(_) => {
                return X11PointerCaptureAcquisition::Unsupported
            },
        };
        let Ok(window) = u32::try_from(u64::from(expected_window)) else {
            return X11PointerCaptureAcquisition::Unavailable;
        };
        if self.xconn.pointer_observation_blocked() {
            return X11PointerCaptureAcquisition::Pending;
        }
        let (domain, press) = expected_press.x11_identity();
        let mut captures = self.xconn.pointer_captures();
        match captures.explicit_capture_status(device.0, window, domain, press) {
            ExplicitCaptureStatus::Acquired => return X11PointerCaptureAcquisition::Acquired,
            ExplicitCaptureStatus::Unavailable => return X11PointerCaptureAcquisition::Unavailable,
            ExplicitCaptureStatus::Pending => {},
        }
        let candidate = captures.begin_explicit_capture(device.0, window, domain, press);
        drop(captures);
        let Some(candidate) = candidate else {
            return X11PointerCaptureAcquisition::Pending;
        };
        let mut dispatch = CaptureDispatch {
            connection: &self.xconn,
            candidate: Some(candidate),
            result: Some(false),
        };
        let _dispatch = self.xconn.begin_native_event();
        let windows = self
            .windows
            .borrow()
            .iter()
            .filter_map(|(id, weak)| weak.upgrade().map(|window| (*id, window)))
            .collect::<Vec<_>>();
        // The candidate already excludes ordinary mutations. Read their existing persistent
        // mode records without holding the window registry or capture mutex across native calls.
        let eligible = windows.iter().any(|(id, _)| *id == WindowId(u64::from(window)))
            && windows.iter().all(|(_, window)| !window.ordinary_pointer_grab_blocks_acquisition())
            && (|| {
                let tree = self.xconn.xcb_connection().query_tree(window).ok()?.reply().ok()?;
                let attributes =
                    self.xconn.xcb_connection().get_window_attributes(window).ok()?.reply().ok()?;
                let devices = self
                    .xconn
                    .xcb_connection()
                    .xinput_xi_query_device(device.0)
                    .ok()?
                    .reply()
                    .ok()?;
                let pointer = self.xconn.query_pointer(self.root, device.0).ok()?;
                // Viewable includes mapped ancestors; the actual source grab below
                // independently checks eligibility after these replies.
                Some(
                    tree.root == self.root
                        && attributes.map_state == xproto::MapState::VIEWABLE
                        && devices.infos.len() == 1
                        && devices.infos[0].deviceid == device.0
                        && devices.infos[0].enabled
                        && devices.infos[0].type_ == xinput::DeviceType::MASTER_POINTER
                        && pointer.same_screen
                        && pointer.root == self.root
                        && pointer.buttons.first().is_some_and(|mask| mask & (1 << 1) != 0),
                )
            })()
            .unwrap_or(false)
            && self.xconn.validate_primary_press(window, device.0, expected_press);
        let result = if eligible && self.xconn.xcb_connection().has_error().is_none() {
            dispatch.result = None;
            let mask = explicit_capture_event_mask();
            // An indeterminate reply retains an exact cleanup duty. Async describes X11 event
            // processing, not a nonblocking Rust request or a continuity proof.
            self.xconn
                .xcb_connection()
                .xinput_xi_grab_device(
                    window,
                    x11rb::CURRENT_TIME,
                    0_u32,
                    device.0,
                    xproto::GrabMode::ASYNC,
                    xproto::GrabMode::ASYNC,
                    xinput::GrabOwner::NO_OWNER,
                    &[u32::from(mask)],
                )
                .ok()
                .and_then(|request| request.reply().ok())
                .and_then(|reply| {
                    self.xconn
                        .xcb_connection()
                        .has_error()
                        .is_none()
                        .then_some(reply.status == xproto::GrabStatus::SUCCESS)
                })
        } else {
            Some(false)
        };
        match dispatch.finish(result) {
            ExplicitCaptureCompletion::AwaitingFence => X11PointerCaptureAcquisition::Pending,
            ExplicitCaptureCompletion::Retiring
            | ExplicitCaptureCompletion::Failed
            | ExplicitCaptureCompletion::Stale => X11PointerCaptureAcquisition::Unavailable,
        }
    }

    pub(crate) fn release_pointer_capture(
        &self,
        expected_window: crate::window::WindowId,
        device_id: DeviceId,
        expected_press: &PointerPressToken,
    ) -> X11PointerCaptureRelease {
        let device = match device_id.0 {
            crate::platform_impl::DeviceId::X(device) => device,
            #[cfg(wayland_platform)]
            crate::platform_impl::DeviceId::Wayland(_) => {
                return X11PointerCaptureRelease::Unsupported
            },
        };
        let Ok(window) = u32::try_from(u64::from(expected_window)) else {
            return X11PointerCaptureRelease::Unavailable;
        };
        let (domain, press) = expected_press.x11_identity();
        let mut captures = self.xconn.pointer_captures();
        if !captures.has_explicit_capture_obligation(device.0, window, domain, press) {
            return X11PointerCaptureRelease::Unavailable;
        }
        let release = captures.release_explicit_capture(device.0, window, domain, press);
        drop(captures);
        let Some(release) = release else {
            return X11PointerCaptureRelease::Pending;
        };
        if self.xconn.settle_explicit_pointer_release(release) {
            X11PointerCaptureRelease::Released
        } else {
            X11PointerCaptureRelease::Pending
        }
    }
}
