//! Battery / AC state.

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct Battery {
    pub percent: u8,
    pub on_battery: bool,
    pub charging: bool,
}

/// Interpret a Win32 `SYSTEM_POWER_STATUS`.
/// `ac`: 0 offline, 1 online, 255 unknown. `flag`: bit3 charging, bit7 no battery, 255 unknown.
/// `percent`: 0..=100 or 255 unknown.
pub fn interpret_win32(ac: u8, flag: u8, percent: u8) -> Option<Battery> {
    let no_battery = flag != 255 && flag & 128 != 0;
    if no_battery || percent > 100 {
        return None;
    }
    Some(Battery { percent, on_battery: ac == 0, charging: flag != 255 && flag & 8 != 0 })
}

#[cfg(windows)]
pub fn read() -> Option<Battery> {
    use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
    let mut s = SYSTEM_POWER_STATUS::default();
    // SAFETY: `s` is a valid, writable SYSTEM_POWER_STATUS.
    unsafe { GetSystemPowerStatus(&mut s) }.ok()?;
    interpret_win32(s.ACLineStatus, s.BatteryFlag, s.BatteryLifePercent)
}

#[cfg(all(target_os = "linux", not(windows)))]
pub fn read() -> Option<Battery> {
    let base = std::path::Path::new("/sys/class/power_supply");
    let mut bat: Option<(u8, String)> = None;
    let mut ac_online: Option<bool> = None;
    for e in std::fs::read_dir(base).ok()?.flatten() {
        let p = e.path();
        let kind = std::fs::read_to_string(p.join("type")).unwrap_or_default();
        match kind.trim() {
            "Battery" if bat.is_none() => {
                let cap: u8 = std::fs::read_to_string(p.join("capacity")).ok()?.trim().parse().ok()?;
                let st = std::fs::read_to_string(p.join("status")).unwrap_or_default();
                bat = Some((cap.min(100), st.trim().to_string()));
            }
            "Mains" | "USB" => {
                if let Ok(v) = std::fs::read_to_string(p.join("online")) {
                    ac_online = Some(ac_online.unwrap_or(false) || v.trim() == "1");
                }
            }
            _ => {}
        }
    }
    let (percent, status) = bat?;
    let charging = status.eq_ignore_ascii_case("Charging");
    let on_battery = match ac_online {
        Some(ac) => !ac,
        None => status.eq_ignore_ascii_case("Discharging"),
    };
    Some(Battery { percent, on_battery, charging })
}

#[cfg(target_os = "macos")]
mod macos {
    //! AppleSmartBattery properties via native IOKit and CoreFoundation.
    //! No external executables, elevated permissions, or extra Rust crates.
    use super::Battery;
    use std::ffi::{c_char, c_void, CString};

    type CfTypeRef = *const c_void;
    type CfStringRef = *const c_void;
    type IoService = u32;

    #[link(name = "IOKit", kind = "framework")]
    unsafe extern "C" {
        fn IOServiceMatching(name: *const c_char) -> *mut c_void;
        fn IOServiceGetMatchingService(main_port: u32, matching: *mut c_void) -> IoService;
        fn IORegistryEntryCreateCFProperty(
            entry: IoService,
            key: CfStringRef,
            allocator: *const c_void,
            options: u32,
        ) -> CfTypeRef;
        fn IOObjectRelease(object: IoService) -> i32;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFStringCreateWithCString(allocator: *const c_void, c_str: *const c_char, encoding: u32) -> CfStringRef;
        fn CFGetTypeID(value: CfTypeRef) -> usize;
        fn CFNumberGetTypeID() -> usize;
        fn CFBooleanGetTypeID() -> usize;
        fn CFNumberGetValue(number: CfTypeRef, number_type: i32, value_ptr: *mut c_void) -> u8;
        fn CFBooleanGetValue(boolean: CfTypeRef) -> u8;
        fn CFRelease(value: CfTypeRef);
    }

    const UTF8: u32 = 0x0800_0100;
    const CF_NUMBER_SINT64_TYPE: i32 = 4;
    // On macOS kIOMainPortDefault / former kIOMasterPortDefault is MACH_PORT_NULL.
    const MAIN_PORT_DEFAULT: u32 = 0;

    struct Service(IoService);
    impl Drop for Service {
        fn drop(&mut self) {
            // SAFETY: service was returned by IOServiceGetMatchingService and is owned here.
            unsafe {
                IOObjectRelease(self.0);
            }
        }
    }

    struct CfOwned(CfTypeRef);
    impl Drop for CfOwned {
        fn drop(&mut self) {
            // SAFETY: all instances wrap non-null CF objects returned by Create/Copy functions.
            unsafe {
                CFRelease(self.0);
            }
        }
    }

    fn property(service: IoService, key: &str) -> Option<CfOwned> {
        let name = CString::new(key).ok()?;
        // SAFETY: CString remains alive for the call; UTF8 and null allocator are valid.
        let cf_key = unsafe { CFStringCreateWithCString(std::ptr::null(), name.as_ptr(), UTF8) };
        if cf_key.is_null() {
            return None;
        }
        let cf_key = CfOwned(cf_key);
        // SAFETY: service and CFStringRef are valid; null means default allocator.
        let value = unsafe { IORegistryEntryCreateCFProperty(service, cf_key.0, std::ptr::null(), 0) };
        if value.is_null() {
            None
        } else {
            Some(CfOwned(value))
        }
    }

    fn number(service: IoService, key: &str) -> Option<i64> {
        let value = property(service, key)?;
        // SAFETY: value is a live CF object; type check precedes CFNumberGetValue.
        if unsafe { CFGetTypeID(value.0) } != unsafe { CFNumberGetTypeID() } {
            return None;
        }
        let mut n = 0_i64;
        // SAFETY: output is a writable i64; CF_NUMBER_SINT64_TYPE matches its layout.
        let ok = unsafe { CFNumberGetValue(value.0, CF_NUMBER_SINT64_TYPE, (&mut n as *mut i64).cast()) };
        (ok != 0).then_some(n)
    }

    fn boolean(service: IoService, key: &str) -> Option<bool> {
        let value = property(service, key)?;
        // SAFETY: value is live; only call CFBooleanGetValue for a CFBoolean.
        if unsafe { CFGetTypeID(value.0) } != unsafe { CFBooleanGetTypeID() } {
            return None;
        }
        Some(unsafe { CFBooleanGetValue(value.0) != 0 })
    }

    pub fn read() -> Option<Battery> {
        let class = CString::new("AppleSmartBattery").ok()?;
        // SAFETY: matching dictionary is created for the IOKit class. IOKit consumes it.
        let matching = unsafe { IOServiceMatching(class.as_ptr()) };
        if matching.is_null() {
            return None;
        }
        // SAFETY: 0 is the default Mach port; IOKit owns/consumes matching dictionary.
        let handle = unsafe { IOServiceGetMatchingService(MAIN_PORT_DEFAULT, matching) };
        if handle == 0 {
            return None;
        }
        let service = Service(handle);

        let current = number(service.0, "CurrentCapacity")?;
        let maximum = number(service.0, "MaxCapacity")?;
        if current < 0 || maximum <= 0 {
            return None;
        }
        // Capacity can be expressed as 0..100 or as physical units, depending on macOS.
        let percent = ((current as f64 / maximum as f64) * 100.0).round().clamp(0.0, 100.0) as u8;
        let external = boolean(service.0, "ExternalConnected")?;
        let charging = boolean(service.0, "IsCharging")?;
        Some(Battery { percent, on_battery: !external, charging })
    }
}

#[cfg(target_os = "macos")]
pub fn read() -> Option<Battery> {
    macos::read()
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
pub fn read() -> Option<Battery> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn win32_status() {
        assert_eq!(interpret_win32(1, 8, 80), Some(Battery { percent: 80, on_battery: false, charging: true }));
        assert_eq!(interpret_win32(0, 0, 42), Some(Battery { percent: 42, on_battery: true, charging: false }));
        assert_eq!(interpret_win32(1, 128, 255), None);
        assert_eq!(interpret_win32(1, 255, 255), None);
        assert_eq!(interpret_win32(1, 0, 80), Some(Battery { percent: 80, on_battery: false, charging: false }));
    }
}
