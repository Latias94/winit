//! Opt-in native interleavings on one isolated X11 event loop.

#![cfg(all(target_os = "linux", feature = "x11"))]

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use x11rb::connection::Connection as _;
use x11rb::protocol::xinput::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{
    ChangeWindowAttributesAux, ConnectionExt as _, CreateWindowAux, MapState, WindowClass,
};
use x11rb::rust_connection::RustConnection;

use super::CaptureSampleCheckpoint;
use crate::application::ApplicationHandler;
use crate::dpi::{PhysicalPosition, PhysicalSize};
use crate::event::{
    DeviceId, ElementState, MouseButton, PointerPressToken, PointerWindowRoute, WindowEvent,
};
use crate::event_loop::{ActiveEventLoop, EventLoop};
use crate::platform::pump_events::EventLoopExtPumpEvents;
use crate::platform::x11::{
    ActiveEventLoopExtX11, EventLoopBuilderExtX11, WindowAttributesExtX11,
    X11PointerCaptureAcquisition as Acquisition, X11PointerCaptureRelease as Release,
    X11PointerObservation,
};
use crate::window::{CursorGrabMode, Window, WindowId};

const OPT_IN: &str = "WINIT_X11_POINTER_INTERLEAVING_TEST";
const MANAGED_OPT_IN: &str = "WINIT_X11_POINTER_ANCESTOR_UNMAP_TEST";
const TIMEOUT: Duration = Duration::from_secs(5);
const TICK: Duration = Duration::from_millis(5);

fn inject(arguments: &[&str]) -> Result<(), String> {
    // Bound the injector as well as the pump, including during panic cleanup.
    let mut child = Command::new("xdotool")
        .args(arguments)
        .stdout(Stdio::null())
        .spawn()
        .map_err(|error| format!("xdotool {arguments:?}: {error}"))?;
    let deadline = Instant::now() + Duration::from_secs(2);
    let result = loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break Ok(()),
            Ok(Some(status)) => break Err(format!("xdotool {arguments:?}: {status}")),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(TICK),
            Ok(None) => break Err(format!("xdotool {arguments:?} timed out")),
            Err(error) => break Err(format!("xdotool {arguments:?}: {error}")),
        }
    };
    if result.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    result
}

#[derive(Clone, Debug)]
struct PrimaryPress {
    device: DeviceId,
    token: PointerPressToken,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sample {
    Fresh,
    OrdinaryGrabs,
    Hide(CaptureSampleCheckpoint),
    AncestorUnmap(CaptureSampleCheckpoint),
    RevokedWhileHidden,
    Revoked,
    NativeEntry,
    ForeignInputOnly,
}

struct ForeignInputOnly {
    connection: RustConnection,
    window: u32,
}

impl Drop for ForeignInputOnly {
    fn drop(&mut self) {
        if let Err(error) = self
            .connection
            .destroy_window(self.window)
            .map_err(x11rb::errors::ReplyError::from)
            .and_then(|request| request.check())
        {
            eprintln!("foreign InputOnly cleanup: {error}");
        }
        // Dropping this distinct connection also destroys any indeterminate native resource.
    }
}

struct ForeignAncestor {
    connection: RustConnection,
    source: u32,
    frame: u32,
    override_redirect: bool,
    unmapped: bool,
}

impl ForeignAncestor {
    fn remap(&mut self) -> Result<(), x11rb::errors::ReplyError> {
        if self.unmapped {
            // A foreign client's MapWindow on the WM frame can be redirected back to the WM.
            // Bypass that restoration request only, then restore the frame's original flag.
            // No Winit mutation may invalidate the identity this test is trying to inspect.
            if !self.override_redirect {
                self.connection
                    .change_window_attributes(
                        self.frame,
                        &ChangeWindowAttributesAux::new().override_redirect(1),
                    )?
                    .check()?;
            }
            let mapped = self
                .connection
                .map_window(self.frame)
                .map_err(x11rb::errors::ReplyError::from)
                .and_then(|request| request.check());
            let restored = if self.override_redirect {
                Ok(())
            } else {
                self.connection
                    .change_window_attributes(
                        self.frame,
                        &ChangeWindowAttributesAux::new().override_redirect(0),
                    )
                    .map_err(x11rb::errors::ReplyError::from)
                    .and_then(|request| request.check())
            };
            mapped?;
            restored?;
            self.unmapped = false;
        }
        Ok(())
    }
}

impl Drop for ForeignAncestor {
    fn drop(&mut self) {
        if let Err(error) = self.remap() {
            eprintln!("foreign ancestor remap cleanup: {error}");
        }
    }
}

#[derive(Default)]
struct Fixture {
    managed: bool,
    source: Option<Window>,
    peer: Option<Window>,
    presses: Vec<PrimaryPress>,
    releases: Vec<(WindowId, DeviceId)>,
    polls: usize,
    acquisition: Option<PrimaryPress>,
    acquisition_was_pending: bool,
    sample: Option<(PrimaryPress, Sample)>,
    release: Option<(PrimaryPress, Release)>,
    unmapped_ancestor: Option<ForeignAncestor>,
}

fn assert_observed(
    observation: X11PointerObservation,
    source: WindowId,
    capture: PointerWindowRoute,
) {
    let X11PointerObservation::Observed { facts, delivery } = observation else {
        panic!("expected a complete source sample, got {observation:?}");
    };
    assert!(facts.desktop_position.is_some());
    assert_eq!(facts.hover, PointerWindowRoute::Window(source));
    assert_eq!(facts.capture, capture);
    assert_eq!(delivery, capture);
    assert!(facts.surface_position.is_none());
    assert!(facts.geometry.is_none());
}

fn queued_events(inner: &super::ActiveEventLoop) -> i32 {
    // SAFETY: this is the live event-loop-thread display. XPending reads but never drains input.
    unsafe { (inner.xconn.xlib.XPending)(inner.xconn.display) }
}

fn queued_xi_crossing(inner: &super::ActiveEventLoop) -> bool {
    struct Probe {
        extension: i32,
        found: bool,
    }

    unsafe extern "C" fn inspect(
        _display: *mut super::super::ffi::Display,
        event: *mut super::super::ffi::XEvent,
        probe: *mut std::os::raw::c_char,
    ) -> std::os::raw::c_int {
        // SAFETY: XCheckIfEvent supplies a live event for this call and the caller retains
        // its stack Probe throughout. Inspect the cookie header only, never its payload.
        let (event, probe) = unsafe { (&*event, &mut *probe.cast::<Probe>()) };
        if event.get_type() == super::super::ffi::GenericEvent {
            // SAFETY: GenericEvent selects this union member.
            let cookie = unsafe { event.generic_event_cookie };
            probe.found |= cookie.extension == probe.extension
                && matches!(
                    cookie.evtype,
                    super::super::ffi::XI_Enter | super::super::ffi::XI_Leave
                );
        }
        // An always-false predicate leaves every event and cookie in the original FIFO.
        0
    }

    let extension = inner
        .xconn
        .xcb_connection()
        .query_extension(xinput::X11_EXTENSION_NAME.as_bytes())
        .unwrap()
        .reply()
        .unwrap();
    assert!(extension.present);
    let mut probe = Probe { extension: i32::from(extension.major_opcode), found: false };
    let mut ignored = std::mem::MaybeUninit::<super::super::ffi::XEvent>::uninit();
    // SAFETY: this is the live event-loop-thread display; the callback never selects an event,
    // dispatches native work or calls Xlib. No uninitialized output is read after this call.
    let selected = unsafe {
        (inner.xconn.xlib.XCheckIfEvent)(
            inner.xconn.display,
            ignored.as_mut_ptr(),
            Some(inspect),
            (&mut probe as *mut Probe).cast(),
        )
    };
    assert_eq!(selected, 0);
    probe.found
}

impl ApplicationHandler for Fixture {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.source.is_some() {
            return;
        }
        let attributes = Window::default_attributes()
            .with_inner_size(PhysicalSize::new(200_u32, 160_u32))
            .with_decorations(self.managed)
            .with_override_redirect(!self.managed);
        self.source = Some(
            event_loop
                .create_window(
                    attributes
                        .clone()
                        .with_title("capture sample interleaving source")
                        .with_position(PhysicalPosition::new(32, 64)),
                )
                .expect("create the interleaving source"),
        );
        self.peer = Some(
            event_loop
                .create_window(
                    attributes
                        .with_title("capture sample interleaving peer")
                        .with_position(PhysicalPosition::new(312, 64)),
                )
                .expect("create the interleaving peer"),
        );
    }

    fn window_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::MouseInput {
                device_id,
                button: MouseButton::Left,
                state: ElementState::Pressed,
                primary_press,
                ..
            } => {
                assert_eq!(Some(window_id), self.source.as_ref().map(Window::id));
                self.presses.push(PrimaryPress {
                    device: device_id,
                    token: primary_press.expect("only a validated real press supplies identity"),
                });
            },
            WindowEvent::MouseInput {
                device_id,
                button: MouseButton::Left,
                state: ElementState::Released,
                ..
            } => self.releases.push((window_id, device_id)),
            _ => (),
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.polls += 1;
        let Some(source) = self.source.as_ref() else {
            return;
        };
        let source_id = source.id();
        if let Some(press) = &self.acquisition {
            match event_loop.acquire_pointer_capture(source_id, press.device, &press.token) {
                Acquisition::Pending => self.acquisition_was_pending = true,
                Acquisition::Acquired => {
                    assert!(self.acquisition_was_pending, "acquisition must cross normal FIFO");
                    // An old exact release cannot complete twice or ungrab this successor.
                    for previous in &self.presses[..self.presses.len() - 1] {
                        assert_eq!(
                            event_loop.release_pointer_capture(
                                source_id,
                                previous.device,
                                &previous.token,
                            ),
                            Release::Unavailable,
                        );
                    }
                    self.acquisition = None;
                },
                result => panic!("real press acquisition failed: {result:?}"),
            }
            return;
        }
        if let Some((press, expected)) = &self.release {
            let result = event_loop.release_pointer_capture(source_id, press.device, &press.token);
            if result == Release::Pending {
                return;
            }
            assert_eq!(result, *expected);
            assert_eq!(
                event_loop.release_pointer_capture(source_id, press.device, &press.token),
                Release::Unavailable,
                "the original lease has exactly one terminal completion",
            );
            assert_eq!(
                event_loop.observe_pointer(source_id, press.device, Some(&press.token)),
                X11PointerObservation::Unavailable,
                "a retired original press cannot supply even an Unknown observation",
            );
            self.release = None;
            return;
        }
        let Some((press, sample)) = &self.sample else {
            return;
        };
        let inner = match &event_loop.p {
            crate::platform_impl::ActiveEventLoop::X(inner) => inner,
            #[cfg(wayland_platform)]
            _ => panic!("the isolated fixture must use X11"),
        };
        if *sample == Sample::RevokedWhileHidden {
            if inner.xconn.pointer_observation_blocked() {
                return;
            }
            let device = match press.device.0 {
                crate::platform_impl::DeviceId::X(device) => device.0,
                #[cfg(wayland_platform)]
                _ => panic!("the press must belong to the X11 master"),
            };
            assert!(
                !inner.xconn.validate_primary_press(
                    u32::try_from(u64::from(source_id)).unwrap(),
                    device,
                    &press.token,
                ),
                "ancestor loss must revoke the original identity before any restoration",
            );
            assert!(matches!(
                inner.xconn.pointer_captures().explicit_capture_phase(device),
                super::super::pointer_facts::ExplicitCapturePhase::Retiring
                    | super::super::pointer_facts::ExplicitCapturePhase::Revoked
                    | super::super::pointer_facts::ExplicitCapturePhase::Unacquired
            ));
            self.sample = None;
            return;
        }
        if *sample == Sample::Revoked {
            let ordinary = event_loop.observe_pointer(source_id, press.device, None);
            if ordinary == X11PointerObservation::Unavailable {
                return;
            }
            // A successful ordinary sample excludes a merely pending queue or hidden source.
            assert_observed(ordinary, source_id, PointerWindowRoute::Unknown);
            assert_eq!(
                event_loop.acquire_pointer_capture(source_id, press.device, &press.token),
                Acquisition::Unavailable,
                "ancestor remap must not renew the original acquisition",
            );
            assert_eq!(
                event_loop.observe_pointer(source_id, press.device, Some(&press.token)),
                X11PointerObservation::Unavailable,
                "ancestor remap must not restore the original press observation",
            );
            self.sample = None;
            return;
        }
        if *sample == Sample::Fresh {
            let ordinary = event_loop.observe_pointer(source_id, press.device, None);
            if ordinary == X11PointerObservation::Unavailable {
                return;
            }
            assert_observed(ordinary, source_id, PointerWindowRoute::Unknown);
            let qualified = event_loop.observe_pointer(source_id, press.device, Some(&press.token));
            if qualified == X11PointerObservation::Unavailable {
                return;
            }
            assert_observed(qualified, source_id, PointerWindowRoute::Window(source_id));
            self.sample = None;
            return;
        }
        let mut checkpoints = Vec::new();
        let mut fault_seen = false;
        let mut foreign = None;
        let mut ancestor = None;
        let observation = inner.observe_pointer_inner(
            source_id,
            press.device,
            Some(&press.token),
            |checkpoint| {
                checkpoints.push(checkpoint);
                match (*sample, checkpoint) {
                    (Sample::OrdinaryGrabs, CaptureSampleCheckpoint::PositionQueried) => {
                        for window in [source, self.peer.as_ref().unwrap()] {
                            assert!(window.set_cursor_grab(CursorGrabMode::Confined).is_err());
                            // Both windows start with cached None. This succeeds without ungrab.
                            assert!(window.set_cursor_grab(CursorGrabMode::None).is_ok());
                        }
                        fault_seen = true;
                    },
                    (Sample::Hide(expected), checkpoint) if expected == checkpoint => {
                        source.set_visible(false);
                        for _ in 0..2 {
                            assert_eq!(
                                event_loop.release_pointer_capture(
                                    source_id,
                                    press.device,
                                    &press.token,
                                ),
                                Release::Pending,
                                "Sampling retirement must retain the in-flight native duty",
                            );
                        }
                        fault_seen = true;
                    },
                    (Sample::AncestorUnmap(expected), checkpoint) if expected == checkpoint => {
                        assert!(self.managed, "only the managed fixture may unmap a WM frame");
                        let (connection, _) =
                            x11rb::connect(None).expect("open the independent ancestor client");
                        let window = u32::try_from(u64::from(source_id)).unwrap();
                        let tree = connection.query_tree(window).unwrap().reply().unwrap();
                        assert_eq!(tree.root, inner.root);
                        assert_ne!(tree.parent, tree.root, "the source must truly be reparented");
                        let attributes =
                            connection.get_window_attributes(window).unwrap().reply().unwrap();
                        assert!(!attributes.override_redirect);
                        assert_eq!(attributes.map_state, MapState::VIEWABLE);
                        let mut guard = ForeignAncestor {
                            override_redirect: connection
                                .get_window_attributes(tree.parent)
                                .unwrap()
                                .reply()
                                .unwrap()
                                .override_redirect,
                            connection,
                            source: window,
                            frame: tree.parent,
                            unmapped: false,
                        };
                        // Retain remap cleanup before dispatch, including an indeterminate unmap.
                        guard.unmapped = true;
                        guard.connection.unmap_window(guard.frame).unwrap().check().unwrap();
                        let source_attributes = guard
                            .connection
                            .get_window_attributes(guard.source)
                            .unwrap()
                            .reply()
                            .unwrap();
                        // UNVIEWABLE means the source itself is still mapped; an ancestor is not.
                        assert_eq!(source_attributes.map_state, MapState::UNVIEWABLE);
                        assert_eq!(
                            guard
                                .connection
                                .get_window_attributes(guard.frame)
                                .unwrap()
                                .reply()
                                .unwrap()
                                .map_state,
                            MapState::UNMAPPED,
                        );
                        assert_eq!(
                            guard
                                .connection
                                .query_tree(guard.source)
                                .unwrap()
                                .reply()
                                .unwrap()
                                .parent,
                            guard.frame,
                            "unmapping the frame must not reparent or unmap the source itself",
                        );
                        ancestor = Some(guard);
                        fault_seen = true;
                    },
                    (Sample::NativeEntry, CaptureSampleCheckpoint::CaptureRefreshed) => {
                        assert_eq!(queued_events(inner), 0, "isolate native interval history");
                        let native_entry = inner.xconn.begin_native_event();
                        drop(native_entry);
                        assert_eq!(queued_events(inner), 0, "the nested entry consumed no event");
                        fault_seen = true;
                    },
                    (Sample::ForeignInputOnly, CaptureSampleCheckpoint::PositionQueried) => {
                        assert_eq!(queued_events(inner), 0, "start before the foreign crossing");
                        let (connection, _) =
                            x11rb::connect(None).expect("open a distinct foreign X11 client");
                        assert_eq!(
                            connection
                                .xinput_xi_query_version(2, 0)
                                .expect("negotiate the foreign client's XI2 version")
                                .reply()
                                .unwrap()
                                .major_version,
                            2
                        );
                        let window = connection.generate_id().expect("allocate an InputOnly XID");
                        let guard = ForeignInputOnly { connection, window };
                        guard
                            .connection
                            .create_window(
                                0,
                                window,
                                inner.root,
                                72,
                                114,
                                16,
                                16,
                                0,
                                WindowClass::INPUT_ONLY,
                                0,
                                &CreateWindowAux::new().override_redirect(1),
                            )
                            .expect("create the foreign direct-root InputOnly child")
                            .check()
                            .expect("check the foreign InputOnly child");
                        guard.connection.map_window(window).expect("map over the source point");
                        guard
                            .connection
                            .get_input_focus()
                            .expect("request a foreign map round trip")
                            .reply()
                            .expect("settle the foreign map before G2");
                        let devices = guard
                            .connection
                            .xinput_xi_query_device(xinput::Device::ALL_MASTER)
                            .expect("query the isolated master pointer")
                            .reply()
                            .unwrap();
                        let pointer = devices
                            .infos
                            .iter()
                            .find(|info| {
                                info.enabled && info.type_ == xinput::DeviceType::MASTER_POINTER
                            })
                            .expect("the isolated display has an enabled master pointer");
                        assert_eq!(
                            devices
                                .infos
                                .iter()
                                .filter(|info| info.enabled
                                    && info.type_ == xinput::DeviceType::MASTER_POINTER)
                                .count(),
                            1
                        );
                        assert_eq!(
                            guard
                                .connection
                                .xinput_xi_query_pointer(inner.root, pointer.deviceid)
                                .expect("query the changed root child")
                                .reply()
                                .unwrap()
                                .child,
                            window,
                            "the foreign map must actually replace Q1's hover subtree"
                        );
                        assert!(queued_events(inner) > 0, "mapping must queue selected crossing");
                        // Root Create/Map notifications can precede the selected crossing.
                        // Locate its header without consuming or reordering that earlier prefix.
                        let queued = queued_events(inner);
                        assert!(queued_xi_crossing(inner));
                        assert!(queued_events(inner) >= queued);
                        foreign = Some(guard);
                        fault_seen = true;
                    },
                    _ => (),
                }
            },
        );
        if checkpoints.is_empty() {
            assert_eq!(observation, X11PointerObservation::Unavailable);
            return;
        }
        assert!(fault_seen, "the real sample must reach its requested checkpoint");
        if matches!(
            sample,
            Sample::Hide(CaptureSampleCheckpoint::PositionQueried)
                | Sample::AncestorUnmap(CaptureSampleCheckpoint::PositionQueried)
        ) {
            // Unmapping before G2 prevents a successful refresh and its checkpoint.
            assert_eq!(checkpoints, [CaptureSampleCheckpoint::PositionQueried]);
        } else {
            assert_eq!(checkpoints, [
                CaptureSampleCheckpoint::PositionQueried,
                CaptureSampleCheckpoint::CaptureRefreshed
            ]);
        }
        if matches!(sample, Sample::AncestorUnmap(_)) {
            match observation {
                X11PointerObservation::Unavailable => (),
                X11PointerObservation::Observed { facts, delivery } => assert!(
                    !matches!(
                        (facts.capture, delivery),
                        (PointerWindowRoute::Window(_), PointerWindowRoute::Window(_))
                    ),
                    "ancestor unmap must reject the sample or withhold capture/delivery: \
                     {facts:?}, {delivery:?}",
                ),
                X11PointerObservation::Unsupported => panic!("the managed source uses X11"),
            }
        } else if *sample == Sample::OrdinaryGrabs {
            assert_observed(observation, source_id, PointerWindowRoute::Window(source_id));
        } else {
            assert_eq!(
                observation,
                X11PointerObservation::Unavailable,
                "reject the whole sample, including its old Q1 position and Unknown routes",
            );
        }
        if *sample == Sample::NativeEntry {
            assert_eq!(queued_events(inner), 0, "history rejects an otherwise empty final queue");
            assert_eq!(
                event_loop.acquire_pointer_capture(source_id, press.device, &press.token),
                Acquisition::Acquired,
                "the exact original press and capture survive the nested interval",
            );
        }
        if *sample == Sample::ForeignInputOnly {
            assert!(queued_events(inner) > 0, "the outer sample must not drain native events");
        }
        drop(foreign);
        if let Some(ancestor) = ancestor {
            assert!(self.unmapped_ancestor.is_none());
            self.unmapped_ancestor = Some(ancestor);
        }
        self.sample = None;
    }
}

// Keep live windows and the one owning event loop available through panic cleanup.
struct Session {
    fixture: Fixture,
    event_loop: EventLoop<()>,
}

struct Cleanup {
    source: WindowId,
    presses: Vec<PrimaryPress>,
}

impl ApplicationHandler for Cleanup {
    fn resumed(&mut self, _event_loop: &ActiveEventLoop) {}

    fn window_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        _event: WindowEvent,
    ) {
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.presses.retain(|press| {
            event_loop.release_pointer_capture(self.source, press.device, &press.token)
                == Release::Pending
        });
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Restore a hidden frame before moving through the ordinary exact-duty cleanup.
        if let Some(mut ancestor) = self.fixture.unmapped_ancestor.take() {
            if let Err(error) = ancestor.remap() {
                eprintln!("capture interleaving ancestor cleanup: {error}");
            }
        }
        if let Err(error) = inject(&[
            "mouseup",
            "1",
            "mouseup",
            "2",
            "mouseup",
            "3",
            "mouseup",
            "4",
            "mouseup",
            "5",
            "mouseup",
            "6",
            "mouseup",
            "7",
            "mouseup",
            "8",
            "mouseup",
            "9",
            "getmouselocation",
        ]) {
            eprintln!("capture interleaving button cleanup: {error}");
        }
        let Some(source) = self.fixture.source.as_ref() else {
            return;
        };
        let mut cleanup = Cleanup { source: source.id(), presses: self.fixture.presses.clone() };
        let deadline = Instant::now() + TIMEOUT;
        loop {
            // This handler neither asserts nor retries acquisition while unwinding.
            self.event_loop.pump_app_events(Some(TICK), &mut cleanup);
            let source_free = source.set_cursor_grab(CursorGrabMode::None).is_ok();
            let peer_free = self
                .fixture
                .peer
                .as_ref()
                .map_or(true, |peer| peer.set_cursor_grab(CursorGrabMode::None).is_ok());
            if cleanup.presses.is_empty() && source_free && peer_free {
                break;
            }
            if Instant::now() >= deadline {
                eprintln!(
                    "capture interleaving cleanup timed out: {} leases",
                    cleanup.presses.len()
                );
                break;
            }
        }
        self.event_loop.pump_app_events(Some(Duration::ZERO), &mut cleanup);
    }
}

fn pump_until(session: &mut Session, description: &str, ready: impl Fn(&Fixture) -> bool) {
    let deadline = Instant::now() + TIMEOUT;
    while !ready(&session.fixture) {
        assert!(Instant::now() < deadline, "timed out waiting for {description}");
        session.event_loop.pump_app_events(Some(TICK), &mut session.fixture);
    }
    session.event_loop.pump_app_events(Some(Duration::ZERO), &mut session.fixture);
}

fn flush_polls(session: &mut Session) {
    let polls = session.fixture.polls + 2;
    pump_until(session, "normal FIFO cleanup", |fixture| fixture.polls >= polls);
}

fn sample(session: &mut Session, press: &PrimaryPress, kind: Sample) {
    assert!(session.fixture.sample.is_none());
    session.fixture.sample = Some((press.clone(), kind));
    pump_until(session, &format!("{kind:?} sample"), |fixture| fixture.sample.is_none());
}

fn press_primary(session: &mut Session) -> PrimaryPress {
    assert_eq!(session.fixture.presses.len(), session.fixture.releases.len());
    let count = session.fixture.presses.len();
    let source = u64::from(session.fixture.source.as_ref().unwrap().id()).to_string();
    inject(&["mousemove", "--window", &source, "40", "50", "mousedown", "1", "getmouselocation"])
        .expect("inject a real XTest Primary press");
    pump_until(session, "validated real Primary press", |fixture| fixture.presses.len() > count);
    assert_eq!(session.fixture.presses.len(), count + 1);
    let press = session.fixture.presses[count].clone();
    for previous in &session.fixture.presses[..count] {
        assert_eq!(press.device, previous.device);
        assert_ne!(press.token, previous.token, "each physical edge has a new original identity");
    }
    session.fixture.acquisition_was_pending = false;
    session.fixture.acquisition = Some(press.clone());
    pump_until(session, "Pending then Acquired", |fixture| fixture.acquisition.is_none());
    sample(session, &press, Sample::Fresh);
    press
}

fn finish_press(session: &mut Session, press: &PrimaryPress, expected: Release) {
    flush_polls(session);
    session.fixture.release = Some((press.clone(), expected));
    pump_until(session, "exact original lease retirement", |fixture| fixture.release.is_none());
    session.fixture.source.as_ref().unwrap().set_visible(true);
    pump_until(session, "source remapped before all-up", |fixture| {
        fixture.source.as_ref().unwrap().is_visible() == Some(true)
    });
    let releases = session.fixture.releases.len();
    let source_id = session.fixture.source.as_ref().unwrap().id();
    let source = u64::from(source_id).to_string();
    inject(&["mousemove", "--window", &source, "40", "50", "mouseup", "1", "getmouselocation"])
        .expect("finish the real Primary stream at all-up");
    pump_until(session, "one real Primary release", |fixture| fixture.releases.len() > releases);
    flush_polls(session);
    assert_eq!(session.fixture.releases.len(), releases + 1, "no duplicate physical terminal");
    assert_eq!(session.fixture.releases[releases], (source_id, press.device));
    assert_eq!(session.fixture.releases.len(), session.fixture.presses.len());
}

#[test]
#[ignore = "isolated X11/XTest, xdotool and WINIT_X11_POINTER_INTERLEAVING_TEST=1"]
fn explicit_capture_sample_rejects_native_interleavings() {
    assert_eq!(
        std::env::var(OPT_IN).as_deref(),
        Ok("1"),
        "set {OPT_IN}=1 only on a dedicated isolated X11 display",
    );
    assert!(std::env::var_os("DISPLAY").is_some(), "the isolated DISPLAY is required");
    let event_loop = EventLoop::builder().with_x11().with_any_thread(true).build().unwrap();
    let mut session = Session { fixture: Fixture::default(), event_loop };
    pump_until(&mut session, "both direct-root windows mapped", |fixture| {
        fixture.source.as_ref().is_some_and(|window| window.is_visible() == Some(true))
            && fixture.peer.as_ref().is_some_and(|window| window.is_visible() == Some(true))
    });

    for checkpoint in
        [CaptureSampleCheckpoint::PositionQueried, CaptureSampleCheckpoint::CaptureRefreshed]
    {
        let press = press_primary(&mut session);
        sample(&mut session, &press, Sample::Hide(checkpoint));
        // Only normal pumping services cleanup after the checkpoint's Pending releases.
        finish_press(&mut session, &press, Release::Unavailable);
    }

    let press = press_primary(&mut session);
    sample(&mut session, &press, Sample::OrdinaryGrabs);
    sample(&mut session, &press, Sample::ForeignInputOnly);
    flush_polls(&mut session);
    sample(&mut session, &press, Sample::Fresh);
    sample(&mut session, &press, Sample::NativeEntry);
    sample(&mut session, &press, Sample::Fresh);
    finish_press(&mut session, &press, Release::Released);
    assert_eq!(session.fixture.presses.len(), 3);
    assert_eq!(session.fixture.releases.len(), 3);
}

#[test]
#[ignore = "reparenting X11 WM, XTest, xdotool and WINIT_X11_POINTER_ANCESTOR_UNMAP_TEST=1"]
fn managed_ancestor_unmap_rejects_capture_samples_at_both_checkpoints() {
    assert_eq!(
        std::env::var(MANAGED_OPT_IN).as_deref(),
        Ok("1"),
        "set {MANAGED_OPT_IN}=1 only on a dedicated isolated X11 display",
    );
    assert!(std::env::var_os("DISPLAY").is_some(), "the isolated DISPLAY is required");
    let (connection, screen) = x11rb::connect(None).expect("open the WM setup observation client");
    let root = connection.setup().roots[screen].root;
    let event_loop = EventLoop::builder().with_x11().with_any_thread(true).build().unwrap();
    let mut session =
        Session { fixture: Fixture { managed: true, ..Fixture::default() }, event_loop };
    pump_until(&mut session, "both managed windows reparented and viewable", |fixture| {
        let (Some(source), Some(peer)) = (&fixture.source, &fixture.peer) else {
            return false;
        };
        [source, peer].into_iter().all(|window| {
            let window = u32::try_from(u64::from(window.id())).unwrap();
            let tree = connection.query_tree(window).unwrap().reply().unwrap();
            let attributes = connection.get_window_attributes(window).unwrap().reply().unwrap();
            assert_eq!(tree.root, root);
            assert!(!attributes.override_redirect);
            tree.parent != root && attributes.map_state == MapState::VIEWABLE
        })
    });

    for checkpoint in
        [CaptureSampleCheckpoint::PositionQueried, CaptureSampleCheckpoint::CaptureRefreshed]
    {
        let source = u64::from(session.fixture.source.as_ref().unwrap().id()).to_string();
        inject(&["windowactivate", "--sync", &source])
            .expect("activate the managed source before its real press");
        flush_polls(&mut session);
        let press = press_primary(&mut session);
        sample(&mut session, &press, Sample::AncestorUnmap(checkpoint));
        // Process the real ungrab/crossing prefix while the ancestor remains unmapped.
        flush_polls(&mut session);
        sample(&mut session, &press, Sample::RevokedWhileHidden);
        let mut ancestor = session
            .fixture
            .unmapped_ancestor
            .take()
            .expect("the exact checkpoint must retain the unmapped ancestor");
        assert_eq!(
            ancestor
                .connection
                .get_window_attributes(ancestor.source)
                .unwrap()
                .reply()
                .unwrap()
                .map_state,
            MapState::UNVIEWABLE,
            "the source must remain mapped beneath the unmapped frame through normal pumping",
        );
        ancestor.remap().expect("remap only the original WM frame");
        pump_until(&mut session, "native source viewability after ancestor remap", |_| {
            ancestor
                .connection
                .get_window_attributes(ancestor.source)
                .unwrap()
                .reply()
                .unwrap()
                .map_state
                == MapState::VIEWABLE
        });
        // Restore hover without releasing Primary or creating a replacement press identity.
        inject(&["mousemove", "--window", &source, "40", "50", "getmouselocation"])
            .expect("move into the remapped source with the original Primary still held");
        flush_polls(&mut session);
        sample(&mut session, &press, Sample::Revoked);
        finish_press(&mut session, &press, Release::Unavailable);
    }
    assert_eq!(session.fixture.presses.len(), 2);
    assert_eq!(session.fixture.releases.len(), 2);
    assert!(session.fixture.unmapped_ancestor.is_none());
}
