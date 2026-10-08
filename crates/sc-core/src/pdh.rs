//! Performance counter queries with wildcard instances.

use windows::Win32::System::Performance::{
    PDH_FMT, PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE, PDH_FMT_LARGE, PDH_HCOUNTER, PDH_HQUERY,
    PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterArrayW, PdhOpenQueryW,
};
use windows::core::PCWSTR;

const PDH_MORE_DATA: u32 = 0x8000_07D2;
const PDH_CSTATUS_VALID_DATA: u32 = 0;
const PDH_CSTATUS_NEW_DATA: u32 = 1;

#[derive(Clone, Copy)]
pub struct Counter(PDH_HCOUNTER);

pub struct Query {
    handle: PDH_HQUERY,
    // u64 storage keeps the counter items aligned.
    buf: Vec<u64>,
}

impl Query {
    pub fn new() -> Option<Self> {
        let mut handle = PDH_HQUERY::default();
        (unsafe { PdhOpenQueryW(None, 0, &mut handle) } == 0).then(|| Self { handle, buf: Vec::new() })
    }

    /// Adds a counter by its English path, e.g. `\PhysicalDisk(*)\% Idle Time`.
    /// `None` when the counter does not exist on this machine.
    pub fn add(&self, path: &str) -> Option<Counter> {
        let wpath: Vec<u16> = path.encode_utf16().chain([0]).collect();
        let mut counter = PDH_HCOUNTER::default();
        (unsafe { PdhAddEnglishCounterW(self.handle, PCWSTR(wpath.as_ptr()), 0, &mut counter) } == 0)
            .then_some(Counter(counter))
    }

    /// Takes a new sample for every counter. Rate counters report the change
    /// since the previous call, so the first call only sets a baseline.
    pub fn collect(&self) -> bool {
        unsafe { PdhCollectQueryData(self.handle) == 0 }
    }

    fn read(&mut self, counter: Counter, format: PDH_FMT, mut f: impl FnMut(&str, &PDH_FMT_COUNTERVALUE_ITEM_W)) {
        unsafe {
            let (mut size, mut count) = (0u32, 0u32);
            if PdhGetFormattedCounterArrayW(counter.0, format, &mut size, &mut count, None) != PDH_MORE_DATA {
                return;
            }
            self.buf.resize((size as usize).div_ceil(8), 0);
            let items = self.buf.as_mut_ptr().cast::<PDH_FMT_COUNTERVALUE_ITEM_W>();
            size = (self.buf.len() * 8) as u32;
            if PdhGetFormattedCounterArrayW(counter.0, format, &mut size, &mut count, Some(items)) != 0 {
                return;
            }
            for item in std::slice::from_raw_parts(items, count as usize) {
                let status = item.FmtValue.CStatus;
                if status != PDH_CSTATUS_VALID_DATA && status != PDH_CSTATUS_NEW_DATA {
                    continue;
                }
                if let Ok(name) = item.szName.to_string() {
                    f(&name, item);
                }
            }
        }
    }

    /// Calls `f(instance, value)` for every instance with valid data.
    pub fn read_f64(&mut self, counter: Counter, mut f: impl FnMut(&str, f64)) {
        self.read(counter, PDH_FMT_DOUBLE, |name, item| f(name, unsafe { item.FmtValue.Anonymous.doubleValue }));
    }

    pub fn read_i64(&mut self, counter: Counter, mut f: impl FnMut(&str, i64)) {
        self.read(counter, PDH_FMT_LARGE, |name, item| f(name, unsafe { item.FmtValue.Anonymous.largeValue }));
    }
}

impl Drop for Query {
    fn drop(&mut self) {
        unsafe {
            PdhCloseQuery(self.handle);
        }
    }
}
