//! Apple Silicon temperature readings via the macOS IOHID event system.
//!
//! The IOHIDEventSystemClient functions are private Apple APIs. They have been
//! observed to work without elevated privileges on Mac14,7 / macOS 15.8.3,
//! but may change in future macOS versions.
//!
//! The primary reading is the highest "PMU tdie*" sensor value. This is a
//! chip-die proxy, not a formally identified CPU core temperature.

use super::SensorReadings;
use std::ffi::{c_char, c_int, c_void};

// Core Foundation uses opaque references. Every Create/Copy result below is
// explicitly released, including values returned by IOHID copy functions.
type CfRef = *const c_void;

const UTF8: u32 = 0x0800_0100;
const SINT32: c_int = 3; // kCFNumberSInt32Type
const TEMP_EVENT: u32 = 15;
const TEMP_FIELD: i32 = (TEMP_EVENT << 16) as i32;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFStringCreateWithCString(allocator: CfRef, text: *const c_char, encoding: u32) -> CfRef;
    fn CFStringGetCString(value: CfRef, buffer: *mut c_char, capacity: isize, encoding: u32) -> u8;
    fn CFNumberCreate(allocator: CfRef, number_type: c_int, value: *const c_void) -> CfRef;
    fn CFDictionaryCreate(
        allocator: CfRef,
        keys: *const CfRef,
        values: *const CfRef,
        count: isize,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> CfRef;
    static kCFTypeDictionaryKeyCallBacks: u8;
    static kCFTypeDictionaryValueCallBacks: u8;
    fn CFArrayGetCount(array: CfRef) -> isize;
    fn CFArrayGetValueAtIndex(array: CfRef, index: isize) -> CfRef;
    fn CFRelease(value: CfRef);
}

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOHIDEventSystemClientCreate(allocator: CfRef) -> CfRef;
    fn IOHIDEventSystemClientSetMatching(client: CfRef, matching: CfRef);
    fn IOHIDEventSystemClientCopyServices(client: CfRef) -> CfRef;
    fn IOHIDServiceClientCopyProperty(service: CfRef, key: CfRef) -> CfRef;
    fn IOHIDServiceClientCopyEvent(service: CfRef, event_type: u32, options: c_int, timestamp: u64) -> CfRef;
    fn IOHIDEventGetFloatValue(event: CfRef, field: i32) -> f64;
}

struct OwnedCf(CfRef);

impl OwnedCf {
    fn new(ptr: CfRef) -> Option<Self> {
        (!ptr.is_null()).then_some(Self(ptr))
    }
}

impl Drop for OwnedCf {
    fn drop(&mut self) {
        // SAFETY: Only non-null Core Foundation references returned by Create/Copy
        // enter this wrapper. Each is released exactly once.
        unsafe { CFRelease(self.0) };
    }
}

fn cf_string(text: &'static [u8]) -> Option<OwnedCf> {
    // SAFETY: The supplied byte strings are static, NUL-terminated ASCII.
    OwnedCf::new(unsafe { CFStringCreateWithCString(std::ptr::null(), text.as_ptr().cast(), UTF8) })
}

fn cf_number(value: i32) -> Option<OwnedCf> {
    // SAFETY: CFNumberCreate synchronously copies the supplied i32.
    OwnedCf::new(unsafe { CFNumberCreate(std::ptr::null(), SINT32, (&value as *const i32).cast()) })
}

fn sensor_matching() -> Option<OwnedCf> {
    let page_key = cf_string(b"PrimaryUsagePage\0")?;
    let usage_key = cf_string(b"PrimaryUsage\0")?;
    let page_value = cf_number(0xff00)?;
    let usage_value = cf_number(5)?;
    let keys = [page_key.0, usage_key.0];
    let values = [page_value.0, usage_value.0];
    // SAFETY: Both arrays are valid for two entries. CFType callbacks retain
    // their objects, so releasing our references after creation is safe.
    OwnedCf::new(unsafe {
        CFDictionaryCreate(
            std::ptr::null(),
            keys.as_ptr(),
            values.as_ptr(),
            2,
            std::ptr::addr_of!(kCFTypeDictionaryKeyCallBacks).cast(),
            std::ptr::addr_of!(kCFTypeDictionaryValueCallBacks).cast(),
        )
    })
}

fn name_of(service: CfRef, product_key: CfRef) -> Option<String> {
    // SAFETY: The service comes from a live services array, and CopyProperty
    // returns a retained CFString on this filtered HID service.
    let name = OwnedCf::new(unsafe { IOHIDServiceClientCopyProperty(service, product_key) })?;
    let mut bytes = [0i8; 128];
    // SAFETY: The buffer is writable and its length is passed as capacity.
    let ok = unsafe { CFStringGetCString(name.0, bytes.as_mut_ptr(), bytes.len() as isize, UTF8) };
    if ok == 0 {
        return None;
    }
    // SAFETY: Successful CFStringGetCString writes a terminating NUL.
    let name = unsafe { std::ffi::CStr::from_ptr(bytes.as_ptr()) };
    Some(name.to_string_lossy().into_owned())
}

fn read_die_max() -> Option<f64> {
    let system = OwnedCf::new(unsafe { IOHIDEventSystemClientCreate(std::ptr::null()) })?;
    let matching = sensor_matching()?;
    // SAFETY: Both references are live and have the expected CoreFoundation types.
    unsafe { IOHIDEventSystemClientSetMatching(system.0, matching.0) };
    let services = OwnedCf::new(unsafe { IOHIDEventSystemClientCopyServices(system.0) })?;
    let product_key = cf_string(b"Product\0")?;
    let mut primary: Option<f64> = None;
    let mut secondary: Option<f64> = None;

    // SAFETY: Service entries remain borrowed and valid while services is live.
    let count = unsafe { CFArrayGetCount(services.0) };
    for i in 0..count {
        let service = unsafe { CFArrayGetValueAtIndex(services.0, i) };
        if service.is_null() {
            continue;
        }
        let Some(name) = name_of(service, product_key.0) else {
            continue;
        };
        let group = if name.starts_with("PMU tdie") {
            &mut primary
        } else if name.starts_with("PMU2 tdie") {
            &mut secondary
        } else {
            continue;
        };
        let Some(event) = OwnedCf::new(unsafe { IOHIDServiceClientCopyEvent(service, TEMP_EVENT, 0, 0) }) else {
            continue;
        };
        let temp = unsafe { IOHIDEventGetFloatValue(event.0, TEMP_FIELD) };
        if temp.is_finite() && (0.0..150.0).contains(&temp) {
            *group = Some(group.map_or(temp, |old| old.max(temp)));
        }
    }
    primary.or(secondary)
}

pub struct Imp;

impl Imp {
    pub fn new() -> Self {
        Self
    }

    pub fn read(&mut self) -> SensorReadings {
        let temp = read_die_max();
        SensorReadings {
            cpu_temp: temp,
            temp_source: temp.map(|_| "IOHID PMU tdie (chip-die proxy)"),
            ..SensorReadings::default()
        }
    }
}
