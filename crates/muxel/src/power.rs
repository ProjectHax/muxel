//! Host power state — whether anyone can be at this machine right now.

/// Starts listening for the lid. Windows reports it only through change
/// notifications, so the listener must be up before anything asks; call once at
/// startup.
#[cfg(target_os = "windows")]
pub fn watch() {
    win::watch();
}

/// Whether the lid is shut on a laptop nobody is using: shut, with no external
/// display in use, the machine is running at all only because of a background or
/// spurious wake (Power Nap, a maintenance or wake-timer wake, a bump in a bag).
/// A lid shut in clamshell mode — working on an external display with the lid
/// down — doesn't count.
///
/// False wherever the lid can't be read: desktops, a Linux without logind, and
/// platforms other than macOS, Linux and Windows.
pub fn lid_shut_unattended() -> bool {
    #[cfg(target_os = "macos")]
    let (closed, in_use) = {
        // A Mac only stops sleeping on lid close in clamshell mode.
        let (closed, causes_sleep) = macos::clamshell();
        (closed, causes_sleep.map(|sleeps| !sleeps))
    };
    #[cfg(target_os = "linux")]
    let (closed, in_use) = linux::logind();
    #[cfg(target_os = "windows")]
    let (closed, in_use) = win::state();
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    let (closed, in_use) = (None, None);
    lid_holds(closed, in_use)
}

/// `closed` is whether the lid is shut, and `in_use` whether the machine is still
/// being worked on regardless (an external display); each `None` when unknown. No
/// lid reading means no lid, so nothing holds; an unknown `in_use` is taken as the
/// common case — shutting the lid means walking away.
fn lid_holds(closed: Option<bool>, in_use: Option<bool>) -> bool {
    closed == Some(true) && in_use != Some(true)
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

#[cfg(target_os = "linux")]
mod linux {
    use std::sync::{Mutex, PoisonError};
    use std::time::Duration;
    use zbus::blocking::{Connection, connection};
    use zbus::zvariant::OwnedValue;

    /// A healthy logind answers in well under a millisecond; this only bounds how
    /// long a wedged one can stall the UI tick that asks.
    const TIMEOUT: Duration = Duration::from_millis(250);

    /// The system bus: opened on first use, reopened after a failed read.
    static BUS: Mutex<Option<Connection>> = Mutex::new(None);

    /// `(LidClosed, Docked)` from logind — or elogind, which serves the same API.
    /// logind counts a connected external display as docked, which is exactly the
    /// lid-shut-but-in-use case.
    pub fn logind() -> (Option<bool>, Option<bool>) {
        let mut bus = BUS.lock().unwrap_or_else(PoisonError::into_inner);
        if bus.is_none() {
            *bus = connection::Builder::system()
                .and_then(|builder| builder.method_timeout(TIMEOUT).build())
                .ok();
        }
        let Some(conn) = bus.as_ref() else {
            return (None, None);
        };
        let Ok(closed) = property(conn, "LidClosed") else {
            *bus = None;
            return (None, None);
        };
        (Some(closed), property(conn, "Docked").ok())
    }

    /// Reads the boolean property `name` of logind's manager object. Asked afresh
    /// every time, rather than through a caching proxy, since logind needn't
    /// announce a lid change.
    fn property(conn: &Connection, name: &str) -> zbus::Result<bool> {
        let reply = conn.call_method(
            Some("org.freedesktop.login1"),
            "/org/freedesktop/login1",
            Some("org.freedesktop.DBus.Properties"),
            "Get",
            &("org.freedesktop.login1.Manager", name),
        )?;
        let value: OwnedValue = reply.body().deserialize()?;
        Ok(bool::try_from(value)?)
    }
}

#[cfg(target_os = "windows")]
mod win {
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicU8, Ordering};
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Power::{
        DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS, POWERBROADCAST_SETTING,
        PowerSettingRegisterNotification,
    };
    use windows::Win32::UI::WindowsAndMessaging::{DEVICE_NOTIFY_CALLBACK, PBT_POWERSETTINGCHANGE};
    use windows::core::GUID;

    /// `GUID_LIDSWITCH_STATE_CHANGE`: a DWORD, 0 closed / 1 open.
    const LID_SWITCH: GUID = GUID::from_u128(0xba3e0f4d_b817_4094_a2d1_d56379e6a0f3);
    /// `GUID_CONSOLE_DISPLAY_STATE`: a DWORD, 0 off / 1 on / 2 dimmed.
    const CONSOLE_DISPLAY: GUID = GUID::from_u128(0x6fe69556_704a_47a0_8f24_c28d936fda47);

    /// Each is 0 until its first notification, then 1 for false and 2 for true.
    static LID_CLOSED: AtomicU8 = AtomicU8::new(0);
    static DISPLAY_ON: AtomicU8 = AtomicU8::new(0);

    /// Registers for both notifications. Each also arrives once with the current
    /// value — the lid's as soon as a lid device is found — so the state is known
    /// shortly after startup, and stays unknown (nothing held) on a desktop.
    pub fn watch() {
        let params = Box::leak(Box::new(DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS {
            Callback: Some(on_setting),
            Context: std::ptr::null_mut(),
        }));
        let recipient = HANDLE(std::ptr::from_mut(params) as isize);
        for guid in [LID_SWITCH, CONSOLE_DISPLAY] {
            let mut registration = std::ptr::null_mut();
            // SAFETY: with DEVICE_NOTIFY_CALLBACK the recipient points to the
            // subscribe parameters, leaked above so they outlive the registration,
            // which is never undone — it lasts as long as the process. A failed
            // registration just leaves its state unknown.
            let _ = unsafe {
                PowerSettingRegisterNotification(
                    &guid,
                    DEVICE_NOTIFY_CALLBACK,
                    recipient,
                    &mut registration,
                )
            };
        }
    }

    /// `(lid closed, display on)`. With the lid shut the built-in panel is off, so
    /// a lit console display is an external one in use.
    pub fn state() -> (Option<bool>, Option<bool>) {
        (load(&LID_CLOSED), load(&DISPLAY_ON))
    }

    fn load(flag: &AtomicU8) -> Option<bool> {
        match flag.load(Ordering::Relaxed) {
            0 => None,
            value => Some(value == 2),
        }
    }

    fn store(flag: &AtomicU8, value: bool) {
        flag.store(if value { 2 } else { 1 }, Ordering::Relaxed);
    }

    /// Called on a system thread for every notification.
    unsafe extern "system" fn on_setting(
        _context: *const c_void,
        kind: u32,
        setting: *const c_void,
    ) -> u32 {
        if kind != PBT_POWERSETTINGCHANGE || setting.is_null() {
            return 0;
        }
        let setting = setting.cast::<POWERBROADCAST_SETTING>();
        // SAFETY: a PBT_POWERSETTINGCHANGE notification points at a
        // POWERBROADCAST_SETTING whose trailing `Data` holds `DataLength` bytes;
        // both settings watched here are a DWORD, read only once it's all there.
        let (guid, value) = unsafe {
            if (*setting).DataLength < 4 {
                return 0;
            }
            let data = setting
                .cast::<u8>()
                .add(std::mem::offset_of!(POWERBROADCAST_SETTING, Data));
            ((*setting).PowerSetting, data.cast::<u32>().read_unaligned())
        };
        if guid == LID_SWITCH {
            store(&LID_CLOSED, value == 0);
        } else if guid == CONSOLE_DISPLAY {
            store(&DISPLAY_ON, value != 0);
        }
        0
    }
}

#[cfg(test)]
mod tests {
    use super::lid_holds;

    #[test]
    fn a_shut_lid_with_nothing_else_in_use_holds() {
        assert!(lid_holds(Some(true), Some(false)));
        // Unreadable: assume the common case, shutting the lid means walking away.
        assert!(lid_holds(Some(true), None));
    }

    #[test]
    fn clamshell_mode_with_an_external_display_does_not_hold() {
        assert!(!lid_holds(Some(true), Some(true)));
    }

    #[test]
    fn an_open_lid_or_no_lid_never_holds() {
        assert!(!lid_holds(Some(false), Some(false)));
        assert!(!lid_holds(None, None));
        assert!(!lid_holds(None, Some(false)));
    }

    /// Exercises the real IOKit read: a laptop publishes both keys, a desktop neither.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_clamshell_keys_are_read_together() {
        let (closed, causes_sleep) = super::macos::clamshell();
        assert_eq!(closed.is_some(), causes_sleep.is_some());
    }
}
