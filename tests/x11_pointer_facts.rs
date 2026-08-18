// Keep the pure XI2 routing state executable on non-X11 hosts. The production X11 backend uses
// this exact source module; this wrapper does not emulate or bypass native event delivery.
#[path = "../src/platform_impl/linux/x11/pointer_facts.rs"]
mod pointer_facts;
