//! Background thread that takes process snapshots and hands the latest one
//! to the UI thread. The UI never calls into the system for process data.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, Thread};
use std::time::Duration;

use sc_core::gpu::GpuSampler;
use sc_core::perf::{PerfSample, PerfSampler};
use sc_core::process::{ProcessRow, Sampler};
use sc_core::system::{self, Memory};
use sc_core::{apps, procinfo};
use sc_ui::Win;

use crate::rules::{self, Rules};

/// Details that need a process handle; fetched once per process.
#[derive(Default)]
pub struct Info {
    pub user: String,
    pub path: String,
    pub cmdline: String,
    /// Company from the image's version resource (unverified).
    pub company: Arc<str>,
}

pub struct Proc {
    pub row: ProcessRow,
    pub info: Arc<Info>,
    /// Owns a window the user can see.
    pub is_app: bool,
    /// Percent of the busiest GPU engine type.
    pub gpu: f32,
    pub gpu_memory: u64,
}

pub struct Frame {
    pub procs: Vec<Proc>,
    pub cpu_total: f32,
    pub gpu_total: f32,
    /// Dedicated video memory in use across all adapters.
    pub gpu_memory_used: u64,
    pub memory: Memory,
    pub perf: PerfSample,
    pub threads: u32,
    pub handles: u32,
}

pub struct SamplerHandle {
    // Holds only the newest frame, so a busy UI never builds up a backlog.
    latest: Arc<Mutex<Option<Frame>>>,
    interval_ms: Arc<AtomicU32>,
    thread: Thread,
}

fn fetch_info(pid: u32, companies: &mut HashMap<String, Arc<str>>) -> Info {
    match pid {
        // The idle process and the kernel have no image to open.
        0 | 4 => Info { user: "SYSTEM".into(), ..Default::default() },
        _ => {
            let path = procinfo::image_path(pid).unwrap_or_default();
            // Many processes share an image (svchost), so look each file up once.
            let company = match companies.get(&path) {
                Some(c) => c.clone(),
                None => {
                    let c: Arc<str> = procinfo::company(&path).unwrap_or_default().into();
                    companies.insert(path.clone(), c.clone());
                    c
                }
            };
            Info {
                user: procinfo::user_name(pid).unwrap_or_default(),
                cmdline: procinfo::command_line(pid).unwrap_or_default(),
                path,
                company,
            }
        }
    }
}

impl SamplerHandle {
    /// Starts sampling. Each new frame is announced to window `win_id` with
    /// `Event::User`. Sampling goes on while the window is minimized: the
    /// activity log and `rules` depend on seeing every process.
    pub fn spawn(win_id: isize, rules: Rules) -> Self {
        let latest = Arc::new(Mutex::new(None));
        let interval_ms = Arc::new(AtomicU32::new(1000));
        let (slot, interval) = (latest.clone(), interval_ms.clone());

        let handle = thread::spawn(move || {
            let mut sampler = Sampler::new();
            let mut gpu = GpuSampler::new();
            let mut perf = PerfSampler::new();
            let mut cache: HashMap<u32, (i64, Arc<Info>)> = HashMap::new();
            let mut next_cache = HashMap::new();
            let mut companies = HashMap::new();
            let mut first = true;
            loop {
                if let Ok(snap) = sampler.sample() {
                    let app_pids = apps::app_pids();
                    let gpu_snap = gpu.as_mut().map(GpuSampler::sample).unwrap_or_default();
                    let procs = snap
                        .rows
                        .into_iter()
                        .map(|row| {
                            let info = match cache.remove(&row.pid) {
                                Some((created, info)) if created == row.create_time => info,
                                _ => {
                                    let info = fetch_info(row.pid, &mut companies);
                                    // Newly seen, so this is the moment to put its remembered settings back.
                                    if let Some(rule) = rules.lock().unwrap().get(&rules::key(&info.path)) {
                                        rules::apply(*rule, row.pid);
                                    }
                                    Arc::new(info)
                                }
                            };
                            next_cache.insert(row.pid, (row.create_time, info.clone()));
                            let g = gpu_snap.by_pid.get(&row.pid).copied().unwrap_or_default();
                            Proc { is_app: app_pids.contains(&row.pid), gpu: g.percent, gpu_memory: g.memory, row, info }
                        })
                        .collect();
                    // Whatever is left in `cache` belongs to processes that exited.
                    cache.clear();
                    std::mem::swap(&mut cache, &mut next_cache);
                    *slot.lock().unwrap() = Some(Frame {
                        procs,
                        cpu_total: snap.cpu_total,
                        gpu_total: gpu_snap.total_percent,
                        gpu_memory_used: gpu_snap.dedicated_used,
                        memory: system::memory(),
                        perf: perf.sample(),
                        threads: snap.threads_total,
                        handles: snap.handles_total,
                    });
                    Win::post(win_id, 0);
                }
                // CPU rates need two samples, so the second one follows quickly.
                let wait = if first { 250 } else { interval.load(Ordering::Relaxed) };
                first = false;
                thread::park_timeout(Duration::from_millis(wait as u64));
            }
        });

        Self { latest, interval_ms, thread: handle.thread().clone() }
    }

    pub fn take(&self) -> Option<Frame> {
        self.latest.lock().unwrap().take()
    }

    /// Samples again right away instead of waiting out the interval.
    pub fn refresh_now(&self) {
        self.thread.unpark();
    }

    /// Changes how often frames are taken, starting now.
    pub fn set_interval(&self, ms: u32) {
        self.interval_ms.store(ms, Ordering::Relaxed);
        self.thread.unpark();
    }
}
