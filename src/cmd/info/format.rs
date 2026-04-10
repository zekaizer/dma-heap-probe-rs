// Human-readable formatting: helpers and main report formatter.

use std::fmt::Write as FmtWrite;

use crate::procfs::{self, BuddyInfoEntry, PageTypeInfoEntry};

use super::collect::{DMA_HEAP_BASE, aggregate_process_usage};
use super::types::InfoReport;

// ---------------------------------------------------------------------------
// Formatting helpers
// ---------------------------------------------------------------------------

#[allow(clippy::cast_precision_loss)]
pub fn format_size(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * 1024 * 1024;
    match bytes {
        0 => "0 B".to_string(),
        b if b < KIB => format!("{b} B"),
        b if b < MIB => format!("{:.1} KiB", b as f64 / KIB as f64),
        b if b < GIB => format!("{:.1} MiB", b as f64 / MIB as f64),
        b => format!("{:.1} GiB", b as f64 / GIB as f64),
    }
}

pub(super) fn format_size_kb(kb: u64) -> String {
    format_size(kb * 1024)
}

#[allow(clippy::cast_precision_loss)]
fn pct(used: u64, total: u64) -> String {
    if total == 0 {
        return "N/A".to_string();
    }
    format!("{:.1}%", (used as f64 / total as f64) * 100.0)
}

#[derive(Clone, Copy)]
enum Align {
    Left,
    Right,
}

fn separator(widths: &[usize]) -> String {
    let total_w: usize = widths.iter().sum::<usize>() + (widths.len() - 1) * 2;
    format!("  {}", "-".repeat(total_w))
}

fn format_row(widths: &[usize], aligns: &[Align], values: &[&str]) -> String {
    let mut out = String::from("  ");
    for (i, val) in values.iter().enumerate() {
        let w = widths.get(i).copied().unwrap_or(val.len());
        match aligns.get(i).copied().unwrap_or(Align::Left) {
            Align::Left => write!(out, "{val:<w$}").unwrap(),
            Align::Right => write!(out, "{val:>w$}").unwrap(),
        }
        if i + 1 < values.len() {
            out.push_str("  ");
        }
    }
    out
}

/// Per-zone fragmentation summary derived from buddyinfo.
///
/// Shows total free memory, largest contiguous block, and how many blocks
/// can satisfy common DMA allocation sizes (>=64 KiB at order 4, >=2 MiB at
/// order 9) without compaction.
/// Write a per-zone order summary table: total free, max block, blocks >= 64K / 2M.
///
/// `rows` yields `(node, zone_name, free_counts)` tuples — works for both
/// `BuddyInfoEntry` and filtered `PageTypeInfoEntry` (CMA rows).
fn format_order_summary(out: &mut String, title: &str, rows: &[(u32, &str, &[u64])]) {
    const PAGE_SIZE: u64 = 4096;
    const ORDER_64K: usize = 4;
    const ORDER_2M: usize = 9;

    if rows.is_empty() {
        return;
    }

    writeln!(out, "  {title}").unwrap();
    let zone_w = rows
        .iter()
        .map(|(_, z, _)| z.len())
        .max()
        .unwrap_or(4)
        .max(4);
    writeln!(
        out,
        "  {:>4}  {:<zone_w$}  {:>10}  {:>10}  {:>10}  {:>10}",
        "NODE", "ZONE", "TOTAL FREE", "MAX BLOCK", "BLK>=64K", "BLK>=2M",
    )
    .unwrap();

    for &(node, zone, free_counts) in rows {
        let total_bytes = procfs::total_free_pages(free_counts) * PAGE_SIZE;
        let max_bytes = (1u64 << procfs::max_contiguous_order(free_counts)) * PAGE_SIZE;

        writeln!(
            out,
            "  {:>4}  {:<zone_w$}  {:>10}  {:>10}  {:>10}  {:>10}",
            node,
            zone,
            format_size(total_bytes),
            format_size(max_bytes),
            procfs::blocks_above_order(free_counts, ORDER_64K),
            procfs::blocks_above_order(free_counts, ORDER_2M),
        )
        .unwrap();
    }
    out.push('\n');
}

/// Per-zone fragmentation summary derived from buddyinfo.
fn format_frag_summary(out: &mut String, buddy: &[BuddyInfoEntry]) {
    let rows: Vec<_> = buddy
        .iter()
        .map(|b| (b.node, b.zone.as_str(), b.free_counts.as_slice()))
        .collect();
    format_order_summary(out, "Summary (page size: 4 KiB):", &rows);
}

/// CMA migrate type summary from pagetypeinfo.
fn format_cma_migrate_summary(out: &mut String, pti: &[PageTypeInfoEntry]) {
    let rows: Vec<_> = pti
        .iter()
        .filter(|p| p.page_type == "CMA")
        .map(|p| (p.node, p.zone.as_str(), p.free_counts.as_slice()))
        .collect();
    format_order_summary(out, "CMA migrate type summary (page size: 4 KiB):", &rows);
}

// ---------------------------------------------------------------------------
// Human-readable formatting
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_lines)]
pub fn format_human(report: &InfoReport) -> String {
    let mut out = String::new();

    // --- DMA Heaps ---
    out.push_str("[DMA Heaps]\n");
    writeln!(out, "  {DMA_HEAP_BASE}/").unwrap();
    if report.heaps.is_empty() {
        out.push_str("  (not available - /dev/dma_heap/ not found)\n");
    } else {
        let name_w = report
            .heaps
            .iter()
            .map(|h| h.name.len())
            .max()
            .unwrap_or(4)
            .max(4);
        let widths = [name_w, 10, 17];
        let aligns = [Align::Left, Align::Left, Align::Left];
        out.push_str(&format_row(&widths, &aligns, &["NAME", "PERM", "STATUS"]));
        out.push('\n');
        for h in &report.heaps {
            let perm = h.permissions.as_deref().unwrap_or("----------");
            let status = if h.accessible {
                "accessible"
            } else {
                "permission denied"
            };
            out.push_str(&format_row(&widths, &aligns, &[&h.name, perm, status]));
            out.push('\n');
        }
    }
    out.push('\n');

    // --- Heap Capabilities (--probe) ---
    if let Some(ref caps) = report.heap_caps {
        out.push_str("[Heap Capabilities]\n");
        if caps.is_empty() {
            out.push_str("  (no heaps probed)\n");
        } else {
            let name_w = caps.iter().map(|c| c.name.len()).max().unwrap_or(4).max(4);
            let widths = [name_w, 5, 4, 4, 5, 6, 9, 3, 11];
            let aligns = [Align::Left; 9];
            out.push_str(&format_row(
                &widths,
                &aligns,
                &[
                    "HEAP",
                    "ALLOC",
                    "MMAP",
                    "SYNC",
                    "WRITE",
                    "LLSEEK",
                    "SYNC_FILE",
                    "DUP",
                    "GRANULARITY",
                ],
            ));
            out.push('\n');
            for c in caps {
                let yn = |b: bool| if b { "Y" } else { "-" };
                out.push_str(&format_row(
                    &widths,
                    &aligns,
                    &[
                        &c.name,
                        yn(c.can_alloc),
                        yn(c.can_mmap),
                        yn(c.can_sync),
                        yn(c.can_write),
                        yn(c.can_llseek),
                        yn(c.can_sync_file),
                        yn(c.can_dup),
                        &c.alloc_granularity.to_string(),
                    ],
                ));
                out.push('\n');
            }
        }
        out.push('\n');
    }

    // --- DMA Heap Page Pool (Android-specific) ---
    if let Some(pool_kb) = report.dma_heap_pool_kb {
        out.push_str("[DMA Heap Page Pool]\n");
        out.push_str("  /sys/kernel/dma_heap/total_pools_kb\n\n");
        writeln!(out, "  Pool size: {}", format_size_kb(pool_kb)).unwrap();
        out.push('\n');
    }

    // --- DMA Buffers ---
    out.push_str("[DMA Buffers]\n");
    if report.buffer_summary.is_empty() && report.total_buffers == 0 {
        out.push_str("  (not available - /sys/kernel/debug/dma_buf/bufinfo not accessible)\n");
        out.push_str("  (hint: run with root privileges, or enable CONFIG_DEBUG_FS)\n\n");
        out.push_str("  fallback: /sys/kernel/dmabuf/buffers/\n");
        out.push_str("  (not available - CONFIG_DMABUF_SYSFS_STATS not enabled)\n");
    } else {
        let name_w = report
            .buffer_summary
            .iter()
            .map(|s| s.exporter_name.len())
            .max()
            .unwrap_or(8)
            .max(8);
        let widths = [name_w, 8, 14, 12, 8];
        let aligns = [
            Align::Left,
            Align::Right,
            Align::Right,
            Align::Right,
            Align::Right,
        ];
        out.push_str(&format_row(
            &widths,
            &aligns,
            &["EXPORTER", "COUNT", "TOTAL SIZE", "AVG SIZE", "REFS"],
        ));
        out.push('\n');

        for s in &report.buffer_summary {
            let avg = if s.count > 0 {
                format_size(s.total_size_bytes / s.count as u64)
            } else {
                "0 B".to_string()
            };
            out.push_str(&format_row(
                &widths,
                &aligns,
                &[
                    &s.exporter_name,
                    &s.count.to_string(),
                    &format_size(s.total_size_bytes),
                    &avg,
                    &s.total_refcount.to_string(),
                ],
            ));
            out.push('\n');
        }

        writeln!(out, "{}", separator(&widths)).unwrap();
        let avg_total = if report.total_buffers > 0 {
            format_size(report.total_buffer_size_bytes / report.total_buffers as u64)
        } else {
            "0 B".to_string()
        };
        let total_refs: i64 = report.buffer_summary.iter().map(|s| s.total_refcount).sum();
        out.push_str(&format_row(
            &widths,
            &aligns,
            &[
                "TOTAL",
                &report.total_buffers.to_string(),
                &format_size(report.total_buffer_size_bytes),
                &avg_total,
                &total_refs.to_string(),
            ],
        ));
        out.push('\n');
    }
    out.push('\n');

    // --- Buffer Details (--detail) ---
    if let Some(ref buffers) = report.buffers {
        out.push_str("[Buffer Details]\n");
        out.push_str("  source: /sys/kernel/debug/dma_buf/bufinfo\n\n");
        if buffers.is_empty() {
            out.push_str("  (no buffers)\n");
        } else {
            let widths = [8, 12, 6, 10, 20, 30];
            let aligns = [
                Align::Right,
                Align::Right,
                Align::Right,
                Align::Left,
                Align::Left,
                Align::Left,
            ];
            out.push_str(&format_row(
                &widths,
                &aligns,
                &["INO", "SIZE", "REFS", "FLAGS", "NAME", "DEVICES"],
            ));
            out.push('\n');
            for b in buffers {
                let devs = if b.attached_devices.len() > 3 {
                    let extra = b.attached_devices.len() - 3;
                    format!("{} (+{extra} more)", b.attached_devices[..3].join(", "))
                } else {
                    b.attached_devices.join(", ")
                };
                out.push_str(&format_row(
                    &widths,
                    &aligns,
                    &[
                        &b.ino.to_string(),
                        &format_size(b.size),
                        &b.count.to_string(),
                        &format!("{:08x}", b.flags),
                        &b.name,
                        &devs,
                    ],
                ));
                out.push('\n');
            }
            writeln!(out, "{}", separator(&widths)).unwrap();
            writeln!(
                out,
                "  {} buffers, {} total",
                buffers.len(),
                format_size(buffers.iter().map(|b| b.size).sum::<u64>())
            )
            .unwrap();
        }
        out.push('\n');
    }

    // --- Per-Process Usage (--detail, Tier 2) ---
    if let Some(ref proc_entries) = report.process_usage {
        out.push_str("[Per-Process Usage]\n");
        out.push_str("  source: /proc/*/fdinfo/*\n\n");
        if proc_entries.is_empty() {
            out.push_str("  (not available - requires root to read /proc/*/fdinfo/*)\n");
        } else {
            let summaries = aggregate_process_usage(proc_entries);
            let widths = [8, 20, 6, 14];
            let aligns = [Align::Right, Align::Left, Align::Right, Align::Right];
            out.push_str(&format_row(
                &widths,
                &aligns,
                &["PID", "PROCESS", "FDS", "TOTAL SIZE"],
            ));
            out.push('\n');
            for s in &summaries {
                out.push_str(&format_row(
                    &widths,
                    &aligns,
                    &[
                        &s.pid.to_string(),
                        &s.comm,
                        &s.fd_count.to_string(),
                        &format_size(s.total_size_bytes),
                    ],
                ));
                out.push('\n');
            }
            writeln!(out, "{}", separator(&widths)).unwrap();
            let total_fds: usize = summaries.iter().map(|s| s.fd_count).sum();
            let total_size: u64 = summaries.iter().map(|s| s.total_size_bytes).sum();
            writeln!(
                out,
                "  {} processes, {} FDs, {} total",
                summaries.len(),
                total_fds,
                format_size(total_size)
            )
            .unwrap();
        }
        out.push('\n');
    }

    // --- Pressure (PSI) ---
    if report.psi_memory.is_some() || report.psi_io.is_some() {
        out.push_str("[Pressure]\n");
        out.push_str("  /proc/pressure/{memory,io}\n\n");
        let fmt_psi = |label: &str, line: &procfs::PsiLine| -> String {
            format!(
                "  {label:<8} avg10={:>6.2}%  avg60={:>6.2}%  avg300={:>6.2}%  total={} us",
                line.avg10, line.avg60, line.avg300, line.total,
            )
        };
        if let Some(ref psi) = report.psi_memory {
            writeln!(out, "{}", fmt_psi("mem some", &psi.some)).unwrap();
            writeln!(out, "{}", fmt_psi("mem full", &psi.full)).unwrap();
        }
        if let Some(ref psi) = report.psi_io {
            writeln!(out, "{}", fmt_psi("io  some", &psi.some)).unwrap();
            writeln!(out, "{}", fmt_psi("io  full", &psi.full)).unwrap();
        }
        out.push('\n');
    }

    // --- Memory ---
    if let Some(ref mem) = report.memory {
        let mi = &mem.meminfo;
        let vs = &mem.vmstat;

        out.push_str("[Memory]\n");
        out.push_str("  /proc/meminfo + /proc/vmstat\n\n");

        // Row 1: Total, Free, Available
        writeln!(
            out,
            "  {:<14} {:>10}    {:<14} {:>10}    {:<14} {:>10}",
            "Total",
            format_size_kb(mi.mem_total_kb),
            "Free",
            format_size_kb(mi.mem_free_kb),
            "Available",
            format_size_kb(mi.mem_available_kb),
        )
        .unwrap();

        // Row 2: Buffers, Cached, Shmem
        if mi.buffers_kb.is_some() || mi.cached_kb.is_some() || mi.shmem_kb.is_some() {
            writeln!(
                out,
                "  {:<14} {:>10}    {:<14} {:>10}    {:<14} {:>10}",
                "Buffers",
                mi.buffers_kb.map_or("-".into(), format_size_kb),
                "Cached",
                mi.cached_kb.map_or("-".into(), format_size_kb),
                "Shmem",
                mi.shmem_kb.map_or("-".into(), format_size_kb),
            )
            .unwrap();
        }

        // Row 3: Active, Inactive, Slab
        if mi.active_kb.is_some() || mi.inactive_kb.is_some() || mi.slab_kb.is_some() {
            writeln!(
                out,
                "  {:<14} {:>10}    {:<14} {:>10}    {:<14} {:>10}",
                "Active",
                mi.active_kb.map_or("-".into(), format_size_kb),
                "Inactive",
                mi.inactive_kb.map_or("-".into(), format_size_kb),
                "Slab",
                mi.slab_kb.map_or("-".into(), format_size_kb),
            )
            .unwrap();
        }

        // Row 4: AnonPages, Mapped, Dirty/Writeback
        if mi.anon_pages_kb.is_some() || mi.mapped_kb.is_some() || mi.dirty_kb.is_some() {
            writeln!(
                out,
                "  {:<14} {:>10}    {:<14} {:>10}    {:<14} {:>10}",
                "AnonPages",
                mi.anon_pages_kb.map_or("-".into(), format_size_kb),
                "Mapped",
                mi.mapped_kb.map_or("-".into(), format_size_kb),
                "Dirty",
                mi.dirty_kb.map_or("-".into(), format_size_kb),
            )
            .unwrap();
        }

        // Row 5: Slab breakdown + Writeback
        if mi.s_reclaimable_kb.is_some() || mi.s_unreclaim_kb.is_some() {
            writeln!(
                out,
                "  {:<14} {:>10}    {:<14} {:>10}    {:<14} {:>10}",
                "SReclaimable",
                mi.s_reclaimable_kb.map_or("-".into(), format_size_kb),
                "SUnreclaim",
                mi.s_unreclaim_kb.map_or("-".into(), format_size_kb),
                "Writeback",
                mi.writeback_kb.map_or("-".into(), format_size_kb),
            )
            .unwrap();
        }
        out.push('\n');

        // Swap
        if let (Some(total), Some(free)) = (mi.swap_total_kb, mi.swap_free_kb) {
            let used = total.saturating_sub(free);
            writeln!(
                out,
                "  Swap Total {:>10}    Swap Free  {:>10}   ({} used)",
                format_size_kb(total),
                format_size_kb(free),
                pct(used, total),
            )
            .unwrap();
        }

        // Zswap (kernel 6.8+, CONFIG_ZSWAP)
        if let (Some(pool), Some(orig)) = (mi.zswap_kb, mi.zswapped_kb) {
            let ratio = if pool > 0 {
                #[allow(clippy::cast_precision_loss)]
                let r = orig as f64 / pool as f64;
                format!("{r:.1}x")
            } else {
                "N/A".to_string()
            };
            writeln!(
                out,
                "  Zswap Pool {:>10}    Zswapped   {:>10}   ({ratio} ratio)",
                format_size_kb(pool),
                format_size_kb(orig),
            )
            .unwrap();
        }

        // CMA
        if let (Some(total), Some(free)) = (mi.cma_total_kb, mi.cma_free_kb) {
            let used = total.saturating_sub(free);
            writeln!(
                out,
                "  CMA Total  {:>10}    CMA Free   {:>10}   ({} used)",
                format_size_kb(total),
                format_size_kb(free),
                pct(used, total),
            )
            .unwrap();
            if vs.cma_alloc_success.is_some() || vs.cma_alloc_fail.is_some() {
                writeln!(
                    out,
                    "  CMA Alloc   ok: {}    fail: {}",
                    vs.cma_alloc_success.map_or("-".into(), |v| v.to_string()),
                    vs.cma_alloc_fail.map_or("-".into(), |v| v.to_string()),
                )
                .unwrap();
            }
        }
        out.push('\n');

        // Compaction
        if vs.compact_stall.is_some() {
            let stall = vs.compact_stall.unwrap_or(0);
            let success = vs.compact_success.unwrap_or(0);
            let fail = vs.compact_fail.unwrap_or(0);
            let rate = if stall > 0 {
                pct(success, stall)
            } else {
                "N/A".to_string()
            };
            writeln!(
                out,
                "  Compaction  stall: {stall}  success: {success}  fail: {fail}  ({rate} success rate)"
            )
            .unwrap();

            // Extended compaction detail
            if vs.compact_isolated.is_some() || vs.compactmigrate_scanned.is_some() {
                write!(out, "              ").unwrap();
                if let Some(v) = vs.compact_isolated {
                    write!(out, "isolated: {v}  ").unwrap();
                }
                if let Some(v) = vs.compactmigrate_scanned {
                    write!(out, "migrate_scan: {v}  ").unwrap();
                }
                if let Some(v) = vs.compactfree_scanned {
                    write!(out, "free_scan: {v}  ").unwrap();
                }
                if let Some(v) = vs.kcompactd_wake {
                    write!(out, "kcompactd: {v}").unwrap();
                }
                out.push('\n');
            }
            out.push('\n');
        }

        // Reclaim activity
        let has_reclaim = vs.pgscan_direct.is_some() || vs.pgscan_kswapd.is_some();
        if has_reclaim {
            out.push_str("  Reclaim\n");

            if let (Some(scan), Some(steal)) = (vs.pgscan_direct, vs.pgsteal_direct) {
                let eff = if scan > 0 {
                    pct(steal, scan)
                } else {
                    "N/A".to_string()
                };
                writeln!(
                    out,
                    "    direct    scan: {scan:<10} steal: {steal:<10} ({eff} efficiency)"
                )
                .unwrap();
            }

            if let (Some(scan), Some(steal)) = (vs.pgscan_kswapd, vs.pgsteal_kswapd) {
                let eff = if scan > 0 {
                    pct(steal, scan)
                } else {
                    "N/A".to_string()
                };
                writeln!(
                    out,
                    "    kswapd    scan: {scan:<10} steal: {steal:<10} ({eff} efficiency)"
                )
                .unwrap();
            }

            // Allocation stalls
            if vs.allocstall_normal.is_some() || vs.allocstall_movable.is_some() {
                writeln!(
                    out,
                    "    stalls    normal: {}  movable: {}",
                    vs.allocstall_normal.unwrap_or(0),
                    vs.allocstall_movable.unwrap_or(0),
                )
                .unwrap();
            }

            if let Some(oom) = vs.oom_kill {
                writeln!(out, "    oom_kill: {oom}").unwrap();
            }
            out.push('\n');
        }

        // Migration
        if vs.pgmigrate_success.is_some() || vs.pgmigrate_fail.is_some() {
            let ok = vs.pgmigrate_success.unwrap_or(0);
            let fail = vs.pgmigrate_fail.unwrap_or(0);
            let total = ok + fail;
            let rate = if total > 0 {
                pct(ok, total)
            } else {
                "N/A".to_string()
            };
            writeln!(
                out,
                "  Migration   ok: {ok}  fail: {fail}  ({rate} success rate)"
            )
            .unwrap();
            out.push('\n');
        }
    } else {
        out.push_str("[Memory]\n");
        out.push_str("  (not available on this platform)\n\n");
    }

    // --- VM Parameters (--procfs) ---
    if let Some(ref params) = report.vm_params {
        out.push_str("[VM Parameters]\n");
        out.push_str("  /proc/sys/vm/\n\n");
        let items = [
            ("min_free_kbytes", params.min_free_kbytes),
            ("watermark_scale_factor", params.watermark_scale_factor),
            ("swappiness", params.swappiness),
            ("compact_unevictable", params.compact_unevictable_allowed),
        ];
        for (name, val) in items {
            if let Some(v) = val {
                writeln!(out, "  {name:<26} {v:>8}").unwrap();
            }
        }
        out.push('\n');
    }

    // --- Zones (--procfs) ---
    if let Some(ref zones) = report.zones {
        out.push_str("[Zones]\n");
        out.push_str("  /proc/zoneinfo\n\n");
        if zones.is_empty() {
            out.push_str("  (not available)\n");
        } else {
            let widths = [6, 12, 10, 10, 10, 10, 20];
            let aligns = [
                Align::Right,
                Align::Left,
                Align::Right,
                Align::Right,
                Align::Right,
                Align::Right,
                Align::Left,
            ];
            out.push_str(&format_row(
                &widths,
                &aligns,
                &["NODE", "ZONE", "FREE", "MIN", "LOW", "HIGH", "PROTECTION"],
            ));
            out.push('\n');
            for z in zones {
                let prot = z
                    .protection
                    .iter()
                    .map(std::string::ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ");
                out.push_str(&format_row(
                    &widths,
                    &aligns,
                    &[
                        &z.node.to_string(),
                        &z.zone,
                        &z.free_pages.to_string(),
                        &z.min_watermark.to_string(),
                        &z.low_watermark.to_string(),
                        &z.high_watermark.to_string(),
                        &prot,
                    ],
                ));
                out.push('\n');
            }
        }
        out.push('\n');
    }

    // --- CMA Areas (--procfs) ---
    if let Some(ref areas) = report.cma_areas {
        out.push_str("[CMA Areas]\n");
        out.push_str("  /sys/kernel/mm/cma/\n\n");
        if areas.is_empty() {
            out.push_str("  (not available - CONFIG_CMA_SYSFS not enabled)\n");
        } else {
            let name_w = areas.iter().map(|a| a.name.len()).max().unwrap_or(4).max(4);
            let widths = [name_w, 14, 14, 14, 14, 10];
            let aligns = [
                Align::Left,
                Align::Right,
                Align::Right,
                Align::Right,
                Align::Right,
                Align::Right,
            ];
            out.push_str(&format_row(
                &widths,
                &aligns,
                &[
                    "AREA",
                    "ALLOC_OK",
                    "ALLOC_FAIL",
                    "RELEASED",
                    "ACTIVE",
                    "FAIL%",
                ],
            ));
            out.push('\n');
            for a in areas {
                let active = a
                    .alloc_pages_success
                    .saturating_sub(a.release_pages_success);
                let total_allocs = a.alloc_pages_success + a.alloc_pages_fail;
                let fail_rate = if total_allocs > 0 {
                    pct(a.alloc_pages_fail, total_allocs)
                } else {
                    "N/A".to_string()
                };
                out.push_str(&format_row(
                    &widths,
                    &aligns,
                    &[
                        &a.name,
                        &a.alloc_pages_success.to_string(),
                        &a.alloc_pages_fail.to_string(),
                        &a.release_pages_success.to_string(),
                        &active.to_string(),
                        &fail_rate,
                    ],
                ));
                out.push('\n');
            }
        }
        out.push('\n');
    }

    // --- Fragmentation (--procfs) ---
    if report.buddyinfo.is_some() || report.pagetypeinfo.is_some() {
        out.push_str("[Fragmentation]\n");

        if let Some(ref buddy) = report.buddyinfo {
            out.push_str("  /proc/buddyinfo\n\n");
            let max_orders = buddy.iter().map(|b| b.free_counts.len()).max().unwrap_or(0);
            // Header
            let mut header = String::from("  NODE  ZONE       ");
            for order in 0..max_orders {
                write!(header, " {order:>5}").unwrap();
            }
            out.push_str(&header);
            out.push('\n');
            for b in buddy {
                let mut row = format!("  {:>4}  {:<11}", b.node, b.zone);
                for count in &b.free_counts {
                    write!(row, " {count:>5}").unwrap();
                }
                out.push_str(&row);
                out.push('\n');
            }
            out.push('\n');

            // Fragmentation summary: actionable per-zone metrics
            format_frag_summary(&mut out, buddy);
        }

        if let Some(ref pti) = report.pagetypeinfo {
            out.push_str("  /proc/pagetypeinfo (free pages count per migrate type)\n\n");
            let max_orders = pti.iter().map(|p| p.free_counts.len()).max().unwrap_or(0);
            // Header
            let mut header = String::from("  NODE  ZONE        TYPE          ");
            for order in 0..max_orders {
                write!(header, " {order:>5}").unwrap();
            }
            out.push_str(&header);
            out.push('\n');
            for p in pti {
                let mut row = format!("  {:>4}  {:<11} {:<14}", p.node, p.zone, p.page_type);
                for count in &p.free_counts {
                    write!(row, " {count:>5}").unwrap();
                }
                out.push_str(&row);
                out.push('\n');
            }
            out.push('\n');

            // CMA migrate type summary
            format_cma_migrate_summary(&mut out, pti);
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::procfs::{self, MemInfo, PsiMemory, VmStat};

    use super::super::types::{
        BufferSummary, DebugfsBufEntry, HeapEntry, InfoReport, MemoryContext,
    };

    #[test]
    fn format_size_bytes() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1023), "1023 B");
    }

    #[test]
    fn format_size_kib() {
        assert_eq!(format_size(1024), "1.0 KiB");
        assert_eq!(format_size(1536), "1.5 KiB");
    }

    #[test]
    fn format_size_mib() {
        assert_eq!(format_size(1024 * 1024), "1.0 MiB");
        assert_eq!(format_size(4 * 1024 * 1024), "4.0 MiB");
    }

    #[test]
    fn format_size_gib() {
        assert_eq!(format_size(1024 * 1024 * 1024), "1.0 GiB");
    }

    #[test]
    fn pct_normal() {
        assert_eq!(pct(50, 100), "50.0%");
        assert_eq!(pct(1, 3), "33.3%");
    }

    #[test]
    fn pct_zero_total() {
        assert_eq!(pct(0, 0), "N/A");
    }

    #[test]
    fn format_human_empty_report() {
        let report = InfoReport {
            heaps: Vec::new(),
            buffer_summary: Vec::new(),
            total_buffers: 0,
            total_buffer_size_bytes: 0,
            buffers: None,
            process_usage: None,
            memory: None,
            vm_params: None,
            zones: None,
            buddyinfo: None,
            pagetypeinfo: None,
            heap_caps: None,
            cma_areas: None,
            dma_heap_pool_kb: None,
            psi_memory: None,
            psi_io: None,
        };
        let output = format_human(&report);
        assert!(output.contains("[DMA Heaps]"));
        assert!(output.contains("not available"));
        assert!(output.contains("[DMA Buffers]"));
        assert!(output.contains("[Memory]"));
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn format_human_with_heaps_and_memory() {
        let report = InfoReport {
            heaps: vec![HeapEntry {
                name: "system".into(),
                path: "/dev/dma_heap/system".into(),
                accessible: true,
                permissions: Some("crw-rw----".into()),
            }],
            buffer_summary: vec![BufferSummary {
                exporter_name: "system".into(),
                count: 5,
                total_size_bytes: 5 * 4096,
                total_refcount: 10,
            }],
            total_buffers: 5,
            total_buffer_size_bytes: 5 * 4096,
            buffers: None,
            process_usage: None,
            memory: Some(MemoryContext {
                meminfo: MemInfo {
                    mem_total_kb: 8_052_444,
                    mem_free_kb: 3_145_728,
                    mem_available_kb: 5_242_880,
                    cma_total_kb: Some(262_144),
                    cma_free_kb: Some(131_072),
                    buffers_kb: Some(123_456),
                    cached_kb: Some(1_234_567),
                    active_kb: Some(3_355_443),
                    inactive_kb: Some(1_572_864),
                    shmem_kb: Some(65_536),
                    slab_kb: Some(262_144),
                    s_reclaimable_kb: Some(196_608),
                    s_unreclaim_kb: Some(65_536),
                    swap_total_kb: Some(2_097_152),
                    swap_free_kb: Some(1_835_008),
                    dirty_kb: Some(12_288),
                    writeback_kb: Some(256),
                    anon_pages_kb: Some(2_097_152),
                    mapped_kb: Some(524_288),
                    zswap_kb: Some(16_384),
                    zswapped_kb: Some(65_536),
                },
                vmstat: VmStat {
                    compact_stall: Some(42),
                    compact_success: Some(30),
                    compact_fail: Some(12),
                    compact_isolated: Some(500),
                    compactmigrate_scanned: Some(8000),
                    compactfree_scanned: Some(12_000),
                    kcompactd_wake: Some(15),
                    pgalloc_normal: Some(123_456),
                    pgfree: Some(234_567),
                    pgscan_direct: Some(3000),
                    pgsteal_direct: Some(2800),
                    allocstall_normal: Some(5),
                    allocstall_movable: Some(2),
                    pgscan_kswapd: Some(50_000),
                    pgsteal_kswapd: Some(48_000),
                    pgmigrate_success: Some(7500),
                    pgmigrate_fail: Some(300),
                    cma_alloc_success: Some(100),
                    cma_alloc_fail: Some(2),
                    nr_free_cma: Some(32_768),
                    oom_kill: Some(0),
                },
            }),
            vm_params: None,
            zones: None,
            buddyinfo: None,
            pagetypeinfo: None,
            heap_caps: None,
            cma_areas: None,
            dma_heap_pool_kb: None,
            psi_memory: Some(PsiMemory {
                some: procfs::PsiLine {
                    avg10: 1.23,
                    avg60: 4.56,
                    avg300: 7.89,
                    total: 123_456,
                },
                full: procfs::PsiLine {
                    avg10: 0.10,
                    avg60: 0.20,
                    avg300: 0.30,
                    total: 7890,
                },
            }),
            psi_io: None,
        };
        let output = format_human(&report);
        assert!(output.contains("system"));
        assert!(output.contains("accessible"));
        assert!(output.contains("EXPORTER"));
        assert!(output.contains("TOTAL"));
        assert!(output.contains("CMA Total"));
        assert!(output.contains("Compaction"));
        // New metric sections
        assert!(output.contains("Swap Total"));
        assert!(output.contains("AnonPages"));
        assert!(output.contains("SReclaimable"));
        assert!(output.contains("Reclaim"));
        assert!(output.contains("direct"));
        assert!(output.contains("kswapd"));
        assert!(output.contains("Migration"));
        // PSI
        assert!(output.contains("[Pressure]"));
        assert!(output.contains("mem some"));
        assert!(output.contains("avg10="));
        assert!(output.contains("1.23%"));
        assert!(output.contains("isolated:"));
        assert!(output.contains("migrate_scan:"));
        // Zswap
        assert!(output.contains("Zswap Pool"));
        assert!(output.contains("Zswapped"));
        assert!(output.contains("4.0x ratio"));
    }

    #[test]
    fn format_human_with_detail() {
        let report = InfoReport {
            heaps: Vec::new(),
            buffer_summary: Vec::new(),
            total_buffers: 1,
            total_buffer_size_bytes: 4096,
            buffers: Some(vec![DebugfsBufEntry {
                size: 4096,
                flags: 0x0000_8002,
                mode: 0x0008_0007,
                count: 2,
                exp_name: "system".into(),
                ino: 1234,
                name: "test_buf".into(),
                attached_devices: vec!["mali0".into()],
            }]),
            process_usage: Some(Vec::new()),
            memory: None,
            vm_params: None,
            zones: None,
            buddyinfo: None,
            pagetypeinfo: None,
            heap_caps: None,
            cma_areas: None,
            dma_heap_pool_kb: None,
            psi_memory: None,
            psi_io: None,
        };
        let output = format_human(&report);
        assert!(output.contains("[Buffer Details]"));
        assert!(output.contains("test_buf"));
        assert!(output.contains("mali0"));
        assert!(output.contains("[Per-Process Usage]"));
    }

    #[test]
    fn info_report_serde_roundtrip() {
        let report = InfoReport {
            heaps: vec![HeapEntry {
                name: "system".into(),
                path: "/dev/dma_heap/system".into(),
                accessible: true,
                permissions: Some("crw-rw----".into()),
            }],
            buffer_summary: Vec::new(),
            total_buffers: 0,
            total_buffer_size_bytes: 0,
            buffers: None,
            process_usage: None,
            memory: None,
            vm_params: None,
            zones: None,
            buddyinfo: None,
            pagetypeinfo: None,
            heap_caps: None,
            cma_areas: None,
            dma_heap_pool_kb: None,
            psi_memory: None,
            psi_io: None,
        };
        let json = serde_json::to_string(&report).unwrap();
        let deserialized: InfoReport = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.heaps.len(), 1);
        assert_eq!(deserialized.heaps[0].name, "system");
    }

    #[test]
    fn format_frag_summary_basic() {
        let buddy = vec![BuddyInfoEntry {
            node: 0,
            zone: "Normal".into(),
            // order: 0   1   2   3   4   5   6   7   8   9   10
            free_counts: vec![512, 320, 189, 100, 52, 28, 15, 7, 3, 1, 98],
        }];
        let mut out = String::new();
        format_frag_summary(&mut out, &buddy);

        assert!(out.contains("Summary"));
        assert!(out.contains("TOTAL FREE"));
        assert!(out.contains("MAX BLOCK"));
        assert!(out.contains("BLK>=64K"));
        assert!(out.contains("BLK>=2M"));
        assert!(out.contains("Normal"));

        // order 10 has 98 blocks, so max block = 4 MiB
        assert!(out.contains("4.0 MiB"));
        // blocks >= order 4 = 52 + 28 + 15 + 7 + 3 + 1 + 98 = 204
        assert!(out.contains("204"));
        // blocks >= order 9 = 1 + 98 = 99
        assert!(out.contains("99"));
    }

    #[test]
    fn format_cma_migrate_summary_basic() {
        let pti = vec![
            PageTypeInfoEntry {
                node: 0,
                zone: "Normal".into(),
                page_type: "Movable".into(),
                free_counts: vec![400, 260, 150, 85, 45, 25, 14, 7, 3, 1, 98],
            },
            PageTypeInfoEntry {
                node: 0,
                zone: "Normal".into(),
                page_type: "CMA".into(),
                free_counts: vec![12, 10, 9, 5, 2, 1, 0, 0, 0, 0, 0],
            },
        ];
        let mut out = String::new();
        format_cma_migrate_summary(&mut out, &pti);

        assert!(out.contains("CMA migrate type"));
        assert!(out.contains("Normal"));
        // blocks >= order 4 = 2 + 1 + 0 + 0 + 0 + 0 + 0 = 3
        assert!(out.contains('3'));
    }

    #[test]
    fn format_cma_migrate_summary_no_cma_rows() {
        let pti = vec![PageTypeInfoEntry {
            node: 0,
            zone: "Normal".into(),
            page_type: "Movable".into(),
            free_counts: vec![100, 50],
        }];
        let mut out = String::new();
        format_cma_migrate_summary(&mut out, &pti);
        // No CMA rows → no output
        assert!(out.is_empty());
    }

    #[test]
    fn format_frag_summary_empty_zone() {
        let buddy = vec![BuddyInfoEntry {
            node: 0,
            zone: "DMA".into(),
            free_counts: vec![0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        }];
        let mut out = String::new();
        format_frag_summary(&mut out, &buddy);
        // Max block should be 4 KiB (order 0 default) since all counts are 0
        assert!(out.contains("4.0 KiB"));
    }
}
