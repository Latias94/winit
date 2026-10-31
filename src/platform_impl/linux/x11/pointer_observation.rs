//! Fresh desktop position/hover observations, fenced by the existing native event boundary.

use x11rb::protocol::xinput::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{self, ConnectionExt as _};

use super::{ActiveEventLoop, WindowId};
use crate::dpi::PhysicalPosition;
use crate::event::{DeviceId, PointerEventFacts, PointerPressToken, PointerWindowRoute};
use crate::platform::x11::X11PointerObservation;

impl ActiveEventLoop {
    pub(crate) fn observe_pointer(
        &self,
        expected_window: crate::window::WindowId,
        device_id: DeviceId,
        expected_press: Option<&PointerPressToken>,
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
            let initial = self.xconn.query_pointer(self.root, device.0).ok()?;
            if !initial.same_screen || initial.root != self.root {
                return None;
            }
            if expected_press.is_some()
                && !initial.buttons.first().is_some_and(|mask| mask & (1 << 1) != 0)
            {
                // Primary-up can reject an expected press. Logical button-down cannot prove
                // uninterrupted capture, delivery or freedom from a protocol freeze.
                return None;
            }
            // Point and root child come from this one reply. Walking a mutable foreign hierarchy
            // with later replies could assemble a hover route that never existed. A WM frame may
            // contain a Winit window, so an unrecognized root child is Unknown, never Foreign.
            let hover = if initial.child == 0 {
                PointerWindowRoute::None
            } else if is_live(initial.child) {
                PointerWindowRoute::Window(super::mkwid(initial.child))
            } else {
                PointerWindowRoute::Unknown
            };
            // XIQueryPointer does not identify the current grab or a protocol freeze. Current
            // buttons and a retained implicit owner cannot prove capture across ancestor unmaps
            // or same-client ungrab commands. Keep both routes unknown, even for the source.
            let fixed = |value| f64::from(value) / 65_536.0;
            Some(X11PointerObservation::Observed {
                facts: PointerEventFacts {
                    desktop_position: Some(PhysicalPosition::new(
                        fixed(initial.root_x),
                        fixed(initial.root_y),
                    )),
                    hover,
                    ..PointerEventFacts::default()
                },
                delivery: PointerWindowRoute::Unknown,
            })
        })();
        // Replies can read earlier native events into Xlib's queue. Do not publish a sample until
        // those events have been delivered normally; a retry takes a new sample after that drain.
        if self.xconn.pointer_observation_blocked() || !press_is_current() {
            X11PointerObservation::Unavailable
        } else {
            observation.unwrap_or(X11PointerObservation::Unavailable)
        }
    }
}
