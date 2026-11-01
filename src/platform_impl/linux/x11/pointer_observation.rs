//! Fresh desktop position/hover observations, fenced by the existing native event boundary.

use x11rb::protocol::xinput::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{self, ConnectionExt as _};

use super::pointer_capture::explicit_capture_event_mask;
use super::pointer_facts::ExplicitCaptureSample;
use super::{ActiveEventLoop, WindowId, XConnection};
use crate::dpi::PhysicalPosition;
use crate::event::{DeviceId, PointerEventFacts, PointerPressToken, PointerWindowRoute};
use crate::platform::x11::X11PointerObservation;

const MAX_HOVER_QUERY_DEPTH: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CaptureSampleCheckpoint {
    PositionQueried,
    CaptureRefreshed,
}

struct CaptureSampleDispatch<'a> {
    connection: &'a XConnection,
    sample: Option<ExplicitCaptureSample>,
    native_result: Option<bool>,
}

impl CaptureSampleDispatch<'_> {
    fn finish(mut self) -> Option<bool> {
        self.connection.pointer_captures().complete_explicit_capture_sample(
            self.sample.take().expect("a sample finishes its exact duty once"),
            self.native_result,
        )
    }
}

impl Drop for CaptureSampleDispatch<'_> {
    fn drop(&mut self) {
        if let Some(sample) = self.sample.take() {
            self.connection
                .pointer_captures()
                .complete_explicit_capture_sample(sample, self.native_result);
        }
    }
}

impl ActiveEventLoop {
    fn query_pointer_hover(
        &self,
        device_id: u16,
        is_live: impl Fn(xproto::Window) -> bool,
    ) -> Option<(xinput::XIQueryPointerReply, PointerWindowRoute)> {
        let mut window = self.root;
        for depth in 0..=MAX_HOVER_QUERY_DEPTH {
            let observed = self.xconn.query_pointer(window, device_id).ok()?;
            if !observed.same_screen || observed.root != self.root {
                return None;
            }
            let hover = if observed.child == 0 {
                if window == self.root {
                    PointerWindowRoute::None
                } else {
                    PointerWindowRoute::Unknown
                }
            } else if is_live(observed.child) {
                PointerWindowRoute::Window(super::mkwid(observed.child))
            } else if depth == MAX_HOVER_QUERY_DEPTH {
                PointerWindowRoute::Unknown
            } else {
                // Earlier replies discover the next object only. Xorg derives child from
                // the current sprite's ancestor chain; the terminal reply itself must name
                // our window, and its own root coordinates supply the position below.
                window = observed.child;
                continue;
            };
            return Some((observed, hover));
        }
        None
    }

    pub(crate) fn observe_pointer(
        &self,
        expected_window: crate::window::WindowId,
        device_id: DeviceId,
        expected_press: Option<&PointerPressToken>,
    ) -> X11PointerObservation {
        self.observe_pointer_inner(expected_window, device_id, expected_press, |_| {})
    }

    fn observe_pointer_inner(
        &self,
        expected_window: crate::window::WindowId,
        device_id: DeviceId,
        expected_press: Option<&PointerPressToken>,
        mut checkpoint: impl FnMut(CaptureSampleCheckpoint),
    ) -> X11PointerObservation {
        let device = match device_id.0 {
            crate::platform_impl::DeviceId::X(device) => device,
            #[cfg(wayland_platform)]
            crate::platform_impl::DeviceId::Wayland(_) => {
                return X11PointerObservation::Unsupported
            },
        };
        let Ok(window) = u32::try_from(u64::from(expected_window)) else {
            return X11PointerObservation::Unavailable;
        };
        let windows = self.windows.borrow();
        let is_live = |window| {
            windows.get(&WindowId(u64::from(window))).and_then(|window| window.upgrade()).is_some()
        };
        let press_is_current = || match expected_press {
            Some(expected) => self.xconn.validate_primary_press(window, device.0, expected),
            None => true,
        };
        if !is_live(window) || self.xconn.pointer_observation_blocked() || !press_is_current() {
            return X11PointerObservation::Unavailable;
        }
        // Enter our own native interval before reserving the sample. Every later
        // native expansion marks this same row stale, even if it empties the queue.
        let native_dispatch = self.xconn.begin_native_event();
        let mut capture_sample = expected_press.and_then(|expected| {
            let (domain, press) = expected.x11_identity();
            self.xconn
                .pointer_captures()
                .begin_explicit_capture_sample(device.0, window, domain, press)
                .map(|sample| CaptureSampleDispatch {
                    connection: &self.xconn,
                    sample: Some(sample),
                    // No refresh has been dispatched yet; the original duty remains active.
                    native_result: Some(false),
                })
        });
        // Reentrant observations cannot consume an interval whose native replies are in flight.
        // Window mutation still revokes this exact sampling row while calls stay outside its lock.
        let observation = (|| {
            let attributes =
                self.xconn.xcb_connection().get_window_attributes(window).ok()?.reply().ok()?;
            if attributes.map_state != xproto::MapState::VIEWABLE {
                return None;
            }
            let devices =
                self.xconn.xcb_connection().xinput_xi_query_device(device.0).ok()?.reply().ok()?;
            if devices.infos.len() != 1
                || devices.infos[0].deviceid != device.0
                || !devices.infos[0].enabled
                || devices.infos[0].type_ != xinput::DeviceType::MASTER_POINTER
            {
                return None;
            }
            let (position, hover) = self.query_pointer_hover(device.0, is_live)?;
            if expected_press.is_some()
                && !position.buttons.first().is_some_and(|mask| mask & (1 << 1) != 0)
            {
                // Primary-up can reject an expected press. Logical button-down cannot prove
                // uninterrupted capture, delivery or freedom from a protocol freeze.
                return None;
            }
            checkpoint(CaptureSampleCheckpoint::PositionQueried);
            if let Some(sample) = capture_sample.as_mut() {
                let tree = self.xconn.xcb_connection().query_tree(window).ok()?.reply().ok()?;
                if tree.root == self.root && self.xconn.xcb_connection().has_error().is_none() {
                    // G2 checks ownership/freeze after the terminal position query. Selected
                    // input or crossing queued before its reply rejects that sample without drain.
                    sample.native_result = None;
                    sample.native_result = self
                        .xconn
                        .xcb_connection()
                        .xinput_xi_grab_device(
                            window,
                            x11rb::CURRENT_TIME,
                            0_u32,
                            device.0,
                            xproto::GrabMode::ASYNC,
                            xproto::GrabMode::ASYNC,
                            xinput::GrabOwner::NO_OWNER,
                            &[u32::from(explicit_capture_event_mask())],
                        )
                        .ok()
                        .and_then(|request| request.reply().ok())
                        .and_then(|reply| {
                            self.xconn
                                .xcb_connection()
                                .has_error()
                                .is_none()
                                .then_some(reply.status == xproto::GrabStatus::SUCCESS)
                        });
                    if sample.native_result == Some(true) {
                        checkpoint(CaptureSampleCheckpoint::CaptureRefreshed);
                    }
                }
            }
            // Position and hover belong to the same terminal reply. Unknown foreign leaves
            // cannot be skipped to infer the Winit client behind a window-manager frame.
            // Query facts alone grant no capture. Only an exact active explicit duty with a fresh
            // successful G2, the final empty native queue and current identity can supply it.
            let fixed = |value| f64::from(value) / 65_536.0;
            Some(X11PointerObservation::Observed {
                facts: PointerEventFacts {
                    desktop_position: Some(PhysicalPosition::new(
                        fixed(position.root_x),
                        fixed(position.root_y),
                    )),
                    hover,
                    ..PointerEventFacts::default()
                },
                delivery: PointerWindowRoute::Unknown,
            })
        })();
        drop(native_dispatch);
        // Replies can read earlier native events into Xlib's queue. Do not publish a sample until
        // those events have been delivered normally; a retry takes a new sample after that drain.
        let blocked = self.xconn.pointer_observation_blocked() || !press_is_current();
        let refreshed = capture_sample.map_or(Some(false), CaptureSampleDispatch::finish);
        if blocked || refreshed.is_none() {
            X11PointerObservation::Unavailable
        } else {
            match observation {
                Some(X11PointerObservation::Observed { mut facts, mut delivery }) => {
                    if refreshed == Some(true) {
                        facts.capture = PointerWindowRoute::Window(expected_window);
                        delivery = PointerWindowRoute::Window(expected_window);
                    }
                    X11PointerObservation::Observed { facts, delivery }
                },
                _ => X11PointerObservation::Unavailable,
            }
        }
    }
}

#[cfg(test)]
#[path = "pointer_observation/tests.rs"]
mod tests;
