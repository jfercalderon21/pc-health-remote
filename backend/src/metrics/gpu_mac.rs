//! macOS Apple Silicon GPU utilisation, using private IOReport APIs.
//! One sample per existing collector tick: no sleeps, helper processes, or sudo.

#[path = "smc_mac.rs"]
mod smc;

use super::GpuInfo;
use std::{
    ffi::{c_char, c_void, CStr, CString},
    ptr,
    time::{Duration, Instant},
};

type CFRef = *const c_void;
const UTF8: u32 = 0x0800_0100;
const RETRY: Duration = Duration::from_secs(30);
const MIN_SAMPLE_INTERVAL: Duration = Duration::from_millis(100);

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFStringCreateWithCString(alloc: CFRef, str: *const c_char, encoding: u32) -> CFRef;
    fn CFStringGetCString(str: CFRef, buf: *mut c_char, size: isize, encoding: u32) -> u8;
    fn CFDictionaryGetValue(dict: CFRef, key: CFRef) -> CFRef;
    fn CFArrayGetCount(array: CFRef) -> isize;
    fn CFArrayGetValueAtIndex(array: CFRef, index: isize) -> CFRef;
    fn CFRelease(cf: CFRef);
}

#[link(name = "IOReport", kind = "dylib")]
unsafe extern "C" {
    fn IOReportCopyChannelsInGroup(group: CFRef, subgroup: CFRef, flags: u64, a: u64, b: u64) -> CFRef;
    fn IOReportCreateSubscription(
        allocator: CFRef,
        channels: CFRef,
        out: *mut CFRef,
        flags: u64,
        unknown: CFRef,
    ) -> CFRef;
    fn IOReportCreateSamples(subscription: CFRef, channels: CFRef, unknown: CFRef) -> CFRef;
    fn IOReportCreateSamplesDelta(first: CFRef, second: CFRef, unknown: CFRef) -> CFRef;
    fn IOReportChannelGetGroup(item: CFRef) -> CFRef;
    fn IOReportChannelGetSubGroup(item: CFRef) -> CFRef;
    fn IOReportChannelGetUnitLabel(item: CFRef) -> CFRef;
    fn IOReportStateGetCount(item: CFRef) -> i32;
    fn IOReportStateGetNameForIndex(item: CFRef, index: i32) -> CFRef;
    fn IOReportStateGetResidency(item: CFRef, index: i32) -> i64;
}

fn cf_str(value: &str) -> Option<CFRef> {
    let c = CString::new(value).ok()?;
    let out = unsafe { CFStringCreateWithCString(ptr::null(), c.as_ptr(), UTF8) };
    (!out.is_null()).then_some(out)
}

fn str_value(value: CFRef) -> String {
    if value.is_null() {
        return String::new();
    }
    let mut buf = [0i8; 256];
    if unsafe { CFStringGetCString(value, buf.as_mut_ptr(), buf.len() as isize, UTF8) } == 0 {
        return String::new();
    }
    unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().into_owned()
}

fn is_active(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    !n.contains("idle") && !n.contains("off") && !n.contains("down")
}

struct Session {
    channels: CFRef,
    subscription: CFRef,
    previous: CFRef,
    at: Option<Instant>,
}

// The session is exclusively owned and accessed by the mutable Collector/Gpu.
unsafe impl Send for Session {}

impl Session {
    fn new() -> Result<Self, String> {
        let group = cf_str("GPU Stats").ok_or("CFString group creation failed")?;
        let subgroup = cf_str("GPU Performance States").ok_or("CFString subgroup creation failed")?;
        let channels = unsafe { IOReportCopyChannelsInGroup(group, subgroup, 0, 0, 0) };
        unsafe {
            CFRelease(group);
            CFRelease(subgroup);
        }
        if channels.is_null() {
            return Err("GPU Stats channels unavailable".into());
        }
        let mut merged: CFRef = ptr::null();
        let subscription = unsafe { IOReportCreateSubscription(ptr::null(), channels, &mut merged, 0, ptr::null()) };
        if !merged.is_null() {
            unsafe {
                CFRelease(merged);
            }
        }
        if subscription.is_null() {
            unsafe {
                CFRelease(channels);
            }
            return Err("IOReport GPU subscription failed".into());
        }
        Ok(Self { channels, subscription, previous: ptr::null(), at: None })
    }

    fn tick(&mut self) -> Result<Option<u32>, String> {
        let now = Instant::now();
        if self.at.is_some_and(|at| now.duration_since(at) < MIN_SAMPLE_INTERVAL) {
            return Ok(None);
        }
        let current = unsafe { IOReportCreateSamples(self.subscription, self.channels, ptr::null()) };
        if current.is_null() {
            return Err("IOReportCreateSamples failed".into());
        }
        let previous = std::mem::replace(&mut self.previous, current);
        self.at = Some(now);
        if previous.is_null() {
            return Ok(None);
        }
        let delta = unsafe { IOReportCreateSamplesDelta(previous, current, ptr::null()) };
        unsafe {
            CFRelease(previous);
        }
        if delta.is_null() {
            return Err("IOReportCreateSamplesDelta failed".into());
        }
        let result = unsafe { usage_from_delta(delta) };
        unsafe {
            CFRelease(delta);
        }
        result.map(Some)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        unsafe {
            if !self.previous.is_null() {
                CFRelease(self.previous);
            }
            if !self.subscription.is_null() {
                CFRelease(self.subscription);
            }
            if !self.channels.is_null() {
                CFRelease(self.channels);
            }
        }
    }
}

unsafe fn usage_from_delta(delta: CFRef) -> Result<u32, String> {
    let key = cf_str("IOReportChannels").ok_or("CFString channels key failed")?;
    let array = CFDictionaryGetValue(delta, key);
    CFRelease(key);
    if array.is_null() {
        return Err("IOReportChannels missing".into());
    }
    for i in 0..CFArrayGetCount(array) {
        let channel = CFArrayGetValueAtIndex(array, i);
        if channel.is_null() {
            continue;
        }
        if str_value(IOReportChannelGetGroup(channel)) != "GPU Stats"
            || str_value(IOReportChannelGetSubGroup(channel)) != "GPU Performance States"
        {
            continue;
        }
        let ticks24m = str_value(IOReportChannelGetUnitLabel(channel)).trim() == "24Mticks";
        let mut total = 0i64;
        let mut active = 0i64;
        for n in 0..IOReportStateGetCount(channel) {
            let name = str_value(IOReportStateGetNameForIndex(channel, n));
            let raw = IOReportStateGetResidency(channel, n);
            let residency = if ticks24m { raw / 24 } else { raw };
            if residency < 0 {
                continue;
            }
            total = total.saturating_add(residency);
            if is_active(&name) {
                active = active.saturating_add(residency);
            }
        }
        if total <= 0 {
            return Err("GPU residency interval is zero".into());
        }
        return Ok(((active as f64 * 100.0 / total as f64).round().clamp(0.0, 100.0)) as u32);
    }
    Err("GPU Performance States channel not found".into())
}

pub struct MacGpu {
    session: Option<Session>,
    last_error: Option<String>,
    next_try: Instant,
    last_value: Option<u32>,
    thermal: Option<smc::SmcGpu>,
    thermal_attempted: bool,
}

impl MacGpu {
    pub fn new() -> Self {
        Self {
            session: None,
            last_error: None,
            next_try: Instant::now(),
            last_value: None,
            thermal: None,
            thermal_attempted: false,
        }
    }
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }
    /// Fan speed uses the existing AppleSMC connection opened by read().
    /// Some(0) means the fan is stopped; None means no valid reading.
    pub fn fan_rpm(&mut self) -> Option<u32> {
        self.thermal.as_mut().and_then(|t| t.read_fan_rpm())
    }
    pub fn read(&mut self) -> Option<GpuInfo> {
        // Temperature collection is independent from GPU utilisation sampling.
        if self.thermal.is_none() {
            self.thermal = smc::SmcGpu::new().ok();
        }
        if self.session.is_none() {
            if Instant::now() < self.next_try {
                return None;
            }
            match Session::new() {
                Ok(session) => {
                    log::info!("Apple Silicon GPU IOReport initialised");
                    self.session = Some(session);
                    self.last_error = None;
                }
                Err(e) => {
                    log::warn!("Apple Silicon GPU IOReport unavailable: {e}");
                    self.last_error = Some(e);
                    self.next_try = Instant::now() + RETRY;
                    return None;
                }
            }
        }
        match self.session.as_mut()?.tick() {
            Ok(Some(value)) => {
                self.last_value = Some(value);
                self.last_error = None;
            }
            Ok(None) => {}
            Err(e) => {
                log::warn!("Apple Silicon GPU IOReport sampling failed: {e}");
                self.last_error = Some(e);
                self.session = None;
                self.last_value = None;
                self.next_try = Instant::now() + RETRY;
                return None;
            }
        }
        self.last_value.map(|value| GpuInfo {
            name: "Apple Silicon GPU".into(),
            load_pct: Some(value),
            temp_c: self.thermal.as_mut().and_then(|t| t.read_avg_c()),
            vram_used_bytes: None,
            vram_total_bytes: None,
            fan_pct: None,
        })
    }
}
