use std::os::unix::net::UnixStream;

use sctk::reexports::client::protocol::wl_compositor::WlCompositor;
use sctk::reexports::client::protocol::wl_registry::WlRegistry;
use sctk::reexports::client::Dispatch;

use crate::dpi::PhysicalSize;
use crate::event::PointerWindowRoute;

use super::super::WinitSeatState;
use super::*;

struct TestDispatch;

sctk::reexports::client::delegate_noop!(TestDispatch: WlRegistry);
sctk::reexports::client::delegate_noop!(TestDispatch: WlSeat);
sctk::reexports::client::delegate_noop!(TestDispatch: WlCompositor);
sctk::reexports::client::delegate_noop!(TestDispatch: WlSurface);

impl Dispatch<WlTouch, TouchData> for TestDispatch {
    fn event(
        _: &mut Self,
        _: &WlTouch,
        _: <WlTouch as Proxy>::Event,
        _: &TouchData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        unreachable!("This fixture only constructs local proxies; it never dispatches events");
    }
}

struct TouchFixture {
    _connection: Connection,
    _peer: UnixStream,
    registry: WlRegistry,
    compositor: WlCompositor,
    queue: QueueHandle<TestDispatch>,
}

impl TouchFixture {
    fn new() -> Self {
        // Proxy construction is local. No compositor, event dispatch, or timing is required.
        let (client, peer) = UnixStream::pair().unwrap();
        let connection = Connection::from_socket(client).unwrap();
        let queue = connection.new_event_queue::<TestDispatch>().handle();
        let registry = connection.display().get_registry(&queue, ());
        let compositor = registry.bind::<WlCompositor, _, _>(1, 4, &queue, ());
        Self { _connection: connection, _peer: peer, registry, compositor, queue }
    }

    fn seat(&self, name: u32) -> WlSeat {
        self.registry.bind(name, 7, &self.queue, ())
    }

    fn touch(&self, seat: &WlSeat) -> WlTouch {
        seat.get_touch(&self.queue, TouchData::new(seat.clone()))
    }

    fn surface(&self) -> WlSurface {
        self.compositor.create_surface(&self.queue, ())
    }
}

#[test]
fn touch_devices_with_matching_contact_ids_have_independent_terminals() {
    let fixture = TouchFixture::new();
    let first_seat = fixture.seat(2);
    let second_seat = fixture.seat(3);
    let mut first = TouchCapability::new(fixture.touch(&first_seat)).unwrap();
    let mut second = TouchCapability::new(fixture.touch(&second_seat)).unwrap();
    let first_sample =
        TouchSample::new(LogicalPosition::new(10.0, 20.0), LogicalSize::new(100, 80), 1.0);
    let second_sample =
        TouchSample::new(LogicalPosition::new(30.0, 40.0), LogicalSize::new(200, 160), 2.0);
    let first_surface = fixture.surface();
    let second_surface = fixture.surface();
    first.contacts.insert(7, TouchPoint { surface: first_surface.clone(), sample: first_sample });
    second
        .contacts
        .insert(7, TouchPoint { surface: second_surface.clone(), sample: second_sample });

    let first_down = first_sample.into_touch(first.device_id, 7, TouchPhase::Started);
    let second_down = second_sample.into_touch(second.device_id, 7, TouchPhase::Started);
    assert_eq!(first_down.id, second_down.id);
    assert_ne!(first_down.device_id, second_down.device_id);
    assert_ne!(first.device_id, DeviceId::dummy());
    assert_ne!(second.device_id, DeviceId::dummy());

    let (window_id, ended) = first.end(7).unwrap();
    assert_eq!(window_id, wayland::make_wid(&first_surface));
    assert_eq!(ended.phase, TouchPhase::Ended);
    assert_eq!(ended.device_id, first_down.device_id);
    assert_eq!(ended.facts, first_down.facts);
    assert!(first.end(7).is_none());
    assert_eq!(first.cancelled().count(), 0);
    assert_eq!(second.contacts.len(), 1);

    let cancelled: Vec<_> = second.cancelled().collect();
    assert_eq!(cancelled.len(), 1);
    assert_eq!(cancelled[0].0, wayland::make_wid(&second_surface));
    assert_eq!(cancelled[0].1.phase, TouchPhase::Cancelled);
    assert_eq!(cancelled[0].1.device_id, second_down.device_id);
    assert_eq!(cancelled[0].1.facts, second_down.facts);
    assert!(second.end(7).is_none());
    assert_eq!(second.cancelled().count(), 0);
}

#[test]
fn touch_capability_removal_cancels_each_contact_once_with_its_frozen_sample() {
    let fixture = TouchFixture::new();
    let seat = fixture.seat(2);
    let touch = fixture.touch(&seat);
    let mut state = WinitSeatState::new();
    state.touch = TouchCapability::new(touch.clone());
    let capability = state.touch_for(&touch).unwrap();
    let expected: Vec<_> = [
        (3, LogicalPosition::new(10.0, 20.0), LogicalSize::new(100, 80), 1.0),
        (9, LogicalPosition::new(30.0, 40.0), LogicalSize::new(200, 160), 2.0),
    ]
    .into_iter()
    .map(|(id, position, size, scale)| {
        let surface = fixture.surface();
        let sample = TouchSample::new(position, size, scale);
        let expected = sample.into_touch(capability.device_id, id, TouchPhase::Cancelled);
        let window_id = wayland::make_wid(&surface);
        capability.contacts.insert(id, TouchPoint { surface, sample });
        (window_id, expected)
    })
    .collect();

    // Capability loss and seat removal both detach this same owner before draining it.
    let mut retired = state.touch.take().unwrap();
    assert!(state.touch_for(&touch).is_none());
    assert!(state.touch.take().is_none());
    let mut cancelled: Vec<_> = retired.cancelled().collect();
    cancelled.sort_by_key(|(_, event)| event.id);
    assert_eq!(cancelled, expected);
    assert!(retired.contacts.is_empty());
    assert_eq!(retired.cancelled().count(), 0);
    assert!(retired.end(3).is_none());
    assert!(retired.end(9).is_none());
}

#[test]
fn touch_capability_recreation_rejects_old_objects_and_allocates_a_new_device_id() {
    let fixture = TouchFixture::new();
    let seat = fixture.seat(2);
    let old_touch = fixture.touch(&seat);
    let mut state = WinitSeatState::new();
    state.touch = TouchCapability::new(old_touch.clone());
    let old_device_id = state.touch_for(&old_touch).unwrap().device_id;
    let sample = TouchSample::new(LogicalPosition::new(10.0, 20.0), LogicalSize::new(100, 80), 1.5);
    state
        .touch_for(&old_touch)
        .unwrap()
        .contacts
        .insert(7, TouchPoint { surface: fixture.surface(), sample });
    let mut retired = state.touch.take().unwrap();
    assert_eq!(retired.cancelled().count(), 1);
    old_touch.release();

    let current_touch = fixture.touch(&seat);
    assert_eq!(old_touch.seat(), current_touch.seat());
    state.touch = TouchCapability::new(current_touch.clone());
    let current = state.touch_for(&current_touch).unwrap();
    let current_device_id = current.device_id;
    assert_ne!(current_device_id, old_device_id);
    current.contacts.insert(7, TouchPoint { surface: fixture.surface(), sample });

    // All incoming phases use this guard, so late old down/up/motion/cancel cannot touch contact 7.
    assert!(state.touch_for(&old_touch).is_none());
    let current = state.touch_for(&current_touch).unwrap();
    assert_eq!(current.contacts.len(), 1);
    let (_, ended) = current.end(7).unwrap();
    assert_eq!(ended, sample.into_touch(current_device_id, 7, TouchPhase::Ended));
    assert_eq!(current.cancelled().count(), 0);

    let other_connection = TouchFixture::new();
    let other_seat = other_connection.seat(2);
    let other = TouchCapability::new(other_connection.touch(&other_seat)).unwrap();
    assert_ne!(other.device_id, old_device_id);
    assert_ne!(other.device_id, current_device_id);
}

#[test]
fn touch_terminals_retain_the_down_pair_after_window_geometry_changes() {
    let device_id = DeviceId::fresh_touch().unwrap();
    let logical = LogicalPosition::new(10.25, 20.5);
    let retained = TouchSample::new(logical, LogicalSize::new(101, 79), 1.0);
    let down = retained.into_touch(device_id, 7, TouchPhase::Started);
    let newer_geometry = TouchSample::new(logical, LogicalSize::new(200, 150), 2.0);
    let newer = newer_geometry.into_touch(device_id, 7, TouchPhase::Moved);
    assert_ne!(down.location, newer.location);
    assert_ne!(down.facts.geometry, newer.facts.geometry);

    for phase in [TouchPhase::Ended, TouchPhase::Cancelled] {
        let terminal = retained.into_touch(device_id, 7, phase);
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
    let device_id = DeviceId::fresh_touch().unwrap();
    let contacts = [
        (3, TouchSample::new(LogicalPosition::new(10.0, 20.0), LogicalSize::new(100, 80), 1.0)),
        (9, TouchSample::new(LogicalPosition::new(30.0, 40.0), LogicalSize::new(200, 160), 2.0)),
    ];
    let first = contacts[0].1.into_touch(device_id, contacts[0].0, TouchPhase::Started);
    let second = contacts[1].1.into_touch(device_id, contacts[1].0, TouchPhase::Started);
    let cancelled =
        contacts.map(|(id, sample)| sample.into_touch(device_id, id, TouchPhase::Cancelled));

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
    let device_id = DeviceId::fresh_touch().unwrap();
    let mut retained =
        TouchSample::new(LogicalPosition::new(10.0, 20.0), LogicalSize::new(100, 80), 1.0);
    let down = retained.into_touch(device_id, 7, TouchPhase::Started);
    retained = TouchSample::new(LogicalPosition::new(12.5, 22.25), LogicalSize::new(200, 160), 1.5);
    let moved = retained.into_touch(device_id, 7, TouchPhase::Moved);
    assert_ne!(moved.location, down.location);
    assert_ne!(moved.facts.geometry, down.facts.geometry);

    for phase in [TouchPhase::Ended, TouchPhase::Cancelled] {
        let terminal = retained.into_touch(device_id, 7, phase);
        assert_eq!(terminal.location, moved.location);
        assert_eq!(terminal.facts, moved.facts);
        assert_eq!(terminal.facts.surface_position, Some(PhysicalPosition::new(18.75, 33.375)));
        assert_eq!(terminal.facts.geometry.unwrap().surface_size, PhysicalSize::new(300, 240));
        assert_eq!(terminal.facts.geometry.unwrap().scale_factor, 1.5);
    }
}

#[test]
fn touch_events_keep_window_size_rounding_and_same_sample_position() {
    let device_id = DeviceId::fresh_touch().unwrap();
    let sample =
        TouchSample::new(LogicalPosition::new(-2.5, 10.25), LogicalSize::new(101, 79), 1.5);
    for phase in [TouchPhase::Started, TouchPhase::Moved, TouchPhase::Ended, TouchPhase::Cancelled]
    {
        let event = sample.into_touch(device_id, 11, phase);
        assert_eq!(event.id, 11);
        assert_eq!(event.phase, phase);
        assert_eq!(event.location, PhysicalPosition::new(-3.75, 15.375));
        assert_eq!(event.facts.surface_position, Some(event.location));
        assert_eq!(event.facts.geometry.unwrap().surface_size, PhysicalSize::new(152, 119));
        assert_eq!(event.facts.geometry.unwrap().scale_factor, 1.5);
        assert_eq!(event.force, None);
    }
}
