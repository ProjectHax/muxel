//! Host power state — whether anyone can be at this machine right now.

/// Whether the lid is shut on a laptop that sleeps when it's shut: nobody can be
/// using this Mac, so it's running at all only because of a background wake
/// (Power Nap, a network-maintenance wake). A lid shut in clamshell mode — an
/// external display keeping the Mac awake — doesn't count; the user is working on
/// it with the lid down.
///
/// False wherever the lid can't be read: desktops, and platforms other than macOS.
pub fn lid_closed_for_sleep() -> bool {
    #[cfg(target_os = "macos")]
    {
        let (closed, causes_sleep) = macos::clamshell();
        lid_holds(closed, causes_sleep)
    }
    #[cfg(not(target_os = "macos"))]
    false
}

/// `closed` is IOKit's `AppleClamshellState` and `causes_sleep` its
/// `AppleClamshellCausesSleep`, each `None` when absent (no lid, or the read
/// failed). An unknown sleep policy is taken as the default one — closing sleeps.
#[cfg(any(target_os = "macos", test))]
fn lid_holds(closed: Option<bool>, causes_sleep: Option<bool>) -> bool {
    closed == Some(true) && causes_sleep != Some(false)
}

#[cfg(target_os = "macos")]
mod macos {
    use core_foundation::base::{CFAllocatorRef, CFType, CFTypeRef, TCFType, kCFAllocatorDefault};
    use core_foundation::boolean::CFBoolean;
    use core_foundation::dictionary::CFMutableDictionaryRef;
    use core_foundation::string::{CFString, CFStringRef};
    use std::ffi::c_char;

    /// `io_object_t` — a mach port name.
    type IoObject = u32;
    /// `kIOMainPortDefault` (`MACH_PORT_NULL`).
    const MAIN_PORT_DEFAULT: u32 = 0;

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOServiceMatching(name: *const c_char) -> CFMutableDictionaryRef;
        fn IOServiceGetMatchingService(
            main_port: u32,
            matching: CFMutableDictionaryRef,
        ) -> IoObject;
        fn IORegistryEntryCreateCFProperty(
            entry: IoObject,
            key: CFStringRef,
            allocator: CFAllocatorRef,
            options: u32,
        ) -> CFTypeRef;
        fn IOObjectRelease(object: IoObject) -> i32;
    }

    /// `(AppleClamshellState, AppleClamshellCausesSleep)` from the power-management
    /// root domain — the same values `ioreg -r -k AppleClamshellState` prints.
    pub fn clamshell() -> (Option<bool>, Option<bool>) {
        // SAFETY: `IOServiceGetMatchingService` consumes the matching dictionary
        // (null is tolerated and yields no service); the service it returns is
        // released below. Each property comes back under the create rule and is
        // owned by the `CFType` that wraps it.
        unsafe {
            let matching = IOServiceMatching(c"IOPMrootDomain".as_ptr());
            let root = IOServiceGetMatchingService(MAIN_PORT_DEFAULT, matching);
            if root == 0 {
                return (None, None);
            }
            let read = |key: &'static str| {
                let key = CFString::from_static_string(key);
                let value = IORegistryEntryCreateCFProperty(
                    root,
                    key.as_concrete_TypeRef(),
                    kCFAllocatorDefault,
                    0,
                );
                if value.is_null() {
                    return None;
                }
                CFType::wrap_under_create_rule(value)
                    .downcast::<CFBoolean>()
                    .map(bool::from)
            };
            let state = (
                read("AppleClamshellState"),
                read("AppleClamshellCausesSleep"),
            );
            IOObjectRelease(root);
            state
        }
    }
}

#[cfg(test)]
mod tests {
    use super::lid_holds;

    #[test]
    fn a_shut_lid_that_sleeps_the_mac_holds() {
        assert!(lid_holds(Some(true), Some(true)));
        // Policy unreadable: assume the default, closing sleeps.
        assert!(lid_holds(Some(true), None));
    }

    #[test]
    fn clamshell_mode_with_an_external_display_does_not_hold() {
        assert!(!lid_holds(Some(true), Some(false)));
    }

    #[test]
    fn an_open_lid_or_no_lid_never_holds() {
        assert!(!lid_holds(Some(false), Some(true)));
        assert!(!lid_holds(None, None));
        assert!(!lid_holds(None, Some(true)));
    }

    /// Exercises the real IOKit read: a laptop publishes both keys, a desktop neither.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_clamshell_keys_are_read_together() {
        let (closed, causes_sleep) = super::macos::clamshell();
        assert_eq!(closed.is_some(), causes_sleep.is_some());
    }
}
