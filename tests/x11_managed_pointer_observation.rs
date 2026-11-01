//! Opt-in native test on an isolated X11 display with Openbox, XTest and xdotool.

#![cfg(all(target_os = "linux", feature = "x11", feature = "rwh_06"))]

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use rwh_06::{HasWindowHandle, RawWindowHandle};
use winit::application::ApplicationHandler;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{
    DeviceId, ElementState, MouseButton, PointerPressToken, PointerWindowRoute, WindowEvent,
};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::platform::pump_events::EventLoopExtPumpEvents;
use winit::platform::x11::{
    ActiveEventLoopExtX11, EventLoopBuilderExtX11, WindowAttributesExtX11,
    X11PointerCaptureAcquisition as Acquisition, X11PointerCaptureRelease as Release,
    X11PointerObservation,
};
use winit::window::{Window, WindowId};
use x11rb::connection::Connection as _;
use x11rb::protocol::xinput::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{self, ConnectionExt as _};
use x11rb::rust_connection::RustConnection;

const OPT_IN: &str = "WINIT_X11_MANAGED_POINTER_OBSERVATION_TEST";
const TIMEOUT: Duration = Duration::from_secs(10);
const TICK: Duration = Duration::from_millis(10);

fn inject(arguments: &[&str]) -> Result<(), String> {
    // Bound the injector too, including during panic cleanup.
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

fn xid(window: &Window) -> u32 {
    match window.window_handle().unwrap().as_raw() {
        RawWindowHandle::Xlib(handle) => u32::try_from(handle.window).unwrap(),
        RawWindowHandle::Xcb(handle) => handle.window.get(),
        _ => panic!("the managed pointer fixture requires X11"),
    }
}

#[derive(Clone)]
struct PrimaryPress {
    device: DeviceId,
    token: PointerPressToken,
}

#[derive(Clone, Copy)]
enum Hover {
    Peer,
    Foreign,
}

#[derive(Default)]
struct Fixture {
    source: Option<Window>,
    peer: Option<Window>,
    presses: Vec<PrimaryPress>,
    releases: Vec<(WindowId, DeviceId)>,
    motions: Vec<(WindowId, DeviceId, Option<PhysicalPosition<f64>>)>,
    acquiring: bool,
    acquisition: Option<Acquisition>,
    acquisition_was_pending: bool,
    observation: Option<(PhysicalPosition<f64>, Hover)>,
    samples: usize,
    releasing: bool,
    release: Option<Release>,
    drop_queued_window: bool,
    retired_window: Option<WindowId>,
    destroyed: usize,
}

impl ApplicationHandler for Fixture {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.source.is_some() {
            return;
        }
        let attributes = Window::default_attributes()
            .with_inner_size(PhysicalSize::new(200_u32, 160_u32))
            .with_decorations(true)
            .with_override_redirect(false);
        self.source = Some(
            event_loop
                .create_window(
                    attributes
                        .clone()
                        .with_title("managed pointer source")
                        .with_position(PhysicalPosition::new(32, 64)),
                )
                .expect("create the managed source"),
        );
        self.peer = Some(
            event_loop
                .create_window(
                    attributes
                        .with_title("managed pointer peer")
                        .with_position(PhysicalPosition::new(352, 64)),
                )
                .expect("create the managed peer"),
        );
    }

    fn window_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        assert!(
            self.retired_window == Some(window_id)
                || [self.source.as_ref(), self.peer.as_ref()]
                    .into_iter()
                    .flatten()
                    .any(|window| window.id() == window_id),
            "a foreign root notification must not become an application window event: {event:?}",
        );
        if matches!(event, WindowEvent::Destroyed) {
            assert_eq!(self.retired_window, Some(window_id));
            self.destroyed += 1;
            assert_eq!(self.destroyed, 1, "a real owned lifetime terminates once");
            return;
        }
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
                    token: primary_press.expect("the real source press must carry its own token"),
                });
            },
            WindowEvent::MouseInput {
                device_id,
                button: MouseButton::Left,
                state: ElementState::Released,
                ..
            } => self.releases.push((window_id, device_id)),
            WindowEvent::CursorMoved { device_id, facts, .. } => {
                self.motions.push((window_id, device_id, facts.desktop_position));
            },
            _ => (),
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if self.drop_queued_window {
            self.drop_queued_window = false;
            let window = event_loop
                .create_window(
                    Window::default_attributes()
                        .with_title("queued owned lifetime")
                        .with_inner_size(PhysicalSize::new(40_u32, 40_u32))
                        .with_position(PhysicalPosition::new(0, 0))
                        .with_override_redirect(true),
                )
                .unwrap();
            self.retired_window = Some(window.id());
            // Real Map/Configure notifications are already queued ahead of DestroyNotify.
            // They must not garbage-collect the sole owned weak row before its terminal.
            drop(window);
        }
        let Some(press) = self.presses.first() else {
            return;
        };
        let source = self.source.as_ref().unwrap().id();
        if self.acquiring {
            let result = event_loop.acquire_pointer_capture(source, press.device, &press.token);
            self.acquisition_was_pending |= result == Acquisition::Pending;
            self.acquisition = Some(result);
            self.acquiring = result == Acquisition::Pending;
        }
        if let Some((point, hover)) = self.observation {
            // Ordinary queries must succeed too: an unavailable query is not a negative proof.
            for (expected_press, capture) in [
                (None, PointerWindowRoute::Unknown),
                (Some(&press.token), PointerWindowRoute::Window(source)),
            ] {
                match event_loop.observe_pointer(source, press.device, expected_press) {
                    X11PointerObservation::Unavailable => return,
                    X11PointerObservation::Unsupported => {
                        panic!("the real X11 master is supported")
                    },
                    X11PointerObservation::Observed { facts, delivery } => {
                        assert_eq!(facts.desktop_position, Some(point));
                        match hover {
                            Hover::Peer => assert_eq!(
                                facts.hover,
                                PointerWindowRoute::Window(self.peer.as_ref().unwrap().id()),
                            ),
                            Hover::Foreign => assert!(
                                matches!(
                                    facts.hover,
                                    PointerWindowRoute::Unknown | PointerWindowRoute::Foreign
                                ),
                                "the foreign InputOnly child must block the peer: {facts:?}",
                            ),
                        }
                        assert_eq!(facts.capture, capture);
                        assert_eq!(delivery, capture);
                        assert!(facts.surface_position.is_none());
                        assert!(facts.geometry.is_none());
                    },
                }
            }
            self.samples += 1;
            self.observation = None;
        }
        if self.releasing {
            let result = event_loop.release_pointer_capture(source, press.device, &press.token);
            self.release = Some(result);
            self.releasing = result == Release::Pending;
        }
    }
}

// This connection is independent of Winit and owns the test's only foreign resource.
struct ForeignClient {
    connection: RustConnection,
    input_only: Option<u32>,
}

impl ForeignClient {
    fn remove_input_only(&mut self) -> Result<(), x11rb::errors::ReplyError> {
        if let Some(window) = self.input_only.take() {
            self.connection.destroy_window(window)?.check()?;
        }
        Ok(())
    }
}

impl Drop for ForeignClient {
    fn drop(&mut self) {
        if let Err(error) = self.remove_input_only() {
            eprintln!("managed pointer InputOnly cleanup: {error}");
        }
    }
}

struct Session {
    fixture: Fixture,
    event_loop: EventLoop<()>,
    foreign: ForeignClient,
}

struct Cleanup {
    source: WindowId,
    press: Option<PrimaryPress>,
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
        if let Some(press) = &self.press {
            if event_loop.release_pointer_capture(self.source, press.device, &press.token)
                != Release::Pending
            {
                self.press = None;
            }
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Keep both windows alive while releasing the real button and the exact original duty.
        if let Err(error) = inject(&["mouseup", "1", "getmouselocation"]) {
            eprintln!("managed pointer button cleanup: {error}");
        }
        if let Err(error) = self.foreign.remove_input_only() {
            eprintln!("managed pointer InputOnly cleanup: {error}");
        }
        let Some(source) = self.fixture.source.as_ref() else {
            return;
        };
        let mut cleanup =
            Cleanup { source: source.id(), press: self.fixture.presses.first().cloned() };
        let deadline = Instant::now() + TIMEOUT;
        loop {
            // Never run acquisition or test assertions while unwinding.
            self.event_loop.pump_app_events(Some(TICK), &mut cleanup);
            if cleanup.press.is_none() {
                break;
            }
            if Instant::now() >= deadline {
                eprintln!("managed pointer exact capture cleanup timed out");
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

#[test]
#[ignore = "requires WINIT_X11_MANAGED_POINTER_OBSERVATION_TEST=1, isolated X11, Openbox and \
            xdotool"]
fn managed_hover_is_independent_of_source_capture_and_foreign_input_only() {
    assert_eq!(
        std::env::var(OPT_IN).as_deref(),
        Ok("1"),
        "set {OPT_IN}=1 only on a dedicated isolated X11 display",
    );
    assert!(std::env::var_os("DISPLAY").is_some(), "the isolated DISPLAY is required");
    let (connection, screen) = x11rb::connect(None).expect("open the independent X11 client");
    let root = connection.setup().roots[screen].root;
    let wm_check =
        connection.intern_atom(false, b"_NET_SUPPORTING_WM_CHECK").unwrap().reply().unwrap();
    let wm = connection
        .get_property(false, root, wm_check.atom, xproto::AtomEnum::WINDOW, 0, 1)
        .unwrap()
        .reply()
        .unwrap()
        .value32()
        .and_then(|mut values| values.next())
        .expect("Openbox must already manage the isolated display");
    let wm_name = connection.intern_atom(false, b"_NET_WM_NAME").unwrap().reply().unwrap();
    let utf8 = connection.intern_atom(false, b"UTF8_STRING").unwrap().reply().unwrap();
    let name = connection
        .get_property(false, wm, wm_name.atom, utf8.atom, 0, 32)
        .unwrap()
        .reply()
        .unwrap();
    assert_eq!(name.value, b"Openbox", "this finite test requires actual Openbox");
    assert_eq!(connection.xinput_xi_query_version(2, 0).unwrap().reply().unwrap().major_version, 2,);
    let devices =
        connection.xinput_xi_query_device(xinput::Device::ALL_MASTER).unwrap().reply().unwrap();
    let pointers = devices
        .infos
        .iter()
        .filter(|info| info.enabled && info.type_ == xinput::DeviceType::MASTER_POINTER)
        .collect::<Vec<_>>();
    assert_eq!(pointers.len(), 1, "the isolated fixture requires one enabled master pointer");
    let pointer = pointers[0].deviceid;

    let event_loop = EventLoop::builder().with_x11().with_any_thread(true).build().unwrap();
    let mut session = Session {
        fixture: Fixture::default(),
        event_loop,
        foreign: ForeignClient { connection, input_only: None },
    };
    pump_until(&mut session, "both managed clients created", |fixture| {
        fixture.source.is_some() && fixture.peer.is_some()
    });
    let source = session.fixture.source.as_ref().unwrap().id();
    let source_xid = xid(session.fixture.source.as_ref().unwrap());
    let peer_xid = xid(session.fixture.peer.as_ref().unwrap());
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let connection = &session.foreign.connection;
        let mut mapped = true;
        for window in [source_xid, peer_xid] {
            let tree = connection.query_tree(window).unwrap().reply().unwrap();
            let attributes = connection.get_window_attributes(window).unwrap().reply().unwrap();
            assert_eq!(tree.root, root);
            assert!(!attributes.override_redirect, "both clients must be ordinary managed windows");
            mapped &= tree.parent != root && attributes.map_state == xproto::MapState::VIEWABLE;
        }
        if mapped {
            break;
        }
        assert!(Instant::now() < deadline, "Openbox must actually reparent and map both clients");
        session.event_loop.pump_app_events(Some(TICK), &mut session.fixture);
    }
    let connection = &session.foreign.connection;
    let source_tree = connection.query_tree(source_xid).unwrap().reply().unwrap();
    let peer_tree = connection.query_tree(peer_xid).unwrap().reply().unwrap();
    assert_ne!(source_tree.parent, root, "the source must actually be reparented");
    assert_ne!(peer_tree.parent, root, "the peer must actually be reparented");
    assert_ne!(source_tree.parent, peer_tree.parent, "each client needs its own WM frame");
    let peer_frame = peer_tree.parent;
    assert_eq!(connection.query_tree(peer_frame).unwrap().reply().unwrap().parent, root);

    let source_argument = source_xid.to_string();
    inject(&["windowactivate", "--sync", &source_argument])
        .expect("activate the source through Openbox before the real press");
    session.event_loop.pump_app_events(Some(TICK), &mut session.fixture);
    inject(&[
        "mousemove",
        "--window",
        &source_argument,
        "40",
        "50",
        "mousedown",
        "1",
        "getmouselocation",
    ])
    .expect("inject the real source Primary press");
    pump_until(&mut session, "the validated native Primary press", |fixture| {
        !fixture.presses.is_empty()
    });
    assert_eq!(session.fixture.presses.len(), 1);
    let press = session.fixture.presses[0].clone();
    assert_ne!(press.device, DeviceId::dummy(), "use the actual native event's master DeviceId");
    let pointer_state =
        session.foreign.connection.xinput_xi_query_pointer(root, pointer).unwrap().reply().unwrap();
    assert!(pointer_state.same_screen);
    assert_eq!(pointer_state.root, root);
    assert_eq!(pointer_state.child, source_tree.parent);
    assert!(pointer_state.buttons.first().is_some_and(|mask| mask & (1 << 1) != 0));

    session.fixture.acquiring = true;
    pump_until(&mut session, "managed source capture acquisition", |fixture| !fixture.acquiring);
    assert_eq!(session.fixture.acquisition, Some(Acquisition::Acquired));
    assert!(
        session.fixture.acquisition_was_pending,
        "native acceptance must precede FIFO settlement"
    );

    let point = session
        .foreign
        .connection
        .translate_coordinates(peer_xid, root, 45, 55)
        .unwrap()
        .reply()
        .unwrap();
    assert!(point.same_screen);
    let expected_point = PhysicalPosition::new(f64::from(point.dst_x), f64::from(point.dst_y));
    let motions = session.fixture.motions.len();
    inject(&["mousemove", "--window", &peer_xid.to_string(), "45", "55", "getmouselocation"])
        .expect("move the real pointer into the managed peer");
    pump_until(&mut session, "source delivery while physically over the peer", |fixture| {
        fixture.motions[motions..].contains(&(source, press.device, Some(expected_point)))
    });
    let connection = &session.foreign.connection;
    let root_before = connection.xinput_xi_query_pointer(root, pointer).unwrap().reply().unwrap();
    let frame_before =
        connection.xinput_xi_query_pointer(peer_frame, pointer).unwrap().reply().unwrap();
    assert_eq!(root_before.child, peer_frame);
    assert_eq!(frame_before.child, peer_xid, "the actual sprite path must enter the peer");
    session.fixture.observation = Some((expected_point, Hover::Peer));
    pump_until(&mut session, "ordinary and fresh captured peer observations", |fixture| {
        fixture.observation.is_none()
    });

    session
        .foreign
        .connection
        .send_event(false, root, xproto::EventMask::SUBSTRUCTURE_NOTIFY, xproto::UnmapNotifyEvent {
            response_type: xproto::UNMAP_NOTIFY_EVENT,
            sequence: 0,
            event: root,
            window: source_tree.parent,
            from_configure: false,
        })
        .unwrap()
        .check()
        .unwrap();
    session.fixture.acquisition = None;
    session.fixture.acquiring = true;
    pump_until(&mut session, "the original capture after a synthetic parent unmap", |fixture| {
        !fixture.acquiring
    });
    assert_eq!(session.fixture.acquisition, Some(Acquisition::Acquired));

    // Root observation must distinguish an unrelated lifetime from the source's WM parent.
    let connection = &session.foreign.connection;
    let unrelated = connection.generate_id().unwrap();
    session.foreign.input_only = Some(unrelated);
    let connection = &session.foreign.connection;
    connection
        .create_window(
            0,
            unrelated,
            root,
            0,
            0,
            16,
            16,
            0,
            xproto::WindowClass::INPUT_ONLY,
            0,
            &xproto::CreateWindowAux::new().override_redirect(1),
        )
        .unwrap()
        .check()
        .unwrap();
    connection.map_window(unrelated).unwrap().check().unwrap();
    assert_eq!(
        connection.get_window_attributes(unrelated).unwrap().reply().unwrap().map_state,
        xproto::MapState::VIEWABLE,
    );
    connection.unmap_window(unrelated).unwrap().check().unwrap();
    session.foreign.remove_input_only().unwrap();
    session.fixture.acquisition = None;
    session.fixture.acquiring = true;
    pump_until(&mut session, "the original capture after unrelated root retirement", |fixture| {
        !fixture.acquiring
    });
    assert_eq!(session.fixture.acquisition, Some(Acquisition::Acquired));

    let connection = &session.foreign.connection;
    let frame_point =
        connection.translate_coordinates(peer_xid, peer_frame, 45, 55).unwrap().reply().unwrap();
    assert!(frame_point.same_screen);
    let foreign = connection.generate_id().unwrap();
    // Record the exact resource before dispatch so all partial failures retain cleanup.
    session.foreign.input_only = Some(foreign);
    let connection = &session.foreign.connection;
    connection
        .create_window(
            0,
            foreign,
            peer_frame,
            frame_point.dst_x - 8,
            frame_point.dst_y - 8,
            16,
            16,
            0,
            xproto::WindowClass::INPUT_ONLY,
            0,
            &xproto::CreateWindowAux::new().override_redirect(1),
        )
        .unwrap()
        .check()
        .unwrap();
    connection
        .configure_window(
            foreign,
            &xproto::ConfigureWindowAux::new()
                .sibling(peer_xid)
                .stack_mode(xproto::StackMode::ABOVE),
        )
        .unwrap()
        .check()
        .unwrap();
    connection.map_window(foreign).unwrap().check().unwrap();
    let foreign_tree = connection.query_tree(foreign).unwrap().reply().unwrap();
    assert_eq!(foreign_tree.parent, peer_frame);
    let attributes = connection.get_window_attributes(foreign).unwrap().reply().unwrap();
    assert_eq!(attributes.class, xproto::WindowClass::INPUT_ONLY);
    assert_eq!(attributes.map_state, xproto::MapState::VIEWABLE);
    let root_after = connection.xinput_xi_query_pointer(root, pointer).unwrap().reply().unwrap();
    let frame_after =
        connection.xinput_xi_query_pointer(peer_frame, pointer).unwrap().reply().unwrap();
    assert_eq!(root_after.child, root_before.child, "the root child must remain the same WM frame");
    assert_eq!(
        frame_after.child, foreign,
        "the foreign sibling must actually intercept the sprite"
    );
    assert_eq!(
        (frame_after.root_x, frame_after.root_y),
        (frame_before.root_x, frame_before.root_y)
    );
    session.fixture.observation = Some((expected_point, Hover::Foreign));
    pump_until(
        &mut session,
        "nonpenetrating observations over the foreign InputOnly sibling",
        |fixture| fixture.observation.is_none(),
    );
    assert_eq!(session.fixture.samples, 2);

    session.foreign.remove_input_only().expect("destroy the exact foreign InputOnly child");
    session.fixture.releasing = true;
    pump_until(&mut session, "release of the exact original explicit capture", |fixture| {
        !fixture.releasing
    });
    assert_eq!(session.fixture.release, Some(Release::Released));
    inject(&["mouseup", "1", "getmouselocation"]).expect("release the real Primary button");
    pump_until(&mut session, "the real Primary release after explicit cleanup", |fixture| {
        !fixture.releases.is_empty()
    });
    assert_eq!(session.fixture.releases.len(), 1);
    assert_eq!(
        session.fixture.releases[0],
        (session.fixture.peer.as_ref().unwrap().id(), press.device)
    );
    assert_eq!(session.fixture.presses.len(), 1, "sampling must never mint another Primary press");
    let buttons = session
        .foreign
        .connection
        .xinput_xi_query_pointer(root, pointer)
        .unwrap()
        .reply()
        .unwrap()
        .buttons;
    assert!(!buttons.first().is_some_and(|mask| mask & (1 << 1) != 0), "Primary must really be up");
    session.fixture.drop_queued_window = true;
    pump_until(&mut session, "the exact queued owned Destroyed callback", |fixture| {
        fixture.destroyed == 1
    });
}
