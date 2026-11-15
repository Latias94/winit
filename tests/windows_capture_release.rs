// Keep the Windows release-order state executable on every host. The native
// callback uses this exact helper around ReleaseCapture and WM_CAPTURECHANGED.
#[path = "../src/platform_impl/windows/mouse_capture.rs"]
mod mouse_capture;

#[cfg(all(target_os = "windows", feature = "rwh_06"))]
mod native_ownership {
    use std::ptr;
    use std::time::Duration;

    use rwh_06::{HasWindowHandle, RawWindowHandle};
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetCapture, ReleaseCapture, SetCapture};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DestroyWindow, IsWindow, SendMessageW, WM_CAPTURECHANGED, WM_LBUTTONDOWN,
        WM_LBUTTONUP, WM_RBUTTONDOWN, WM_RBUTTONUP, WS_POPUP,
    };
    use winit::application::ApplicationHandler;
    use winit::event::{ElementState, PointerWindowRoute, WindowEvent};
    use winit::event_loop::{ActiveEventLoop, EventLoop};
    use winit::platform::pump_events::EventLoopExtPumpEvents;
    use winit::platform::windows::{EventLoopBuilderExtWindows, WindowAttributesExtWindows};
    use winit::window::{Window, WindowId};

    #[derive(Default)]
    struct PointerEvents {
        captures: Vec<(WindowId, PointerWindowRoute)>,
        releases: Vec<(WindowId, PointerWindowRoute)>,
    }

    impl ApplicationHandler for PointerEvents {
        fn resumed(&mut self, _: &ActiveEventLoop) {}

        fn window_event(&mut self, _: &ActiveEventLoop, window: WindowId, event: WindowEvent) {
            match event {
                WindowEvent::PointerCaptureChanged { capture, .. } => {
                    self.captures.push((window, capture));
                },
                WindowEvent::MouseInput { state: ElementState::Released, facts, .. } => {
                    self.releases.push((window, facts.capture));
                },
                _ => {},
            }
        }
    }

    fn hwnd(window: &Window) -> HWND {
        match window.window_handle().unwrap().as_raw() {
            RawWindowHandle::Win32(handle) => handle.hwnd.get(),
            _ => unreachable!("the regression runs only on Windows"),
        }
    }

    #[test]
    #[ignore = "requires an isolated Windows desktop"]
    fn a_late_release_keeps_the_new_owner_and_classifies_its_managed_window() {
        let mut event_loop = EventLoop::builder().with_any_thread(true).build().unwrap();
        let attributes = Window::default_attributes()
            .with_visible(false)
            .with_active(false)
            .with_drag_and_drop(false);
        #[allow(deprecated)]
        let source = event_loop.create_window(attributes.clone()).unwrap();
        #[allow(deprecated)]
        let successor = event_loop.create_window(attributes).unwrap();
        let mut events = PointerEvents::default();
        event_loop.pump_app_events(Some(Duration::ZERO), &mut events);
        events.captures.clear();
        let source_hwnd = hwnd(&source);
        let successor_hwnd = hwnd(&successor);
        // Real window callbacks establish A, then transfer capture to B. A's
        // old up must neither release B nor classify this managed HWND as foreign.
        let after_late_release = unsafe {
            SendMessageW(source_hwnd, WM_LBUTTONDOWN, 1, 0);
            assert_eq!(GetCapture(), source_hwnd);
            SendMessageW(successor_hwnd, WM_LBUTTONDOWN, 1, 0);
            assert_eq!(GetCapture(), successor_hwnd);
            SendMessageW(source_hwnd, WM_LBUTTONUP, 0, 0);
            let owner = GetCapture();
            // Settle B's own release before any assertion can abandon this fixture.
            SendMessageW(successor_hwnd, WM_LBUTTONUP, 0, 0);
            owner
        };
        event_loop.pump_app_events(Some(Duration::ZERO), &mut events);
        assert_eq!(
            (
                after_late_release,
                events.captures.iter().any(|(window, capture)| {
                    *window == source.id()
                        && *capture == PointerWindowRoute::Window(successor.id())
                }),
                events.captures.iter().any(|(window, _)| *window == successor.id()),
                events.releases.first().copied(),
            ),
            (
                successor_hwnd,
                true,
                false,
                Some((source.id(), PointerWindowRoute::Window(successor.id()))),
            ),
            "A's stale up must preserve B; transfer reports B, while B's normal up has no early cancellation"
        );

        events.captures.clear();
        let while_right_held = unsafe {
            SendMessageW(source_hwnd, WM_LBUTTONDOWN, 1, 0);
            SendMessageW(source_hwnd, WM_RBUTTONDOWN, 3, 0);
            SendMessageW(source_hwnd, WM_LBUTTONUP, 2, 0);
            let owner = GetCapture();
            SendMessageW(source_hwnd, WM_RBUTTONUP, 0, 0);
            owner
        };
        event_loop.pump_app_events(Some(Duration::ZERO), &mut events);
        assert_eq!(while_right_held, source_hwnd, "the other held button retains capture");
        assert!(
            events.captures.iter().all(|(window, capture)| {
                *window == source.id() && *capture == PointerWindowRoute::Window(source.id())
            }),
            "normal chord release may retain ownership but must not emit cancellation"
        );
        events.captures.clear();

        drop(successor);
        event_loop.pump_app_events(Some(Duration::ZERO), &mut events);
        unsafe {
            assert_eq!(IsWindow(successor_hwnd), 0, "the queued destruction has completed");
            SendMessageW(source_hwnd, WM_CAPTURECHANGED, 0, successor_hwnd);
        }
        event_loop.pump_app_events(Some(Duration::ZERO), &mut events);
        assert_eq!(
            events.captures,
            [(source.id(), PointerWindowRoute::Unknown)],
            "a destroyed managed HWND must leave the event loop's owner roster"
        );
        events.captures.clear();

        // A valid non-winit HWND is foreign. Once destroyed, its stale handle
        // cannot establish a current owner and must instead remain unknown.
        let static_class =
            [b'S' as u16, b'T' as u16, b'A' as u16, b'T' as u16, b'I' as u16, b'C' as u16, 0];
        unsafe {
            let foreign = CreateWindowExW(
                0,
                static_class.as_ptr(),
                ptr::null(),
                WS_POPUP,
                0,
                0,
                1,
                1,
                0,
                0,
                0,
                ptr::null(),
            );
            assert_ne!(foreign, 0, "the unmanaged fixture window is created");
            SendMessageW(source_hwnd, WM_LBUTTONDOWN, 1, 0);
            SetCapture(foreign);
            assert_eq!(GetCapture(), foreign);
            assert_ne!(ReleaseCapture(), 0);
            assert_ne!(DestroyWindow(foreign), 0);
            SendMessageW(source_hwnd, WM_CAPTURECHANGED, 0, foreign);
        }
        event_loop.pump_app_events(Some(Duration::ZERO), &mut events);
        assert_eq!(
            events.captures,
            [
                (source.id(), PointerWindowRoute::Foreign),
                (source.id(), PointerWindowRoute::Unknown),
            ],
        );
    }
}
