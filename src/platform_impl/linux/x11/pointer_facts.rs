//! Pure XI2 pointer-route state. Keeping Xlib/XCB calls outside this module lets the same state
//! transitions run as ordinary unit tests on non-X11 development hosts.

use std::collections::{HashMap, HashSet};
use std::os::raw::c_int;

/// Bound hierarchy probing so a malformed or concurrently changing X11 window tree fails closed.
const MAX_POINTER_HIERARCHY_DEPTH: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum NativePointerRoute {
    Unknown,
    None,
    Window(u32),
    Foreign,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImplicitCaptureOwner {
    Window(u32),
    Unknown,
}

impl ImplicitCaptureOwner {
    fn route(self) -> NativePointerRoute {
        match self {
            Self::Window(window) => NativePointerRoute::Window(window),
            Self::Unknown => NativePointerRoute::Unknown,
        }
    }
}

#[derive(Debug, Default)]
pub(super) struct ImplicitPointerCaptures {
    owners: HashMap<u16, ImplicitCaptureOwner>,
}

impl ImplicitPointerCaptures {
    pub(super) fn after_button_event(
        &mut self,
        device_id: u16,
        delivery_window: u32,
        delivery_is_winit: bool,
        pressed: bool,
        transition: Option<ButtonStateTransition>,
    ) -> NativePointerRoute {
        let Some(transition) = transition else {
            self.owners.insert(device_id, ImplicitCaptureOwner::Unknown);
            return NativePointerRoute::Unknown;
        };

        if !transition.any_after {
            self.owners.remove(&device_id);
            return NativePointerRoute::None;
        }

        if pressed && !transition.any_before {
            let owner = if delivery_is_winit {
                ImplicitCaptureOwner::Window(delivery_window)
            } else {
                ImplicitCaptureOwner::Unknown
            };
            self.owners.insert(device_id, owner);
            return owner.route();
        }

        self.owners.entry(device_id).or_insert(ImplicitCaptureOwner::Unknown).route()
    }

    pub(super) fn for_button_state(
        &mut self,
        device_id: u16,
        any_pressed: Option<bool>,
    ) -> NativePointerRoute {
        match any_pressed {
            Some(false) => {
                self.owners.remove(&device_id);
                NativePointerRoute::None
            },
            Some(true) | None => {
                self.owners.entry(device_id).or_insert(ImplicitCaptureOwner::Unknown).route()
            },
        }
    }

    pub(super) fn reset_device(&mut self, device_id: u16) -> Option<u32> {
        match self.owners.remove(&device_id) {
            Some(ImplicitCaptureOwner::Window(window)) => Some(window),
            Some(ImplicitCaptureOwner::Unknown) | None => None,
        }
    }

    pub(super) fn reset_window(&mut self, window: u32) -> Vec<u16> {
        let devices = self
            .owners
            .iter()
            .filter_map(|(device_id, owner)| {
                (*owner == ImplicitCaptureOwner::Window(window)).then_some(*device_id)
            })
            .collect::<Vec<_>>();
        for device_id in &devices {
            self.owners.remove(device_id);
        }
        devices
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ButtonStateTransition {
    pub(super) any_before: bool,
    pub(super) any_after: bool,
}

pub(super) fn button_state_transition(
    mask: &[u8],
    detail: c_int,
    pressed: bool,
) -> Option<ButtonStateTransition> {
    let detail = usize::try_from(detail).ok().filter(|detail| *detail > 0)?;
    let button_count = mask.len().checked_mul(8)?;
    if detail >= button_count {
        return None;
    }
    let mut any_before = false;
    let mut any_after = false;

    for button in 0..button_count {
        let byte = mask[button / 8];
        let pressed_before = byte & (1_u8 << (button % 8)) != 0;
        let pressed_after = if button == detail { pressed } else { pressed_before };
        any_before |= pressed_before;
        any_after |= pressed_after;
    }

    Some(ButtonStateTransition { any_before, any_after })
}

pub(super) fn classify_pointer_hierarchy(
    first_child: u32,
    mut is_winit_window: impl FnMut(u32) -> bool,
    mut child_at_pointer: impl FnMut(u32) -> Result<u32, ()>,
) -> NativePointerRoute {
    if first_child == 0 {
        return NativePointerRoute::None;
    }

    let mut visited = HashSet::new();
    let mut window = first_child;
    for _ in 0..MAX_POINTER_HIERARCHY_DEPTH {
        if !visited.insert(window) {
            return NativePointerRoute::Unknown;
        }
        if is_winit_window(window) {
            return NativePointerRoute::Window(window);
        }

        match child_at_pointer(window) {
            Ok(child) if child != 0 => window = child,
            Ok(_) => return NativePointerRoute::Foreign,
            Err(()) => return NativePointerRoute::Unknown,
        }
    }

    NativePointerRoute::Unknown
}

pub(super) fn fp1616_matches_event(value: i32, event_value: f64) -> bool {
    event_value.is_finite() && f64::from(value) == event_value * 65_536.0
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEVICE: u16 = 2;
    const OWNER: u32 = 100;
    const OTHER_WINDOW: u32 = 200;

    fn transition(any_before: bool, any_after: bool) -> Option<ButtonStateTransition> {
        Some(ButtonStateTransition { any_before, any_after })
    }

    #[test]
    fn button_transition_uses_post_event_state() {
        assert_eq!(button_state_transition(&[0], 1, true), transition(false, true));
        assert_eq!(button_state_transition(&[0b0000_0010], 1, false), transition(true, false));
        assert_eq!(button_state_transition(&[0b0000_1010], 1, false), transition(true, true));
        assert_eq!(button_state_transition(&[], 1, true), None);
        assert_eq!(button_state_transition(&[0], 0, true), None);
    }

    #[test]
    fn implicit_capture_stays_with_first_delivery_until_last_release() {
        let mut captures = ImplicitPointerCaptures::default();

        assert_eq!(
            captures.after_button_event(DEVICE, OWNER, true, true, transition(false, true)),
            NativePointerRoute::Window(OWNER)
        );
        assert_eq!(
            captures.for_button_state(DEVICE, Some(true)),
            NativePointerRoute::Window(OWNER)
        );
        assert_eq!(
            captures.after_button_event(DEVICE, OWNER, true, true, transition(true, true)),
            NativePointerRoute::Window(OWNER)
        );
        assert_eq!(
            captures.after_button_event(DEVICE, OWNER, true, false, transition(true, true)),
            NativePointerRoute::Window(OWNER)
        );
        assert_eq!(
            captures.after_button_event(DEVICE, OWNER, true, false, transition(true, false)),
            NativePointerRoute::None
        );
    }

    #[test]
    fn capture_without_observed_press_fails_closed_until_buttons_are_up() {
        let mut captures = ImplicitPointerCaptures::default();

        assert_eq!(captures.for_button_state(DEVICE, Some(true)), NativePointerRoute::Unknown);
        assert_eq!(captures.for_button_state(DEVICE, None), NativePointerRoute::Unknown);
        assert_eq!(captures.for_button_state(DEVICE, Some(false)), NativePointerRoute::None);
    }

    #[test]
    fn resetting_window_capture_is_scoped_and_idempotent() {
        let mut captures = ImplicitPointerCaptures::default();
        let other_device = DEVICE + 1;
        captures.owners.insert(DEVICE, ImplicitCaptureOwner::Window(OWNER));
        captures.owners.insert(other_device, ImplicitCaptureOwner::Window(OTHER_WINDOW));

        assert_eq!(captures.reset_window(OWNER), vec![DEVICE]);
        assert!(captures.reset_window(OWNER).is_empty());
        assert_eq!(
            captures.for_button_state(other_device, Some(true)),
            NativePointerRoute::Window(OTHER_WINDOW)
        );
        assert_eq!(captures.reset_device(other_device), Some(OTHER_WINDOW));
        assert_eq!(captures.reset_device(other_device), None);
    }

    #[test]
    fn hierarchy_distinguishes_desktop_foreign_and_winit_windows() {
        assert_eq!(
            classify_pointer_hierarchy(0, |_| false, |_| unreachable!()),
            NativePointerRoute::None
        );
        assert_eq!(
            classify_pointer_hierarchy(OWNER, |window| window == OWNER, |_| unreachable!()),
            NativePointerRoute::Window(OWNER)
        );
        assert_eq!(
            classify_pointer_hierarchy(
                50,
                |window| window == OWNER,
                |window| Ok(if window == 50 { OWNER } else { 0 }),
            ),
            NativePointerRoute::Window(OWNER)
        );
        assert_eq!(
            classify_pointer_hierarchy(50, |_| false, |_| Ok(0)),
            NativePointerRoute::Foreign
        );
        assert_eq!(
            classify_pointer_hierarchy(50, |_| false, |_| Err(())),
            NativePointerRoute::Unknown
        );
        assert_eq!(classify_pointer_hierarchy(50, |_| false, Ok), NativePointerRoute::Unknown);
    }

    #[test]
    fn hover_route_does_not_change_capture_owner() {
        let mut captures = ImplicitPointerCaptures::default();
        captures.owners.insert(DEVICE, ImplicitCaptureOwner::Window(OWNER));

        let hover = classify_pointer_hierarchy(
            OTHER_WINDOW,
            |window| window == OWNER || window == OTHER_WINDOW,
            |_| unreachable!(),
        );
        assert_eq!(hover, NativePointerRoute::Window(OTHER_WINDOW));
        assert_eq!(
            captures.for_button_state(DEVICE, Some(true)),
            NativePointerRoute::Window(OWNER)
        );
    }

    #[test]
    fn pointer_query_coordinates_must_match_the_native_event() {
        assert!(fp1616_matches_event(12 * 65_536 + 32_768, 12.5));
        assert!(fp1616_matches_event(-(12 * 65_536 + 32_768), -12.5));
        assert!(!fp1616_matches_event(12 * 65_536, 12.5));
        assert!(!fp1616_matches_event(0, f64::NAN));
    }
}
