//! Apple Silicon GPU thermal readings and continuous fan RPM sampling from AppleSMC (IOKit).
//! Uses the same 4-byte float key scheme as vladkens/macmon (MIT license).
//! Discover Tg** and F*Ac keys once and cache metadata; never spawn a process or use sudo.
//! The AppleSMC service and key format are undocumented/private interfaces.

use std::{
    collections::HashMap,
    ffi::{c_char, c_void, CStr, CString},
    mem::size_of,
};

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOServiceMatching(name: *const c_char) -> *mut c_void;
    fn IOServiceGetMatchingServices(port: u32, matching: *mut c_void, iterator: *mut u32) -> i32;
    fn IOIteratorNext(iterator: u32) -> u32;
    fn IORegistryEntryGetName(entry: u32, name: *mut c_char) -> i32;
    fn IOObjectRelease(obj: u32) -> i32;
    fn mach_task_self() -> u32;
    fn IOServiceOpen(device: u32, task: u32, kind: u32, conn: *mut u32) -> i32;
    fn IOServiceClose(conn: u32) -> i32;
    fn IOConnectCallStructMethod(
        conn: u32,
        selector: u32,
        input: *const c_void,
        input_size: usize,
        output: *mut c_void,
        output_size: *mut usize,
    ) -> i32;
}

#[repr(C)]
#[derive(Default)]
struct KeyDataVer {
    major: u8,
    minor: u8,
    build: u8,
    reserved: u8,
    release: u16,
}
#[repr(C)]
#[derive(Default)]
struct PLimitData {
    version: u16,
    length: u16,
    cpu_p_limit: u32,
    gpu_p_limit: u32,
    mem_p_limit: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct KeyInfo {
    data_size: u32,
    data_type: u32,
    data_attributes: u8,
}
#[repr(C)]
#[derive(Default)]
struct KeyData {
    key: u32,
    vers: KeyDataVer,
    p_limit_data: PLimitData,
    key_info: KeyInfo,
    result: u8,
    status: u8,
    data8: u8,
    data32: u32,
    bytes: [u8; 32],
}

struct Smc {
    conn: u32,
    metadata: HashMap<u32, KeyInfo>,
}

impl Drop for Smc {
    fn drop(&mut self) {
        unsafe {
            IOServiceClose(self.conn);
        }
    }
}

impl Smc {
    fn open() -> Result<Self, String> {
        let class = CString::new("AppleSMC").unwrap();
        let matching = unsafe { IOServiceMatching(class.as_ptr()) };
        if matching.is_null() {
            return Err("AppleSMC matching unavailable".into());
        }
        let mut iterator = 0u32;
        let rc = unsafe { IOServiceGetMatchingServices(0, matching, &mut iterator) };
        if rc != 0 {
            return Err(format!("AppleSMC discovery: {rc}"));
        }
        if iterator == 0 {
            return Err("AppleSMC iterator unavailable".into());
        }
        let mut connection = 0u32;
        loop {
            let service = unsafe { IOIteratorNext(iterator) };
            if service == 0 {
                break;
            }
            let mut name = [0i8; 128];
            let named = unsafe { IORegistryEntryGetName(service, name.as_mut_ptr()) } == 0;
            let is_endpoint = named && unsafe { CStr::from_ptr(name.as_ptr()) }.to_bytes() == b"AppleSMCKeysEndpoint";
            if is_endpoint {
                let result = unsafe { IOServiceOpen(service, mach_task_self(), 0, &mut connection) };
                if result != 0 {
                    log::debug!("AppleSMC endpoint open failed: {result}");
                }
            }
            unsafe {
                IOObjectRelease(service);
            }
            if connection != 0 {
                break;
            }
        }
        unsafe {
            IOObjectRelease(iterator);
        }
        if connection == 0 {
            return Err("AppleSMCKeysEndpoint not accessible".into());
        }
        Ok(Self { conn: connection, metadata: HashMap::new() })
    }

    fn request(&self, input: &KeyData) -> Result<KeyData, String> {
        let mut output = KeyData::default();
        let mut output_size = size_of::<KeyData>();
        let rc = unsafe {
            IOConnectCallStructMethod(
                self.conn,
                2,
                (input as *const KeyData).cast(),
                size_of::<KeyData>(),
                (&mut output as *mut KeyData).cast(),
                &mut output_size,
            )
        };
        if rc != 0 || output.result != 0 || output_size < size_of::<KeyData>() {
            return Err(format!("SMC request failed (IOKit={rc}, result={})", output.result));
        }
        Ok(output)
    }

    fn metadata(&mut self, key: u32) -> Result<KeyInfo, String> {
        if let Some(info) = self.metadata.get(&key) {
            return Ok(*info);
        }
        let result = self.request(&KeyData { key, data8: 9, ..KeyData::default() })?;
        self.metadata.insert(key, result.key_info);
        Ok(result.key_info)
    }

    fn read_raw(&mut self, key: u32) -> Result<(KeyInfo, [u8; 32]), String> {
        let info = self.metadata(key)?;
        let value = self.request(&KeyData { key, key_info: info, data8: 5, ..KeyData::default() })?;
        Ok((info, value.bytes))
    }

    fn float(&mut self, key: u32) -> Result<f32, String> {
        let (info, data) = self.read_raw(key)?;
        if info.data_size != 4 || info.data_type != u32::from_be_bytes(*b"flt ") {
            return Err("SMC value is not a 4-byte float".into());
        }
        Ok(f32::from_le_bytes(data[..4].try_into().unwrap()))
    }

    /// Read RPM for AppleSMC fan keys across the value formats macmon supports.
    /// Zero is a valid reading; non-finite and out-of-range values are rejected.
    fn fan_rpm(&mut self, key: u32) -> Option<u32> {
        let (info, data) = self.read_raw(key).ok()?;
        let n = info.data_size as usize;
        let rpm = match (info.data_type, n) {
            (t, 4) if t == u32::from_be_bytes(*b"flt ") => f32::from_le_bytes(data[..4].try_into().ok()?),
            (t, size) if t == u32::from_be_bytes(*b"fpe2") && size >= 2 => {
                (((data[0] as u16) << 6) | ((data[1] as u16) >> 2)) as f32
            }
            (t, size) if t == u32::from_be_bytes(*b"ui8 ") && size >= 1 => data[0] as f32,
            (t, size) if t == u32::from_be_bytes(*b"ui16") && size >= 2 => {
                u16::from_be_bytes(data[..2].try_into().ok()?) as f32
            }
            (t, size) if t == u32::from_be_bytes(*b"ui32") && size >= 4 => {
                u32::from_be_bytes(data[..4].try_into().ok()?) as f32
            }
            _ => return None,
        };
        if rpm.is_finite() && (0.0..=100_000.0).contains(&rpm) {
            Some(rpm.trunc() as u32)
        } else {
            None
        }
    }

    fn keys(&mut self) -> Result<Vec<u32>, String> {
        let (_, bytes) = self.read_raw(u32::from_be_bytes(*b"#KEY"))?;
        let count = u32::from_be_bytes(bytes[..4].try_into().unwrap());
        // Bound enumeration to avoid an unbounded scan on unexpected hardware.
        if count > 50_000 {
            return Err("Unexpected SMC key count".into());
        }
        let mut result = Vec::new();
        for i in 0..count {
            let request = KeyData { data8: 8, data32: i, ..KeyData::default() };
            if let Ok(item) = self.request(&request) {
                result.push(item.key);
            }
        }
        Ok(result)
    }
}

pub(super) struct SmcGpu {
    smc: Smc,
    keys: Vec<u32>,
    fan_keys: Vec<u32>,
}

impl SmcGpu {
    pub(super) fn new() -> Result<Self, String> {
        let mut smc = Smc::open()?;
        let all_keys = smc.keys()?;
        let keys: Vec<u32> = all_keys
            .iter()
            .copied()
            .filter(|key| key.to_be_bytes().starts_with(b"Tg"))
            .filter(|key| {
                let Ok(value) = smc.float(*key) else { return false };
                value.is_finite() && (0.0..150.0).contains(&value)
            })
            .collect();
        if keys.is_empty() {
            return Err("No readable AppleSMC Tg temperature sensors".into());
        }
        log::info!("AppleSMC GPU temperature initialised ({} sensors)", keys.len());

        // Discover fan keys once and reuse the same SMC connection each tick.
        // A stopped fan returns Some(0), distinct from an unreadable sensor.
        let fan_keys: Vec<u32> = all_keys
            .into_iter()
            .filter(|key| {
                let name = key.to_be_bytes();
                name[0] == b'F' && name[2] == b'A' && name[3] == b'c' && smc.fan_rpm(*key).is_some()
            })
            .collect();
        if fan_keys.is_empty() {
            log::info!("AppleSMC fan: no readable RPM sensors");
        } else {
            let names: Vec<String> =
                fan_keys.iter().map(|key| String::from_utf8_lossy(&key.to_be_bytes()).into_owned()).collect();
            log::info!("AppleSMC fan: {} RPM sensor(s): {}", fan_keys.len(), names.join(", "));
        }
        Ok(Self { smc, keys, fan_keys })
    }

    /// Highest valid fan speed; 0 RPM is a valid reading.
    pub(super) fn read_fan_rpm(&mut self) -> Option<u32> {
        let mut rpm: Option<u32> = None;
        for key in &self.fan_keys {
            if let Some(value) = self.smc.fan_rpm(*key) {
                rpm = Some(rpm.map_or(value, |old| old.max(value)));
            }
        }
        rpm
    }

    pub(super) fn read_avg_c(&mut self) -> Option<u32> {
        let values: Vec<f32> = self
            .keys
            .iter()
            .filter_map(|key| self.smc.float(*key).ok())
            .filter(|v| v.is_finite() && (0.0..150.0).contains(v))
            .collect();
        if values.is_empty() {
            return None;
        }
        let mean = values.iter().sum::<f32>() / values.len() as f32;
        Some(mean.round().clamp(0.0, 255.0) as u32)
    }
}
