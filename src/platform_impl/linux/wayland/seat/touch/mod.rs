//! Touch handling.

use ahash::AHashMap;
use tracing::warn;

use sctk::reexports::client::protocol::wl_seat::WlSeat;
use sctk::reexports::client::protocol::wl_surface::WlSurface;
use sctk::reexports::client::protocol::wl_touch::WlTouch;
use sctk::reexports::client::{Connection, Proxy, QueueHandle};

use sctk::seat::touch::{TouchData, TouchHandler};

use crate::dpi::{LogicalPosition, LogicalSize, PhysicalPosition};
use crate::event::{PointerEventFacts, PointerEventGeometry, Touch, TouchPhase, WindowEvent};

use crate::platform_impl::wayland::state::WinitState;
use crate::platform_impl::wayland::{self, DeviceId, WindowId};

/// One seat's current touch capability and the contacts owned by that incarnation.
#[derive(Debug)]
pub(super) struct TouchCapability {
    touch: WlTouch,
    device_id: DeviceId,
    contacts: AHashMap<i32, TouchPoint>,
}

impl TouchCapability {
    pub(super) fn new(touch: WlTouch) -> Option<Self> {
        match DeviceId::fresh_touch() {
            Some(device_id) => Some(Self { touch, device_id, contacts: AHashMap::default() }),
            None => {
                warn!("Touch device identities exhausted; ignoring new touch capability");
                if touch.version() >= 3 {
                    touch.release();
                }
                None
            },
        }
    }

    pub(super) fn matches(&self, touch: &WlTouch) -> bool {
        self.touch == *touch
    }

    fn end(&mut self, id: i32) -> Option<(WindowId, Touch)> {
        self.contacts.remove(&id).map(|point| {
            let window_id = wayland::make_wid(&point.surface);
            (window_id, point.sample.into_touch(self.device_id, id, TouchPhase::Ended))
        })
    }

    fn cancelled(&mut self) -> impl Iterator<Item = (WindowId, Touch)> + '_ {
        let device_id = self.device_id;
        self.contacts.drain().map(move |(id, point)| {
            let window_id = wayland::make_wid(&point.surface);
            (window_id, point.sample.into_touch(device_id, id, TouchPhase::Cancelled))
        })
    }
}

impl WinitState {
    pub(super) fn on_touch_destroy(&mut self, mut touch: TouchCapability) {
        for (window_id, event) in touch.cancelled() {
            if self.windows.get_mut().contains_key(&window_id) {
                self.events_sink.push_window_event(WindowEvent::Touch(event), window_id);
            }
        }

        if touch.touch.version() >= 3 {
            touch.touch.release();
        }
    }
}

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
        let capability = match self.seats.get_mut(&touch.seat().id()) {
            Some(seat_state) => match seat_state.touch_for(touch) {
                Some(capability) => capability,
                None => return,
            },
            None => {
                warn!("Received wl_touch::down without seat");
                return;
            },
        };

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

        // Update the state of the point.
        capability.contacts.insert(id, TouchPoint { surface, sample });

        self.events_sink.push_window_event(
            WindowEvent::Touch(sample.into_touch(capability.device_id, id, TouchPhase::Started)),
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
        let capability = match self.seats.get_mut(&touch.seat().id()) {
            Some(seat_state) => match seat_state.touch_for(touch) {
                Some(capability) => capability,
                None => return,
            },
            None => {
                warn!("Received wl_touch::up without seat");
                return;
            },
        };

        // Up has no position. Preserve the last down/motion pair without resampling geometry.
        let (window_id, event) = match capability.end(id) {
            Some(event) => event,
            None => return,
        };

        if !self.windows.get_mut().contains_key(&window_id) {
            return;
        }

        self.events_sink.push_window_event(WindowEvent::Touch(event), window_id);
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
        let capability = match self.seats.get_mut(&touch.seat().id()) {
            Some(seat_state) => match seat_state.touch_for(touch) {
                Some(capability) => capability,
                None => return,
            },
            None => {
                warn!("Received wl_touch::motion without seat");
                return;
            },
        };

        // Update the existing touch point.
        let touch_point = match capability.contacts.get_mut(&id) {
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
            WindowEvent::Touch(touch_point.sample.into_touch(
                capability.device_id,
                id,
                TouchPhase::Moved,
            )),
            window_id,
        );
    }

    fn cancel(&mut self, _: &Connection, _: &QueueHandle<Self>, touch: &WlTouch) {
        let capability = match self.seats.get_mut(&touch.seat().id()) {
            Some(seat_state) => match seat_state.touch_for(touch) {
                Some(capability) => capability,
                None => return,
            },
            None => {
                warn!("Received wl_touch::cancel without seat");
                return;
            },
        };

        for (window_id, event) in capability.cancelled() {
            if !self.windows.get_mut().contains_key(&window_id) {
                continue;
            }

            // Cancellation preserves each contact's last observed pair, not current geometry.
            self.events_sink.push_window_event(WindowEvent::Touch(event), window_id);
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
    fn into_touch(self, device_id: DeviceId, id: i32, phase: TouchPhase) -> Touch {
        Touch {
            device_id: crate::event::DeviceId(crate::platform_impl::DeviceId::Wayland(device_id)),
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
struct TouchPoint {
    /// The surface on which the point is present.
    surface: WlSurface,

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
mod tests;
