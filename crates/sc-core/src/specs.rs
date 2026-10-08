//! Hardware and OS inventory, read once: CPU, memory modules, GPUs, storage,
//! board and firmware, displays, network adapters and Windows itself.

use std::arch::x86_64::__cpuid_count;
use std::fmt::Write;

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE, IDXGIFactory1};
use windows::Win32::Graphics::Gdi::{
    DEVMODEW, DISPLAY_DEVICE_ATTACHED_TO_DESKTOP, DISPLAY_DEVICE_PRIMARY_DEVICE, DISPLAY_DEVICEW,
    ENUM_CURRENT_SETTINGS, EnumDisplayDevicesW, EnumDisplaySettingsW,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::IO::DeviceIoControl;
use windows::Win32::System::Ioctl::{
    DEVICE_SEEK_PENALTY_DESCRIPTOR, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, IOCTL_STORAGE_QUERY_PROPERTY,
    PropertyStandardQuery, STORAGE_PROPERTY_QUERY, StorageDeviceProperty, StorageDeviceSeekPenaltyProperty,
};
use windows::Win32::System::SystemInformation::{
    GetLogicalProcessorInformationEx, GetSystemFirmwareTable, GetTickCount64, RSMB, RelationAll,
    SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
};
use windows::Win32::System::Threading::{IsProcessorFeaturePresent, PROCESSOR_FEATURE_ID};
use windows::core::PCWSTR;

use crate::perf::PerfSampler;
use crate::reg::{Hive, Key, View};
use crate::system;

#[derive(Debug, Clone, Default)]
pub struct Section {
    pub title: &'static str,
    pub rows: Vec<(String, String)>,
}

impl Section {
    fn new(title: &'static str) -> Self {
        Self { title, rows: Vec::new() }
    }
    fn add(&mut self, label: &str, value: impl Into<String>) {
        let value = value.into();
        let value = value.trim();
        if !value.is_empty() {
            self.rows.push((label.to_owned(), value.to_owned()));
        }
    }
}

pub fn bytes_text(n: u64) -> String {
    const GB: f64 = (1u64 << 30) as f64;
    const MB: f64 = (1u64 << 20) as f64;
    const KB: f64 = 1024.0;
    let n = n as f64;
    if n >= GB {
        let v = n / GB;
        if v.fract() == 0.0 { format!("{v:.0} GB") } else { format!("{v:.1} GB") }
    } else if n >= MB {
        format!("{:.0} MB", n / MB)
    } else {
        format!("{:.0} KB", n / KB)
    }
}

// ---------------------------------------------------------------------------
// CPU

fn cpuid(leaf: u32, sub: u32) -> [u32; 4] {
    let r = __cpuid_count(leaf, sub);
    [r.eax, r.ebx, r.ecx, r.edx]
}

fn cpu_brand() -> String {
    if cpuid(0x8000_0000, 0)[0] < 0x8000_0004 {
        return String::new();
    }
    let bytes: Vec<u8> = (0x8000_0002..=0x8000_0004u32)
        .flat_map(|leaf| cpuid(leaf, 0))
        .flat_map(u32::to_le_bytes)
        .take_while(|&b| b != 0)
        .collect();
    String::from_utf8_lossy(&bytes).split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Family, model and stepping decoded from CPUID leaf 1 EAX.
pub fn decode_signature(eax: u32) -> (u32, u32, u32) {
    let stepping = eax & 0xF;
    let base_model = (eax >> 4) & 0xF;
    let base_family = (eax >> 8) & 0xF;
    let ext_model = (eax >> 16) & 0xF;
    let ext_family = (eax >> 20) & 0xFF;
    let family = if base_family == 0xF { base_family + ext_family } else { base_family };
    let model = if base_family == 0x6 || base_family == 0xF { (ext_model << 4) | base_model } else { base_model };
    (family, model, stepping)
}

fn cpu_features() -> String {
    let l1 = cpuid(1, 0);
    let l7 = if cpuid(0, 0)[0] >= 7 { cpuid(7, 0) } else { [0; 4] };
    let table: [(&str, bool); 14] = [
        ("SSE2", l1[3] & (1 << 26) != 0),
        ("SSE3", l1[2] & 1 != 0),
        ("SSSE3", l1[2] & (1 << 9) != 0),
        ("SSE4.1", l1[2] & (1 << 19) != 0),
        ("SSE4.2", l1[2] & (1 << 20) != 0),
        ("AES", l1[2] & (1 << 25) != 0),
        ("AVX", l1[2] & (1 << 28) != 0),
        ("FMA3", l1[2] & (1 << 12) != 0),
        ("F16C", l1[2] & (1 << 29) != 0),
        ("AVX2", l7[1] & (1 << 5) != 0),
        ("BMI2", l7[1] & (1 << 8) != 0),
        ("AVX-512F", l7[1] & (1 << 16) != 0),
        ("SHA", l7[1] & (1 << 29) != 0),
        ("VAES", l7[2] & (1 << 9) != 0),
    ];
    table.iter().filter(|f| f.1).map(|f| f.0).collect::<Vec<_>>().join(", ")
}

#[derive(Default)]
struct Topology {
    cores: u32,
    logical: u32,
    // (level, is data-or-unified, size in bytes, how many of them)
    caches: Vec<(u8, bool, u64, u32)>,
}

fn topology() -> Topology {
    let mut topo = Topology::default();
    let mut len = 0u32;
    unsafe {
        let _ = GetLogicalProcessorInformationEx(RelationAll, None, &mut len);
        let mut buf = vec![0u64; (len as usize).div_ceil(8)];
        if GetLogicalProcessorInformationEx(RelationAll, Some(buf.as_mut_ptr().cast()), &mut len).is_err() {
            return topo;
        }
        let base = buf.as_ptr() as *const u8;
        let mut off = 0usize;
        while off + 8 <= len as usize {
            let info = &*(base.add(off) as *const SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX);
            match info.Relationship.0 {
                // RelationProcessorCore
                0 => {
                    topo.cores += 1;
                    let p = &info.Anonymous.Processor;
                    let groups = std::slice::from_raw_parts(p.GroupMask.as_ptr(), p.GroupCount as usize);
                    topo.logical += groups.iter().map(|g| g.Mask.count_ones()).sum::<u32>();
                }
                // RelationCache
                2 => {
                    let c = &info.Anonymous.Cache;
                    // Type 1 is the instruction cache; 0 unified, 2 data.
                    let data = c.Type.0 != 1;
                    match topo.caches.iter_mut().find(|e| e.0 == c.Level && e.1 == data && e.2 == c.CacheSize as u64) {
                        Some(e) => e.3 += 1,
                        None => topo.caches.push((c.Level, data, c.CacheSize as u64, 1)),
                    }
                }
                _ => {}
            }
            if info.Size == 0 {
                break;
            }
            off += info.Size as usize;
        }
    }
    topo.caches.sort();
    topo
}

fn cpu_section(smbios: &Smbios) -> Section {
    let mut s = Section::new("CPU");
    s.add("Model", cpu_brand());
    let topo = topology();
    s.add("Cores", format!("{} physical, {} logical", topo.cores, topo.logical));
    s.add("Socket", smbios.socket.clone());
    let key = Key::open(Hive::Hklm, r"HARDWARE\DESCRIPTION\System\CentralProcessor\0", View::Native);
    if let Some(mhz) = key.as_ref().and_then(|k| k.dword("~MHz")) {
        s.add("Base speed", format!("{:.2} GHz", mhz as f32 / 1000.0));
    }
    for (level, data, size, count) in &topo.caches {
        let kind = match (level, data) {
            (1, true) => "L1 data cache".to_owned(),
            (1, false) => "L1 code cache".to_owned(),
            (l, _) => format!("L{l} cache"),
        };
        let each = bytes_text(*size);
        s.add(&kind, if *count > 1 { format!("{count} x {each}") } else { each });
    }
    s.add("Instructions", cpu_features());
    let (family, model, stepping) = decode_signature(cpuid(1, 0)[0]);
    s.add("Family / model", format!("{family} / {model}, stepping {stepping}"));
    if let Some(k) = &key {
        if let Some(rev) = k.raw("Update Revision").filter(|r| r.1.len() >= 8) {
            s.add("Microcode", format!("0x{:X}", u32::from_le_bytes([rev.1[4], rev.1[5], rev.1[6], rev.1[7]])));
        }
    }
    const PF_VIRT_FIRMWARE_ENABLED: PROCESSOR_FEATURE_ID = PROCESSOR_FEATURE_ID(21);
    let virt = unsafe { IsProcessorFeaturePresent(PF_VIRT_FIRMWARE_ENABLED) }.as_bool();
    // With a hypervisor running Windows reports false, so only say "enabled" when sure.
    let hypervisor = cpuid(1, 0)[2] & (1 << 31) != 0;
    s.add(
        "Virtualization",
        if virt {
            "Enabled in firmware"
        } else if hypervisor {
            "In use by a hypervisor"
        } else {
            "Disabled in firmware"
        },
    );
    s
}

// ---------------------------------------------------------------------------
// SMBIOS: board, firmware, memory modules

#[derive(Debug, Clone, Default, PartialEq)]
pub struct MemoryModule {
    pub slot: String,
    pub bank: String,
    pub size: u64,
    pub speed_mhz: u32,
    pub kind: &'static str,
    pub manufacturer: String,
    pub part_number: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Smbios {
    pub bios_vendor: String,
    pub bios_version: String,
    pub bios_date: String,
    pub system_maker: String,
    pub system_model: String,
    pub board_maker: String,
    pub board_model: String,
    pub board_version: String,
    pub socket: String,
    pub modules: Vec<MemoryModule>,
    pub empty_slots: u32,
}

fn memory_kind(code: u8) -> &'static str {
    match code {
        0x18 => "DDR3",
        0x1A => "DDR4",
        0x1B => "LPDDR",
        0x1C => "LPDDR2",
        0x1D => "LPDDR3",
        0x1E => "LPDDR4",
        0x22 => "DDR5",
        0x23 => "LPDDR5",
        _ => "",
    }
}

/// Parses the SMBIOS structure table (the part after Windows' 8-byte header).
/// Truncated or malformed tables yield whatever was readable.
pub fn parse_smbios(table: &[u8]) -> Smbios {
    let mut out = Smbios::default();
    let mut off = 0usize;
    while off + 4 <= table.len() {
        let kind = table[off];
        let len = table[off + 1] as usize;
        if len < 4 || off + len > table.len() {
            break;
        }
        let body = &table[off..off + len];
        // The string set follows the formatted area and ends with a double NUL.
        let mut strings: Vec<&[u8]> = Vec::new();
        let mut p = off + len;
        loop {
            let Some(end) = table.get(p..).and_then(|rest| rest.iter().position(|&b| b == 0)) else {
                return out;
            };
            if end == 0 {
                // Empty string: end of set (a structure without strings has two NULs).
                p += if strings.is_empty() { 2 } else { 1 };
                break;
            }
            strings.push(&table[p..p + end]);
            p += end + 1;
        }
        let text = |index_at: usize| -> String {
            let idx = body.get(index_at).copied().unwrap_or(0) as usize;
            match idx.checked_sub(1).and_then(|i| strings.get(i)) {
                Some(s) => String::from_utf8_lossy(s).trim().to_owned(),
                None => String::new(),
            }
        };
        let u16_at = |o: usize| body.get(o..o + 2).map_or(0, |b| u16::from_le_bytes([b[0], b[1]]));
        let u32_at = |o: usize| body.get(o..o + 4).map_or(0, |b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));

        match kind {
            0 => {
                out.bios_vendor = text(4);
                out.bios_version = text(5);
                out.bios_date = text(8);
            }
            1 => {
                out.system_maker = text(4);
                out.system_model = text(5);
            }
            2 => {
                out.board_maker = text(4);
                out.board_model = text(5);
                out.board_version = text(6);
            }
            4 if out.socket.is_empty() => out.socket = text(4),
            17 => {
                let raw = u16_at(0x0C);
                let size = match raw {
                    0 | 0xFFFF => 0,
                    // Size is in the extended field, in MB.
                    0x7FFF => (u32_at(0x1C) & 0x7FFF_FFFF) as u64 * (1 << 20),
                    // Bit 15 set means the unit is KB instead of MB.
                    s if s & 0x8000 != 0 => (s & 0x7FFF) as u64 * 1024,
                    s => s as u64 * (1 << 20),
                };
                if size == 0 {
                    out.empty_slots += 1;
                } else {
                    let configured = u16_at(0x20);
                    out.modules.push(MemoryModule {
                        slot: text(0x10),
                        bank: text(0x11),
                        size,
                        speed_mhz: if configured != 0 { configured } else { u16_at(0x15) } as u32,
                        kind: memory_kind(body.get(0x12).copied().unwrap_or(0)),
                        manufacturer: text(0x17),
                        part_number: text(0x1A),
                    });
                }
            }
            127 => break,
            _ => {}
        }
        off = p;
    }
    out
}

fn read_smbios() -> Smbios {
    unsafe {
        let size = GetSystemFirmwareTable(RSMB, 0, None);
        if size == 0 {
            return Smbios::default();
        }
        let mut buf = vec![0u8; size as usize];
        let got = GetSystemFirmwareTable(RSMB, 0, Some(&mut buf)) as usize;
        // RawSMBIOSData: four version bytes, a u32 length, then the table.
        if got < 8 || got > buf.len() {
            return Smbios::default();
        }
        let len = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
        parse_smbios(&buf[8..(8 + len).min(got)])
    }
}

/// OEM boards often leave placeholder text in SMBIOS fields.
fn real(s: &str) -> String {
    let l = s.to_ascii_lowercase();
    let placeholder = ["to be filled", "default string", "system product name", "system manufacturer", "not specified", "unknown"];
    if placeholder.iter().any(|p| l.contains(p)) { String::new() } else { s.to_owned() }
}

fn memory_section(smbios: &Smbios) -> Section {
    let mut s = Section::new("Memory");
    let total: u64 = smbios.modules.iter().map(|m| m.size).sum();
    if total > 0 {
        s.add("Installed", bytes_text(total));
    }
    let slots = smbios.modules.len() as u32 + smbios.empty_slots;
    if slots > 0 {
        s.add("Slots", format!("{} of {slots} used", smbios.modules.len()));
    }
    for m in &smbios.modules {
        let mut v = bytes_text(m.size);
        if !m.kind.is_empty() {
            let _ = write!(v, " {}", m.kind);
        }
        if m.speed_mhz > 0 {
            let _ = write!(v, ", {} MHz", m.speed_mhz);
        }
        let maker = real(&m.manufacturer);
        if !maker.is_empty() {
            let _ = write!(v, ", {maker}");
        }
        let part = real(&m.part_number);
        if !part.is_empty() {
            let _ = write!(v, " {part}");
        }
        let label = match (m.slot.is_empty(), m.bank.is_empty()) {
            (true, _) => "Module".to_owned(),
            (false, true) => m.slot.clone(),
            (false, false) => format!("{} ({})", m.slot, m.bank),
        };
        s.add(&label, v);
    }
    s
}

fn board_section(smbios: &Smbios) -> Section {
    let mut s = Section::new("Board");
    s.add("Motherboard", format!("{} {}", real(&smbios.board_maker), real(&smbios.board_model)));
    s.add("Board version", real(&smbios.board_version));
    s.add("System", format!("{} {}", real(&smbios.system_maker), real(&smbios.system_model)));
    s.add("BIOS", format!("{} {}", real(&smbios.bios_vendor), real(&smbios.bios_version)));
    s.add("BIOS date", real(&smbios.bios_date));
    s
}

// ---------------------------------------------------------------------------
// GPU, storage, displays, network, Windows

fn wtext(s: &[u16]) -> String {
    let end = s.iter().position(|&c| c == 0).unwrap_or(s.len());
    String::from_utf16_lossy(&s[..end]).trim().to_owned()
}

/// Driver version and date for a display adapter, by its device description.
fn gpu_driver(description: &str) -> Option<(String, String)> {
    let class = r"SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}";
    let root = Key::open(Hive::Hklm, class, View::Native)?;
    for sub in root.subkeys() {
        let Some(key) = Key::open(Hive::Hklm, &format!(r"{class}\{sub}"), View::Native) else { continue };
        if key.string("DriverDesc").is_some_and(|d| d.eq_ignore_ascii_case(description)) {
            return Some((key.string("DriverVersion").unwrap_or_default(), key.string("DriverDate").unwrap_or_default()));
        }
    }
    None
}

fn gpu_section() -> Section {
    let mut s = Section::new("GPU");
    unsafe {
        let Ok(factory) = CreateDXGIFactory1::<IDXGIFactory1>() else { return s };
        let mut index = 0;
        let mut seen = Vec::new();
        while let Ok(adapter) = factory.EnumAdapters1(index) {
            index += 1;
            let Ok(desc) = adapter.GetDesc1() else { continue };
            if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
                continue;
            }
            // A card driving outputs through another one is listed once per route.
            let id = (desc.VendorId, desc.DeviceId, desc.SubSysId, desc.Revision);
            if seen.contains(&id) {
                continue;
            }
            seen.push(id);
            let name = wtext(&desc.Description);
            let n = s.rows.iter().filter(|r| r.0.starts_with("Adapter")).count() + 1;
            let tag = if n == 1 { String::new() } else { format!(" {n}") };
            s.add(&format!("Adapter{tag}"), name.clone());
            s.add(&format!("Video memory{tag}"), bytes_text(desc.DedicatedVideoMemory as u64));
            if desc.SharedSystemMemory > 0 {
                s.add(&format!("Shared memory{tag}"), bytes_text(desc.SharedSystemMemory as u64));
            }
            if let Some((version, date)) = gpu_driver(&name) {
                s.add(&format!("Driver{tag}"), format!("{version} ({date})"));
            }
        }
    }
    s
}

fn bus_name(bus: i32) -> &'static str {
    match bus {
        1 => "SCSI",
        3 => "ATA",
        7 => "USB",
        8 => "RAID",
        10 => "SAS",
        11 => "SATA",
        12 => "SD",
        13 => "MMC",
        17 => "NVMe",
        _ => "",
    }
}

unsafe fn ioctl<I, O>(h: HANDLE, code: u32, input: Option<&I>, out: &mut [u8]) -> Option<usize>
where
    O: Sized,
{
    unsafe {
        let mut returned = 0u32;
        DeviceIoControl(
            h,
            code,
            input.map(|i| i as *const I as *const core::ffi::c_void),
            input.map_or(0, |_| size_of::<I>() as u32),
            Some(out.as_mut_ptr().cast()),
            out.len() as u32,
            Some(&mut returned),
            None,
        )
        .ok()?;
        (returned as usize >= size_of::<O>().min(out.len())).then_some(returned as usize)
    }
}

fn storage_section() -> Section {
    let mut s = Section::new("Storage");
    for n in 0..32 {
        let path: Vec<u16> = format!(r"\\.\PhysicalDrive{n}").encode_utf16().chain([0]).collect();
        // Zero access is enough for property and geometry queries, elevated or not.
        let Ok(h) = (unsafe {
            CreateFileW(
                PCWSTR(path.as_ptr()),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                None,
            )
        }) else {
            continue;
        };
        unsafe {
            let mut buf = [0u8; 1024];
            let query = STORAGE_PROPERTY_QUERY {
                PropertyId: StorageDeviceProperty,
                QueryType: PropertyStandardQuery,
                ..Default::default()
            };
            let mut model = String::new();
            let mut bus = "";
            if ioctl::<_, [u8; 40]>(h, IOCTL_STORAGE_QUERY_PROPERTY, Some(&query), &mut buf).is_some() {
                // STORAGE_DEVICE_DESCRIPTOR: string offsets at 12 (vendor) and 16 (product), bus type at 28.
                let off = |o: usize| u32::from_le_bytes([buf[o], buf[o + 1], buf[o + 2], buf[o + 3]]) as usize;
                let cstr = |o: usize| -> String {
                    if o == 0 || o >= buf.len() {
                        return String::new();
                    }
                    let end = buf[o..].iter().position(|&b| b == 0).map_or(buf.len(), |e| o + e);
                    String::from_utf8_lossy(&buf[o..end]).trim().to_owned()
                };
                model = format!("{} {}", cstr(off(12)), cstr(off(16))).trim().to_owned();
                bus = bus_name(off(28) as i32);
            }
            let seek_query = STORAGE_PROPERTY_QUERY {
                PropertyId: StorageDeviceSeekPenaltyProperty,
                QueryType: PropertyStandardQuery,
                ..Default::default()
            };
            let mut seek = [0u8; size_of::<DEVICE_SEEK_PENALTY_DESCRIPTOR>()];
            let kind = match ioctl::<_, DEVICE_SEEK_PENALTY_DESCRIPTOR>(h, IOCTL_STORAGE_QUERY_PROPERTY, Some(&seek_query), &mut seek) {
                // IncursSeekPenalty is the byte after the two u32 header fields.
                Some(_) if seek[8] == 0 => "SSD",
                Some(_) => "HDD",
                None => "",
            };
            // DISK_GEOMETRY_EX: a 24-byte DISK_GEOMETRY, then the size in bytes.
            let mut geo = [0u8; 64];
            let size = ioctl::<(), [u8; 32]>(h, IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, None, &mut geo)
                .map(|_| i64::from_le_bytes(geo[24..32].try_into().unwrap()) as u64);
            let _ = CloseHandle(h);

            let mut v = if model.is_empty() { "Unknown drive".to_owned() } else { model };
            if let Some(size) = size {
                let _ = write!(v, ", {}", bytes_text(size));
            }
            for extra in [bus, kind] {
                if !extra.is_empty() {
                    let _ = write!(v, ", {extra}");
                }
            }
            s.add(&format!("Disk {n}"), v);
        }
    }
    s
}

fn displays_section() -> Section {
    let mut s = Section::new("Displays");
    unsafe {
        for i in 0.. {
            let mut dev = DISPLAY_DEVICEW { cb: size_of::<DISPLAY_DEVICEW>() as u32, ..Default::default() };
            if !EnumDisplayDevicesW(None, i, &mut dev, 0).as_bool() {
                break;
            }
            if dev.StateFlags.0 & DISPLAY_DEVICE_ATTACHED_TO_DESKTOP.0 == 0 {
                continue;
            }
            let mut mode = DEVMODEW { dmSize: size_of::<DEVMODEW>() as u16, ..Default::default() };
            if !EnumDisplaySettingsW(PCWSTR(dev.DeviceName.as_ptr()), ENUM_CURRENT_SETTINGS, &mut mode).as_bool() {
                continue;
            }
            // The monitor behind this output is the first child device.
            let mut mon = DISPLAY_DEVICEW { cb: size_of::<DISPLAY_DEVICEW>() as u32, ..Default::default() };
            let name = if EnumDisplayDevicesW(PCWSTR(dev.DeviceName.as_ptr()), 0, &mut mon, 0).as_bool() {
                wtext(&mon.DeviceString)
            } else {
                String::new()
            };
            let primary = dev.StateFlags.0 & DISPLAY_DEVICE_PRIMARY_DEVICE.0 != 0;
            let mut v = format!("{} x {} at {} Hz", mode.dmPelsWidth, mode.dmPelsHeight, mode.dmDisplayFrequency);
            if !name.is_empty() {
                let _ = write!(v, ", {name}");
            }
            if primary {
                v.push_str(", primary");
            }
            s.add(&format!("Display {}", s.rows.len() + 1), v);
        }
    }
    s
}

fn network_section() -> Section {
    let mut s = Section::new("Network");
    for a in PerfSampler::new().sample().adapters {
        let speed = a.link_speed as f64;
        let link = if speed >= 1e9 { format!("{:.1} Gbps", speed / 1e9) } else { format!("{:.0} Mbps", speed / 1e6) };
        s.add(&a.name, format!("{}, {link}{}", a.description, if a.wifi { ", Wi-Fi" } else { "" }));
    }
    s
}

fn windows_section() -> Section {
    let mut s = Section::new("Windows");
    if let Some(k) = Key::open(Hive::Hklm, r"SOFTWARE\Microsoft\Windows NT\CurrentVersion", View::Native) {
        let build: u32 = k.string("CurrentBuild").and_then(|b| b.parse().ok()).unwrap_or(0);
        let mut name = k.string("ProductName").unwrap_or_default();
        // The product name still says "Windows 10" on Windows 11.
        if build >= 22000 {
            name = name.replace("Windows 10", "Windows 11");
        }
        s.add("Edition", name);
        s.add("Version", k.string("DisplayVersion").unwrap_or_default());
        s.add("Build", format!("{build}.{}", k.dword("UBR").unwrap_or(0)));
        if let Some(installed) = k.dword("InstallDate") {
            s.add("Installed", system::format_filetime(system::unix_to_filetime(installed as u64)));
        }
    }
    s.add("Computer name", std::env::var("COMPUTERNAME").unwrap_or_default());
    let up = unsafe { GetTickCount64() } / 1000;
    s.add("Running for", format!("{} d {} h {} min", up / 86400, up % 86400 / 3600, up % 3600 / 60));
    s
}

/// Reads everything. Takes a few hundred milliseconds; call off the UI thread.
pub fn collect() -> Vec<Section> {
    let smbios = read_smbios();
    let sections = vec![
        cpu_section(&smbios),
        memory_section(&smbios),
        gpu_section(),
        storage_section(),
        board_section(&smbios),
        displays_section(),
        network_section(),
        windows_section(),
    ];
    sections.into_iter().filter(|s| !s.rows.is_empty()).collect()
}

/// All sections as plain text, for the clipboard.
pub fn to_text(sections: &[Section]) -> String {
    let mut out = String::new();
    for s in sections {
        let _ = writeln!(out, "{}", s.title);
        for (label, value) in &s.rows {
            let _ = writeln!(out, "  {label}: {value}");
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds one SMBIOS structure: header, formatted bytes, then strings.
    fn structure(kind: u8, formatted: &[u8], strings: &[&str]) -> Vec<u8> {
        let mut v = vec![kind, (4 + formatted.len()) as u8, 0, 0];
        v.extend_from_slice(formatted);
        if strings.is_empty() {
            v.extend([0, 0]);
        } else {
            for s in strings {
                v.extend(s.as_bytes());
                v.push(0);
            }
            v.push(0);
        }
        v
    }

    fn memory_device(size: u16, ext_mb: u32, kind: u8, speed: u16, configured: u16, strings: &[&str]) -> Vec<u8> {
        // Formatted area from offset 4 up to 0x22.
        let mut f = vec![0u8; 0x22 - 4];
        let mut put = |off: usize, bytes: &[u8]| f[off - 4..off - 4 + bytes.len()].copy_from_slice(bytes);
        put(0x0C, &size.to_le_bytes());
        put(0x10, &[1]); // device locator
        put(0x11, &[4]); // bank locator
        put(0x12, &[kind]);
        put(0x15, &speed.to_le_bytes());
        put(0x17, &[2]); // manufacturer
        put(0x1A, &[3]); // part number
        put(0x1C, &ext_mb.to_le_bytes());
        put(0x20, &configured.to_le_bytes());
        structure(17, &f, strings)
    }

    #[test]
    fn smbios_board_bios_and_memory() {
        let mut t = Vec::new();
        // BIOS: vendor=1, version=2, ..., date=3 at offset 8.
        t.extend(structure(0, &[1, 2, 0, 0, 3, 0, 0, 0], &["American Megatrends", "F12", "03/14/2025"]));
        t.extend(structure(1, &[1, 2], &["Maker Inc", "Model X"]));
        t.extend(structure(2, &[1, 2, 3], &["BoardCo", "B650 PRO", "1.0"]));
        t.extend(structure(4, &[1], &["AM5"]));
        t.extend(memory_device(16384, 0, 0x22, 4800, 6000, &["DIMM_A2", "G.Skill", "F5-6000J3038F16G", "BANK 1"]));
        // 32 GB needs the extended size field.
        t.extend(memory_device(0x7FFF, 32768, 0x1A, 3200, 0, &["DIMM_B2", "Kingston", "KF432C16"]));
        t.extend(memory_device(0, 0, 0, 0, 0, &["DIMM_A1", "", ""]));
        t.extend(structure(127, &[], &[]));

        let s = parse_smbios(&t);
        assert_eq!((s.bios_vendor.as_str(), s.bios_version.as_str(), s.bios_date.as_str()), ("American Megatrends", "F12", "03/14/2025"));
        assert_eq!((s.system_maker.as_str(), s.system_model.as_str()), ("Maker Inc", "Model X"));
        assert_eq!((s.board_maker.as_str(), s.board_model.as_str(), s.board_version.as_str()), ("BoardCo", "B650 PRO", "1.0"));
        assert_eq!(s.socket, "AM5");
        assert_eq!(s.empty_slots, 1);
        assert_eq!(s.modules.len(), 2);
        let m = &s.modules[0];
        assert_eq!((m.slot.as_str(), m.size, m.speed_mhz, m.kind), ("DIMM_A2", 16 << 30, 6000, "DDR5"));
        assert_eq!((m.manufacturer.as_str(), m.part_number.as_str()), ("G.Skill", "F5-6000J3038F16G"));
        assert_eq!((s.modules[1].size, s.modules[1].speed_mhz, s.modules[1].kind), (32 << 30, 3200, "DDR4"));

        let mem = memory_section(&s);
        assert_eq!(mem.rows[0], ("Installed".to_owned(), "48 GB".to_owned()));
        assert_eq!(mem.rows[1].1, "2 of 3 used");
        assert_eq!(
            mem.rows[2],
            ("DIMM_A2 (BANK 1)".to_owned(), "16 GB DDR5, 6000 MHz, G.Skill F5-6000J3038F16G".to_owned())
        );
        assert_eq!(mem.rows[3].0, "DIMM_B2");
    }

    #[test]
    fn smbios_garbage_does_not_panic() {
        let mut t = Vec::new();
        t.extend(structure(2, &[1, 2, 3], &["BoardCo", "B650 PRO", "1.0"]));
        t.extend(memory_device(8192, 0, 0x1A, 2666, 0, &["A1", "X", "Y"]));
        for len in 0..t.len() {
            parse_smbios(&t[..len]);
        }
        assert_eq!(parse_smbios(&[17, 2, 0, 0]), Smbios::default());
        assert_eq!(parse_smbios(&[9; 64]), Smbios::default());
    }

    #[test]
    fn helpers() {
        assert_eq!(bytes_text(32 << 30), "32 GB");
        assert_eq!(bytes_text(1536 << 20), "1.5 GB");
        assert_eq!(bytes_text(512 << 20), "512 MB");
        assert_eq!(bytes_text(64 << 10), "64 KB");
        // AMD family 25 (0x19): base family 0xF + extended 0x0A; model 0x61.
        assert_eq!(decode_signature(0x00A6_0F12), (25, 0x61, 2));
        // Intel family 6 model 0x9E.
        assert_eq!(decode_signature(0x0009_06EA), (6, 0x9E, 10));
        assert_eq!(real("To Be Filled By O.E.M."), "");
        assert_eq!(real("ASUSTeK"), "ASUSTeK");
    }

    #[test]
    fn live_inventory_has_the_basics() {
        let sections = collect();
        let find = |t: &str| sections.iter().find(|s| s.title == t);
        let cpu = find("CPU").expect("cpu section");
        assert!(cpu.rows.iter().any(|r| r.0 == "Model" && !r.1.is_empty()));
        assert!(cpu.rows.iter().any(|r| r.0 == "Cores" && r.1.contains("logical")));
        assert!(find("Windows").is_some_and(|w| w.rows.iter().any(|r| r.0 == "Build")));
        let text = to_text(&sections);
        assert!(text.starts_with("CPU\n  Model: "));
        eprintln!("{text}");
    }
}
