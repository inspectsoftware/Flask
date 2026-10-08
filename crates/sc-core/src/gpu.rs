//! Per-process GPU use from the "GPU Engine" and "GPU Process Memory"
//! performance counters, the same source Task Manager reads.

use std::collections::HashMap;

use crate::pdh::{Counter, Query};

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct GpuUse {
    /// Share of the busiest engine type (3D, video decode, ...) in percent.
    pub percent: f32,
    /// Dedicated video memory in bytes.
    pub memory: u64,
}

#[derive(Debug, Default)]
pub struct GpuSnapshot {
    pub by_pid: HashMap<u32, GpuUse>,
    /// Whole-system use of the busiest engine type, in percent.
    pub total_percent: f32,
    /// Dedicated video memory in use across all adapters, in bytes.
    pub dedicated_used: u64,
}

/// Splits a counter instance like `pid_1234_luid_0x0_0x1_phys_0_eng_0_engtype_3D`
/// into the PID and the engine type (empty for memory counters).
pub fn parse_instance(name: &str) -> Option<(u32, &str)> {
    let rest = name.strip_prefix("pid_")?;
    let end = rest.find('_').unwrap_or(rest.len());
    let pid = rest[..end].parse().ok()?;
    let engine = name.rfind("engtype_").map_or("", |i| &name[i + "engtype_".len()..]);
    Some((pid, engine))
}

/// Folds per-engine readings into per-process and system figures. A process
/// using two engine types at 30 % and 50 % is reported at 50 %, as Task
/// Manager does, since the types are separate hardware queues.
pub fn fold_usage(readings: impl Iterator<Item = (u32, String, f64)>) -> (HashMap<u32, f32>, f32) {
    let mut per: HashMap<(u32, String), f64> = HashMap::new();
    let mut system: HashMap<String, f64> = HashMap::new();
    for (pid, engine, value) in readings {
        *system.entry(engine.clone()).or_default() += value;
        *per.entry((pid, engine)).or_default() += value;
    }
    let mut by_pid: HashMap<u32, f32> = HashMap::new();
    for ((pid, _), value) in per {
        let slot = by_pid.entry(pid).or_default();
        *slot = slot.max(value.min(100.0) as f32);
    }
    let total = system.values().fold(0.0f64, |a, &b| a.max(b)).min(100.0) as f32;
    (by_pid, total)
}

pub struct GpuSampler {
    query: Query,
    usage: Counter,
    memory: Counter,
    adapter_memory: Option<Counter>,
}

impl GpuSampler {
    /// `None` when the counters do not exist (no WDDM 2.0 GPU driver).
    pub fn new() -> Option<Self> {
        let query = Query::new()?;
        let usage = query.add(r"\GPU Engine(*)\Utilization Percentage")?;
        let memory = query.add(r"\GPU Process Memory(*)\Dedicated Usage")?;
        let adapter_memory = query.add(r"\GPU Adapter Memory(*)\Dedicated Usage");
        // Rate counters need a baseline; the first real sample diffs against this.
        query.collect();
        Some(Self { query, usage, memory, adapter_memory })
    }

    pub fn sample(&mut self) -> GpuSnapshot {
        let mut snap = GpuSnapshot::default();
        if !self.query.collect() {
            return snap;
        }
        let mut readings = Vec::new();
        self.query.read_f64(self.usage, |name, value| {
            if let Some((pid, engine)) = parse_instance(name) {
                readings.push((pid, engine.to_owned(), value));
            }
        });
        let (by_pid, total) = fold_usage(readings.into_iter());
        snap.total_percent = total;
        for (pid, percent) in by_pid {
            snap.by_pid.entry(pid).or_default().percent = percent;
        }
        self.query.read_i64(self.memory, |name, value| {
            if let Some((pid, _)) = parse_instance(name) {
                snap.by_pid.entry(pid).or_default().memory += value.max(0) as u64;
            }
        });
        if let Some(counter) = self.adapter_memory {
            self.query.read_i64(counter, |_, value| snap.dedicated_used += value.max(0) as u64);
        }
        snap
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_instance_names() {
        assert_eq!(parse_instance("pid_1234_luid_0x00000000_0x0000D3A1_phys_0_eng_3_engtype_3D"), Some((1234, "3D")));
        assert_eq!(parse_instance("pid_88_luid_0x0_0x1_phys_0_eng_1_engtype_VideoDecode"), Some((88, "VideoDecode")));
        assert_eq!(parse_instance("pid_500_luid_0x00000000_0x0000D3A1_phys_0"), Some((500, "")));
        assert_eq!(parse_instance("luid_0x0"), None);
        assert_eq!(parse_instance("pid_x_luid"), None);
    }

    #[test]
    fn folds_engines_like_task_manager() {
        let readings = vec![
            (1, "3D".to_owned(), 20.0),
            (1, "3D".to_owned(), 10.0),
            (1, "VideoDecode".to_owned(), 50.0),
            (2, "3D".to_owned(), 40.0),
            (3, "Copy".to_owned(), 250.0),
        ];
        let (by_pid, total) = fold_usage(readings.into_iter());
        assert_eq!(by_pid[&1], 50.0);
        assert_eq!(by_pid[&2], 40.0);
        assert_eq!(by_pid[&3], 100.0);
        // 3D sums to 70 across processes, Copy is clamped at 100.
        assert_eq!(total, 100.0);
    }

    #[test]
    fn live_sampling_stays_in_range() {
        let Some(mut gpu) = GpuSampler::new() else { return };
        std::thread::sleep(std::time::Duration::from_millis(200));
        let snap = gpu.sample();
        assert!((0.0..=100.0).contains(&snap.total_percent));
        assert!(snap.by_pid.values().all(|g| (0.0..=100.0).contains(&g.percent)));
    }
}
