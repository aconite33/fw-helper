//! Per-process CPU accounting, for attributing package power.
//!
//! **Nothing here measures a process's power, because nothing on Linux can.** RAPL
//! counts joules for a whole domain and the hardware has no idea which process caused
//! them. What this does is measure CPU time per process, so package power can be
//! divided up in proportion to it — an attribution, and a rough one. It misses GPU,
//! disk and radio power entirely, and under-counts a process that wakes the CPU
//! constantly without using much of it.
//!
//! The AMD board's `core` RAPL domain would have been a better basis than `package`,
//! since it excludes the memory controller and I/O. It is not usable: measured
//! 2026-08-30, loading CPU0 moved it 6.70 W while loading CPU1 moved it 0.014 W, so
//! the driver is reporting AMD's per-core energy MSR for core 0 alone.

use std::collections::HashMap;
use std::fs;

/// One process's cumulative CPU time, in kernel ticks.
pub struct Sample {
    pub name: String,
    pub ticks: u64,
}

/// A process's share of the machine over an interval.
pub struct Share {
    pub name: String,
    pub pid: u32,
    /// Fraction of ONE core, as `top` reports it: 1.0 is one core fully used.
    pub cpu_cores: f64,
    /// Fraction of the whole machine, all cores counted.
    pub of_machine: f64,
}

/// Total jiffies across all CPUs, idle included.
///
/// The denominator is deliberately the whole machine rather than the busy part: at 5%
/// busy, dividing package power among the few running processes would hand them the
/// idle draw as well, which no process caused.
pub fn total_ticks() -> Option<u64> {
    let stat = fs::read_to_string("/proc/stat").ok()?;
    let line = stat.lines().next()?;
    if !line.starts_with("cpu ") {
        return None;
    }
    let mut total = 0u64;
    for field in line.split_whitespace().skip(1) {
        total += field.parse::<u64>().ok()?;
    }
    Some(total)
}

/// Every process's cumulative user+system time, keyed by pid.
pub fn sample() -> HashMap<u32, Sample> {
    let mut out = HashMap::new();
    let Ok(entries) = fs::read_dir("/proc") else {
        return out;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Ok(pid) = name.parse::<u32>() else {
            continue;
        };
        let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        // comm is parenthesised and may itself contain spaces or parens, so the fields
        // can only be split after the LAST closing paren.
        let (Some(open), Some(close)) = (stat.find('('), stat.rfind(')')) else {
            continue;
        };
        if close < open {
            continue;
        }
        let comm = stat[open + 1..close].to_string();
        let rest: Vec<&str> = stat[close + 1..].split_whitespace().collect();
        // rest[0] is field 3 (state), so field N is rest[N - 3]: utime 14, stime 15.
        let (Some(utime), Some(stime)) = (rest.get(11), rest.get(12)) else {
            continue;
        };
        let (Ok(utime), Ok(stime)) = (utime.parse::<u64>(), stime.parse::<u64>()) else {
            continue;
        };
        out.insert(
            pid,
            Sample {
                name: comm,
                ticks: utime + stime,
            },
        );
    }
    out
}

/// Difference two samples into per-process shares, busiest first.
///
/// `total_delta` is the whole machine's tick delta over the same interval. Processes
/// that appeared or exited mid-interval are skipped rather than guessed at: a process
/// with no earlier sample would otherwise appear to have used its entire lifetime's CPU
/// in this one window.
pub fn shares(
    before: &HashMap<u32, Sample>,
    after: &HashMap<u32, Sample>,
    total_delta: u64,
    cores: f64,
) -> Vec<Share> {
    if total_delta == 0 {
        return Vec::new();
    }
    let mut out: Vec<Share> = after
        .iter()
        .filter_map(|(pid, now)| {
            let was = before.get(pid)?;
            let delta = now.ticks.saturating_sub(was.ticks);
            if delta == 0 {
                return None;
            }
            let of_machine = delta as f64 / total_delta as f64;
            Some(Share {
                name: now.name.clone(),
                pid: *pid,
                cpu_cores: of_machine * cores,
                of_machine,
            })
        })
        .collect();
    out.sort_by(|a, b| b.of_machine.total_cmp(&a.of_machine));
    out
}
