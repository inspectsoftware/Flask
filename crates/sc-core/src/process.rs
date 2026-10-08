//! Process snapshots. One `NtQuerySystemInformation` call returns every
//! process with its threads; rates come from diffing consecutive snapshots.

use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use std::time::Instant;

use windows::Win32::System::Threading::GetActiveProcessorCount;

use crate::nt;

// x64 layout of SYSTEM_PROCESS_INFORMATION / SYSTEM_THREAD_INFORMATION.
const PROC_SIZE: usize = 256;
const THREAD_SIZE: usize = 80;
const THREAD_STATE_WAITING: u32 = 5;
const WAIT_REASON_SUSPENDED: u32 = 5;

/// One entry as the kernel reports it, before any diffing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RawProcess {
    pub pid: u32,
    pub parent_pid: u32,
    pub create_time: i64,
    /// Kernel + user time in 100 ns units.
    pub cpu_time: u64,
    pub threads: u32,
    pub handles: u32,
    pub session: u32,
    pub base_priority: i32,
    pub working_set: u64,
    pub private_working_set: u64,
    pub private_bytes: u64,
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub suspended: bool,
    /// Byte range of the UTF-16 image name inside the snapshot buffer.
    name: (usize, usize),
}

fn u16_at(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(o..o + 2)?.try_into().ok()?))
}
fn u32_at(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?))
}
fn u64_at(b: &[u8], o: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(o..o + 8)?.try_into().ok()?))
}

/// Walks a `SystemProcessInformation` buffer. `base` is the address the buffer
/// was filled at, used to turn the embedded name pointers back into offsets.
/// Malformed input ends the walk instead of reading out of bounds.
pub fn parse_snapshot(buf: &[u8], base: u64, mut f: impl FnMut(&RawProcess, &[u8])) {
    let mut off = 0usize;
    loop {
        let Some(e) = buf.get(off..).filter(|e| e.len() >= PROC_SIZE) else {
            return;
        };
        let rd32 = |o| u32_at(e, o).unwrap_or(0);
        let rd64 = |o| u64_at(e, o).unwrap_or(0);

        let threads = rd32(4);
        let mut suspended = threads > 0;
        for i in 0..threads as usize {
            let t = PROC_SIZE + i * THREAD_SIZE;
            match (u32_at(e, t + 68), u32_at(e, t + 72)) {
                (Some(THREAD_STATE_WAITING), Some(WAIT_REASON_SUSPENDED)) => {}
                _ => {
                    suspended = false;
                    break;
                }
            }
        }

        let name_len = u16_at(e, 56).unwrap_or(0) as usize;
        let name_ptr = rd64(64);
        let name = name_ptr
            .checked_sub(base)
            .map(|o| o as usize)
            .filter(|o| name_len > 0 && o.checked_add(name_len).is_some_and(|end| end <= buf.len()))
            .map_or((0, 0), |o| (o, name_len));

        let raw = RawProcess {
            pid: rd64(80) as u32,
            parent_pid: rd64(88) as u32,
            create_time: rd64(32) as i64,
            cpu_time: rd64(40).wrapping_add(rd64(48)),
            threads,
            handles: rd32(96),
            session: rd32(100),
            base_priority: rd32(72) as i32,
            working_set: rd64(144),
            private_working_set: rd64(8),
            private_bytes: rd64(200),
            read_bytes: rd64(232),
            write_bytes: rd64(240),
            suspended,
            name,
        };
        f(&raw, &buf[name.0..name.0 + name.1]);

        let next = rd32(0) as usize;
        if next == 0 {
            return;
        }
        off += next;
    }
}

fn decode_name(pid: u32, utf16: &[u8]) -> Arc<str> {
    if utf16.is_empty() {
        return Arc::from(if pid == 0 { "System Idle Process" } else { "Unknown" });
    }
    let units = utf16.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]]));
    char::decode_utf16(units)
        .map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect::<String>()
        .into()
}

/// Share of total CPU capacity used over an interval, in percent.
pub fn cpu_percent(prev_time: u64, cur_time: u64, elapsed_secs: f64, cpus: u32) -> f32 {
    if elapsed_secs <= 0.0 || cpus == 0 {
        return 0.0;
    }
    let used = cur_time.saturating_sub(prev_time) as f64 / 10_000_000.0;
    (used / (elapsed_secs * cpus as f64) * 100.0).clamp(0.0, 100.0) as f32
}

fn rate(prev: u64, cur: u64, elapsed_secs: f64) -> u64 {
    if elapsed_secs <= 0.0 {
        return 0;
    }
    (cur.saturating_sub(prev) as f64 / elapsed_secs) as u64
}

#[derive(Debug, Clone)]
pub struct ProcessRow {
    pub pid: u32,
    pub parent_pid: u32,
    pub create_time: i64,
    pub name: Arc<str>,
    /// Percent of total CPU capacity since the previous snapshot.
    pub cpu: f32,
    pub cpu_time: u64,
    pub working_set: u64,
    /// What Task Manager calls "Memory".
    pub private_working_set: u64,
    /// Committed private memory.
    pub private_bytes: u64,
    /// Bytes per second.
    pub read_rate: u64,
    pub write_rate: u64,
    pub threads: u32,
    pub handles: u32,
    pub session: u32,
    pub base_priority: i32,
    pub suspended: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub rows: Vec<ProcessRow>,
    /// Total CPU use excluding the idle process, in percent.
    pub cpu_total: f32,
    pub threads_total: u32,
    pub handles_total: u32,
}

struct Prev {
    create_time: i64,
    cpu_time: u64,
    read_bytes: u64,
    write_bytes: u64,
    name: Arc<str>,
}

pub struct Sampler {
    buf: Vec<u8>,
    prev: HashMap<u32, Prev>,
    next: HashMap<u32, Prev>,
    last: Option<Instant>,
    cpus: u32,
}

impl Default for Sampler {
    fn default() -> Self {
        Self::new()
    }
}

impl Sampler {
    pub fn new() -> Self {
        const ALL_PROCESSOR_GROUPS: u16 = 0xFFFF;
        Self {
            buf: vec![0; 512 * 1024],
            prev: HashMap::new(),
            next: HashMap::new(),
            last: None,
            cpus: unsafe { GetActiveProcessorCount(ALL_PROCESSOR_GROUPS) }.max(1),
        }
    }

    fn query(&mut self) -> io::Result<()> {
        loop {
            let mut needed = 0u32;
            let status = unsafe {
                nt::NtQuerySystemInformation(
                    nt::SYSTEM_PROCESS_INFORMATION,
                    self.buf.as_mut_ptr().cast(),
                    self.buf.len() as u32,
                    &mut needed,
                )
            };
            if status == nt::STATUS_INFO_LENGTH_MISMATCH {
                // Leave headroom: processes can appear between the two calls.
                self.buf.resize(needed as usize + 64 * 1024, 0);
                continue;
            }
            if status < 0 {
                return Err(io::Error::other(format!("NtQuerySystemInformation failed: {status:#x}")));
            }
            return Ok(());
        }
    }

    /// Takes a snapshot. CPU and I/O rates are zero on the first call and for
    /// processes that were not present in the previous one.
    pub fn sample(&mut self) -> io::Result<Snapshot> {
        self.query()?;
        let now = Instant::now();
        let elapsed = self.last.map_or(0.0, |t| now.duration_since(t).as_secs_f64());
        self.last = Some(now);

        let Self { buf, prev, next, cpus, .. } = self;
        next.clear();
        let mut snap = Snapshot { rows: Vec::with_capacity(prev.len() + 16), ..Default::default() };

        parse_snapshot(buf, buf.as_ptr() as u64, |raw, name_utf16| {
            // A reused PID with a different creation time is a different process.
            let old = prev.remove(&raw.pid).filter(|p| p.create_time == raw.create_time);
            let (cpu, read_rate, write_rate, name) = match old {
                Some(p) => (
                    cpu_percent(p.cpu_time, raw.cpu_time, elapsed, *cpus),
                    rate(p.read_bytes, raw.read_bytes, elapsed),
                    rate(p.write_bytes, raw.write_bytes, elapsed),
                    p.name,
                ),
                None => (0.0, 0, 0, decode_name(raw.pid, name_utf16)),
            };
            if raw.pid != 0 {
                snap.cpu_total += cpu;
            }
            snap.threads_total += raw.threads;
            snap.handles_total += raw.handles;
            next.insert(
                raw.pid,
                Prev {
                    create_time: raw.create_time,
                    cpu_time: raw.cpu_time,
                    read_bytes: raw.read_bytes,
                    write_bytes: raw.write_bytes,
                    name: name.clone(),
                },
            );
            snap.rows.push(ProcessRow {
                pid: raw.pid,
                parent_pid: raw.parent_pid,
                create_time: raw.create_time,
                name,
                cpu,
                cpu_time: raw.cpu_time,
                working_set: raw.working_set,
                private_working_set: raw.private_working_set,
                private_bytes: raw.private_bytes,
                read_rate,
                write_rate,
                threads: raw.threads,
                handles: raw.handles,
                session: raw.session,
                base_priority: raw.base_priority,
                suspended: raw.suspended,
            });
        });
        snap.cpu_total = snap.cpu_total.min(100.0);
        std::mem::swap(prev, next);
        Ok(snap)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: u64 = 0x1000_0000;

    /// Appends one process entry with `threads` given as (state, wait_reason).
    fn push_entry(buf: &mut Vec<u8>, pid: u64, parent: u64, name: &str, threads: &[(u32, u32)], last: bool) {
        let start = buf.len();
        let name16: Vec<u8> = name.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let name_off = PROC_SIZE + threads.len() * THREAD_SIZE;
        let total = (name_off + name16.len() + 7) & !7;
        buf.resize(start + total, 0);
        let e = &mut buf[start..];
        let mut put = |o: usize, bytes: &[u8]| e[o..o + bytes.len()].copy_from_slice(bytes);
        put(0, &(if last { 0u32 } else { total as u32 }).to_le_bytes());
        put(4, &(threads.len() as u32).to_le_bytes());
        put(8, &4096u64.to_le_bytes());
        put(32, &(pid * 10).to_le_bytes());
        put(40, &300u64.to_le_bytes());
        put(48, &700u64.to_le_bytes());
        put(56, &(name16.len() as u16).to_le_bytes());
        put(64, &(BASE + (start + name_off) as u64).to_le_bytes());
        put(72, &8u32.to_le_bytes());
        put(80, &pid.to_le_bytes());
        put(88, &parent.to_le_bytes());
        put(96, &42u32.to_le_bytes());
        put(144, &8192u64.to_le_bytes());
        put(200, &16384u64.to_le_bytes());
        put(232, &111u64.to_le_bytes());
        put(240, &222u64.to_le_bytes());
        for (i, (state, reason)) in threads.iter().enumerate() {
            let t = PROC_SIZE + i * THREAD_SIZE;
            put(t + 68, &state.to_le_bytes());
            put(t + 72, &reason.to_le_bytes());
        }
        put(name_off, &name16);
    }

    fn collect(buf: &[u8]) -> Vec<(RawProcess, String)> {
        let mut out = Vec::new();
        parse_snapshot(buf, BASE, |raw, name| out.push((*raw, decode_name(raw.pid, name).to_string())));
        out
    }

    #[test]
    fn parses_entries_and_names() {
        let mut buf = Vec::new();
        push_entry(&mut buf, 0, 0, "", &[(2, 0)], false);
        push_entry(&mut buf, 1234, 4, "notepad.exe", &[(5, 5), (5, 5)], false);
        push_entry(&mut buf, 5678, 1234, "ünï.exe", &[(5, 5), (2, 0)], true);
        let got = collect(&buf);

        assert_eq!(got.len(), 3);
        assert_eq!(got[0].1, "System Idle Process");
        let (np, name) = &got[1];
        assert_eq!(name, "notepad.exe");
        assert_eq!((np.pid, np.parent_pid, np.threads, np.handles), (1234, 4, 2, 42));
        assert_eq!((np.cpu_time, np.create_time, np.base_priority), (1000, 12340, 8));
        assert_eq!((np.private_working_set, np.working_set, np.private_bytes), (4096, 8192, 16384));
        assert_eq!((np.read_bytes, np.write_bytes), (111, 222));
        assert!(np.suspended);
        assert_eq!(got[2].1, "ünï.exe");
        assert!(!got[2].0.suspended);
    }

    #[test]
    fn truncated_or_garbage_input_does_not_panic() {
        let mut buf = Vec::new();
        push_entry(&mut buf, 1, 0, "a.exe", &[(5, 5)], false);
        push_entry(&mut buf, 2, 1, "b.exe", &[], true);
        for len in 0..buf.len() {
            collect(&buf[..len]);
        }
        // Name pointer outside the buffer yields the fallback, not a read.
        buf[64..72].copy_from_slice(&0xdead_beef_u64.to_le_bytes());
        assert_eq!(collect(&buf)[0].1, "Unknown");
        // Thread count far beyond the buffer.
        buf[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(!collect(&buf)[0].0.suspended);
    }

    #[test]
    fn cpu_percent_math() {
        // One full core for one second on a 4-core machine is 25 %.
        assert_eq!(cpu_percent(0, 10_000_000, 1.0, 4), 25.0);
        assert_eq!(cpu_percent(5_000_000, 10_000_000, 0.5, 1), 100.0);
        assert_eq!(cpu_percent(0, 90_000_000, 1.0, 4), 100.0);
        assert_eq!(cpu_percent(10, 5, 1.0, 4), 0.0);
        assert_eq!(cpu_percent(0, 10, 0.0, 4), 0.0);
    }

    #[test]
    fn live_snapshot_contains_this_process() {
        let mut s = Sampler::new();
        let first = s.sample().unwrap();
        let me = std::process::id();
        let row = first.rows.iter().find(|r| r.pid == me).expect("own pid present");
        assert!(row.threads >= 1 && row.working_set > 0);
        assert!(first.rows.iter().any(|r| r.pid == 4 && &*r.name == "System"));

        // Burn a little CPU so the second sample has a non-zero delta.
        let t = Instant::now();
        while t.elapsed().as_millis() < 150 {
            std::hint::black_box(0u64);
        }
        let second = s.sample().unwrap();
        let row = second.rows.iter().find(|r| r.pid == me).unwrap();
        assert!(row.cpu > 0.0, "cpu = {}", row.cpu);
        assert!(second.cpu_total > 0.0 && second.cpu_total <= 100.0);
    }
}
