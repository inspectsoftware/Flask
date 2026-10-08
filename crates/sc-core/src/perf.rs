//! Whole-system performance samples: per-core CPU, memory, disks, network.
//! GPU figures come from [`crate::gpu`].

use std::collections::HashMap;
use std::time::Instant;

use windows::Win32::NetworkManagement::IpHelper::{FreeMibTable, GetIfTable2, MIB_IF_TABLE2};
use windows::Win32::System::Power::{CallNtPowerInformation, PROCESSOR_POWER_INFORMATION, ProcessorInformation};
use windows::Win32::System::ProcessStatus::{GetPerformanceInfo, PERFORMANCE_INFORMATION};

use crate::nt;
use crate::pdh::{Counter, Query};

const SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION: u32 = 8;
/// Bytes per core in that information class: five i64 times and a count.
const CORE_INFO_SIZE: usize = 48;
const IF_TYPE_ETHERNET: u32 = 6;
const IF_TYPE_WIFI: u32 = 71;
const IF_OPER_STATUS_UP: i32 = 1;

#[derive(Debug, Clone, Default)]
pub struct MemoryDetail {
    pub total: u64,
    pub available: u64,
    pub committed: u64,
    pub commit_limit: u64,
    pub cached: u64,
    pub paged_pool: u64,
    pub nonpaged_pool: u64,
}

impl MemoryDetail {
    pub fn used(&self) -> u64 {
        self.total.saturating_sub(self.available)
    }
    pub fn used_percent(&self) -> f32 {
        if self.total == 0 { 0.0 } else { self.used() as f32 / self.total as f32 * 100.0 }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Disk {
    /// Instance name as Windows reports it, e.g. "0 C:".
    pub name: String,
    pub read_rate: u64,
    pub write_rate: u64,
    /// Share of time the disk was busy, in percent.
    pub active: f32,
}

#[derive(Debug, Clone, Default)]
pub struct Adapter {
    pub name: String,
    pub description: String,
    /// Bytes per second.
    pub rx_rate: u64,
    pub tx_rate: u64,
    /// Link speed in bits per second.
    pub link_speed: u64,
    pub wifi: bool,
}

#[derive(Debug, Clone, Default)]
pub struct PerfSample {
    pub cpu_total: f32,
    /// Per logical processor, in percent.
    pub cores: Vec<f32>,
    /// Current clock of the fastest core, in MHz.
    pub mhz: u32,
    pub memory: MemoryDetail,
    pub disks: Vec<Disk>,
    pub adapters: Vec<Adapter>,
    pub processes: u32,
    pub threads: u32,
    pub handles: u32,
}

impl PerfSample {
    pub fn disk_active(&self) -> f32 {
        self.disks.iter().map(|d| d.active).fold(0.0, f32::max)
    }
    pub fn disk_rates(&self) -> (u64, u64) {
        self.disks.iter().fold((0, 0), |(r, w), d| (r + d.read_rate, w + d.write_rate))
    }
    pub fn net_rates(&self) -> (u64, u64) {
        self.adapters.iter().fold((0, 0), |(r, t), a| (r + a.rx_rate, t + a.tx_rate))
    }
}

/// Busy share of one core between two readings of (idle, kernel + user) time.
/// Kernel time includes idle time, so busy = total - idle.
pub fn core_usage(prev: (i64, i64), cur: (i64, i64)) -> f32 {
    let idle = (cur.0 - prev.0).max(0) as f64;
    let total = (cur.1 - prev.1).max(0) as f64;
    if total <= 0.0 { 0.0 } else { ((1.0 - idle / total) * 100.0).clamp(0.0, 100.0) as f32 }
}

pub struct PerfSampler {
    core_times: Vec<(i64, i64)>,
    core_buf: Vec<u8>,
    query: Option<Query>,
    disk_read: Option<Counter>,
    disk_write: Option<Counter>,
    disk_idle: Option<Counter>,
    // Interface LUID -> (received, sent) octet counters at the last sample.
    net_prev: HashMap<u64, (u64, u64)>,
    last: Option<Instant>,
}

impl Default for PerfSampler {
    fn default() -> Self {
        Self::new()
    }
}

impl PerfSampler {
    pub fn new() -> Self {
        let query = Query::new();
        let add = |path| query.as_ref().and_then(|q| q.add(path));
        let (disk_read, disk_write, disk_idle) = (
            add(r"\PhysicalDisk(*)\Disk Read Bytes/sec"),
            add(r"\PhysicalDisk(*)\Disk Write Bytes/sec"),
            add(r"\PhysicalDisk(*)\% Idle Time"),
        );
        if let Some(q) = &query {
            q.collect();
        }
        Self {
            core_times: Vec::new(),
            core_buf: vec![0; CORE_INFO_SIZE * 256],
            query,
            disk_read,
            disk_write,
            disk_idle,
            net_prev: HashMap::new(),
            last: None,
        }
    }

    fn cores(&mut self) -> Vec<f32> {
        let mut len = 0u32;
        let status = unsafe {
            nt::NtQuerySystemInformation(
                SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION,
                self.core_buf.as_mut_ptr().cast(),
                self.core_buf.len() as u32,
                &mut len,
            )
        };
        if status < 0 {
            return Vec::new();
        }
        let rd = |b: &[u8], o: usize| i64::from_le_bytes(b[o..o + 8].try_into().unwrap());
        let times: Vec<(i64, i64)> = self.core_buf[..len as usize]
            .chunks_exact(CORE_INFO_SIZE)
            .map(|c| (rd(c, 0), rd(c, 8) + rd(c, 16)))
            .collect();
        let usage = if times.len() == self.core_times.len() {
            times.iter().zip(&self.core_times).map(|(cur, prev)| core_usage(*prev, *cur)).collect()
        } else {
            vec![0.0; times.len()]
        };
        self.core_times = times;
        usage
    }

    fn mhz(cores: usize) -> u32 {
        let mut info = vec![PROCESSOR_POWER_INFORMATION::default(); cores.max(1)];
        let size = (info.len() * size_of::<PROCESSOR_POWER_INFORMATION>()) as u32;
        let status =
            unsafe { CallNtPowerInformation(ProcessorInformation, None, 0, Some(info.as_mut_ptr().cast()), size) };
        if status.is_err() { 0 } else { info.iter().map(|i| i.CurrentMhz).max().unwrap_or(0) }
    }

    fn memory() -> (MemoryDetail, u32, u32, u32) {
        let mut pi = PERFORMANCE_INFORMATION::default();
        let size = size_of::<PERFORMANCE_INFORMATION>() as u32;
        if unsafe { GetPerformanceInfo(&mut pi, size) }.is_err() {
            return Default::default();
        }
        let page = pi.PageSize as u64;
        let detail = MemoryDetail {
            total: pi.PhysicalTotal as u64 * page,
            available: pi.PhysicalAvailable as u64 * page,
            committed: pi.CommitTotal as u64 * page,
            commit_limit: pi.CommitLimit as u64 * page,
            cached: pi.SystemCache as u64 * page,
            paged_pool: pi.KernelPaged as u64 * page,
            nonpaged_pool: pi.KernelNonpaged as u64 * page,
        };
        (detail, pi.ProcessCount, pi.ThreadCount, pi.HandleCount)
    }

    fn disks(&mut self) -> Vec<Disk> {
        let Some(query) = &mut self.query else { return Vec::new() };
        if !query.collect() {
            return Vec::new();
        }
        let mut disks: Vec<Disk> = Vec::new();
        let slot = |disks: &mut Vec<Disk>, name: &str| -> usize {
            match disks.iter().position(|d| d.name == name) {
                Some(i) => i,
                None => {
                    disks.push(Disk { name: name.to_owned(), ..Default::default() });
                    disks.len() - 1
                }
            }
        };
        if let Some(c) = self.disk_read {
            query.read_f64(c, |name, v| {
                if name != "_Total" {
                    let i = slot(&mut disks, name);
                    disks[i].read_rate = v.max(0.0) as u64;
                }
            });
        }
        if let Some(c) = self.disk_write {
            query.read_f64(c, |name, v| {
                if name != "_Total" {
                    let i = slot(&mut disks, name);
                    disks[i].write_rate = v.max(0.0) as u64;
                }
            });
        }
        if let Some(c) = self.disk_idle {
            query.read_f64(c, |name, v| {
                if name != "_Total" {
                    let i = slot(&mut disks, name);
                    disks[i].active = (100.0 - v).clamp(0.0, 100.0) as f32;
                }
            });
        }
        disks.sort_by(|a, b| a.name.cmp(&b.name));
        disks
    }

    fn adapters(&mut self, elapsed: f64) -> Vec<Adapter> {
        let mut out = Vec::new();
        let mut table: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
        unsafe {
            if GetIfTable2(&mut table).is_err() || table.is_null() {
                return out;
            }
            let rows = std::slice::from_raw_parts((*table).Table.as_ptr(), (*table).NumEntries as usize);
            let mut seen = HashMap::new();
            for row in rows {
                let wifi = row.Type == IF_TYPE_WIFI;
                // Bit 0: backed by hardware. Bit 1: a filter layered on another interface.
                let flags = row.InterfaceAndOperStatusFlags._bitfield;
                let physical = flags & 1 != 0 && flags & 2 == 0;
                if !(row.Type == IF_TYPE_ETHERNET || wifi) || !physical || row.OperStatus.0 != IF_OPER_STATUS_UP {
                    continue;
                }
                let luid = row.InterfaceLuid.Value;
                let (rx, tx) = (row.InOctets, row.OutOctets);
                let (rx_rate, tx_rate) = match self.net_prev.get(&luid) {
                    Some(&(prx, ptx)) if elapsed > 0.0 => (
                        (rx.saturating_sub(prx) as f64 / elapsed) as u64,
                        (tx.saturating_sub(ptx) as f64 / elapsed) as u64,
                    ),
                    _ => (0, 0),
                };
                seen.insert(luid, (rx, tx));
                let text = |s: &[u16]| {
                    let end = s.iter().position(|&c| c == 0).unwrap_or(s.len());
                    String::from_utf16_lossy(&s[..end])
                };
                out.push(Adapter {
                    name: text(&row.Alias),
                    description: text(&row.Description),
                    rx_rate,
                    tx_rate,
                    link_speed: row.TransmitLinkSpeed,
                    wifi,
                });
            }
            self.net_prev = seen;
            FreeMibTable(table.cast());
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Takes a sample. Rates and CPU shares are zero on the first call.
    pub fn sample(&mut self) -> PerfSample {
        let now = Instant::now();
        let elapsed = self.last.map_or(0.0, |t| now.duration_since(t).as_secs_f64());
        self.last = Some(now);

        let cores = self.cores();
        let cpu_total = if cores.is_empty() { 0.0 } else { cores.iter().sum::<f32>() / cores.len() as f32 };
        let (memory, processes, threads, handles) = Self::memory();
        PerfSample {
            cpu_total,
            mhz: Self::mhz(cores.len()),
            cores,
            memory,
            disks: self.disks(),
            adapters: self.adapters(elapsed),
            processes,
            threads,
            handles,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_usage_math() {
        // 1 s elapsed (10^7 ticks), 0.25 s idle: 75 % busy.
        assert_eq!(core_usage((0, 0), (2_500_000, 10_000_000)), 75.0);
        assert_eq!(core_usage((5, 5), (5, 5)), 0.0);
        assert_eq!(core_usage((0, 0), (20, 10)), 0.0);
        assert_eq!(core_usage((0, 0), (0, 10)), 100.0);
    }

    #[test]
    fn live_sample_is_plausible() {
        let mut s = PerfSampler::new();
        let first = s.sample();
        assert!(!first.cores.is_empty() && first.cores.iter().all(|&c| c == 0.0));
        std::thread::sleep(std::time::Duration::from_millis(300));
        let second = s.sample();
        assert_eq!(second.cores.len(), first.cores.len());
        assert!(second.cores.iter().all(|c| (0.0..=100.0).contains(c)));
        assert!((0.0..=100.0).contains(&second.cpu_total));
        let m = &second.memory;
        assert!(m.total > 1 << 30 && m.available <= m.total && m.committed <= m.commit_limit);
        assert!(second.processes > 10 && second.threads > second.processes && second.handles > second.threads);
        assert!(second.disks.iter().all(|d| (0.0..=100.0).contains(&d.active)));
        assert!(second.mhz > 100);
    }
}
