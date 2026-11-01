//! Opt-in native capture test: set WINIT_X11_EXPLICIT_CAPTURE_TEST=1 on an isolated X11 display.

#![cfg(all(target_os = "linux", feature = "x11"))]

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
use winit::window::{CursorGrabMode, Window, WindowId};

const OPT_IN: &str = "WINIT_X11_EXPLICIT_CAPTURE_TEST";
const TIMEOUT: Duration = Duration::from_secs(5);
const TICK: Duration = Duration::from_millis(5);

fn inject(arguments: &[&str]) -> Result<(), String> {
    // A hung injector must not bypass the pump deadline or prevent Drop cleanup.
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

fn xid(window: &Window) -> String {
    match window.window_handle().unwrap().as_raw() {
        RawWindowHandle::Xlib(handle) => handle.window.to_string(),
        RawWindowHandle::Xcb(handle) => handle.window.get().to_string(),
        _ => panic!("the opted-in fixture requires X11"),
    }
}

#[derive(Clone, Debug)]
struct PrimaryPress {
    device: DeviceId,
    token: PointerPressToken,
}

#[derive(Default)]
struct CaptureFixture {
    source: Option<Window>,
    peer: Option<Window>,
    presses: Vec<PrimaryPress>,
    primary_releases: Vec<(WindowId, DeviceId)>,
    secondary_down: bool,
    secondary_edges: usize,
    motions: Vec<(WindowId, DeviceId, Option<PhysicalPosition<f64>>, PointerWindowRoute)>,
    polls: usize,
    acquisition: Option<PrimaryPress>,
    acquisition_result: Option<Acquisition>,
    acquisition_was_pending: bool,
    release: Option<PrimaryPress>,
    release_result: Option<Release>,
    observation: Option<(PrimaryPress, WindowId)>,
    observation_complete: bool,
}

impl ApplicationHandler for CaptureFixture {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.source.is_some() {
            return;
        }
        let attributes = Window::default_attributes()
            .with_inner_size(PhysicalSize::new(200_u32, 160_u32))
            .with_decorations(false)
            // Direct root children keep source qualification independent of WM reparenting.
            .with_override_redirect(true);
        self.source = Some(
            event_loop
                .create_window(
                    attributes
                        .clone()
                        .with_title("explicit capture source")
                        .with_position(PhysicalPosition::new(32, 64)),
                )
                .expect("create the source root child"),
        );
        self.peer = Some(
            event_loop
                .create_window(
                    attributes
                        .with_title("explicit capture peer")
                        .with_position(PhysicalPosition::new(312, 64)),
                )
                .expect("create the peer root child"),
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
                    token: primary_press.expect("a fresh real Primary press must be validated"),
                });
            },
            WindowEvent::MouseInput {
                device_id,
                button: MouseButton::Left,
                state: ElementState::Released,
                ..
            } => self.primary_releases.push((window_id, device_id)),
            WindowEvent::MouseInput {
                button: MouseButton::Right, state, primary_press, ..
            } => {
                assert!(primary_press.is_none(), "Secondary cannot mint a Primary identity");
                self.secondary_down = state == ElementState::Pressed;
                self.secondary_edges += 1;
            },
            WindowEvent::CursorMoved { device_id, facts, .. } => {
                self.motions.push((window_id, device_id, facts.desktop_position, facts.capture));
            },
            _ => (),
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.polls += 1;
        let Some(source) = self.source.as_ref().map(Window::id) else {
            return;
        };
        if let Some(press) = &self.acquisition {
            let result = event_loop.acquire_pointer_capture(source, press.device, &press.token);
            self.acquisition_was_pending |= matches!(result, Acquisition::Pending);
            let pending = matches!(result, Acquisition::Pending);
            self.acquisition_result = Some(result);
            if !pending {
                self.acquisition = None;
            }
        }
        if let Some(press) = &self.release {
            let result = event_loop.release_pointer_capture(source, press.device, &press.token);
            let pending = matches!(result, Release::Pending);
            self.release_result = Some(result);
            if !pending {
                self.release = None;
            }
        }
        if let Some((press, hover)) = &self.observation {
            // A successful ordinary query prevents a negative check from passing on a full queue.
            for expected in [None, Some(&press.token)] {
                match event_loop.observe_pointer(source, press.device, expected) {
                    X11PointerObservation::Unavailable => return,
                    X11PointerObservation::Unsupported => {
                        panic!("the real XTest master is supported")
                    },
                    X11PointerObservation::Observed { facts, delivery } => {
                        assert_eq!(facts.capture, PointerWindowRoute::Unknown);
                        assert_eq!(delivery, PointerWindowRoute::Unknown);
                        assert_eq!(facts.hover, PointerWindowRoute::Window(*hover));
                    },
                }
            }
            self.observation_complete = true;
            self.observation = None;
        }
    }
}

// Keep the owning event loop and live windows available during success and panic cleanup.
struct CaptureSession {
    fixture: CaptureFixture,
    event_loop: EventLoop<()>,
}

struct ReleaseCaptures {
    source: WindowId,
    presses: Vec<PrimaryPress>,
}

impl ApplicationHandler for ReleaseCaptures {
    fn resumed(&mut self, _event_loop: &ActiveEventLoop) {}

    fn window_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        _event: WindowEvent,
    ) {
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let source = self.source;
        self.presses.retain(|press| {
            matches!(
                event_loop.release_pointer_capture(source, press.device, &press.token),
                Release::Pending,
            )
        });
    }
}

impl Drop for CaptureSession {
    fn drop(&mut self) {
        // Release all standard buttons, including either injected button after a partial failure.
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
            eprintln!("explicit capture button cleanup: {error}");
        }
        let Some(source) = self.fixture.source.as_ref() else {
            return;
        };
        let mut cleanup =
            ReleaseCaptures { source: source.id(), presses: self.fixture.presses.clone() };
        let deadline = Instant::now() + TIMEOUT;
        loop {
            // This handler has no test assertions and never retries an acquisition while unwinding.
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
                    "explicit capture cleanup timed out with {} pending releases",
                    cleanup.presses.len()
                );
                break;
            }
        }
        self.event_loop.pump_app_events(Some(Duration::ZERO), &mut cleanup);
    }
}

fn pump_until(
    session: &mut CaptureSession,
    description: &str,
    ready: impl Fn(&CaptureFixture) -> bool,
) {
    let deadline = Instant::now() + TIMEOUT;
    while !ready(&session.fixture) {
        assert!(Instant::now() < deadline, "timed out waiting for {description}");
        session.event_loop.pump_app_events(Some(TICK), &mut session.fixture);
    }
    // Let normal native FIFO dispatch, cleanup and fences finish before the next physical edge.
    session.event_loop.pump_app_events(Some(Duration::ZERO), &mut session.fixture);
}

fn flush_polls(session: &mut CaptureSession) {
    let polls = session.fixture.polls + 2;
    pump_until(session, "normal poll flush", |fixture| fixture.polls >= polls);
}

fn press_primary(session: &mut CaptureSession) -> PrimaryPress {
    let count = session.fixture.presses.len();
    let source = xid(session.fixture.source.as_ref().unwrap());
    inject(&["mousemove", "--window", &source, "40", "50", "mousedown", "1", "getmouselocation"])
        .expect("inject a real XTest Primary press");
    pump_until(session, "validated real Primary press", |fixture| fixture.presses.len() > count);
    assert_eq!(session.fixture.presses.len(), count + 1, "one physical edge produces one press");
    let press = session.fixture.presses.last().unwrap().clone();
    for previous in &session.fixture.presses[..count] {
        assert_eq!(press.device, previous.device, "keep using the original real master pointer");
        assert_ne!(press.token, previous.token, "each physical Primary edge has its own identity");
    }
    press
}

fn release_primary(session: &mut CaptureSession) -> (WindowId, DeviceId) {
    let releases = session.fixture.primary_releases.len() + 1;
    inject(&["mouseup", "1", "getmouselocation"]).expect("release real Primary");
    pump_until(session, "real Primary release", |fixture| {
        fixture.primary_releases.len() >= releases
    });
    assert_eq!(session.fixture.primary_releases.len(), releases);
    session.fixture.primary_releases[releases - 1]
}

fn acquisition_status(session: &mut CaptureSession, press: &PrimaryPress, expected: Acquisition) {
    assert!(session.fixture.acquisition.is_none());
    session.fixture.acquisition_result = None;
    session.fixture.acquisition_was_pending = false;
    session.fixture.acquisition = Some(press.clone());
    pump_until(session, "acquisition result after normal FIFO", |fixture| {
        fixture.acquisition.is_none()
    });
    assert_eq!(session.fixture.acquisition_result.as_ref(), Some(&expected));
}

fn release_status(session: &mut CaptureSession, press: &PrimaryPress, expected: Release) {
    assert!(session.fixture.release.is_none());
    session.fixture.release_result = None;
    session.fixture.release = Some(press.clone());
    pump_until(session, "exact explicit release", |fixture| fixture.release.is_none());
    assert_eq!(session.fixture.release_result.as_ref(), Some(&expected));
}

fn observe_unknown(session: &mut CaptureSession, press: &PrimaryPress, hover: WindowId) {
    session.fixture.observation_complete = false;
    session.fixture.observation = Some((press.clone(), hover));
    pump_until(session, "fresh ordinary and press-qualified observations", |fixture| {
        fixture.observation_complete
    });
}

fn move_over_peer(session: &mut CaptureSession, press: &PrimaryPress, x: i32, y: i32) {
    let peer = session.fixture.peer.as_ref().unwrap();
    let origin = peer.inner_position().unwrap();
    let target = PhysicalPosition::new(f64::from(origin.x + x), f64::from(origin.y + y));
    let peer_xid = xid(peer);
    let motions = session.fixture.motions.len();
    let source = session.fixture.source.as_ref().unwrap().id();
    inject(&[
        "mousemove",
        "--window",
        &peer_xid,
        &x.to_string(),
        &y.to_string(),
        "getmouselocation",
    ])
    .expect("move the real pointer over the peer");
    // XI_Enter can synthesize CursorMoved on the peer. Only a matching source event proves
    // delivery.
    pump_until(session, "source motion over peer while explicitly captured", |fixture| {
        fixture.motions[motions..].iter().any(|(window, device, point, _)| {
            *window == source && *device == press.device && *point == Some(target)
        })
    });
    let peer = session.fixture.peer.as_ref().unwrap().id();
    observe_unknown(session, press, peer);
}

#[test]
#[ignore = "requires WINIT_X11_EXPLICIT_CAPTURE_TEST=1, isolated X11, XTest and xdotool"]
fn explicit_capture_serializes_ordinary_grabs_and_cleans_up_exact_original_presses() {
    assert_eq!(
        std::env::var(OPT_IN).as_deref(),
        Ok("1"),
        "set {OPT_IN}=1 only on a dedicated isolated X11 display",
    );
    assert!(std::env::var_os("DISPLAY").is_some(), "the isolated DISPLAY is required");
    // The test thread owns and pumps the event loop throughout; no background GUI thread is used.
    let event_loop = EventLoop::builder().with_x11().with_any_thread(true).build().unwrap();
    let mut session = CaptureSession { fixture: CaptureFixture::default(), event_loop };
    pump_until(&mut session, "both override-redirect windows mapped", |fixture| {
        fixture.source.as_ref().is_some_and(|window| window.is_visible() == Some(true))
            && fixture.peer.as_ref().is_some_and(|window| window.is_visible() == Some(true))
    });
    let source = session.fixture.source.as_ref().unwrap().id();

    let first = press_primary(&mut session);
    acquisition_status(&mut session, &first, Acquisition::Acquired);
    assert!(
        session.fixture.acquisition_was_pending,
        "native acceptance must precede FIFO authority"
    );
    assert!(
        session.fixture.source.as_ref().unwrap().set_cursor_grab(CursorGrabMode::Confined).is_err(),
        "an active explicit capture must reject ordinary confinement before any ungrab",
    );
    acquisition_status(&mut session, &first, Acquisition::Acquired);
    move_over_peer(&mut session, &first, 45, 55);
    assert_eq!(session.fixture.presses.len(), 1);
    assert_eq!(
        session.fixture.primary_releases.len(),
        0,
        "a rejected ordinary request must preserve input"
    );

    release_status(&mut session, &first, Release::Released);
    // Releasing cannot renew this original press, including when native Ungrab revokes its token.
    acquisition_status(&mut session, &first, Acquisition::Unavailable);
    let peer = session.fixture.peer.as_ref().unwrap();
    let peer_id = peer.id();
    let origin = peer.inner_position().unwrap();
    let target = PhysicalPosition::new(f64::from(origin.x + 65), f64::from(origin.y + 75));
    let peer_xid = xid(peer);
    let motions = session.fixture.motions.len();
    inject(&["mousemove", "--window", &peer_xid, "65", "75", "getmouselocation"])
        .expect("move over the peer after explicit ungrab while Primary stays held");
    pump_until(&mut session, "peer delivery with revoked implicit capture", |fixture| {
        fixture.motions[motions..].iter().any(|(window, device, point, capture)| {
            *window == peer_id
                && *device == first.device
                && *point == Some(target)
                && *capture == PointerWindowRoute::Unknown
        })
    });
    assert!(
        session.fixture.motions[motions..]
            .iter()
            .all(|(_, _, _, capture)| { *capture != PointerWindowRoute::Window(source) }),
        "ungrab cannot restore the old automatic source capture"
    );
    release_primary(&mut session);
    flush_polls(&mut session);

    session.fixture.source.as_ref().unwrap().set_cursor_grab(CursorGrabMode::Confined).unwrap();
    flush_polls(&mut session);
    let confined = press_primary(&mut session);
    observe_unknown(&mut session, &confined, source);
    acquisition_status(&mut session, &confined, Acquisition::Unavailable);
    session.fixture.source.as_ref().unwrap().set_cursor_grab(CursorGrabMode::None).unwrap();
    flush_polls(&mut session);
    acquisition_status(&mut session, &confined, Acquisition::Unavailable);
    release_primary(&mut session);

    let automatic = press_primary(&mut session);
    acquisition_status(&mut session, &automatic, Acquisition::Acquired);
    assert!(session.fixture.acquisition_was_pending);
    inject(&["mousedown", "3", "getmouselocation"])
        .expect("hold Secondary during explicit capture");
    pump_until(&mut session, "real Secondary down", |fixture| fixture.secondary_down);
    assert_eq!(release_primary(&mut session), (source, automatic.device));
    assert!(session.fixture.secondary_down, "Primary-up must leave Secondary physically held");

    // Do not call release_pointer_capture here: only normal polling may retire this native grab.
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if session
            .fixture
            .source
            .as_ref()
            .unwrap()
            .set_cursor_grab(CursorGrabMode::Confined)
            .is_ok()
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "Primary-up did not automatically clean up explicit capture"
        );
        session.event_loop.pump_app_events(Some(TICK), &mut session.fixture);
    }
    assert!(session.fixture.secondary_down);
    assert_eq!(session.fixture.secondary_edges, 1);
    session.fixture.source.as_ref().unwrap().set_cursor_grab(CursorGrabMode::None).unwrap();
    flush_polls(&mut session);
    inject(&["mouseup", "3", "getmouselocation"]).expect("finish the Secondary-held stream");
    pump_until(&mut session, "all-up after automatic cleanup", |fixture| {
        !fixture.secondary_down && fixture.secondary_edges == 2
    });

    let successor = press_primary(&mut session);
    acquisition_status(&mut session, &successor, Acquisition::Acquired);
    assert!(session.fixture.acquisition_was_pending);
    release_status(&mut session, &automatic, Release::Unavailable);
    release_status(&mut session, &first, Release::Unavailable);
    acquisition_status(&mut session, &successor, Acquisition::Acquired);
    move_over_peer(&mut session, &successor, 80, 70);
    assert_eq!(session.fixture.primary_releases.len() + 1, session.fixture.presses.len());

    // Keep capture active through the physical release over the peer, after both stale releases.
    assert_eq!(
        release_primary(&mut session),
        (source, successor.device),
        "the held Primary release over the peer must reach the source exactly once",
    );
    acquisition_status(&mut session, &successor, Acquisition::Unavailable);
    release_status(&mut session, &successor, Release::Unavailable);
    assert_eq!(session.fixture.primary_releases.len(), session.fixture.presses.len());
    assert_eq!(session.fixture.presses.len(), 4);
    assert!(!session.fixture.secondary_down);
}
