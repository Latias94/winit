//! # X11
#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

use crate::event_loop::{ActiveEventLoop, EventLoop, EventLoopBuilder};
use crate::monitor::MonitorHandle;
use crate::window::{Window, WindowAttributes};

use crate::dpi::{PhysicalPosition, PhysicalSize, Size};

/// One fresh, synchronous observation of an exact X11 pointer.
///
/// This never creates a motion event or consumes pending native events. Sampling an exact
/// already acquired explicit capture may refresh that same source-directed XI2 grab.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum X11PointerObservation {
    /// Facts sampled after the preceding native event has been fully dispatched.
    Observed {
        /// Queried pointer facts, with capture only when independently refreshed.
        facts: crate::event::PointerEventFacts,
        /// Native delivery from independently refreshed explicit capture, otherwise Unknown.
        delivery: crate::event::PointerWindowRoute,
    },
    /// Native events, window/device changes, or an incomplete query prevent an ordered sample.
    Unavailable,
    /// The active backend or device does not support this observation.
    Unsupported,
}

/// One monitor in an exact X11 work-area authority snapshot.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct X11WorkAreaRecord {
    crtc_id: u32,
    monitor_position: PhysicalPosition<i32>,
    monitor_size: PhysicalSize<u32>,
    work_area_position: PhysicalPosition<i32>,
    work_area_size: PhysicalSize<u32>,
    scale_factor: f64,
}

impl X11WorkAreaRecord {
    pub(crate) const fn new(
        crtc_id: u32,
        monitor_position: PhysicalPosition<i32>,
        monitor_size: PhysicalSize<u32>,
        work_area_position: PhysicalPosition<i32>,
        work_area_size: PhysicalSize<u32>,
        scale_factor: f64,
    ) -> Self {
        Self {
            crtc_id,
            monitor_position,
            monitor_size,
            work_area_position,
            work_area_size,
            scale_factor,
        }
    }

    /// The RandR CRTC that identifies this monitor.
    pub const fn crtc_id(&self) -> u32 {
        self.crtc_id
    }

    /// The monitor position in root-window physical coordinates.
    pub const fn monitor_position(&self) -> PhysicalPosition<i32> {
        self.monitor_position
    }

    /// The full monitor size in physical pixels.
    pub const fn monitor_size(&self) -> PhysicalSize<u32> {
        self.monitor_size
    }

    /// The usable work-area position in root-window physical coordinates.
    pub const fn work_area_position(&self) -> PhysicalPosition<i32> {
        self.work_area_position
    }

    /// The usable work-area size in physical pixels.
    pub const fn work_area_size(&self) -> PhysicalSize<u32> {
        self.work_area_size
    }

    /// Winit's scale factor for this monitor in the same authority generation.
    pub const fn scale_factor(&self) -> f64 {
        self.scale_factor
    }
}

/// A complete, exact X11 monitor work-area roster from one authority generation.
///
/// Winit currently publishes this snapshot only when one active RandR CRTC exactly covers the root
/// window. EWMH exposes a desktop-wide work area, so multi-monitor work areas remain unknown until
/// their per-monitor projection can be proven.
#[derive(Clone, Debug, PartialEq)]
pub struct X11WorkAreaAuthoritySnapshot {
    generation: u64,
    records: Vec<X11WorkAreaRecord>,
}

impl X11WorkAreaAuthoritySnapshot {
    pub(crate) const fn new(generation: u64, records: Vec<X11WorkAreaRecord>) -> Self {
        Self { generation, records }
    }

    /// Monotonically increasing generation of the validated authority snapshot.
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// The exact records proven for this generation.
    pub fn records(&self) -> &[X11WorkAreaRecord] {
        &self.records
    }
}

/// X window type. Maps directly to
/// [`_NET_WM_WINDOW_TYPE`](https://specifications.freedesktop.org/wm-spec/wm-spec-1.5.html).
#[derive(Debug, Default, Copy, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum WindowType {
    /// A desktop feature. This can include a single window containing desktop icons with the same
    /// dimensions as the screen, allowing the desktop environment to have full control of the
    /// desktop, without the need for proxying root window clicks.
    Desktop,
    /// A dock or panel feature. Typically a Window Manager would keep such windows on top of all
    /// other windows.
    Dock,
    /// Toolbar windows. "Torn off" from the main application.
    Toolbar,
    /// Pinnable menu windows. "Torn off" from the main application.
    Menu,
    /// A small persistent utility window, such as a palette or toolbox.
    Utility,
    /// The window is a splash screen displayed as an application is starting up.
    Splash,
    /// This is a dialog window.
    Dialog,
    /// A dropdown menu that usually appears when the user clicks on an item in a menu bar.
    /// This property is typically used on override-redirect windows.
    DropdownMenu,
    /// A popup menu that usually appears when the user right clicks on an object.
    /// This property is typically used on override-redirect windows.
    PopupMenu,
    /// A tooltip window. Usually used to show additional information when hovering over an object
    /// with the cursor. This property is typically used on override-redirect windows.
    Tooltip,
    /// The window is a notification.
    /// This property is typically used on override-redirect windows.
    Notification,
    /// This should be used on the windows that are popped up by combo boxes.
    /// This property is typically used on override-redirect windows.
    Combo,
    /// This indicates the window is being dragged.
    /// This property is typically used on override-redirect windows.
    Dnd,
    /// This is a normal, top-level window.
    #[default]
    Normal,
}

/// The first argument in the provided hook will be the pointer to `XDisplay`
/// and the second one the pointer to [`XErrorEvent`]. The returned `bool` is an
/// indicator whether the error was handled by the callback.
///
/// [`XErrorEvent`]: https://linux.die.net/man/3/xerrorevent
pub type XlibErrorHook =
    Box<dyn Fn(*mut std::ffi::c_void, *mut std::ffi::c_void) -> bool + Send + Sync>;

/// A unique identifier for an X11 visual.
pub type XVisualID = u32;

/// A unique identifier for an X11 window.
pub type XWindow = u32;

/// Hook to winit's xlib error handling callback.
///
/// This method is provided as a safe way to handle the errors coming from X11
/// when using xlib in external crates, like glutin for GLX access. Trying to
/// handle errors by speculating with `XSetErrorHandler` is [`unsafe`].
///
/// **Be aware that your hook is always invoked and returning `true` from it will
/// prevent `winit` from getting the error itself. It's wise to always return
/// `false` if you're not initiated the `Sync`.**
///
/// [`unsafe`]: https://www.remlab.net/op/xlib.shtml
#[inline]
pub fn register_xlib_error_hook(hook: XlibErrorHook) {
    // Append new hook.
    crate::platform_impl::XLIB_ERROR_HOOKS.lock().unwrap().push(hook);
}

/// Result of an explicit source-directed capture operation.
///
/// Acquired reports a native effect followed by normal FIFO settlement. It does not establish
/// fresh capture/delivery observation, uninterrupted earlier capture or freedom from freeze.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum X11PointerCaptureAcquisition {
    /// The effect or its normal FIFO fence has not settled.
    Pending,
    /// The operation was accepted and its FIFO fence completed.
    Acquired,
    /// The original press or finite source profile cannot acquire capture.
    Unavailable,
    /// The active backend does not implement this X11 operation.
    Unsupported,
}

/// Result of releasing the exact original explicit capture operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum X11PointerCaptureRelease {
    /// The exact checked native release completed.
    Released,
    /// The original operation or its release still owns an unresolved duty.
    Pending,
    /// This identity has no current explicit release duty.
    Unavailable,
    /// The active backend does not implement this X11 operation.
    Unsupported,
}

/// Additional methods on [`ActiveEventLoop`] that are specific to X11.
pub trait ActiveEventLoopExtX11 {
    /// True if the [`ActiveEventLoop`] uses X11.
    fn is_x11(&self) -> bool;

    /// Returns one complete work-area authority snapshot when X11 can prove it exactly.
    ///
    /// This returns `None` for non-X11 backends and whenever EWMH, RandR, or scale authority is
    /// missing, malformed, stale, or ambiguous. A cached snapshot is refreshed only after a
    /// relevant native event invalidates it. The current proof is limited to the X connection's
    /// first screen because Winit's X resource database uses that screen's root.
    fn work_area_authority_snapshot(&self) -> Option<X11WorkAreaAuthoritySnapshot>;

    /// Acquires unconfined, asynchronous, source-directed capture for one original Primary press.
    ///
    /// Call only after application drag ownership is established. This changes native routing;
    /// it is not a pointer observation. The finite profile accepts a viewable source directly
    /// below this root or below one root-observed parent, and an enabled exact master pointer.
    /// Deeper or unobserved source ancestry cannot issue a qualified original press identity.
    /// Ordinary window grabs and unresolved cleanup
    /// exclude acquisition. One original press permits one attempt; a failed or revoked attempt
    /// cannot be renewed into continuity. Drain normal events while Pending, then poll this same
    /// identity. Acquired does not authorize a stationary action without independent fresh facts.
    fn acquire_pointer_capture(
        &self,
        expected_window: crate::window::WindowId,
        device_id: crate::event::DeviceId,
        expected_press: &crate::event::PointerPressToken,
    ) -> X11PointerCaptureAcquisition;

    /// Releases only the explicit operation tied to this connection, window, device and press.
    ///
    /// Primary termination, window mutation and device retirement also retain automatic cleanup
    /// in the same owner. A late release cannot ungrab a successor. Pending retains the duty for
    /// normal event-loop service; it must not be treated as release completion.
    fn release_pointer_capture(
        &self,
        expected_window: crate::window::WindowId,
        device_id: crate::event::DeviceId,
        expected_press: &crate::event::PointerPressToken,
    ) -> X11PointerCaptureRelease;

    /// Samples a managed master pointer without replaying cached position or capture facts.
    ///
    /// `expected_window` must still identify the live source window. Pending native events and
    /// calls made while a native event is being expanded return `Unavailable`, so callers must
    /// drain and commit their own accepted input before trying again. Capture/delivery cannot
    /// be reconstructed from Winit's retained implicit owner. With the exact `expected_press`
    /// and an already settled explicit capture, a source with qualified parent history may refresh
    /// its same-client XI2 grab after the position query. Successful refresh, unchanged exact
    /// duty and an empty native event queue through the final reply can report both routes as
    /// the source at that sample. This refresh changes native grab state; it never initially
    /// acquires capture or proves the older grab was uninterrupted. A refused refresh retains
    /// Unknown routes; an indeterminate refresh retains cleanup without granting authority.
    /// Without that independent proof both routes remain Unknown. X11 local event geometry is
    /// not supplied by this desktop observation. Position describes the server's current logical
    /// state. Bounded child queries may discover a window-manager hierarchy. Only the terminal
    /// reply's direct live child and that same reply's root coordinates supply known
    /// hover/position; earlier replies only identify the next query object. An unknown leaf,
    /// subtree exit or depth limit keeps hover Unknown, and a failed query makes the sample
    /// Unavailable. This identifies that child's subtree, not an embedded child's deepest hit
    /// or delivery. The expected window's viewability is checked before sampling; it is not an
    /// atomic part of the terminal pointer reply and grants no source-window authority. Native
    /// parent unmap/reparent/destruction revokes the original identity through root structure
    /// events; synthetic parent notifications and unrelated root windows cannot revoke that
    /// owner.
    ///
    /// When `expected_press` is supplied, the exact original Primary press must remain active
    /// for this connection, window and device before and after sampling. A revoked or replaced
    /// press returns `Unavailable`. Matching identity alone does not establish capture or delivery.
    /// Events encountered during the query reject its entire sample, rather than being drained
    /// and combined with older facts. Logical buttons do not prove physical input state, and
    /// a successful sample does not guarantee the absence of later protocol freezes.
    /// `None` preserves ordinary position/hover observation without a press requirement.
    fn observe_pointer(
        &self,
        expected_window: crate::window::WindowId,
        device_id: crate::event::DeviceId,
        expected_press: Option<&crate::event::PointerPressToken>,
    ) -> X11PointerObservation;
}

impl ActiveEventLoopExtX11 for ActiveEventLoop {
    fn acquire_pointer_capture(
        &self,
        expected_window: crate::window::WindowId,
        device_id: crate::event::DeviceId,
        expected_press: &crate::event::PointerPressToken,
    ) -> X11PointerCaptureAcquisition {
        match &self.p {
            crate::platform_impl::ActiveEventLoop::X(event_loop) => {
                event_loop.acquire_pointer_capture(expected_window, device_id, expected_press)
            },
            #[cfg(wayland_platform)]
            crate::platform_impl::ActiveEventLoop::Wayland(_) => {
                X11PointerCaptureAcquisition::Unsupported
            },
        }
    }

    fn release_pointer_capture(
        &self,
        expected_window: crate::window::WindowId,
        device_id: crate::event::DeviceId,
        expected_press: &crate::event::PointerPressToken,
    ) -> X11PointerCaptureRelease {
        match &self.p {
            crate::platform_impl::ActiveEventLoop::X(event_loop) => {
                event_loop.release_pointer_capture(expected_window, device_id, expected_press)
            },
            #[cfg(wayland_platform)]
            crate::platform_impl::ActiveEventLoop::Wayland(_) => {
                X11PointerCaptureRelease::Unsupported
            },
        }
    }

    #[inline]
    fn is_x11(&self) -> bool {
        !self.p.is_wayland()
    }

    #[inline]
    fn work_area_authority_snapshot(&self) -> Option<X11WorkAreaAuthoritySnapshot> {
        match &self.p {
            crate::platform_impl::ActiveEventLoop::X(event_loop) => {
                event_loop.work_area_authority_snapshot()
            },
            #[cfg(wayland_platform)]
            crate::platform_impl::ActiveEventLoop::Wayland(_) => None,
        }
    }

    #[inline]
    fn observe_pointer(
        &self,
        expected_window: crate::window::WindowId,
        device_id: crate::event::DeviceId,
        expected_press: Option<&crate::event::PointerPressToken>,
    ) -> X11PointerObservation {
        match &self.p {
            crate::platform_impl::ActiveEventLoop::X(event_loop) => {
                event_loop.observe_pointer(expected_window, device_id, expected_press)
            },
            #[cfg(wayland_platform)]
            crate::platform_impl::ActiveEventLoop::Wayland(_) => X11PointerObservation::Unsupported,
        }
    }
}

/// Additional methods on [`EventLoop`] that are specific to X11.
pub trait EventLoopExtX11 {
    /// True if the [`EventLoop`] uses X11.
    fn is_x11(&self) -> bool;
}

impl<T: 'static> EventLoopExtX11 for EventLoop<T> {
    #[inline]
    fn is_x11(&self) -> bool {
        !self.event_loop.is_wayland()
    }
}

/// Additional methods on [`EventLoopBuilder`] that are specific to X11.
pub trait EventLoopBuilderExtX11 {
    /// Force using X11.
    fn with_x11(&mut self) -> &mut Self;

    /// Whether to allow the event loop to be created off of the main thread.
    ///
    /// By default, the window is only allowed to be created on the main
    /// thread, to make platform compatibility easier.
    fn with_any_thread(&mut self, any_thread: bool) -> &mut Self;
}

impl<T> EventLoopBuilderExtX11 for EventLoopBuilder<T> {
    #[inline]
    fn with_x11(&mut self) -> &mut Self {
        self.platform_specific.forced_backend = Some(crate::platform_impl::Backend::X);
        self
    }

    #[inline]
    fn with_any_thread(&mut self, any_thread: bool) -> &mut Self {
        self.platform_specific.any_thread = any_thread;
        self
    }
}

/// Additional methods on [`Window`] that are specific to X11.
pub trait WindowExtX11 {
    /// Returns the exact physical outer position and size when the active X11
    /// window manager publishes authoritative EWMH frame extents.
    ///
    /// `None` means the active backend is not X11 or the window manager only
    /// permits a heuristic frame calculation. Callers must not promote that
    /// absence into layout authority.
    fn exact_outer_position_and_size(&self) -> Option<(PhysicalPosition<i32>, PhysicalSize<u32>)>;
}

impl WindowExtX11 for Window {
    #[inline]
    fn exact_outer_position_and_size(&self) -> Option<(PhysicalPosition<i32>, PhysicalSize<u32>)> {
        self.window.x11_exact_outer_position_and_size()
    }
}

/// Additional methods on [`WindowAttributes`] that are specific to X11.
pub trait WindowAttributesExtX11 {
    /// Create this window with a specific X11 visual.
    fn with_x11_visual(self, visual_id: XVisualID) -> Self;

    fn with_x11_screen(self, screen_id: i32) -> Self;

    /// Build window with the given `general` and `instance` names.
    ///
    /// The `general` sets general class of `WM_CLASS(STRING)`, while `instance` set the
    /// instance part of it. The resulted property looks like `WM_CLASS(STRING) = "instance",
    /// "general"`.
    ///
    /// For details about application ID conventions, see the
    /// [Desktop Entry Spec](https://specifications.freedesktop.org/desktop-entry-spec/desktop-entry-spec-latest.html#desktop-file-id)
    fn with_name(self, general: impl Into<String>, instance: impl Into<String>) -> Self;

    /// Build window with override-redirect flag; defaults to false.
    fn with_override_redirect(self, override_redirect: bool) -> Self;

    /// Build window with `_NET_WM_WINDOW_TYPE` hints; defaults to `Normal`.
    fn with_x11_window_type(self, x11_window_type: Vec<WindowType>) -> Self;

    /// Build window with base size hint.
    ///
    /// ```
    /// # use winit::dpi::{LogicalSize, PhysicalSize};
    /// # use winit::window::Window;
    /// # use winit::platform::x11::WindowAttributesExtX11;
    /// // Specify the size in logical dimensions like this:
    /// Window::default_attributes().with_base_size(LogicalSize::new(400.0, 200.0));
    ///
    /// // Or specify the size in physical dimensions like this:
    /// Window::default_attributes().with_base_size(PhysicalSize::new(400, 200));
    /// ```
    fn with_base_size<S: Into<Size>>(self, base_size: S) -> Self;

    /// Embed this window into another parent window.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use winit::window::Window;
    /// use winit::event_loop::ActiveEventLoop;
    /// use winit::platform::x11::{XWindow, WindowAttributesExtX11};
    /// # fn create_window(event_loop: &ActiveEventLoop) -> Result<(), Box<dyn std::error::Error>> {
    /// let parent_window_id = std::env::args().nth(1).unwrap().parse::<XWindow>()?;
    /// let window_attributes = Window::default_attributes().with_embed_parent_window(parent_window_id);
    /// let window = event_loop.create_window(window_attributes)?;
    /// # Ok(()) }
    /// ```
    fn with_embed_parent_window(self, parent_window_id: XWindow) -> Self;
}

impl WindowAttributesExtX11 for WindowAttributes {
    #[inline]
    fn with_x11_visual(mut self, visual_id: XVisualID) -> Self {
        self.platform_specific.x11.visual_id = Some(visual_id);
        self
    }

    #[inline]
    fn with_x11_screen(mut self, screen_id: i32) -> Self {
        self.platform_specific.x11.screen_id = Some(screen_id);
        self
    }

    #[inline]
    fn with_name(mut self, general: impl Into<String>, instance: impl Into<String>) -> Self {
        self.platform_specific.name =
            Some(crate::platform_impl::ApplicationName::new(general.into(), instance.into()));
        self
    }

    #[inline]
    fn with_override_redirect(mut self, override_redirect: bool) -> Self {
        self.platform_specific.x11.override_redirect = override_redirect;
        self
    }

    #[inline]
    fn with_x11_window_type(mut self, x11_window_types: Vec<WindowType>) -> Self {
        self.platform_specific.x11.x11_window_types = x11_window_types;
        self
    }

    #[inline]
    fn with_base_size<S: Into<Size>>(mut self, base_size: S) -> Self {
        self.platform_specific.x11.base_size = Some(base_size.into());
        self
    }

    #[inline]
    fn with_embed_parent_window(mut self, parent_window_id: XWindow) -> Self {
        self.platform_specific.x11.embed_window = Some(parent_window_id);
        self
    }
}

/// Additional methods on `MonitorHandle` that are specific to X11.
pub trait MonitorHandleExtX11 {
    /// Returns the inner identifier of the monitor.
    fn native_id(&self) -> u32;
}

impl MonitorHandleExtX11 for MonitorHandle {
    #[inline]
    fn native_id(&self) -> u32 {
        self.inner.native_identifier()
    }
}
