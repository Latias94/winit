//! Pure validation for exact X11 monitor work-area snapshots.
//!
//! XCB collection and event subscriptions live next to the connection. Keeping the proof rules in
//! this module makes fail-closed behavior testable without an X server.

use std::collections::HashSet;

const MAX_DESKTOPS: usize = 256;
const MAX_SUBSCRIBED_AUTHORITY_WINDOWS: usize = 64;
const IDENTITY_CRTC_TRANSFORM: [i32; 9] = [1 << 16, 0, 0, 0, 1 << 16, 0, 0, 0, 1 << 16];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct RootGeometry {
    pub(super) window: u32,
    pub(super) width: u32,
    pub(super) height: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct EwmhAtoms {
    pub(super) supporting_wm_check: u32,
    pub(super) wm_name: u32,
    pub(super) current_desktop: u32,
    pub(super) number_of_desktops: u32,
    pub(super) desktop_geometry: u32,
    pub(super) desktop_viewport: u32,
    pub(super) workarea: u32,
}

impl EwmhAtoms {
    fn required(self) -> [u32; 7] {
        [
            self.supporting_wm_check,
            self.wm_name,
            self.current_desktop,
            self.number_of_desktops,
            self.desktop_geometry,
            self.desktop_viewport,
            self.workarea,
        ]
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct EwmhSnapshot {
    pub(super) supported: Vec<u32>,
    pub(super) queried_supporting_wm: u32,
    pub(super) root_supporting_wm: u32,
    pub(super) wm_supporting_wm: u32,
    pub(super) wm_tree_root: u32,
    pub(super) wm_parent: u32,
    pub(super) wm_name: Vec<u8>,
    pub(super) current_desktop: Vec<u32>,
    pub(super) number_of_desktops: Vec<u32>,
    pub(super) desktop_geometry: Vec<u32>,
    pub(super) desktop_viewport: Vec<u32>,
    pub(super) workarea: Vec<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PhysicalRect {
    pub(super) x: i32,
    pub(super) y: i32,
    pub(super) width: u32,
    pub(super) height: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct RandrMonitor {
    pub(super) crtc_id: u32,
    pub(super) rect: PhysicalRect,
    pub(super) outputs: Vec<u32>,
    pub(super) current_transform: [i32; 9],
    pub(super) scale_factor: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct MonitorWorkArea {
    pub(super) crtc_id: u32,
    pub(super) monitor: PhysicalRect,
    pub(super) work_area: PhysicalRect,
    pub(super) scale_factor: f64,
}

pub(super) fn validate_ewmh(
    snapshot: &EwmhSnapshot,
    atoms: EwmhAtoms,
    root: RootGeometry,
) -> Option<PhysicalRect> {
    if root.width == 0 || root.height == 0 {
        return None;
    }

    let supported = snapshot.supported.iter().copied().collect::<HashSet<_>>();
    if atoms.required().into_iter().any(|atom| atom == 0 || !supported.contains(&atom)) {
        return None;
    }

    let wm_name = std::str::from_utf8(&snapshot.wm_name).ok()?;
    if snapshot.queried_supporting_wm == 0
        || snapshot.root_supporting_wm != snapshot.queried_supporting_wm
        || snapshot.root_supporting_wm != snapshot.wm_supporting_wm
        || snapshot.wm_tree_root != root.window
        || snapshot.wm_parent != root.window
        || wm_name.contains('\0')
        || wm_name.trim().is_empty()
    {
        return None;
    }

    let &[desktop] = snapshot.current_desktop.as_slice() else {
        return None;
    };
    let &[desktop_count] = snapshot.number_of_desktops.as_slice() else {
        return None;
    };
    let desktop_count = usize::try_from(desktop_count).ok()?;
    let desktop = usize::try_from(desktop).ok()?;
    if desktop_count == 0 || desktop_count > MAX_DESKTOPS || desktop >= desktop_count {
        return None;
    }

    if snapshot.desktop_geometry.as_slice() != [root.width, root.height]
        || snapshot.desktop_viewport.len() != desktop_count.checked_mul(2)?
        || snapshot.workarea.len() != desktop_count.checked_mul(4)?
    {
        return None;
    }

    let viewport = snapshot.desktop_viewport.get(desktop * 2..desktop * 2 + 2)?;
    if viewport != [0, 0] {
        return None;
    }

    let workarea = snapshot.workarea.get(desktop * 4..desktop * 4 + 4)?;
    let x = i32::try_from(workarea[0]).ok()?;
    let y = i32::try_from(workarea[1]).ok()?;
    let rect = PhysicalRect { x, y, width: workarea[2], height: workarea[3] };
    rect_inside_root(rect, root).then_some(rect)
}

pub(super) const fn merge_event_masks(existing: u32, required: u32) -> u32 {
    existing | required
}

pub(super) fn property_reply_length_is_exact(format: u8, value_len: u32, length: u32) -> bool {
    let bytes_per_item = match format {
        0 => return value_len == 0 && length == 0,
        8 => 1,
        16 => 2,
        32 => 4,
        _ => return false,
    };
    value_len
        .checked_mul(bytes_per_item)
        .and_then(|bytes| bytes.checked_add(3))
        .is_some_and(|padded_bytes| padded_bytes / 4 == length)
}

pub(super) const fn crtc_transform_is_identity(transform: [i32; 9]) -> bool {
    transform[0] == IDENTITY_CRTC_TRANSFORM[0]
        && transform[1] == 0
        && transform[2] == 0
        && transform[3] == 0
        && transform[4] == IDENTITY_CRTC_TRANSFORM[4]
        && transform[5] == 0
        && transform[6] == 0
        && transform[7] == 0
        && transform[8] == IDENTITY_CRTC_TRANSFORM[8]
}

pub(super) fn global_work_area_is_exact_for_monitors(
    monitors: &[RandrMonitor],
    root: RootGeometry,
) -> bool {
    matches!(
        monitors,
        [monitor]
            if monitor.rect
                == PhysicalRect { x: 0, y: 0, width: root.width, height: root.height }
    )
}

pub(super) fn exact_randr_scale_factor(
    (width_px, height_px): (u32, u32),
    (width_mm, height_mm): (u64, u64),
) -> Option<f64> {
    if width_px == 0 || height_px == 0 || width_mm == 0 || height_mm == 0 {
        return None;
    }
    let ppmm = ((width_px as f64 * height_px as f64) / (width_mm as f64 * height_mm as f64)).sqrt();
    if !ppmm.is_finite() || ppmm <= 0.0 {
        return None;
    }
    let scale_factor = (ppmm * (12.0 * 25.4 / 96.0)).round() / 12.0;
    (scale_factor.is_finite() && (1.0..=20.0).contains(&scale_factor)).then_some(scale_factor)
}

#[derive(Debug, Default)]
struct AuthorityWindows {
    supporting_wm: Option<u32>,
    xsettings_owner: Option<u32>,
    subscribed: HashSet<u32>,
}

impl AuthorityWindows {
    fn can_subscribe(&self, window: u32) -> bool {
        self.subscribed.contains(&window)
            || self.subscribed.len() < MAX_SUBSCRIBED_AUTHORITY_WINDOWS
    }

    fn set_supporting_wm(&mut self, window: u32) -> bool {
        if !self.can_subscribe(window) {
            return false;
        }
        self.subscribed.insert(window);
        self.supporting_wm = Some(window);
        true
    }

    fn set_xsettings_owner(&mut self, window: Option<u32>) -> bool {
        if let Some(window) = window {
            if !self.can_subscribe(window) {
                return false;
            }
            self.subscribed.insert(window);
        }
        self.xsettings_owner = window;
        true
    }

    fn contains(&self, window: u32) -> bool {
        self.subscribed.contains(&window)
    }

    fn destroyed(&mut self, window: u32) -> AuthorityWindowDestroyed {
        if !self.subscribed.remove(&window) {
            return AuthorityWindowDestroyed::NotSubscribed;
        }
        let mut current_changed = false;
        if self.supporting_wm == Some(window) {
            self.supporting_wm = None;
            current_changed = true;
        }
        if self.xsettings_owner == Some(window) {
            self.xsettings_owner = None;
            current_changed = true;
        }
        AuthorityWindowDestroyed::Subscribed { current_changed }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AuthorityWindowDestroyed {
    NotSubscribed,
    Subscribed { current_changed: bool },
}

pub(super) fn validate_roster(
    monitors: &[RandrMonitor],
    root: RootGeometry,
    desktop_work_area: PhysicalRect,
) -> Option<Vec<MonitorWorkArea>> {
    if monitors.is_empty() || !rect_inside_root(desktop_work_area, root) {
        return None;
    }

    let mut crtc_ids = HashSet::with_capacity(monitors.len());
    let mut output_ids = HashSet::with_capacity(monitors.len());
    for (index, monitor) in monitors.iter().enumerate() {
        if monitor.crtc_id == 0
            || !crtc_ids.insert(monitor.crtc_id)
            || monitor.outputs.len() != 1
            || monitor.outputs[0] == 0
            || !output_ids.insert(monitor.outputs[0])
            || !crtc_transform_is_identity(monitor.current_transform)
            || !monitor.scale_factor.is_finite()
            || monitor.scale_factor <= 0.0
            || !rect_inside_root(monitor.rect, root)
            || monitors[..index].iter().any(|other| intersects(other.rect, monitor.rect))
        {
            return None;
        }
    }

    monitors
        .iter()
        .map(|monitor| {
            Some(MonitorWorkArea {
                crtc_id: monitor.crtc_id,
                monitor: monitor.rect,
                work_area: intersection(monitor.rect, desktop_work_area)?,
                scale_factor: monitor.scale_factor,
            })
        })
        .collect()
}

fn rect_inside_root(rect: PhysicalRect, root: RootGeometry) -> bool {
    rect.width != 0
        && rect.height != 0
        && rect.x >= 0
        && rect.y >= 0
        && i64::from(rect.x) + i64::from(rect.width) <= i64::from(root.width)
        && i64::from(rect.y) + i64::from(rect.height) <= i64::from(root.height)
}

fn intersects(left: PhysicalRect, right: PhysicalRect) -> bool {
    intersection(left, right).is_some()
}

fn intersection(left: PhysicalRect, right: PhysicalRect) -> Option<PhysicalRect> {
    let x1 = i64::from(left.x).max(i64::from(right.x));
    let y1 = i64::from(left.y).max(i64::from(right.y));
    let x2 = (i64::from(left.x) + i64::from(left.width))
        .min(i64::from(right.x) + i64::from(right.width));
    let y2 = (i64::from(left.y) + i64::from(left.height))
        .min(i64::from(right.y) + i64::from(right.height));
    let width = u32::try_from(x2.checked_sub(x1)?).ok()?;
    let height = u32::try_from(y2.checked_sub(y1)?).ok()?;
    if width == 0 || height == 0 {
        return None;
    }
    Some(PhysicalRect { x: i32::try_from(x1).ok()?, y: i32::try_from(y1).ok()?, width, height })
}

#[cfg(x11_platform)]
mod native {
    use super::{
        crtc_transform_is_identity, global_work_area_is_exact_for_monitors,
        property_reply_length_is_exact, validate_ewmh, validate_roster, AuthorityWindowDestroyed,
        AuthorityWindows, EwmhAtoms, EwmhSnapshot, PhysicalRect, RandrMonitor, RootGeometry,
    };
    use crate::dpi::{validate_scale_factor, PhysicalPosition, PhysicalSize};
    use crate::platform::x11::{X11WorkAreaAuthoritySnapshot, X11WorkAreaRecord};
    use crate::platform_impl::x11::atoms::*;
    use crate::platform_impl::x11::util::randr::{resolve_scale_authority, ScaleAuthority};
    use crate::platform_impl::x11::xsettings::parse_xsettings_dpi_exact;
    use crate::platform_impl::x11::XConnection;
    use std::collections::HashSet;
    use std::str::FromStr;
    use x11rb::connection::Connection as _;
    use x11rb::protocol::randr::{self, ConnectionExt as _};
    use x11rb::protocol::render;
    use x11rb::protocol::xproto::{self, ConnectionExt as _};

    const MAX_SUPPORTED_ATOMS: usize = 512;
    const MAX_WM_NAME_BYTES: usize = 1024;
    const MAX_XSETTINGS_BYTES: usize = 256 * 1024;
    const MAX_RESOURCE_MANAGER_BYTES: usize = 256 * 1024;
    const MAX_DESKTOPS: usize = 256;
    const MAX_CRTCS: usize = 32;
    const MAX_OUTPUTS: usize = 64;
    const MAX_MODES: usize = 4096;
    const MAX_MODE_NAMES_BYTES: usize = 64 * 1024;
    const MAX_OUTPUT_NAME_BYTES: usize = 1024;
    const MAX_SUPPORTING_WM_CHILDREN: usize = 256;
    const MAX_TRANSFORM_FILTER_NAME_BYTES: usize = 256;
    const MAX_TRANSFORM_PARAMS: usize = 64;

    pub(crate) struct WorkAreaCache {
        tracking_enabled: bool,
        dirty: bool,
        generation: u64,
        root: Option<xproto::Window>,
        authority_windows: AuthorityWindows,
        snapshot: Option<X11WorkAreaAuthoritySnapshot>,
    }

    impl WorkAreaCache {
        pub(crate) fn new() -> Self {
            Self {
                tracking_enabled: false,
                dirty: true,
                generation: 0,
                root: None,
                authority_windows: AuthorityWindows::default(),
                snapshot: None,
            }
        }

        fn invalidate(&mut self) {
            self.dirty = true;
            self.snapshot = None;
        }
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct ResourcesStamp {
        timestamp: xproto::Timestamp,
        config_timestamp: xproto::Timestamp,
        crtcs: Vec<randr::Crtc>,
        outputs: Vec<randr::Output>,
        modes: Vec<randr::Mode>,
        names: Vec<u8>,
    }

    #[derive(Clone, Debug, PartialEq)]
    struct ScaleAuthorityStamp {
        authority: ScaleAuthority,
        xsettings_owner: Option<xproto::Window>,
        xsettings_data: Option<Vec<u8>>,
        resource_manager_data: Option<Vec<u8>>,
        database_xft_dpi: Option<String>,
    }

    impl ResourcesStamp {
        fn from_reply(reply: &randr::GetScreenResourcesCurrentReply) -> Option<Self> {
            if reply.crtcs.len() > MAX_CRTCS
                || reply.outputs.len() > MAX_OUTPUTS
                || reply.modes.len() > MAX_MODES
                || reply.names.len() > MAX_MODE_NAMES_BYTES
            {
                return None;
            }
            let crtc_ids = reply.crtcs.iter().copied().collect::<HashSet<_>>();
            let output_ids = reply.outputs.iter().copied().collect::<HashSet<_>>();
            let mode_ids = reply.modes.iter().map(|mode| mode.id).collect::<HashSet<_>>();
            let mode_name_bytes = reply
                .modes
                .iter()
                .try_fold(0usize, |total, mode| total.checked_add(usize::from(mode.name_len)))?;
            if crtc_ids.len() != reply.crtcs.len()
                || crtc_ids.contains(&0)
                || output_ids.len() != reply.outputs.len()
                || output_ids.contains(&0)
                || mode_ids.len() != reply.modes.len()
                || mode_ids.contains(&0)
                || mode_name_bytes != reply.names.len()
            {
                return None;
            }
            Some(Self {
                timestamp: reply.timestamp,
                config_timestamp: reply.config_timestamp,
                crtcs: reply.crtcs.clone(),
                outputs: reply.outputs.clone(),
                modes: reply.modes.iter().map(|mode| mode.id).collect(),
                names: reply.names.clone(),
            })
        }
    }

    impl XConnection {
        pub(crate) fn enable_work_area_tracking(
            &self,
            root: xproto::Window,
            xsettings_owner_events: bool,
        ) {
            let root_events =
                self.select_events_preserving(root, xproto::EventMask::PROPERTY_CHANGE);
            let resource_root_matches = self
                .xcb_connection()
                .setup()
                .roots
                .first()
                .is_some_and(|screen| screen.root == root);
            let tracking_enabled = root_events
                && resource_root_matches
                && (self.xsettings_screen().is_none() || xsettings_owner_events);

            let mut cache = self.work_area_cache.lock().unwrap_or_else(|error| error.into_inner());
            cache.root = Some(root);
            cache.tracking_enabled = tracking_enabled;
            cache.invalidate();
            // This startup path preserves ordinary Winit DPI notifications even if the public
            // work-area snapshot API is never called.
            let _ = self.refresh_xsettings_subscription(&mut cache);
        }

        pub(crate) fn invalidate_work_area_authority(&self) {
            self.work_area_cache.lock().unwrap_or_else(|error| error.into_inner()).invalidate();
        }

        pub(crate) fn work_area_property_changed(
            &self,
            window: xproto::Window,
            atom: xproto::Atom,
        ) -> bool {
            let mut cache = self.work_area_cache.lock().unwrap_or_else(|error| error.into_inner());
            let atoms = self.atoms();
            let root_property = cache.root == Some(window)
                && matches!(
                    atom,
                    value if value == atoms[_NET_SUPPORTED]
                        || value == atoms[_NET_SUPPORTING_WM_CHECK]
                        || value == atoms[_NET_CURRENT_DESKTOP]
                        || value == atoms[_NET_NUMBER_OF_DESKTOPS]
                        || value == atoms[_NET_DESKTOP_GEOMETRY]
                        || value == atoms[_NET_DESKTOP_VIEWPORT]
                        || value == atoms[_NET_WORKAREA]
                        || value == xproto::AtomEnum::RESOURCE_MANAGER.into()
                );
            let supporting_wm_property = cache.authority_windows.supporting_wm == Some(window)
                && (atom == atoms[_NET_SUPPORTING_WM_CHECK] || atom == atoms[_NET_WM_NAME]);
            let xsettings_property = cache.authority_windows.xsettings_owner == Some(window)
                && atom == atoms[_XSETTINGS_SETTINGS];
            if root_property || supporting_wm_property || xsettings_property {
                cache.invalidate();
            }
            xsettings_property
        }

        pub(crate) fn is_work_area_authority_window(&self, window: xproto::Window) -> bool {
            self.work_area_cache
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .is_foreign_authority_window(window)
        }

        pub(crate) fn work_area_authority_window_destroyed(&self, window: xproto::Window) -> bool {
            let mut cache = self.work_area_cache.lock().unwrap_or_else(|error| error.into_inner());
            match cache.authority_windows.destroyed(window) {
                AuthorityWindowDestroyed::NotSubscribed => false,
                AuthorityWindowDestroyed::Subscribed { current_changed: _ } => {
                    // A historical subscription disappearing may free bounded capacity for the
                    // current authority, so every subscribed destruction permits one retry.
                    cache.invalidate();
                    true
                },
            }
        }

        pub(crate) fn xsettings_owner_changed(&self, owner: xproto::Window) {
            let mut cache = self.work_area_cache.lock().unwrap_or_else(|error| error.into_inner());
            let owner = (owner != 0
                && cache.authority_windows.can_subscribe(owner)
                && self.select_xsettings_owner_events(cache.root, owner))
            .then_some(owner);
            let inserted = cache.authority_windows.set_xsettings_owner(owner);
            debug_assert!(inserted, "capacity was checked before subscribing");
            cache.invalidate();
        }

        pub(crate) fn work_area_authority_snapshot(&self) -> Option<X11WorkAreaAuthoritySnapshot> {
            let mut cache = self.work_area_cache.lock().unwrap_or_else(|error| error.into_inner());
            if !cache.tracking_enabled {
                return None;
            }
            if !cache.dirty {
                return cache.snapshot.clone();
            }

            let Some(root) = cache.root else {
                return self.finish_unknown_snapshot(&mut cache);
            };
            if !self.refresh_xsettings_subscription(&mut cache) {
                return self.finish_unknown_snapshot(&mut cache);
            }
            let supporting_wm = self
                .bounded_property32(
                    root,
                    self.atoms()[_NET_SUPPORTING_WM_CHECK],
                    xproto::AtomEnum::WINDOW.into(),
                    1,
                )
                .and_then(|windows| match windows.as_slice() {
                    [window] => Some(*window),
                    _ => None,
                });
            let Some(supporting_wm) =
                supporting_wm.filter(|window| *window != 0 && *window != root)
            else {
                return self.finish_unknown_snapshot(&mut cache);
            };
            if cache.authority_windows.supporting_wm != Some(supporting_wm) {
                if !cache.authority_windows.can_subscribe(supporting_wm)
                    || !self.select_supporting_wm_events(root, supporting_wm)
                {
                    return self.finish_unknown_snapshot(&mut cache);
                }
                let inserted = cache.authority_windows.set_supporting_wm(supporting_wm);
                debug_assert!(inserted, "capacity was checked before subscribing");
            }

            let records = self.capture_exact_work_area(
                root,
                supporting_wm,
                cache.authority_windows.xsettings_owner,
            );
            cache.dirty = false;
            let records = match records {
                Some(records) => records,
                None => {
                    cache.snapshot = None;
                    return None;
                },
            };
            let Some(generation) = cache.generation.checked_add(1) else {
                return self.finish_unknown_snapshot(&mut cache);
            };
            cache.generation = generation;
            let snapshot = X11WorkAreaAuthoritySnapshot::new(cache.generation, records);
            cache.snapshot = Some(snapshot.clone());
            Some(snapshot)
        }

        fn finish_unknown_snapshot(
            &self,
            cache: &mut WorkAreaCache,
        ) -> Option<X11WorkAreaAuthoritySnapshot> {
            cache.dirty = false;
            cache.snapshot = None;
            None
        }

        fn select_supporting_wm_events(
            &self,
            root: xproto::Window,
            window: xproto::Window,
        ) -> bool {
            if window == root {
                return false;
            }
            self.select_events_preserving(
                window,
                xproto::EventMask::PROPERTY_CHANGE | xproto::EventMask::STRUCTURE_NOTIFY,
            )
        }

        fn select_xsettings_owner_events(
            &self,
            root: Option<xproto::Window>,
            window: xproto::Window,
        ) -> bool {
            let mut events = xproto::EventMask::PROPERTY_CHANGE;
            if root != Some(window) {
                events |= xproto::EventMask::STRUCTURE_NOTIFY;
            }
            self.select_events_preserving(window, events)
        }

        fn refresh_xsettings_subscription(&self, cache: &mut WorkAreaCache) -> bool {
            let Some(selection) = self.xsettings_screen() else {
                let inserted = cache.authority_windows.set_xsettings_owner(None);
                debug_assert!(inserted, "clearing an authority cannot exceed capacity");
                return true;
            };
            let owner = match self.xcb_connection().get_selection_owner(selection) {
                Ok(cookie) => match cookie.reply() {
                    Ok(reply) => reply.owner,
                    Err(_) => return false,
                },
                Err(_) => return false,
            };
            if owner == 0 {
                let inserted = cache.authority_windows.set_xsettings_owner(None);
                debug_assert!(inserted, "clearing an authority cannot exceed capacity");
                return true;
            }
            if cache.authority_windows.xsettings_owner == Some(owner) {
                return true;
            }
            if !cache.authority_windows.can_subscribe(owner)
                || !self.select_xsettings_owner_events(cache.root, owner)
            {
                return false;
            }
            let inserted = cache.authority_windows.set_xsettings_owner(Some(owner));
            debug_assert!(inserted, "capacity was checked before subscribing");
            true
        }

        fn capture_exact_work_area(
            &self,
            root: xproto::Window,
            supporting_wm: xproto::Window,
            xsettings_owner: Option<xproto::Window>,
        ) -> Option<Vec<X11WorkAreaRecord>> {
            let root_geometry_before = self.root_geometry(root)?;
            let ewmh_before = self.collect_ewmh(root, supporting_wm)?;
            let scale_before = self.collect_scale_authority(root, xsettings_owner)?;
            let resources_before = self.screen_resources(root)?;
            let resources_stamp = ResourcesStamp::from_reply(&resources_before)?;
            let randr_monitors =
                self.collect_active_crtcs(&resources_before, scale_before.authority)?;
            if !global_work_area_is_exact_for_monitors(&randr_monitors, root_geometry_before) {
                return None;
            }

            let resources_after = self.screen_resources(root)?;
            let scale_after = self.collect_scale_authority(root, xsettings_owner)?;
            if resources_stamp != ResourcesStamp::from_reply(&resources_after)?
                || root_geometry_before != self.root_geometry(root)?
                || ewmh_before != self.collect_ewmh(root, supporting_wm)?
                || scale_before != scale_after
                || randr_monitors
                    != self.collect_active_crtcs(&resources_after, scale_after.authority)?
            {
                return None;
            }

            let atoms = self.atoms();
            let desktop_work_area = validate_ewmh(
                &ewmh_before,
                EwmhAtoms {
                    supporting_wm_check: atoms[_NET_SUPPORTING_WM_CHECK],
                    wm_name: atoms[_NET_WM_NAME],
                    current_desktop: atoms[_NET_CURRENT_DESKTOP],
                    number_of_desktops: atoms[_NET_NUMBER_OF_DESKTOPS],
                    desktop_geometry: atoms[_NET_DESKTOP_GEOMETRY],
                    desktop_viewport: atoms[_NET_DESKTOP_VIEWPORT],
                    workarea: atoms[_NET_WORKAREA],
                },
                root_geometry_before,
            )?;

            validate_roster(&randr_monitors, root_geometry_before, desktop_work_area).map(
                |records| {
                    records
                        .into_iter()
                        .map(|record| {
                            X11WorkAreaRecord::new(
                                record.crtc_id,
                                PhysicalPosition::new(record.monitor.x, record.monitor.y),
                                PhysicalSize::new(record.monitor.width, record.monitor.height),
                                PhysicalPosition::new(record.work_area.x, record.work_area.y),
                                PhysicalSize::new(record.work_area.width, record.work_area.height),
                                record.scale_factor,
                            )
                        })
                        .collect()
                },
            )
        }

        fn root_geometry(&self, root: xproto::Window) -> Option<RootGeometry> {
            let geometry = self.xcb_connection().get_geometry(root).ok()?.reply().ok()?;
            if geometry.root != root || geometry.x != 0 || geometry.y != 0 {
                return None;
            }
            Some(RootGeometry {
                window: root,
                width: geometry.width.into(),
                height: geometry.height.into(),
            })
        }

        fn collect_ewmh(
            &self,
            root: xproto::Window,
            supporting_wm: xproto::Window,
        ) -> Option<EwmhSnapshot> {
            let atoms = self.atoms();
            let root_supporting_wm = self.bounded_property32(
                root,
                atoms[_NET_SUPPORTING_WM_CHECK],
                xproto::AtomEnum::WINDOW.into(),
                1,
            )?;
            let wm_supporting_wm = self.bounded_property32(
                supporting_wm,
                atoms[_NET_SUPPORTING_WM_CHECK],
                xproto::AtomEnum::WINDOW.into(),
                1,
            )?;
            let wm_tree = self.xcb_connection().query_tree(supporting_wm).ok()?.reply().ok()?;
            if wm_tree.children.len() > MAX_SUPPORTING_WM_CHILDREN {
                return None;
            }
            Some(EwmhSnapshot {
                supported: self.bounded_property32(
                    root,
                    atoms[_NET_SUPPORTED],
                    xproto::AtomEnum::ATOM.into(),
                    MAX_SUPPORTED_ATOMS,
                )?,
                queried_supporting_wm: supporting_wm,
                root_supporting_wm: *root_supporting_wm.first()?,
                wm_supporting_wm: *wm_supporting_wm.first()?,
                wm_tree_root: wm_tree.root,
                wm_parent: wm_tree.parent,
                wm_name: self.bounded_property8(
                    supporting_wm,
                    atoms[_NET_WM_NAME],
                    atoms.UTF8_STRING,
                    MAX_WM_NAME_BYTES,
                )?,
                current_desktop: self.bounded_property32(
                    root,
                    atoms[_NET_CURRENT_DESKTOP],
                    xproto::AtomEnum::CARDINAL.into(),
                    1,
                )?,
                number_of_desktops: self.bounded_property32(
                    root,
                    atoms[_NET_NUMBER_OF_DESKTOPS],
                    xproto::AtomEnum::CARDINAL.into(),
                    1,
                )?,
                desktop_geometry: self.bounded_property32(
                    root,
                    atoms[_NET_DESKTOP_GEOMETRY],
                    xproto::AtomEnum::CARDINAL.into(),
                    2,
                )?,
                desktop_viewport: self.bounded_property32(
                    root,
                    atoms[_NET_DESKTOP_VIEWPORT],
                    xproto::AtomEnum::CARDINAL.into(),
                    MAX_DESKTOPS * 2,
                )?,
                workarea: self.bounded_property32(
                    root,
                    atoms[_NET_WORKAREA],
                    xproto::AtomEnum::CARDINAL.into(),
                    MAX_DESKTOPS * 4,
                )?,
            })
        }

        fn bounded_property32(
            &self,
            window: xproto::Window,
            property: xproto::Atom,
            property_type: xproto::Atom,
            max_items: usize,
        ) -> Option<Vec<u32>> {
            let max_items = u32::try_from(max_items).ok()?;
            let reply = self
                .xcb_connection()
                .get_property(false, window, property, property_type, 0, max_items)
                .ok()?
                .reply()
                .ok()?;
            if reply.type_ != property_type
                || reply.format != 32
                || !property_reply_length_is_exact(reply.format, reply.value_len, reply.length)
                || reply.bytes_after != 0
                || reply.value_len > max_items
                || reply.value.len() != usize::try_from(reply.value_len).ok()?.checked_mul(4)?
            {
                return None;
            }
            let values = reply.value32()?.collect();
            Some(values)
        }

        fn bounded_property8(
            &self,
            window: xproto::Window,
            property: xproto::Atom,
            property_type: xproto::Atom,
            max_bytes: usize,
        ) -> Option<Vec<u8>> {
            let long_length = u32::try_from(max_bytes.checked_add(3)?.checked_div(4)?).ok()?;
            let reply = self
                .xcb_connection()
                .get_property(false, window, property, property_type, 0, long_length)
                .ok()?
                .reply()
                .ok()?;
            if reply.type_ != property_type
                || reply.format != 8
                || !property_reply_length_is_exact(reply.format, reply.value_len, reply.length)
                || reply.bytes_after != 0
                || usize::try_from(reply.value_len).ok()? > max_bytes
                || reply.value.len() != usize::try_from(reply.value_len).ok()?
            {
                return None;
            }
            Some(reply.value)
        }

        fn bounded_optional_property8(
            &self,
            window: xproto::Window,
            property: xproto::Atom,
            property_type: xproto::Atom,
            max_bytes: usize,
        ) -> Option<xproto::GetPropertyReply> {
            let long_length = u32::try_from(max_bytes.checked_add(3)?.checked_div(4)?).ok()?;
            let reply = self
                .xcb_connection()
                .get_property(false, window, property, property_type, 0, long_length)
                .ok()?
                .reply()
                .ok()?;
            if reply.type_ == xproto::AtomEnum::NONE.into()
                && reply.format == 0
                && property_reply_length_is_exact(reply.format, reply.value_len, reply.length)
                && reply.bytes_after == 0
                && reply.value_len == 0
                && reply.value.is_empty()
            {
                return Some(reply);
            }
            if reply.type_ != property_type
                || reply.format != 8
                || !property_reply_length_is_exact(reply.format, reply.value_len, reply.length)
                || reply.bytes_after != 0
                || usize::try_from(reply.value_len).ok()? > max_bytes
                || reply.value.len() != usize::try_from(reply.value_len).ok()?
            {
                return None;
            }
            Some(reply)
        }

        fn collect_scale_authority(
            &self,
            root: xproto::Window,
            expected_xsettings_owner: Option<xproto::Window>,
        ) -> Option<ScaleAuthorityStamp> {
            let (xsettings_owner, xsettings_data, xsettings_dpi) = match self.xsettings_screen() {
                Some(selection) => {
                    let owner = self
                        .xcb_connection()
                        .get_selection_owner(selection)
                        .ok()?
                        .reply()
                        .ok()?
                        .owner;
                    if owner == 0 {
                        if expected_xsettings_owner.is_some() {
                            return None;
                        }
                        (None, None, None)
                    } else {
                        if expected_xsettings_owner != Some(owner) {
                            return None;
                        }
                        let data = self.bounded_property8(
                            owner,
                            self.atoms()[_XSETTINGS_SETTINGS],
                            self.atoms()[_XSETTINGS_SETTINGS],
                            MAX_XSETTINGS_BYTES,
                        )?;
                        let dpi = parse_xsettings_dpi_exact(&data).ok()?;
                        (Some(owner), Some(data), dpi)
                    }
                },
                None => {
                    if expected_xsettings_owner.is_some() {
                        return None;
                    }
                    (None, None, None)
                },
            };

            let (resource_manager_data, database_xft_dpi) = if xsettings_dpi.is_some() {
                (None, None)
            } else {
                let resource_manager_reply = self.bounded_optional_property8(
                    root,
                    xproto::AtomEnum::RESOURCE_MANAGER.into(),
                    xproto::AtomEnum::STRING.into(),
                    MAX_RESOURCE_MANAGER_BYTES,
                )?;
                let database_xft_dpi =
                    self.database().get_string("Xft.dpi", "").map(ToOwned::to_owned);
                let rebuilt_database = x11rb::resource_manager::Database::new_from_default(
                    &resource_manager_reply,
                    gethostname::gethostname(),
                );
                let rebuilt_xft_dpi =
                    rebuilt_database.get_string("Xft.dpi", "").map(ToOwned::to_owned);
                if database_xft_dpi != rebuilt_xft_dpi {
                    return None;
                }
                let resource_manager_data = (resource_manager_reply.type_
                    != xproto::AtomEnum::NONE.into())
                .then_some(resource_manager_reply.value);
                (resource_manager_data, database_xft_dpi)
            };
            let xft_dpi = xsettings_dpi
                .or_else(|| database_xft_dpi.as_deref().and_then(|dpi| f64::from_str(dpi).ok()));
            let authority = resolve_scale_authority(xft_dpi).ok()?;
            if matches!(authority, ScaleAuthority::Fixed(scale) if !validate_scale_factor(scale)) {
                return None;
            }

            Some(ScaleAuthorityStamp {
                authority,
                xsettings_owner,
                xsettings_data,
                resource_manager_data,
                database_xft_dpi,
            })
        }

        fn screen_resources(
            &self,
            root: xproto::Window,
        ) -> Option<randr::GetScreenResourcesCurrentReply> {
            let (major, minor) = self.randr_version();
            if major < 1 || (major == 1 && minor < 3) {
                return None;
            }
            self.xcb_connection().randr_get_screen_resources_current(root).ok()?.reply().ok()
        }

        fn collect_active_crtcs(
            &self,
            resources: &randr::GetScreenResourcesCurrentReply,
            scale_authority: ScaleAuthority,
        ) -> Option<Vec<RandrMonitor>> {
            let stamp = ResourcesStamp::from_reply(resources)?;
            let mut cookies = Vec::with_capacity(resources.crtcs.len());
            for crtc in &resources.crtcs {
                cookies.push((
                    *crtc,
                    self.xcb_connection()
                        .randr_get_crtc_info(*crtc, resources.config_timestamp)
                        .ok()?,
                ));
            }

            let mut active = Vec::new();
            for (crtc_id, cookie) in cookies {
                let crtc = cookie.reply().ok()?;
                if crtc.status != randr::SetConfig::SUCCESS
                    || crtc.timestamp != resources.timestamp
                    || crtc.possible.len() > MAX_OUTPUTS
                {
                    return None;
                }
                let inactive = crtc.mode == 0
                    && crtc.width == 0
                    && crtc.height == 0
                    && crtc.outputs.is_empty();
                if inactive {
                    continue;
                }
                if crtc.mode == 0
                    || crtc.width == 0
                    || crtc.height == 0
                    || crtc.outputs.len() != 1
                    || !stamp.modes.contains(&crtc.mode)
                    || !stamp.outputs.contains(&crtc.outputs[0])
                    || !crtc.possible.contains(&crtc.outputs[0])
                {
                    return None;
                }
                let transform_cookie =
                    self.xcb_connection().randr_get_crtc_transform(crtc_id).ok()?;
                let output_cookie = self
                    .xcb_connection()
                    .randr_get_output_info(crtc.outputs[0], resources.config_timestamp)
                    .ok()?;
                let transform = transform_cookie.reply().ok()?;
                let current_transform = transform_matrix(transform.current_transform);
                if transform.pending_filter_name.len() > MAX_TRANSFORM_FILTER_NAME_BYTES
                    || transform.current_filter_name.len() > MAX_TRANSFORM_FILTER_NAME_BYTES
                    || transform.pending_params.len() > MAX_TRANSFORM_PARAMS
                    || transform.current_params.len() > MAX_TRANSFORM_PARAMS
                    || !crtc_transform_is_identity(current_transform)
                {
                    return None;
                }
                let output = output_cookie.reply().ok()?;
                let output_name = std::str::from_utf8(&output.name).ok()?;
                if output.status != randr::SetConfig::SUCCESS
                    || output.timestamp != resources.timestamp
                    || output.connection != randr::Connection::CONNECTED
                    || output.crtc != crtc_id
                    || output.crtcs.len() > MAX_CRTCS
                    || output.modes.len() > MAX_MODES
                    || output.clones.len() > MAX_OUTPUTS
                    || output.name.is_empty()
                    || output.name.len() > MAX_OUTPUT_NAME_BYTES
                    || output_name.contains('\0')
                    || output_name.trim().is_empty()
                    || !output.crtcs.contains(&crtc_id)
                    || !output.modes.contains(&crtc.mode)
                {
                    return None;
                }
                let scale_factor = scale_authority.exact_scale_factor(
                    (crtc.width.into(), crtc.height.into()),
                    (output.mm_width.into(), output.mm_height.into()),
                )?;
                active.push(RandrMonitor {
                    crtc_id,
                    rect: PhysicalRect {
                        x: crtc.x.into(),
                        y: crtc.y.into(),
                        width: crtc.width.into(),
                        height: crtc.height.into(),
                    },
                    outputs: crtc.outputs,
                    current_transform,
                    scale_factor,
                });
            }
            (!active.is_empty()).then_some(active)
        }
    }

    const fn transform_matrix(transform: render::Transform) -> [i32; 9] {
        [
            transform.matrix11,
            transform.matrix12,
            transform.matrix13,
            transform.matrix21,
            transform.matrix22,
            transform.matrix23,
            transform.matrix31,
            transform.matrix32,
            transform.matrix33,
        ]
    }

    impl WorkAreaCache {
        fn is_foreign_authority_window(&self, window: xproto::Window) -> bool {
            self.authority_windows.contains(window)
        }
    }
}

#[cfg(x11_platform)]
pub(crate) use native::WorkAreaCache;

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT: RootGeometry = RootGeometry { window: 1, width: 3840, height: 1080 };
    const ATOMS: EwmhAtoms = EwmhAtoms {
        supporting_wm_check: 1,
        wm_name: 2,
        current_desktop: 3,
        number_of_desktops: 4,
        desktop_geometry: 5,
        desktop_viewport: 6,
        workarea: 7,
    };

    fn ewmh() -> EwmhSnapshot {
        EwmhSnapshot {
            supported: ATOMS.required().to_vec(),
            queried_supporting_wm: 42,
            root_supporting_wm: 42,
            wm_supporting_wm: 42,
            wm_tree_root: ROOT.window,
            wm_parent: ROOT.window,
            wm_name: b"Test WM".to_vec(),
            current_desktop: vec![1],
            number_of_desktops: vec![2],
            desktop_geometry: vec![ROOT.width, ROOT.height],
            desktop_viewport: vec![0, 0, 0, 0],
            workarea: vec![0, 24, 3840, 1056, 0, 24, 3840, 1056],
        }
    }

    fn monitors() -> Vec<RandrMonitor> {
        vec![
            RandrMonitor {
                crtc_id: 10,
                rect: PhysicalRect { x: 0, y: 0, width: 1920, height: 1080 },
                outputs: vec![20],
                current_transform: IDENTITY_CRTC_TRANSFORM,
                scale_factor: 1.0,
            },
            RandrMonitor {
                crtc_id: 11,
                rect: PhysicalRect { x: 1920, y: 0, width: 1920, height: 1080 },
                outputs: vec![21],
                current_transform: IDENTITY_CRTC_TRANSFORM,
                scale_factor: 1.5,
            },
        ]
    }

    #[test]
    fn accepts_exact_current_desktop_and_clips_each_monitor() {
        let desktop = validate_ewmh(&ewmh(), ATOMS, ROOT).unwrap();
        let roster = validate_roster(&monitors(), ROOT, desktop).unwrap();

        assert_eq!(roster.len(), 2);
        assert_eq!(roster[0].work_area, PhysicalRect { x: 0, y: 24, width: 1920, height: 1056 });
        assert_eq!(roster[1].work_area, PhysicalRect { x: 1920, y: 24, width: 1920, height: 1056 });
    }

    #[test]
    fn rejects_unadvertised_or_unverified_wm_authority() {
        let mut snapshot = ewmh();
        snapshot.supported.retain(|atom| *atom != ATOMS.workarea);
        assert_eq!(validate_ewmh(&snapshot, ATOMS, ROOT), None);

        let mut snapshot = ewmh();
        snapshot.wm_supporting_wm += 1;
        assert_eq!(validate_ewmh(&snapshot, ATOMS, ROOT), None);

        let mut snapshot = ewmh();
        snapshot.queried_supporting_wm += 1;
        assert_eq!(validate_ewmh(&snapshot, ATOMS, ROOT), None);

        let mut snapshot = ewmh();
        snapshot.wm_name = vec![0xff];
        assert_eq!(validate_ewmh(&snapshot, ATOMS, ROOT), None);

        let mut snapshot = ewmh();
        snapshot.wm_name = vec![0];
        assert_eq!(validate_ewmh(&snapshot, ATOMS, ROOT), None);

        let mut snapshot = ewmh();
        snapshot.wm_parent = 99;
        assert_eq!(validate_ewmh(&snapshot, ATOMS, ROOT), None);

        let mut snapshot = ewmh();
        snapshot.wm_tree_root = 99;
        assert_eq!(validate_ewmh(&snapshot, ATOMS, ROOT), None);
    }

    #[test]
    fn rejects_inexact_desktop_arrays_and_nonzero_current_viewport() {
        let mut snapshot = ewmh();
        snapshot.desktop_viewport.pop();
        assert_eq!(validate_ewmh(&snapshot, ATOMS, ROOT), None);

        let mut snapshot = ewmh();
        snapshot.desktop_viewport[2] = 1;
        assert_eq!(validate_ewmh(&snapshot, ATOMS, ROOT), None);

        let mut snapshot = ewmh();
        snapshot.workarea.extend([0, 0, 1, 1]);
        assert_eq!(validate_ewmh(&snapshot, ATOMS, ROOT), None);
    }

    #[test]
    fn rejects_geometry_and_work_area_outside_root() {
        let mut snapshot = ewmh();
        snapshot.desktop_geometry[0] -= 1;
        assert_eq!(validate_ewmh(&snapshot, ATOMS, ROOT), None);

        let mut snapshot = ewmh();
        snapshot.workarea[7] = ROOT.height + 1;
        assert_eq!(validate_ewmh(&snapshot, ATOMS, ROOT), None);
    }

    #[test]
    fn rejects_empty_cloned_overlapping_or_invalid_scale_rosters() {
        let desktop = validate_ewmh(&ewmh(), ATOMS, ROOT).unwrap();
        assert_eq!(validate_roster(&[], ROOT, desktop), None);

        let mut roster = monitors();
        roster[0].outputs.push(22);
        assert_eq!(validate_roster(&roster, ROOT, desktop), None);

        let mut roster = monitors();
        roster[1].rect.x = 1919;
        assert_eq!(validate_roster(&roster, ROOT, desktop), None);

        let mut roster = monitors();
        roster[0].scale_factor = f64::NAN;
        assert_eq!(validate_roster(&roster, ROOT, desktop), None);
    }

    #[test]
    fn rejects_duplicate_crtc_or_output_identity() {
        let desktop = validate_ewmh(&ewmh(), ATOMS, ROOT).unwrap();
        let mut roster = monitors();
        roster[1].crtc_id = roster[0].crtc_id;
        assert_eq!(validate_roster(&roster, ROOT, desktop), None);

        let mut roster = monitors();
        roster[1].outputs[0] = roster[0].outputs[0];
        assert_eq!(validate_roster(&roster, ROOT, desktop), None);
    }

    #[test]
    fn event_mask_merge_preserves_every_existing_bit() {
        let existing = 0b1010_0101;
        let required = 0b0011_0000;

        assert_eq!(merge_event_masks(existing, required), 0b1011_0101);
        assert_eq!(merge_event_masks(existing, 0), existing);
    }

    #[test]
    fn property_reply_length_must_match_format_and_value_count() {
        assert!(property_reply_length_is_exact(0, 0, 0));
        assert!(property_reply_length_is_exact(8, 1, 1));
        assert!(property_reply_length_is_exact(8, 4, 1));
        assert!(property_reply_length_is_exact(8, 5, 2));
        assert!(property_reply_length_is_exact(32, 3, 3));

        assert!(!property_reply_length_is_exact(0, 0, 1));
        assert!(!property_reply_length_is_exact(0, 1, 0));
        assert!(!property_reply_length_is_exact(8, 4, 2));
        assert!(!property_reply_length_is_exact(32, 3, 4));
        assert!(!property_reply_length_is_exact(7, 1, 1));
    }

    #[test]
    fn non_identity_crtc_transform_cannot_publish_scalar_scale_authority() {
        assert!(crtc_transform_is_identity(IDENTITY_CRTC_TRANSFORM));

        let desktop = validate_ewmh(&ewmh(), ATOMS, ROOT).unwrap();
        let mut roster = monitors();
        roster[0].current_transform[0] = 2 << 16;
        assert!(!crtc_transform_is_identity(roster[0].current_transform));
        assert_eq!(validate_roster(&roster, ROOT, desktop), None);
    }

    #[test]
    fn global_ewmh_work_area_is_not_published_as_per_monitor_authority() {
        let full_root = RandrMonitor {
            crtc_id: 10,
            rect: PhysicalRect { x: 0, y: 0, width: ROOT.width, height: ROOT.height },
            outputs: vec![20],
            current_transform: IDENTITY_CRTC_TRANSFORM,
            scale_factor: 1.0,
        };
        assert!(global_work_area_is_exact_for_monitors(std::slice::from_ref(&full_root), ROOT));
        assert!(!global_work_area_is_exact_for_monitors(&[], ROOT));
        assert!(!global_work_area_is_exact_for_monitors(&monitors(), ROOT));

        let mut partial_root = full_root;
        partial_root.rect.width -= 1;
        assert!(!global_work_area_is_exact_for_monitors(&[partial_root], ROOT));
    }

    #[test]
    fn exact_randr_scale_rejects_winit_compatibility_clamps() {
        assert!(exact_randr_scale_factor((1920, 1080), (509, 286)).is_some());
        assert_eq!(exact_randr_scale_factor((1920, 1080), (0, 286)), None);
        assert_eq!(exact_randr_scale_factor((1920, 1080), (1, 1)), None);
        assert_eq!(exact_randr_scale_factor((100, 100), (10_000, 10_000)), None);
    }

    #[test]
    fn subscribed_foreign_authorities_survive_owner_switches_until_destroyed() {
        let mut windows = AuthorityWindows::default();
        assert!(windows.set_supporting_wm(10));
        assert!(windows.set_supporting_wm(11));
        assert!(windows.set_supporting_wm(10));
        assert!(windows.set_xsettings_owner(Some(20)));
        assert!(windows.set_xsettings_owner(Some(21)));

        assert!(windows.contains(10));
        assert!(windows.contains(11));
        assert!(windows.contains(20));
        assert!(windows.contains(21));
        assert_eq!(windows.destroyed(10), AuthorityWindowDestroyed::Subscribed {
            current_changed: true
        });
        assert_eq!(windows.supporting_wm, None);
        assert!(windows.contains(11));
        assert_eq!(windows.destroyed(20), AuthorityWindowDestroyed::Subscribed {
            current_changed: false
        });
        assert_eq!(windows.xsettings_owner, Some(21));
        assert_eq!(windows.destroyed(11), AuthorityWindowDestroyed::Subscribed {
            current_changed: false
        });
        assert_eq!(windows.supporting_wm, None);
        assert_eq!(windows.destroyed(10), AuthorityWindowDestroyed::NotSubscribed);
    }

    #[test]
    fn authority_subscription_roster_is_bounded_and_recovers_after_destroy() {
        let mut windows = AuthorityWindows::default();
        for window in 1..=u32::try_from(MAX_SUBSCRIBED_AUTHORITY_WINDOWS).unwrap() {
            assert!(windows.set_supporting_wm(window));
        }

        let overflow = u32::try_from(MAX_SUBSCRIBED_AUTHORITY_WINDOWS + 1).unwrap();
        assert!(!windows.set_supporting_wm(overflow));
        assert!(!windows.contains(overflow));

        assert_eq!(windows.destroyed(1), AuthorityWindowDestroyed::Subscribed {
            current_changed: false
        });
        assert!(windows.set_supporting_wm(overflow));
        assert!(windows.contains(overflow));
    }
}
