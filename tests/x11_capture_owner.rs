//! Opt-in capture evidence checks on an isolated X11 display with XTest and xdotool.

#![cfg(all(target_os = "linux", feature = "x11"))]

use std::process::Command;
use std::time::{Duration, Instant};

use rwh_06::{HasWindowHandle, RawWindowHandle};
use winit::application::ApplicationHandler;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, MouseButton, PointerPressToken, PointerWindowRoute, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::platform::pump_events::EventLoopExtPumpEvents;
use winit::platform::x11::{EventLoopBuilderExtX11, WindowAttributesExtX11};
use winit::window::{CursorGrabMode, Window, WindowId};

fn inject(arguments: &[&str]) {
    let output = Command::new("xdotool").args(arguments).output().expect("xdotool starts");
    assert!(
        output.status.success(),
        "xdotool {arguments:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

struct ReleasePrimary;

impl Drop for ReleasePrimary {
    fn drop(&mut self) {
        let _ = Command::new("xdotool").args(["mouseup", "1"]).output();
        let _ = Command::new("xdotool").args(["mouseup", "3"]).output();
    }
}

#[derive(Default)]
struct CaptureFixture {
    window: Option<Window>,
    presses: Vec<PointerWindowRoute>,
    primary_presses: Vec<Option<PointerPressToken>>,
    releases: usize,
    primary_releases: Vec<Option<PointerPressToken>>,
    release_captures: Vec<PointerWindowRoute>,
    secondary_edges: usize,
    motions: Vec<PointerWindowRoute>,
}

impl ApplicationHandler for CaptureFixture {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_none() {
            self.window = Some(
                event_loop
                    .create_window(
                        Window::default_attributes()
                            .with_title("winit-capture-owner-boundary")
                            .with_position(PhysicalPosition::new(180, 180))
                            .with_inner_size(PhysicalSize::new(240, 180))
                            .with_decorations(false)
                            .with_override_redirect(true),
                    )
                    .expect("the source window is created"),
            );
        }
    }

    fn window_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        let Some(window) = &self.window else { return };
        assert_eq!(window_id, window.id());
        match event {
            WindowEvent::MouseInput {
                button: MouseButton::Left,
                state: ElementState::Pressed,
                primary_press,
                facts,
                ..
            } => {
                self.presses.push(facts.capture);
                self.primary_presses.push(primary_press);
                match self.presses.len() {
                    1 => {
                        // Both native edges precede the mutation while normal dispatch is
                        // inside this original press callback. XSync must not admit them again.
                        inject(&["mouseup", "1", "mousedown", "1"]);
                        // A WM may own the active grab. Even a rejected mutation must revoke
                        // retained evidence: this request already sent its preceding ungrab.
                        if let Err(error) = window.set_cursor_grab(CursorGrabMode::Confined) {
                            eprintln!("the WM rejected confinement after mutation: {error}");
                        }
                        window.set_cursor_grab(CursorGrabMode::None).unwrap();
                    },
                    3 => {
                        // An actual unmap loses the implicit server grab. Remapping must not
                        // restore its retained evidence while Primary remains physically held.
                        window.set_visible(false);
                        window.set_visible(true);
                    },
                    _ => (),
                }
            },
            WindowEvent::MouseInput {
                button: MouseButton::Left,
                state: ElementState::Released,
                primary_press,
                facts,
                ..
            } => {
                self.releases += 1;
                self.primary_releases.push(primary_press);
                self.release_captures.push(facts.capture);
            },
            WindowEvent::MouseInput { button: MouseButton::Right, primary_press, .. } => {
                assert!(primary_press.is_none(), "Secondary never carries Primary identity");
                self.secondary_edges += 1;
            },
            WindowEvent::CursorMoved { facts, .. } => self.motions.push(facts.capture),
            _ => (),
        }
    }
}

fn pump_until(
    event_loop: &mut EventLoop<()>,
    fixture: &mut CaptureFixture,
    description: &str,
    ready: impl Fn(&CaptureFixture) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready(fixture) {
        assert!(Instant::now() < deadline, "timed out waiting for {description}");
        event_loop.pump_app_events(Some(Duration::from_millis(5)), fixture);
    }
    // Finish normal FIFO dispatch, including the backend's mutation fence, before
    // injecting the next physical edge. This does not consume or fabricate input.
    event_loop.pump_app_events(Some(Duration::ZERO), fixture);
}

#[test]
#[ignore = "requires isolated X11, XTest, xdotool and a window manager"]
fn local_mutations_revoke_queued_and_remapped_capture_owners() {
    let _release = ReleasePrimary;
    let mut event_loop = EventLoop::builder().with_x11().with_any_thread(true).build().unwrap();
    let mut fixture = CaptureFixture::default();
    pump_until(&mut event_loop, &mut fixture, "initial mapping", |fixture| {
        fixture.window.as_ref().is_some_and(|window| window.is_visible() == Some(true))
    });
    let window = fixture.window.as_ref().unwrap();
    let owner = PointerWindowRoute::Window(window.id());
    let xid = match window.window_handle().unwrap().as_raw() {
        RawWindowHandle::Xlib(handle) => handle.window.to_string(),
        RawWindowHandle::Xcb(handle) => handle.window.get().to_string(),
        _ => panic!("the fixture requires X11"),
    };
    inject(&["mousemove", "--window", &xid, "30", "30", "mousedown", "1"]);
    pump_until(&mut event_loop, &mut fixture, "queued predecessor press", |fixture| {
        fixture.presses.len() >= 2
    });
    assert_eq!(fixture.presses[0], owner, "the original physical press owns capture");
    assert!(fixture.primary_presses[0].is_some(), "the original press has a validated identity");
    assert_eq!(
        fixture.presses[1],
        PointerWindowRoute::Unknown,
        "a queued predecessor press cannot restore capture after a local grab mutation"
    );
    assert!(fixture.primary_presses[1].is_none(), "the queued predecessor cannot mint an identity");

    inject(&["mouseup", "1"]);
    pump_until(&mut event_loop, &mut fixture, "all-up after ungrab", |fixture| {
        fixture.releases >= 2
    });
    inject(&["mousedown", "1"]);
    pump_until(&mut event_loop, &mut fixture, "fresh press and remap", |fixture| {
        fixture.presses.len() >= 3 && fixture.window.as_ref().unwrap().is_visible() == Some(true)
    });
    assert_eq!(fixture.presses[2], owner, "a new all-up-to-press sequence recovers");
    assert!(fixture.primary_presses[2].is_some(), "the fresh press has its own identity");
    assert_ne!(fixture.primary_presses[0], fixture.primary_presses[2]);
    let motions = fixture.motions.len();
    inject(&["mousemove", "--window", &xid, "45", "45"]);
    pump_until(&mut event_loop, &mut fixture, "held motion after remap", |fixture| {
        fixture.motions.len() > motions
    });
    assert_eq!(
        fixture.motions.last(),
        Some(&PointerWindowRoute::Unknown),
        "unmap/remap cannot revive the held owner"
    );

    inject(&["mouseup", "1"]);
    pump_until(&mut event_loop, &mut fixture, "all-up after remap", |fixture| {
        fixture.releases >= 3
    });
    inject(&["mousedown", "1"]);
    pump_until(&mut event_loop, &mut fixture, "fresh press after remap", |fixture| {
        fixture.presses.len() >= 4
    });
    assert_eq!(fixture.presses[3], owner);
    assert!(fixture.primary_presses[3].is_some(), "the fresh press after remap has an identity");
    assert_ne!(fixture.primary_presses[0], fixture.primary_presses[3]);
    assert_ne!(fixture.primary_presses[2], fixture.primary_presses[3]);
    inject(&["mouseup", "1"]);
    pump_until(&mut event_loop, &mut fixture, "final Primary release", |fixture| {
        fixture.releases >= 4
    });
    assert_eq!(
        fixture.primary_releases.last(),
        fixture.primary_presses.last(),
        "an unrevoked release retains only the historical identity it ended"
    );

    inject(&["mousedown", "3", "mousedown", "1"]);
    pump_until(&mut event_loop, &mut fixture, "Primary with Secondary held", |fixture| {
        fixture.secondary_edges >= 1 && fixture.presses.len() >= 5
    });
    let predecessor = fixture.primary_presses[4].as_ref().unwrap().clone();
    assert_eq!(fixture.presses[4], owner);
    inject(&["mouseup", "1"]);
    pump_until(&mut event_loop, &mut fixture, "Primary up with Secondary held", |fixture| {
        fixture.releases >= 5
    });
    assert_eq!(fixture.primary_releases[4].as_ref(), Some(&predecessor));
    assert_eq!(fixture.release_captures[4], owner, "Secondary retains the implicit grab");

    inject(&["mousedown", "1"]);
    pump_until(&mut event_loop, &mut fixture, "successor Primary with Secondary held", |fixture| {
        fixture.presses.len() >= 6
    });
    let successor = fixture.primary_presses[5].as_ref().unwrap();
    assert_ne!(successor, &predecessor, "a new Primary edge has a distinct identity");
    assert_eq!(fixture.presses[5], owner);
    inject(&["mouseup", "1", "mouseup", "3"]);
    pump_until(&mut event_loop, &mut fixture, "final all-up", |fixture| {
        fixture.releases >= 6 && fixture.secondary_edges >= 2
    });
}
