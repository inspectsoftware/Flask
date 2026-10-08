//! Whole-machine facts.

use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};

#[derive(Debug, Clone, Copy, Default)]
pub struct Memory {
    /// Physical memory visible to Windows, in bytes.
    pub total: u64,
    pub available: u64,
}

impl Memory {
    pub fn used_percent(self) -> f32 {
        if self.total == 0 {
            return 0.0;
        }
        (self.total - self.available.min(self.total)) as f32 / self.total as f32 * 100.0
    }
}

pub fn memory() -> Memory {
    let mut status = MEMORYSTATUSEX { dwLength: size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
    match unsafe { GlobalMemoryStatusEx(&mut status) } {
        Ok(()) => Memory { total: status.ullTotalPhys, available: status.ullAvailPhys },
        Err(_) => Memory::default(),
    }
}

/// Offset between the Windows (1601) and Unix (1970) epochs in 100 ns ticks.
const UNIX_EPOCH_TICKS: i64 = 116_444_736_000_000_000;

pub fn unix_to_filetime(secs: u64) -> i64 {
    secs as i64 * 10_000_000 + UNIX_EPOCH_TICKS
}

/// Formats a FILETIME (100 ns ticks since 1601, UTC) as local
/// `YYYY-MM-DD HH:MM:SS`. Empty for zero or unrepresentable times.
pub fn format_filetime(ticks: i64) -> String {
    if ticks <= 0 {
        return String::new();
    }
    let ft = FILETIME { dwLowDateTime: ticks as u32, dwHighDateTime: (ticks >> 32) as u32 };
    let (mut utc, mut local) = (SYSTEMTIME::default(), SYSTEMTIME::default());
    unsafe {
        if FileTimeToSystemTime(&ft, &mut utc).is_err()
            || SystemTimeToTzSpecificLocalTime(None, &utc, &mut local).is_err()
        {
            return String::new();
        }
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        local.wYear, local.wMonth, local.wDay, local.wHour, local.wMinute, local.wSecond
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filetime_formatting() {
        assert_eq!(format_filetime(0), "");
        assert_eq!(unix_to_filetime(0), UNIX_EPOCH_TICKS);
        // Local time zone varies, so check shape and the year neighbourhood.
        let s = format_filetime(unix_to_filetime(1_700_000_000));
        assert_eq!(s.len(), 19);
        assert!(s.starts_with("2023-11-1"), "{s}");
    }

    #[test]
    fn memory_is_plausible() {
        let m = memory();
        assert!(m.total > 1 << 30 && m.available <= m.total);
        assert!((0.0..=100.0).contains(&m.used_percent()));
        assert_eq!(Memory { total: 100, available: 25 }.used_percent(), 75.0);
        assert_eq!(Memory::default().used_percent(), 0.0);
    }
}
