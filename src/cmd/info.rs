// System DMA heap information and buffer status display.

use std::fmt::Write as FmtWrite;
use std::path::PathBuf;

use anyhow::Context;

use serde::{Deserialize, Serialize};

use crate::procfs::{self, BuddyInfoEntry, MemInfo, PageTypeInfoEntry, VmStat};
use crate::sysfs;
use crate::{tee_print, tee_println};

// ---------------------------------------------------------------------------
// Data structures
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeapEntry {
    pub name: String,
    pub path: String,
    pub accessible: bool,
    pub permissions: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DebugfsBufEntry {
    pub size: u64,
    pub flags: u32,
    pub mode: u32,
    pub count: i64,
    pub exp_name: String,
    pub ino: u64,
    pub name: String,
    pub attached_devices: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessBufEntry {
    pub pid: u32,
    pub comm: String,
    pub fd: u32,
    pub size: u64,
    pub count: i64,
    pub exp_name: String,
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BufferSummary {
    pub exporter_name: String,
    pub count: usize,
    pub total_size_bytes: u64,
    pub total_refcount: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryContext {
    pub mem_total_kb: u64,
    pub mem_free_kb: u64,
    pub mem_available_kb: u64,
    pub cma_total_kb: Option<u64>,
    pub cma_free_kb: Option<u64>,
    pub buffers_kb: Option<u64>,
    pub cached_kb: Option<u64>,
    pub active_kb: Option<u64>,
    pub inactive_kb: Option<u64>,
    pub shmem_kb: Option<u64>,
    pub slab_kb: Option<u64>,
    pub compact_stall: Option<u64>,
    pub compact_success: Option<u64>,
    pub compact_fail: Option<u64>,
    pub cma_alloc_success: Option<u64>,
    pub cma_alloc_fail: Option<u64>,
    pub nr_free_cma: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmParams {
    pub min_free_kbytes: Option<u64>,
    pub watermark_scale_factor: Option<u64>,
    pub compact_unevictable_allowed: Option<u64>,
    pub swappiness: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZoneEntry {
    pub node: u32,
    pub zone: String,
    pub free_pages: u64,
    pub min_watermark: u64,
    pub low_watermark: u64,
    pub high_watermark: u64,
    pub protection: Vec<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InfoReport {
    pub heaps: Vec<HeapEntry>,
    pub buffer_summary: Vec<BufferSummary>,
    pub total_buffers: usize,
    pub total_buffer_size_bytes: u64,
    pub buffers: Option<Vec<DebugfsBufEntry>>,
    pub process_usage: Option<Vec<ProcessBufEntry>>,
    pub memory: Option<MemoryContext>,
    pub vm_params: Option<VmParams>,
    pub zones: Option<Vec<ZoneEntry>>,
    pub buddyinfo: Option<Vec<BuddyInfoEntry>>,
    pub pagetypeinfo: Option<Vec<PageTypeInfoEntry>>,
}

// ---------------------------------------------------------------------------
// Heap enumeration
// ---------------------------------------------------------------------------

const DMA_HEAP_BASE: &str = "/dev/dma_heap";

pub fn enumerate_heaps(base_path: &str) -> Vec<HeapEntry> {
    let base = std::path::Path::new(base_path);
    let Ok(entries) = std::fs::read_dir(base) else {
        return Vec::new();
    };

    let mut heaps: Vec<HeapEntry> = entries
        .filter_map(std::result::Result::ok)
        .map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            let path = entry.path().to_string_lossy().to_string();
            let metadata = entry.metadata().ok();

            let accessible = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(entry.path())
                .is_ok();

            let permissions = metadata.as_ref().map(format_permissions);

            HeapEntry {
                name,
                path,
                accessible,
                permissions,
            }
        })
        .collect();

    heaps.sort_by(|a, b| a.name.cmp(&b.name));
    heaps
}

#[cfg(unix)]
fn format_permissions(metadata: &std::fs::Metadata) -> String {
    use std::os::unix::fs::MetadataExt;
    let mode = metadata.mode();
    let mut s = String::with_capacity(10);
    // File type
    s.push(if mode & 0o170_000 == 0o020_000 {
        'c'
    } else if mode & 0o170_000 == 0o060_000 {
        'b'
    } else {
        '-'
    });
    // Owner
    s.push(if mode & 0o400 != 0 { 'r' } else { '-' });
    s.push(if mode & 0o200 != 0 { 'w' } else { '-' });
    s.push(if mode & 0o100 != 0 { 'x' } else { '-' });
    // Group
    s.push(if mode & 0o040 != 0 { 'r' } else { '-' });
    s.push(if mode & 0o020 != 0 { 'w' } else { '-' });
    s.push(if mode & 0o010 != 0 { 'x' } else { '-' });
    // Other
    s.push(if mode & 0o004 != 0 { 'r' } else { '-' });
    s.push(if mode & 0o002 != 0 { 'w' } else { '-' });
    s.push(if mode & 0o001 != 0 { 'x' } else { '-' });
    s
}

#[cfg(not(unix))]
fn format_permissions(_metadata: &std::fs::Metadata) -> String {
    "----------".to_string()
}

// ---------------------------------------------------------------------------
// debugfs bufinfo parser
// ---------------------------------------------------------------------------

/// Parse `/sys/kernel/debug/dma_buf/bufinfo` content.
///
/// Format (per buffer entry):
/// ```text
/// <size>\t<flags>\t<mode>\t<count>\t<exp_name>\t<ino>\t<name>
/// \t<attached device>\t<total_nents>\n
/// ```
///
/// Multiple attached devices may appear on subsequent indented lines.
#[allow(clippy::unnecessary_wraps)]
pub fn parse_debugfs_bufinfo(content: &str) -> anyhow::Result<Vec<DebugfsBufEntry>> {
    let mut entries = Vec::new();
    let lines: Vec<&str> = content.lines().collect();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i].trim();
        if line.is_empty() || line.starts_with("Dma-buf") {
            i += 1;
            continue;
        }

        // Try to parse a buffer header line.
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 7 {
            i += 1;
            continue;
        }

        let Ok(size) = parts[0].trim().parse::<u64>() else {
            i += 1;
            continue;
        };
        let flags = u32::from_str_radix(parts[1].trim().trim_start_matches("0x"), 16)
            .or_else(|_| parts[1].trim().parse::<u32>())
            .unwrap_or(0);
        let mode = u32::from_str_radix(parts[2].trim().trim_start_matches("0x"), 16)
            .or_else(|_| parts[2].trim().parse::<u32>())
            .unwrap_or(0);
        let count = parts[3].trim().parse::<i64>().unwrap_or(0);
        let exp_name = parts[4].trim().to_string();
        let ino = parts[5].trim().parse::<u64>().unwrap_or(0);
        let name = parts[6].trim().to_string();

        // Collect attached devices from this line and continuation lines.
        let mut attached_devices = Vec::new();
        // The first attached device may be on the same line at parts[7] if present.
        if parts.len() > 7 {
            let dev = parts[7].trim();
            if !dev.is_empty() && dev != "Total" {
                attached_devices.push(dev.to_string());
            }
        }

        // Continuation lines start with a tab.
        i += 1;
        while i < lines.len() && lines[i].starts_with('\t') {
            let dev_line = lines[i].trim();
            if !dev_line.is_empty() {
                // May contain device name and nents separated by tab.
                if let Some(dev_name) = dev_line.split('\t').next() {
                    let dev_name = dev_name.trim();
                    if !dev_name.is_empty()
                        && dev_name != "Total"
                        && dev_name.parse::<u64>().is_err()
                    {
                        attached_devices.push(dev_name.to_string());
                    }
                }
            }
            i += 1;
        }

        entries.push(DebugfsBufEntry {
            size,
            flags,
            mode,
            count,
            exp_name,
            ino,
            name,
            attached_devices,
        });
    }

    Ok(entries)
}

/// Read and parse `/sys/kernel/debug/dma_buf/bufinfo`.
pub fn read_debugfs_bufinfo() -> anyhow::Result<Vec<DebugfsBufEntry>> {
    let content = std::fs::read_to_string("/sys/kernel/debug/dma_buf/bufinfo")
        .context("failed to read /sys/kernel/debug/dma_buf/bufinfo")?;
    parse_debugfs_bufinfo(&content)
}

// ---------------------------------------------------------------------------
// Process fdinfo scanner (Tier 2)
// ---------------------------------------------------------------------------

/// Parse a single fdinfo file content to extract DMA-BUF fields.
///
/// Looks for lines like:
/// ```text
/// exp_name:    system
/// size:    4096
/// count:    2
/// buf_name:    camera_preview
/// ```
fn parse_fdinfo_for_dmabuf(content: &str) -> Option<(u64, i64, String, Option<String>)> {
    let mut size = None;
    let mut count = None;
    let mut exp_name = None;
    let mut buf_name = None;

    for line in content.lines() {
        if let Some((key, val)) = line.split_once(':') {
            let key = key.trim();
            let val = val.trim();
            match key {
                "exp_name" => exp_name = Some(val.to_string()),
                "size" => size = val.parse().ok(),
                "count" => count = val.parse().ok(),
                "buf_name" => buf_name = Some(val.to_string()),
                _ => {}
            }
        }
    }

    // Only return if this fdinfo belongs to a DMA-BUF (has exp_name).
    let exp = exp_name?;
    Some((size.unwrap_or(0), count.unwrap_or(0), exp, buf_name))
}

/// Scan `/proc/<pid>/fdinfo/<fd>` for DMA-BUF usage across all processes.
#[allow(clippy::unnecessary_wraps)]
pub fn scan_process_dmabufs() -> anyhow::Result<Vec<ProcessBufEntry>> {
    let mut results = Vec::new();
    let Ok(proc_dir) = std::fs::read_dir("/proc") else {
        return Ok(results);
    };

    for proc_entry in proc_dir.filter_map(std::result::Result::ok) {
        let pid_str = proc_entry.file_name();
        let pid_str = pid_str.to_string_lossy();
        let pid: u32 = match pid_str.parse() {
            Ok(p) => p,
            Err(_) => continue,
        };

        let comm = std::fs::read_to_string(format!("/proc/{pid}/comm"))
            .map(|s| s.trim().to_string())
            .unwrap_or_default();

        let fdinfo_path = format!("/proc/{pid}/fdinfo");
        let Ok(fdinfo_dir) = std::fs::read_dir(&fdinfo_path) else {
            continue;
        };

        for fd_entry in fdinfo_dir.filter_map(std::result::Result::ok) {
            let fd_str = fd_entry.file_name();
            let fd: u32 = match fd_str.to_string_lossy().parse() {
                Ok(f) => f,
                Err(_) => continue,
            };

            let Ok(content) = std::fs::read_to_string(fd_entry.path()) else {
                continue;
            };

            if let Some((size, count, exp_name, name)) = parse_fdinfo_for_dmabuf(&content) {
                results.push(ProcessBufEntry {
                    pid,
                    comm: comm.clone(),
                    fd,
                    size,
                    count,
                    exp_name,
                    name,
                });
            }
        }
    }

    results.sort_by(|a, b| b.size.cmp(&a.size).then(a.pid.cmp(&b.pid)));
    Ok(results)
}

// ---------------------------------------------------------------------------
// zoneinfo parser
// ---------------------------------------------------------------------------

/// Parse `/proc/zoneinfo` content.
///
/// Extracts per-zone: free pages, watermarks, protection array.
#[allow(clippy::unnecessary_wraps)]
pub fn parse_zoneinfo(content: &str) -> anyhow::Result<Vec<ZoneEntry>> {
    let mut entries = Vec::new();
    let mut node: Option<u32> = None;
    let mut zone: Option<String> = None;
    let mut free_pages: u64 = 0;
    let mut min_wm: u64 = 0;
    let mut low_wm: u64 = 0;
    let mut high_wm: u64 = 0;
    let mut protection: Vec<u64> = Vec::new();

    for line in content.lines() {
        let trimmed = line.trim();

        // Zone header: "Node 0, zone   Normal"
        if trimmed.starts_with("Node") && trimmed.contains("zone") {
            // Save previous entry if any.
            if let (Some(n), Some(z)) = (node, zone.take()) {
                entries.push(ZoneEntry {
                    node: n,
                    zone: z,
                    free_pages,
                    min_watermark: min_wm,
                    low_watermark: low_wm,
                    high_watermark: high_wm,
                    protection: std::mem::take(&mut protection),
                });
            }

            // Parse "Node <n>, zone <name>"
            let rest = trimmed.strip_prefix("Node").unwrap_or("").trim_start();
            if let Some((node_str, rest)) = rest.split_once(',') {
                node = node_str.trim().parse().ok();
                let z = rest
                    .trim()
                    .strip_prefix("zone")
                    .unwrap_or("")
                    .trim()
                    .to_string();
                zone = Some(z);
            }
            free_pages = 0;
            min_wm = 0;
            low_wm = 0;
            high_wm = 0;
            continue;
        }

        if zone.is_none() {
            continue;
        }

        // Key-value lines within a zone section.
        // zoneinfo uses "pages free <n>" and "min <n>", "low <n>", "high <n>".
        let mut parts = trimmed.split_whitespace();
        if let Some(key) = parts.next() {
            match key {
                "pages" => {
                    // "pages free 3968"
                    if let Some("free") = parts.next() {
                        free_pages = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
                    }
                }
                "nr_free_pages" => {
                    free_pages = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
                }
                "min" => {
                    min_wm = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
                }
                "low" => {
                    low_wm = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
                }
                "high" => {
                    high_wm = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
                }
                "protection:" => {
                    // "(0, 0, 3520, 3520, 3520)"
                    let rest_str = trimmed
                        .strip_prefix("protection:")
                        .unwrap_or("")
                        .trim()
                        .trim_start_matches('(')
                        .trim_end_matches(')');
                    protection = rest_str
                        .split(',')
                        .filter_map(|v| v.trim().parse().ok())
                        .collect();
                }
                _ => {}
            }
        }
    }

    // Save last entry.
    if let (Some(n), Some(z)) = (node, zone.take()) {
        entries.push(ZoneEntry {
            node: n,
            zone: z,
            free_pages,
            min_watermark: min_wm,
            low_watermark: low_wm,
            high_watermark: high_wm,
            protection,
        });
    }

    Ok(entries)
}

/// Read and parse `/proc/zoneinfo`.
pub fn read_zoneinfo() -> anyhow::Result<Vec<ZoneEntry>> {
    let content =
        std::fs::read_to_string("/proc/zoneinfo").context("failed to read /proc/zoneinfo")?;
    parse_zoneinfo(&content)
}

// ---------------------------------------------------------------------------
// VM parameters
// ---------------------------------------------------------------------------

/// Read `/proc/sys/vm/*` parameters.
pub fn read_vm_params() -> VmParams {
    fn read_u64(path: &str) -> Option<u64> {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| s.trim().parse().ok())
    }

    VmParams {
        min_free_kbytes: read_u64("/proc/sys/vm/min_free_kbytes"),
        watermark_scale_factor: read_u64("/proc/sys/vm/watermark_scale_factor"),
        compact_unevictable_allowed: read_u64("/proc/sys/vm/compact_unevictable_allowed"),
        swappiness: read_u64("/proc/sys/vm/swappiness"),
    }
}

// ---------------------------------------------------------------------------
// Aggregation
// ---------------------------------------------------------------------------

/// Aggregate buffer entries by exporter name.
pub fn aggregate_debugfs(entries: &[DebugfsBufEntry]) -> Vec<BufferSummary> {
    use std::collections::BTreeMap;
    let mut map: BTreeMap<&str, (usize, u64, i64)> = BTreeMap::new();
    for e in entries {
        let entry = map.entry(&e.exp_name).or_insert((0, 0, 0));
        entry.0 += 1;
        entry.1 += e.size;
        entry.2 += e.count;
    }
    map.into_iter()
        .map(|(name, (count, total, refs))| BufferSummary {
            exporter_name: name.to_string(),
            count,
            total_size_bytes: total,
            total_refcount: refs,
        })
        .collect()
}

/// Aggregate sysfs buffer entries (fallback when debugfs unavailable).
pub fn aggregate_sysfs(buffers: &[sysfs::DmaBufInfo]) -> Vec<BufferSummary> {
    use std::collections::BTreeMap;
    let mut map: BTreeMap<&str, (usize, u64)> = BTreeMap::new();
    for b in buffers {
        let entry = map.entry(&b.exporter_name).or_insert((0, 0));
        entry.0 += 1;
        entry.1 += b.size;
    }
    map.into_iter()
        .map(|(name, (count, total))| BufferSummary {
            exporter_name: name.to_string(),
            count,
            total_size_bytes: total,
            total_refcount: 0,
        })
        .collect()
}

/// Aggregate per-process DMA-BUF usage for display.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessSummary {
    pub pid: u32,
    pub comm: String,
    pub fd_count: usize,
    pub total_size_bytes: u64,
}

pub fn aggregate_process_usage(entries: &[ProcessBufEntry]) -> Vec<ProcessSummary> {
    use std::collections::BTreeMap;
    let mut map: BTreeMap<u32, (String, usize, u64)> = BTreeMap::new();
    for e in entries {
        let entry = map.entry(e.pid).or_insert_with(|| (e.comm.clone(), 0, 0));
        entry.1 += 1;
        entry.2 += e.size;
    }
    let mut result: Vec<ProcessSummary> = map
        .into_iter()
        .map(|(pid, (comm, fds, size))| ProcessSummary {
            pid,
            comm,
            fd_count: fds,
            total_size_bytes: size,
        })
        .collect();
    result.sort_by(|a, b| b.total_size_bytes.cmp(&a.total_size_bytes));
    result
}

// ---------------------------------------------------------------------------
// Memory context builder
// ---------------------------------------------------------------------------

fn build_memory_context(meminfo: &MemInfo, vmstat: &VmStat) -> MemoryContext {
    MemoryContext {
        mem_total_kb: meminfo.mem_total_kb,
        mem_free_kb: meminfo.mem_free_kb,
        mem_available_kb: meminfo.mem_available_kb,
        cma_total_kb: meminfo.cma_total_kb,
        cma_free_kb: meminfo.cma_free_kb,
        buffers_kb: meminfo.buffers_kb,
        cached_kb: meminfo.cached_kb,
        active_kb: meminfo.active_kb,
        inactive_kb: meminfo.inactive_kb,
        shmem_kb: meminfo.shmem_kb,
        slab_kb: meminfo.slab_kb,
        compact_stall: vmstat.compact_stall,
        compact_success: vmstat.compact_success,
        compact_fail: vmstat.compact_fail,
        cma_alloc_success: vmstat.cma_alloc_success,
        cma_alloc_fail: vmstat.cma_alloc_fail,
        nr_free_cma: vmstat.nr_free_cma,
    }
}

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

fn format_size_kb(kb: u64) -> String {
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

    // --- Memory ---
    if let Some(ref mem) = report.memory {
        out.push_str("[Memory]\n");
        out.push_str("  /proc/meminfo + /proc/vmstat\n\n");

        // Row 1: Total, Free, Available
        writeln!(
            out,
            "  {:<14} {:>10}    {:<14} {:>10}    {:<14} {:>10}",
            "Total",
            format_size_kb(mem.mem_total_kb),
            "Free",
            format_size_kb(mem.mem_free_kb),
            "Available",
            format_size_kb(mem.mem_available_kb),
        )
        .unwrap();

        // Row 2: Buffers, Cached, Shmem
        if mem.buffers_kb.is_some() || mem.cached_kb.is_some() || mem.shmem_kb.is_some() {
            writeln!(
                out,
                "  {:<14} {:>10}    {:<14} {:>10}    {:<14} {:>10}",
                "Buffers",
                mem.buffers_kb.map_or("-".into(), format_size_kb),
                "Cached",
                mem.cached_kb.map_or("-".into(), format_size_kb),
                "Shmem",
                mem.shmem_kb.map_or("-".into(), format_size_kb),
            )
            .unwrap();
        }

        // Row 3: Active, Inactive, Slab
        if mem.active_kb.is_some() || mem.inactive_kb.is_some() || mem.slab_kb.is_some() {
            writeln!(
                out,
                "  {:<14} {:>10}    {:<14} {:>10}    {:<14} {:>10}",
                "Active",
                mem.active_kb.map_or("-".into(), format_size_kb),
                "Inactive",
                mem.inactive_kb.map_or("-".into(), format_size_kb),
                "Slab",
                mem.slab_kb.map_or("-".into(), format_size_kb),
            )
            .unwrap();
        }
        out.push('\n');

        // CMA
        if let (Some(total), Some(free)) = (mem.cma_total_kb, mem.cma_free_kb) {
            let used = total.saturating_sub(free);
            writeln!(
                out,
                "  CMA Total  {:>10}    CMA Free   {:>10}   ({} used)",
                format_size_kb(total),
                format_size_kb(free),
                pct(used, total),
            )
            .unwrap();
            if mem.cma_alloc_success.is_some() || mem.cma_alloc_fail.is_some() {
                writeln!(
                    out,
                    "  CMA Alloc   ok: {}    fail: {}",
                    mem.cma_alloc_success.map_or("-".into(), |v| v.to_string()),
                    mem.cma_alloc_fail.map_or("-".into(), |v| v.to_string()),
                )
                .unwrap();
            }
            out.push('\n');
        }

        // Compaction
        if mem.compact_stall.is_some() {
            let stall = mem.compact_stall.unwrap_or(0);
            let success = mem.compact_success.unwrap_or(0);
            let fail = mem.compact_fail.unwrap_or(0);
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
        }
    }

    out
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Run the info subcommand.
///
/// - `detail`: show individual buffer list and per-process usage.
/// - `heap_filter`: when detail is true, filter buffers by heap/exporter name(s).
/// - `show_procfs`: include extended memory info (zoneinfo, buddyinfo, pagetypeinfo, vm params).
/// - `output`: if Some, write JSON report to file instead of human-readable stdout.
pub fn run(
    detail: bool,
    heap_filter: Option<&[&str]>,
    show_procfs: bool,
    output: Option<&PathBuf>,
) -> anyhow::Result<()> {
    // 1. Enumerate heaps
    let heaps = enumerate_heaps(DMA_HEAP_BASE);

    // 2. Collect buffer information (debugfs first, sysfs fallback)
    let debugfs_entries = read_debugfs_bufinfo().ok();
    let (buffer_summary, total_buffers, total_size) = if let Some(ref entries) = debugfs_entries {
        let summary = aggregate_debugfs(entries);
        let total = entries.len();
        let size: u64 = entries.iter().map(|e| e.size).sum();
        (summary, total, size)
    } else {
        // Fallback to sysfs
        let snap = sysfs::snapshot().unwrap_or(sysfs::SysfsSnapshot {
            buffers: Vec::new(),
        });
        let summary = aggregate_sysfs(&snap.buffers);
        let total = snap.buffers.len();
        let size: u64 = snap.buffers.iter().map(|b| b.size).sum();
        (summary, total, size)
    };

    // 3. Detail: individual buffers + process usage
    let buffers = if detail {
        debugfs_entries.map(|mut entries| {
            if let Some(filter) = heap_filter {
                entries.retain(|e| filter.contains(&e.exp_name.as_str()));
            }
            entries
        })
    } else {
        None
    };

    let process_usage = if detail {
        scan_process_dmabufs().ok().map(|mut entries| {
            if let Some(filter) = heap_filter {
                entries.retain(|e| filter.contains(&e.exp_name.as_str()));
            }
            entries
        })
    } else {
        None
    };

    // 4. Memory context
    let memory = match (procfs::read_meminfo(), procfs::read_vmstat()) {
        (Ok(meminfo), Ok(vmstat)) => Some(build_memory_context(&meminfo, &vmstat)),
        (Ok(meminfo), Err(_)) => Some(build_memory_context(&meminfo, &VmStat::default())),
        _ => None,
    };

    // 5. Extended procfs (--procfs)
    let vm_params = show_procfs.then(read_vm_params);
    let zones = show_procfs.then(|| read_zoneinfo().ok()).flatten();
    let buddyinfo = show_procfs.then(|| procfs::read_buddyinfo().ok()).flatten();
    let pagetypeinfo = show_procfs
        .then(|| procfs::read_pagetypeinfo().ok())
        .flatten();

    let report = InfoReport {
        heaps,
        buffer_summary,
        total_buffers,
        total_buffer_size_bytes: total_size,
        buffers,
        process_usage,
        memory,
        vm_params,
        zones,
        buddyinfo,
        pagetypeinfo,
    };

    // Output
    if let Some(output_path) = output {
        let json = serde_json::to_string_pretty(&report)?;
        std::fs::write(output_path, &json)
            .with_context(|| format!("failed to write info report to {}", output_path.display()))?;
        tee_println!(
            "Info report written to {} ({} heaps, {} buffers, {})",
            output_path.display(),
            report.heaps.len(),
            report.total_buffers,
            format_size(report.total_buffer_size_bytes),
        );
    } else {
        tee_print!("{}", format_human(&report));
    }

    Ok(())
}

/// Compact snapshot for follow mode: buffer count + total size + available memory.
struct FollowSnapshot {
    heap_count: usize,
    total_buffers: usize,
    total_size: u64,
    mem_available_kb: Option<u64>,
}

/// Collect a lightweight snapshot for follow mode.
fn collect_follow_snapshot() -> FollowSnapshot {
    let heaps = enumerate_heaps(DMA_HEAP_BASE);
    let heap_count = heaps.len();

    let (total_buffers, total_size) = if let Ok(entries) = read_debugfs_bufinfo() {
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

    FollowSnapshot {
        heap_count,
        total_buffers,
        total_size,
        mem_available_kb,
    }
}

/// Run info in follow mode: periodically print a compact status line.
pub fn run_follow(interval: std::time::Duration, detail: bool, heaps: &[String]) {
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

        tee_println!(
            "[{}] heaps={} bufs={} size={} mem_avail={}{}",
            ts,
            snap.heap_count,
            snap.total_buffers,
            format_size(snap.total_size),
            mem_str,
            delta,
        );

        if detail && !heaps.is_empty() {
            // Per-heap summary from debugfs
            if let Ok(entries) = read_debugfs_bufinfo() {
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

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
    fn enumerate_heaps_nonexistent_dir() {
        let heaps = enumerate_heaps("/nonexistent/path/dma_heap");
        assert!(heaps.is_empty());
    }

    #[test]
    fn enumerate_heaps_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let heaps = enumerate_heaps(dir.path().to_str().unwrap());
        assert!(heaps.is_empty());
    }

    #[test]
    fn enumerate_heaps_with_entries() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("system"), "").unwrap();
        std::fs::write(dir.path().join("reserved"), "").unwrap();
        let heaps = enumerate_heaps(dir.path().to_str().unwrap());
        assert_eq!(heaps.len(), 2);
        assert_eq!(heaps[0].name, "reserved");
        assert_eq!(heaps[1].name, "system");
    }

    const DEBUGFS_BUFINFO_FIXTURE: &str = "\
Dma-buf Objects:
size\tflags\tmode\tcount\texp_name\tino\tname
65536\t00008002\t00080007\t2\tsystem\t1234\tcamera_preview
\tmali0\t1
\tdisplay0\t1
\tTotal 2 devices attached
4096\t00008002\t00080007\t1\tsystem\t1235\tnpu_input
\tnpu0\t1
\tTotal 1 devices attached
1048576\t00008002\t00080007\t3\treserved\t1236\t<none>
\tmali0\t1
\tTotal 1 devices attached

Total 3 objects, 1118208 bytes
";

    #[test]
    fn parse_debugfs_bufinfo_basic() {
        let entries = parse_debugfs_bufinfo(DEBUGFS_BUFINFO_FIXTURE).unwrap();
        assert_eq!(entries.len(), 3);

        assert_eq!(entries[0].size, 65536);
        assert_eq!(entries[0].exp_name, "system");
        assert_eq!(entries[0].ino, 1234);
        assert_eq!(entries[0].name, "camera_preview");
        assert_eq!(entries[0].count, 2);
        assert!(entries[0].attached_devices.contains(&"mali0".to_string()));
        assert!(
            entries[0]
                .attached_devices
                .contains(&"display0".to_string())
        );

        assert_eq!(entries[1].size, 4096);
        assert_eq!(entries[1].name, "npu_input");

        assert_eq!(entries[2].size, 1_048_576);
        assert_eq!(entries[2].exp_name, "reserved");
    }

    #[test]
    fn parse_debugfs_bufinfo_empty() {
        let entries = parse_debugfs_bufinfo("").unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn aggregate_debugfs_groups_by_exporter() {
        let entries = parse_debugfs_bufinfo(DEBUGFS_BUFINFO_FIXTURE).unwrap();
        let summary = aggregate_debugfs(&entries);
        assert_eq!(summary.len(), 2); // "reserved" and "system"

        let reserved = summary
            .iter()
            .find(|s| s.exporter_name == "reserved")
            .unwrap();
        assert_eq!(reserved.count, 1);
        assert_eq!(reserved.total_size_bytes, 1_048_576);

        let system = summary
            .iter()
            .find(|s| s.exporter_name == "system")
            .unwrap();
        assert_eq!(system.count, 2);
        assert_eq!(system.total_size_bytes, 65536 + 4096);
        assert_eq!(system.total_refcount, 3); // 2 + 1
    }

    #[test]
    fn aggregate_sysfs_groups_by_exporter() {
        let buffers = vec![
            sysfs::DmaBufInfo {
                ino: 1,
                size: 4096,
                exporter_name: "system".into(),
            },
            sysfs::DmaBufInfo {
                ino: 2,
                size: 8192,
                exporter_name: "system".into(),
            },
            sysfs::DmaBufInfo {
                ino: 3,
                size: 1024,
                exporter_name: "other".into(),
            },
        ];
        let summary = aggregate_sysfs(&buffers);
        assert_eq!(summary.len(), 2);
        let sys = summary
            .iter()
            .find(|s| s.exporter_name == "system")
            .unwrap();
        assert_eq!(sys.count, 2);
        assert_eq!(sys.total_size_bytes, 4096 + 8192);
    }

    #[test]
    fn parse_fdinfo_dmabuf() {
        let content = "\
pos:	0
flags:	02000002
mnt_id:	13
ino:	1234
exp_name:	system
size:	65536
count:	2
buf_name:	camera_preview
";
        let result = parse_fdinfo_for_dmabuf(content);
        assert!(result.is_some());
        let (size, count, exp, name) = result.unwrap();
        assert_eq!(size, 65536);
        assert_eq!(count, 2);
        assert_eq!(exp, "system");
        assert_eq!(name, Some("camera_preview".to_string()));
    }

    #[test]
    fn parse_fdinfo_non_dmabuf() {
        let content = "\
pos:	0
flags:	02000002
mnt_id:	13
";
        assert!(parse_fdinfo_for_dmabuf(content).is_none());
    }

    const ZONEINFO_FIXTURE: &str = "\
Node 0, zone      DMA
  pages free     3968
        boost    0
        min      4
        low      8
        high     12
        spanned  4095
        present  3998
        managed  3968
        cma      0
        protection: (0, 1944, 1944, 1944, 1944)
Node 0, zone    Normal
  pages free     497664
        boost    0
        min      4636
        low      10044
        high     15452
        spanned  524288
        present  524288
        managed  498324
        cma      32768
        protection: (0, 0, 0, 0, 0)
";

    #[test]
    fn parse_zoneinfo_basic() {
        let zones = parse_zoneinfo(ZONEINFO_FIXTURE).unwrap();
        assert_eq!(zones.len(), 2);

        assert_eq!(zones[0].node, 0);
        assert_eq!(zones[0].zone, "DMA");
        assert_eq!(zones[0].free_pages, 3968);
        assert_eq!(zones[0].min_watermark, 4);
        assert_eq!(zones[0].low_watermark, 8);
        assert_eq!(zones[0].high_watermark, 12);
        assert_eq!(zones[0].protection, vec![0, 1944, 1944, 1944, 1944]);

        assert_eq!(zones[1].zone, "Normal");
        assert_eq!(zones[1].free_pages, 497_664);
        assert_eq!(zones[1].min_watermark, 4636);
    }

    #[test]
    fn parse_zoneinfo_empty() {
        let zones = parse_zoneinfo("").unwrap();
        assert!(zones.is_empty());
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
        };
        let output = format_human(&report);
        assert!(output.contains("[DMA Heaps]"));
        assert!(output.contains("not available"));
        assert!(output.contains("[DMA Buffers]"));
        assert!(output.contains("[Memory]"));
    }

    #[test]
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
                compact_stall: Some(42),
                compact_success: Some(30),
                compact_fail: Some(12),
                cma_alloc_success: Some(100),
                cma_alloc_fail: Some(2),
                nr_free_cma: Some(32_768),
            }),
            vm_params: None,
            zones: None,
            buddyinfo: None,
            pagetypeinfo: None,
        };
        let output = format_human(&report);
        assert!(output.contains("system"));
        assert!(output.contains("accessible"));
        assert!(output.contains("EXPORTER"));
        assert!(output.contains("TOTAL"));
        assert!(output.contains("CMA Total"));
        assert!(output.contains("Compaction"));
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
        };
        let json = serde_json::to_string(&report).unwrap();
        let deserialized: InfoReport = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.heaps.len(), 1);
        assert_eq!(deserialized.heaps[0].name, "system");
    }

    #[test]
    fn aggregate_process_usage_groups_by_pid() {
        let entries = vec![
            ProcessBufEntry {
                pid: 100,
                comm: "proc_a".into(),
                fd: 3,
                size: 4096,
                count: 1,
                exp_name: "system".into(),
                name: None,
            },
            ProcessBufEntry {
                pid: 100,
                comm: "proc_a".into(),
                fd: 4,
                size: 8192,
                count: 1,
                exp_name: "system".into(),
                name: None,
            },
            ProcessBufEntry {
                pid: 200,
                comm: "proc_b".into(),
                fd: 5,
                size: 65536,
                count: 2,
                exp_name: "reserved".into(),
                name: Some("buf".into()),
            },
        ];
        let summaries = aggregate_process_usage(&entries);
        assert_eq!(summaries.len(), 2);
        // Sorted by total_size descending.
        assert_eq!(summaries[0].pid, 200);
        assert_eq!(summaries[0].total_size_bytes, 65536);
        assert_eq!(summaries[0].fd_count, 1);
        assert_eq!(summaries[1].pid, 100);
        assert_eq!(summaries[1].total_size_bytes, 4096 + 8192);
        assert_eq!(summaries[1].fd_count, 2);
    }
}
