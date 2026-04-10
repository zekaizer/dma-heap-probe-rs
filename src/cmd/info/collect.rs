// Data collection: heap enumeration, parsers, readers, and aggregation.

use anyhow::Context;

use crate::sysfs;

use super::types::{
    BufferSummary, DebugfsBufEntry, HeapEntry, ProcessBufEntry, ProcessSummary, VmParams, ZoneEntry,
};

// ---------------------------------------------------------------------------
// Heap enumeration
// ---------------------------------------------------------------------------

pub const DMA_HEAP_BASE: &str = "/dev/dma_heap";

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

#[cfg(test)]
mod tests {
    use super::*;

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
