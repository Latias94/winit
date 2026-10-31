#![cfg(x11_platform)]

//! Opt-in native test: WINIT_X11_POINTER_OBSERVATION_TEST=1 with DISPLAY and xdotool available.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use winit::application::ApplicationHandler;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{
    DeviceId, ElementState, MouseButton, PointerWindowRoute, StartCause, WindowEvent,
};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::platform::run_on_demand::EventLoopExtRunOnDemand;
use winit::platform::x11::{
    ActiveEventLoopExtX11, EventLoopBuilderExtX11, WindowAttributesExtX11, X11PointerObservation,
};
use winit::window::{Window, WindowId};

const OPT_IN: &str = "WINIT_X11_POINTER_OBSERVATION_TEST";
const TIMEOUT: Duration = Duration::from_secs(20);
const TICK: Duration = Duration::from_millis(10);

// Bound subprocesses too, so a stuck injector cannot prevent the event-loop deadline or cleanup.
fn xdotool(args: &[&str]) -> Result<(), String> {
    let mut child = Command::new("xdotool")
        .args(args)
        .stdout(Stdio::null())
        .spawn()
        .map_err(|error| format!("xdotool {args:?}: {error}"))?;
    let deadline = Instant::now() + Duration::from_secs(2);
    let result = loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break Ok(()),
            Ok(Some(status)) => break Err(format!("xdotool {args:?}: {status}")),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(TICK),
            Ok(None) => break Err(format!("xdotool {args:?} timed out")),
            Err(error) => break Err(format!("xdotool {args:?}: {error}")),
        }
    };
    if result.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    result
}

struct ReleasePrimary;

impl Drop for ReleasePrimary {
    fn drop(&mut self) {
        if let Err(error) = xdotool(&["mouseup", "1", "getmouselocation"]) {
            eprintln!("X11 pointer test cleanup: {error}");
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Map,
    SourceMotion,
    HeldSamples,
    QuietQueries,
    Release,
    Destroyed,
    Done,
}

struct App {
    source: Option<Window>,
    target: Option<Window>,
    source_id: Option<WindowId>,
    device: Option<DeviceId>,
    last_motion: Option<(WindowId, DeviceId, PhysicalPosition<f64>)>,
    points: [PhysicalPosition<i32>; 3],
    phase: Phase,
    deadline: Instant,
    settle_until: Instant,
    held: bool,
    released: bool,
    destroyed: bool,
    moved_count: usize,
    samples: usize,
    pending_checks: usize,
    callback_checks: usize,
    quiet_moved_count: usize,
    quiet_queries: usize,
    quiet_since: Instant,
    failure: Option<String>,
}

impl App {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            source: None,
            target: None,
            source_id: None,
            device: None,
            last_motion: None,
            points: [PhysicalPosition::new(0, 0); 3],
            phase: Phase::Map,
            deadline: now + TIMEOUT,
            settle_until: now,
            held: false,
            released: false,
            destroyed: false,
            moved_count: 0,
            samples: 0,
            pending_checks: 0,
            callback_checks: 0,
            quiet_moved_count: 0,
            quiet_queries: 0,
            quiet_since: now,
            failure: None,
        }
    }

    fn fail(&mut self, event_loop: &ActiveEventLoop, error: String) {
        self.failure.get_or_insert(error);
        event_loop.exit();
    }

    fn check_deadline(&mut self, event_loop: &ActiveEventLoop) -> bool {
        if self.failure.is_some() || self.phase == Phase::Done {
            return false;
        }
        if Instant::now() >= self.deadline {
            let error = format!(
                "timed out in {:?}; samples={}, motion={:?}, held={}, released={}, destroyed={}",
                self.phase,
                self.samples,
                self.last_motion,
                self.held,
                self.released,
                self.destroyed,
            );
            self.fail(event_loop, error);
            return false;
        }
        true
    }

    fn expected_point(&self) -> PhysicalPosition<f64> {
        let point = self.points[self.samples.min(2)];
        PhysicalPosition::new(f64::from(point.x), f64::from(point.y))
    }

    fn pending_query(&mut self, event_loop: &ActiveEventLoop) -> Result<(), String> {
        let result =
            event_loop.observe_pointer(self.source_id.unwrap(), self.device.unwrap(), None);
        if result != X11PointerObservation::Unavailable {
            return Err(format!("queued native input must block observation: {result:?}"));
        }
        self.pending_checks += 1;
        Ok(())
    }

    fn move_to_sample(&mut self, event_loop: &ActiveEventLoop) -> Result<(), String> {
        let point = self.points[self.samples];
        // The final query is an injector-side server round trip, not a timing sleep. The motion
        // must have been processed before probing Winit's still-undispatched native prefix.
        xdotool(&["mousemove", &point.x.to_string(), &point.y.to_string(), "getmouselocation"])?;
        self.pending_query(event_loop)
    }

    fn sample(&self, event_loop: &ActiveEventLoop, expected: WindowId) -> Result<bool, String> {
        match event_loop.observe_pointer(expected, self.device.unwrap(), None) {
            X11PointerObservation::Unavailable => Ok(false),
            X11PointerObservation::Unsupported => {
                Err("live X11 master pointer is unsupported".into())
            },
            X11PointerObservation::Observed { facts, delivery } => {
                if facts.capture != PointerWindowRoute::Unknown
                    || delivery != PointerWindowRoute::Unknown
                {
                    return Err(format!(
                        "query promoted capture/delivery: {facts:?}, {delivery:?}"
                    ));
                }
                let hover = if self.samples == 0 {
                    self.source_id.unwrap()
                } else {
                    self.target.as_ref().unwrap().id()
                };
                if facts.desktop_position != Some(self.expected_point())
                    || facts.hover != PointerWindowRoute::Window(hover)
                    || facts.surface_position.is_some()
                    || facts.geometry.is_some()
                {
                    return Err(format!(
                        "wrong queried facts at sample {}: {facts:?}",
                        self.samples
                    ));
                }
                Ok(true)
            },
        }
    }

    fn advance(&mut self, event_loop: &ActiveEventLoop) -> Result<(), String> {
        match self.phase {
            Phase::Map if Instant::now() >= self.settle_until => {
                let source =
                    self.source.as_ref().unwrap().inner_position().map_err(|e| e.to_string())?;
                let target =
                    self.target.as_ref().unwrap().inner_position().map_err(|e| e.to_string())?;
                self.points = [
                    PhysicalPosition::new(source.x + 40, source.y + 50),
                    PhysicalPosition::new(target.x + 40, target.y + 50),
                    PhysicalPosition::new(target.x + 140, target.y + 110),
                ];
                let point = self.points[0];
                // Two distinct positions also produce a real motion when the cursor starts at
                // the eventual source point. No cached or focus-generated DeviceId is accepted.
                xdotool(&[
                    "mousemove",
                    &(point.x + 5).to_string(),
                    &point.y.to_string(),
                    "mousemove",
                    &point.x.to_string(),
                    &point.y.to_string(),
                    "getmouselocation",
                ])?;
                self.phase = Phase::SourceMotion;
            },
            Phase::SourceMotion if self.device.is_some() => {
                xdotool(&["mousedown", "1", "getmouselocation"])?;
                self.pending_query(event_loop)?;
                self.phase = Phase::HeldSamples;
            },
            Phase::HeldSamples if self.held => {
                let expected_motion =
                    (self.source_id.unwrap(), self.device.unwrap(), self.expected_point());
                if self.last_motion != Some(expected_motion)
                    || !self.sample(event_loop, self.source_id.unwrap())?
                {
                    return Ok(());
                }
                self.samples += 1;
                if self.samples < self.points.len() {
                    self.move_to_sample(event_loop)?;
                } else {
                    self.quiet_moved_count = self.moved_count;
                    self.quiet_since = Instant::now();
                    self.phase = Phase::QuietQueries;
                }
            },
            Phase::QuietQueries => {
                if !self.held || self.moved_count != self.quiet_moved_count {
                    return Err("stationary queries released the primary button or produced \
                                CursorMoved"
                        .into());
                }
                if self.sample(event_loop, self.source_id.unwrap())? {
                    self.quiet_queries += 1;
                }
                if self.quiet_queries >= 6
                    && self.quiet_since.elapsed() >= Duration::from_millis(100)
                {
                    xdotool(&["mouseup", "1", "getmouselocation"])?;
                    self.pending_query(event_loop)?;
                    self.phase = Phase::Release;
                }
            },
            Phase::Release if self.released && !self.held => {
                if !self.sample(event_loop, self.source_id.unwrap())? {
                    return Ok(());
                }
                // A successful real-device sample above keeps this negative check from passing
                // merely because an unrelated pending event blocks every device.
                let dummy = DeviceId::dummy();
                if self.device == Some(dummy) {
                    return Err("dummy DeviceId unexpectedly names the real pointer".into());
                }
                let result = event_loop.observe_pointer(self.source_id.unwrap(), dummy, None);
                if !matches!(
                    result,
                    X11PointerObservation::Unavailable | X11PointerObservation::Unsupported
                ) {
                    return Err(format!("dummy device produced an observation: {result:?}"));
                }
                drop(self.source.take());
                self.phase = Phase::Destroyed;
            },
            Phase::Destroyed if self.destroyed => {
                if !self.sample(event_loop, self.target.as_ref().unwrap().id())? {
                    return Ok(());
                }
                let result =
                    event_loop.observe_pointer(self.source_id.unwrap(), self.device.unwrap(), None);
                if result != X11PointerObservation::Unavailable {
                    return Err(format!(
                        "destroyed expected window produced an observation: {result:?}"
                    ));
                }
                self.phase = Phase::Done;
                event_loop.exit();
            },
            _ => (),
        }
        Ok(())
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.source_id.is_some() || self.failure.is_some() {
            return;
        }
        let attributes = Window::default_attributes()
            .with_inner_size(PhysicalSize::new(200_u32, 160_u32))
            .with_decorations(false)
            // Root queries can prove these direct children without composing a WM frame path.
            .with_override_redirect(true);
        let result = (|| {
            let source = event_loop
                .create_window(
                    attributes
                        .clone()
                        .with_title("pointer observation A")
                        .with_position(PhysicalPosition::new(32, 64)),
                )
                .map_err(|e| e.to_string())?;
            let target = event_loop
                .create_window(
                    attributes
                        .with_title("pointer observation B")
                        .with_position(PhysicalPosition::new(312, 64)),
                )
                .map_err(|e| e.to_string())?;
            Ok::<_, String>((source, target))
        })();
        match result {
            Ok((source, target)) => {
                self.source_id = Some(source.id());
                self.source = Some(source);
                self.target = Some(target);
                self.settle_until = Instant::now() + Duration::from_millis(150);
                event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + TICK));
            },
            Err(error) => self.fail(event_loop, error),
        }
    }

    fn new_events(&mut self, event_loop: &ActiveEventLoop, _cause: StartCause) {
        self.check_deadline(event_loop);
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        if !self.check_deadline(event_loop) {
            return;
        }
        match event {
            WindowEvent::CursorMoved { device_id, facts, .. } => {
                self.moved_count += 1;
                if let Some(point) = facts.desktop_position {
                    self.last_motion = Some((window_id, device_id, point));
                    if self.phase == Phase::SourceMotion
                        && Some(window_id) == self.source_id
                        && point == self.expected_point()
                    {
                        self.device = Some(device_id);
                    }
                }
            },
            WindowEvent::MouseInput { device_id, state, button: MouseButton::Left, .. } => {
                let result = event_loop.observe_pointer(self.source_id.unwrap(), device_id, None);
                if result != X11PointerObservation::Unavailable {
                    self.fail(
                        event_loop,
                        format!("MouseInput expansion allowed observation: {result:?}"),
                    );
                    return;
                }
                if self.device != Some(device_id) || Some(window_id) != self.source_id {
                    self.fail(
                        event_loop,
                        format!(
                            "primary input changed device/source: {device_id:?}, {window_id:?}"
                        ),
                    );
                    return;
                }
                self.callback_checks += 1;
                self.held = state == ElementState::Pressed;
                self.released |= state == ElementState::Released;
            },
            WindowEvent::Destroyed if Some(window_id) == self.source_id => self.destroyed = true,
            _ => (),
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if !self.check_deadline(event_loop) {
            return;
        }
        if let Err(error) = self.advance(event_loop) {
            self.fail(event_loop, error);
        }
        if self.failure.is_none() && self.phase != Phase::Done {
            event_loop.set_control_flow(ControlFlow::WaitUntil(
                (Instant::now() + TICK).min(self.deadline),
            ));
        }
    }
}

#[test]
fn queried_position_and_hover_follow_native_motion_without_proving_capture() {
    if std::env::var(OPT_IN).as_deref() != Ok("1") || std::env::var_os("DISPLAY").is_none() {
        eprintln!("skipped: set {OPT_IN}=1 with DISPLAY and xdotool on a dedicated X11 display");
        return;
    }

    let _release_primary = ReleasePrimary;
    let mut builder = EventLoop::builder();
    builder.with_x11().with_any_thread(true);
    let mut event_loop = builder.build().expect("create opted-in X11 event loop");
    let mut app = App::new();
    let result = event_loop.run_app_on_demand(&mut app);
    // Release before reporting failures, including a deadline that fired while the button was down.
    drop(_release_primary);
    result.expect("run opted-in X11 event loop");
    assert!(app.failure.is_none(), "{}", app.failure.as_deref().unwrap_or(""));
    assert_eq!(app.phase, Phase::Done);
    assert_eq!(app.samples, 3);
    assert_eq!(app.callback_checks, 2, "real press and release both reached native guards");
    assert_eq!(app.pending_checks, 4, "press, two held moves, and release kept their raw prefixes");
    assert!(app.quiet_queries >= 6);
}
