//! Touch handling.

use tracing::warn;

use sctk::reexports::client::protocol::wl_seat::WlSeat;
use sctk::reexports::client::protocol::wl_surface::WlSurface;
use sctk::reexports::client::protocol::wl_touch::WlTouch;
use sctk::reexports::client::{Connection, Proxy, QueueHandle};

use sctk::seat::touch::{TouchData, TouchHandler};

use crate::dpi::{LogicalPosition, LogicalSize, PhysicalPosition};
use crate::event::{PointerEventFacts, PointerEventGeometry, Touch, TouchPhase, WindowEvent};

use crate::platform_impl::wayland::state::WinitState;
use crate::platform_impl::wayland::{self, DeviceId};

impl TouchHandler for WinitState {
    fn down(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        touch: &WlTouch,
        _: u32,
        _: u32,
        surface: WlSurface,
        id: i32,
        position: (f64, f64),
    ) {
        let window_id = wayland::make_wid(&surface);
        let sample = match self.windows.get_mut().get(&window_id) {
            Some(window) => {
                let window = window.lock().unwrap();
                TouchSample::new(
                    LogicalPosition::from(position),
                    window.inner_size(),
                    window.scale_factor(),
                )
            },
            None => return,
        };

        let seat_state = match self.seats.get_mut(&touch.seat().id()) {
            Some(seat_state) => seat_state,
            None => {
                warn!("Received wl_touch::down without seat");
                return;
            },
        };

        // Update the state of the point.
        seat_state.touch_map.insert(id, TouchPoint { surface, sample });

        self.events_sink.push_window_event(
            WindowEvent::Touch(sample.into_touch(id, TouchPhase::Started)),
            window_id,
        );
    }

    fn up(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        touch: &WlTouch,
        _: u32,
        _: u32,
        id: i32,
    ) {
        let seat_state = match self.seats.get_mut(&touch.seat().id()) {
            Some(seat_state) => seat_state,
            None => {
                warn!("Received wl_touch::up without seat");
                return;
            },
        };

        // Remove the touch point.
        let touch_point = match seat_state.touch_map.remove(&id) {
            Some(touch_point) => touch_point,
            None => return,
        };

        let window_id = wayland::make_wid(&touch_point.surface);
        if !self.windows.get_mut().contains_key(&window_id) {
            return;
        }

        // Up has no position. Preserve the last down/motion pair without resampling geometry.
        self.events_sink.push_window_event(
            WindowEvent::Touch(touch_point.sample.into_touch(id, TouchPhase::Ended)),
            window_id,
        );
    }

    fn motion(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        touch: &WlTouch,
        _: u32,
        id: i32,
        position: (f64, f64),
    ) {
        let seat_state = match self.seats.get_mut(&touch.seat().id()) {
            Some(seat_state) => seat_state,
            None => {
                warn!("Received wl_touch::motion without seat");
                return;
            },
        };

        // Update the existing touch point.
        let touch_point = match seat_state.touch_map.get_mut(&id) {
            Some(touch_point) => touch_point,
            None => return,
        };

        let window_id = wayland::make_wid(&touch_point.surface);
        let sample = match self.windows.get_mut().get(&window_id) {
            Some(window) => {
                let window = window.lock().unwrap();
                TouchSample::new(
                    LogicalPosition::from(position),
                    window.inner_size(),
                    window.scale_factor(),
                )
            },
            None => return,
        };

        touch_point.sample = sample;

        self.events_sink.push_window_event(
            WindowEvent::Touch(touch_point.sample.into_touch(id, TouchPhase::Moved)),
            window_id,
        );
    }

    fn cancel(&mut self, _: &Connection, _: &QueueHandle<Self>, touch: &WlTouch) {
        let seat_state = match self.seats.get_mut(&touch.seat().id()) {
            Some(seat_state) => seat_state,
            None => {
                warn!("Received wl_touch::cancel without seat");
                return;
            },
        };

        for (id, touch_point) in seat_state.touch_map.drain() {
            let window_id = wayland::make_wid(&touch_point.surface);
            if !self.windows.get_mut().contains_key(&window_id) {
                continue;
            }

            // Cancellation preserves each contact's last observed pair, not current geometry.
            self.events_sink.push_window_event(
                WindowEvent::Touch(touch_point.sample.into_touch(id, TouchPhase::Cancelled)),
                window_id,
            );
        }
    }

    fn shape(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlTouch,
        _: i32,
        _: f64,
        _: f64,
    ) {
        // Blank.
    }

    fn orientation(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlTouch, _: i32, _: f64) {
        // Blank.
    }
}

/// One physical position and its geometry, frozen together at down/motion.
#[derive(Debug, Clone, Copy)]
struct TouchSample {
    location: PhysicalPosition<f64>,
    geometry: PointerEventGeometry,
}

impl TouchSample {
    fn new(
        position: LogicalPosition<f64>,
        surface_size: LogicalSize<u32>,
        scale_factor: f64,
    ) -> Self {
        // The caller holds the WindowState lock until this complete sample is frozen.
        Self {
            location: position.to_physical(scale_factor),
            geometry: PointerEventGeometry {
                surface_size: wayland::logical_to_physical_rounded(surface_size, scale_factor),
                scale_factor,
            },
        }
    }

    /// Builds any contact phase from this pair; end/cancel never samples new window state.
    fn into_touch(self, id: i32, phase: TouchPhase) -> Touch {
        Touch {
            device_id: crate::event::DeviceId(crate::platform_impl::DeviceId::Wayland(DeviceId)),
            phase,
            location: self.location,
            facts: PointerEventFacts {
                surface_position: Some(self.location),
                geometry: Some(self.geometry),
                ..PointerEventFacts::default()
            },
            force: None,
            id: id as u64,
        }
    }
}

/// The last position and geometry observed for a live contact.
#[derive(Debug)]
pub struct TouchPoint {
    /// The surface on which the point is present.
    pub surface: WlSurface,

    /// The complete down/motion pair, retained unchanged for end/cancel.
    sample: TouchSample,
}

pub trait TouchDataExt {
    fn seat(&self) -> &WlSeat;
}

impl TouchDataExt for WlTouch {
    fn seat(&self) -> &WlSeat {
        self.data::<TouchData>().expect("failed to get touch data.").seat()
    }
}

sctk::delegate_touch!(WinitState);

#[cfg(test)]
mod tests {
    use crate::dpi::PhysicalSize;
    use crate::event::PointerWindowRoute;

    use super::*;

    #[test]
    fn touch_terminals_retain_the_down_pair_after_window_geometry_changes() {
        let logical = LogicalPosition::new(10.25, 20.5);
        let retained = TouchSample::new(logical, LogicalSize::new(101, 79), 1.0);
        let down = retained.into_touch(7, TouchPhase::Started);
        let newer_geometry = TouchSample::new(logical, LogicalSize::new(200, 150), 2.0);
        let newer = newer_geometry.into_touch(7, TouchPhase::Moved);
        assert_ne!(down.location, newer.location);
        assert_ne!(down.facts.geometry, newer.facts.geometry);

        for phase in [TouchPhase::Ended, TouchPhase::Cancelled] {
            let terminal = retained.into_touch(7, phase);
            assert_eq!(terminal.phase, phase);
            assert_eq!(terminal.id, down.id);
            assert_eq!(terminal.device_id, down.device_id);
            assert_eq!(terminal.location, down.location);
            assert_eq!(terminal.facts, down.facts);
            assert_eq!(terminal.facts.surface_position, Some(terminal.location));
            assert_eq!(terminal.facts.capture, PointerWindowRoute::Unknown);
            assert_eq!(terminal.facts.hover, PointerWindowRoute::Unknown);
            assert_eq!(terminal.facts.desktop_position, None);
            assert_eq!(terminal.facts.modifiers, None);
        }
    }

    #[test]
    fn touch_cancellation_preserves_each_contacts_own_pair() {
        let contacts = [
            (3, TouchSample::new(LogicalPosition::new(10.0, 20.0), LogicalSize::new(100, 80), 1.0)),
            (
                9,
                TouchSample::new(LogicalPosition::new(30.0, 40.0), LogicalSize::new(200, 160), 2.0),
            ),
        ];
        let first = contacts[0].1.into_touch(contacts[0].0, TouchPhase::Started);
        let second = contacts[1].1.into_touch(contacts[1].0, TouchPhase::Started);
        let cancelled = contacts.map(|(id, sample)| sample.into_touch(id, TouchPhase::Cancelled));

        for (terminal, down) in cancelled.into_iter().zip([first, second]) {
            assert_eq!(terminal.id, down.id);
            assert_eq!(terminal.phase, TouchPhase::Cancelled);
            assert_eq!(terminal.location, down.location);
            assert_eq!(terminal.facts, down.facts);
        }
        assert_ne!(first.location, second.location);
        assert_ne!(first.facts.geometry, second.facts.geometry);
    }

    #[test]
    fn touch_motion_replaces_the_complete_pair_retained_by_terminals() {
        let mut retained =
            TouchSample::new(LogicalPosition::new(10.0, 20.0), LogicalSize::new(100, 80), 1.0);
        let down = retained.into_touch(7, TouchPhase::Started);
        retained =
            TouchSample::new(LogicalPosition::new(12.5, 22.25), LogicalSize::new(200, 160), 1.5);
        let moved = retained.into_touch(7, TouchPhase::Moved);
        assert_ne!(moved.location, down.location);
        assert_ne!(moved.facts.geometry, down.facts.geometry);

        for phase in [TouchPhase::Ended, TouchPhase::Cancelled] {
            let terminal = retained.into_touch(7, phase);
            assert_eq!(terminal.location, moved.location);
            assert_eq!(terminal.facts, moved.facts);
            assert_eq!(terminal.facts.surface_position, Some(PhysicalPosition::new(18.75, 33.375)));
            assert_eq!(terminal.facts.geometry.unwrap().surface_size, PhysicalSize::new(300, 240));
            assert_eq!(terminal.facts.geometry.unwrap().scale_factor, 1.5);
        }
    }

    #[test]
    fn touch_events_keep_window_size_rounding_and_same_sample_position() {
        let sample =
            TouchSample::new(LogicalPosition::new(-2.5, 10.25), LogicalSize::new(101, 79), 1.5);
        for phase in
            [TouchPhase::Started, TouchPhase::Moved, TouchPhase::Ended, TouchPhase::Cancelled]
        {
            let event = sample.into_touch(11, phase);
            assert_eq!(event.id, 11);
            assert_eq!(event.phase, phase);
            assert_eq!(event.location, PhysicalPosition::new(-3.75, 15.375));
            assert_eq!(event.facts.surface_position, Some(event.location));
            assert_eq!(event.facts.geometry.unwrap().surface_size, PhysicalSize::new(152, 119));
            assert_eq!(event.facts.geometry.unwrap().scale_factor, 1.5);
            assert_eq!(event.force, None);
        }
    }
}
