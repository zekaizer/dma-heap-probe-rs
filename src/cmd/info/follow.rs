// Follow mode: periodic compact status display with memory pressure deltas.

use crate::procfs::{self, VmStat};
use crate::sysfs;
use crate::{tee_print, tee_println};

use super::collect::{DMA_HEAP_BASE, enumerate_heaps, read_debugfs_bufinfo};
use super::format::{format_size, format_size_kb};
use super::types::DebugfsBufEntry;

/// Compact snapshot for follow mode: buffer count + total size + memory state.
struct FollowSnapshot {
    total_buffers: usize,
    total_size: u64,
    mem_available_kb: Option<u64>,
    vmstat: Option<VmStat>,
    psi_mem_avg10: Option<f64>,
    pool_kb: Option<u64>,
    /// Retained debugfs entries for detail mode (avoids double read).
    debugfs_entries: Option<Vec<DebugfsBufEntry>>,
}

/// Selected vmstat counters for follow-mode delta display.
struct VmStatDelta {
    pgscan_direct: u64,
    pgsteal_direct: u64,
    pgscan_kswapd: u64,
    compact_stall: u64,
    allocstall_normal: u64,
    oom_kill: u64,
}

impl VmStatDelta {
    /// Compute delta between two vmstat snapshots. Returns None if either is absent.
    fn compute(prev: Option<&VmStat>, curr: Option<&VmStat>) -> Option<Self> {
        let (p, c) = (prev?, curr?);
        Some(Self {
            pgscan_direct: c
                .pgscan_direct
                .unwrap_or(0)
                .saturating_sub(p.pgscan_direct.unwrap_or(0)),
            pgsteal_direct: c
                .pgsteal_direct
                .unwrap_or(0)
                .saturating_sub(p.pgsteal_direct.unwrap_or(0)),
            pgscan_kswapd: c
                .pgscan_kswapd
                .unwrap_or(0)
                .saturating_sub(p.pgscan_kswapd.unwrap_or(0)),
            compact_stall: c
                .compact_stall
                .unwrap_or(0)
                .saturating_sub(p.compact_stall.unwrap_or(0)),
            allocstall_normal: c
                .allocstall_normal
                .unwrap_or(0)
                .saturating_sub(p.allocstall_normal.unwrap_or(0)),
            oom_kill: c
                .oom_kill
                .unwrap_or(0)
                .saturating_sub(p.oom_kill.unwrap_or(0)),
        })
    }

    /// True if any counter changed since last sample.
    fn has_activity(&self) -> bool {
        self.pgscan_direct > 0
            || self.pgscan_kswapd > 0
            || self.compact_stall > 0
            || self.allocstall_normal > 0
            || self.oom_kill > 0
    }
}

/// Collect a lightweight snapshot for follow mode.
fn collect_follow_snapshot() -> FollowSnapshot {
    let debugfs_entries = read_debugfs_bufinfo().ok();

    let (total_buffers, total_size) = if let Some(ref entries) = debugfs_entries {
        let total = entries.len();
        let size: u64 = entries.iter().map(|e| e.size).sum();
        (total, size)
    } else if let Ok(snap) = sysfs::snapshot() {
        let total = snap.buffers.len();
        let size: u64 = snap.buffers.iter().map(|b| b.size).sum();
        (total, size)
    } else {
        (0, 0)
    };

    let mem_available_kb = procfs::read_meminfo().ok().map(|m| m.mem_available_kb);
    let vmstat = procfs::read_vmstat().ok();
    let psi_mem_avg10 = procfs::read_psi_memory().map(|p| p.some.avg10);
    let pool_kb = sysfs::read_dma_heap_pool_kb();

    FollowSnapshot {
        total_buffers,
        total_size,
        mem_available_kb,
        vmstat,
        psi_mem_avg10,
        pool_kb,
        debugfs_entries,
    }
}

/// Run info in follow mode: periodically print a compact status line.
///
/// When reclaim/compaction activity is detected between samples, a second
/// line shows vmstat counter deltas so memory pressure is immediately visible.
pub fn run_follow(interval: std::time::Duration, detail: bool, heaps: &[String]) {
    // Heap count is static — enumerate once outside the loop.
    let heap_count = enumerate_heaps(DMA_HEAP_BASE).len();
    let mut prev: Option<FollowSnapshot> = None;

    loop {
        let snap = collect_follow_snapshot();
        let ts = chrono_now();
        let mem_str = snap
            .mem_available_kb
            .map_or_else(|| "N/A".to_string(), |kb| format_size(kb * 1024));

        #[allow(clippy::cast_possible_wrap)]
        let delta = if let Some(ref p) = prev {
            let buf_diff = snap.total_buffers as i64 - p.total_buffers as i64;
            let size_diff = snap.total_size as i64 - p.total_size as i64;
            if buf_diff == 0 && size_diff == 0 {
                "  (no change)".to_string()
            } else {
                format!("  ({:+} bufs, {})", buf_diff, format_size_signed(size_diff),)
            }
        } else {
            String::new()
        };

        let psi_str = snap
            .psi_mem_avg10
            .map_or(String::new(), |v| format!(" psi={v:.1}%"));
        let pool_str = snap
            .pool_kb
            .map_or(String::new(), |kb| format!(" pool={}", format_size_kb(kb)));

        tee_println!(
            "[{}] heaps={} bufs={} size={} mem_avail={}{psi_str}{pool_str}{}",
            ts,
            heap_count,
            snap.total_buffers,
            format_size(snap.total_size),
            mem_str,
            delta,
        );

        // Show vmstat delta when reclaim/compaction activity detected
        if let Some(vd) = VmStatDelta::compute(
            prev.as_ref().and_then(|p| p.vmstat.as_ref()),
            snap.vmstat.as_ref(),
        )
        .filter(VmStatDelta::has_activity)
        {
            let mut parts = Vec::new();
            if vd.pgscan_direct > 0 {
                parts.push(format!(
                    "direct_reclaim: +{}/+{}",
                    vd.pgscan_direct, vd.pgsteal_direct
                ));
            }
            if vd.pgscan_kswapd > 0 {
                parts.push(format!("kswapd: +{}", vd.pgscan_kswapd));
            }
            if vd.compact_stall > 0 {
                parts.push(format!("compact: +{}", vd.compact_stall));
            }
            if vd.allocstall_normal > 0 {
                parts.push(format!("alloc_stall: +{}", vd.allocstall_normal));
            }
            if vd.oom_kill > 0 {
                parts.push(format!("oom_kill: +{}", vd.oom_kill));
            }
            tee_println!("           {}", parts.join("  "));
        }

        if detail && !heaps.is_empty() {
            // Reuse debugfs entries from snapshot (avoids double read)
            if let Some(ref entries) = snap.debugfs_entries {
                for heap in heaps {
                    let matching: Vec<_> = entries.iter().filter(|e| e.exp_name == *heap).collect();
                    let count = matching.len();
                    let size: u64 = matching.iter().map(|e| e.size).sum();
                    let buf_diff = if prev.is_some() {
                        format!(" ({count})")
                    } else {
                        String::new()
                    };
                    tee_print!("  {heap}: {count} bufs {}{buf_diff}", format_size(size));
                }
                if !heaps.is_empty() {
                    tee_println!();
                }
            }
        }

        prev = Some(snap);
        crate::log::flush();
        std::thread::sleep(interval);
    }
}

/// Format a signed byte delta with units.
fn format_size_signed(bytes: i64) -> String {
    let sign = if bytes >= 0 { "+" } else { "-" };
    let abs = bytes.unsigned_abs();
    format!("{sign}{}", format_size(abs))
}

/// Simple timestamp (HH:MM:SS) without external crate.
fn chrono_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // UTC HH:MM:SS
    let h = (secs % 86400) / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    format!("{h:02}:{m:02}:{s:02}")
}
