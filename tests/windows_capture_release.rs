// Keep the Windows release-order state executable on every host. The native
// callback uses this exact helper around ReleaseCapture and WM_CAPTURECHANGED.
#[path = "../src/platform_impl/windows/mouse_capture.rs"]
mod mouse_capture;
