#![cfg(not(x11_platform))]

// Keep exact X11 work-area proof rules executable on non-X11 hosts. Production uses this source
// module directly; the wrapper supplies no native facts and performs no emulation.
#[path = "../src/platform_impl/linux/x11/work_area.rs"]
mod work_area;
