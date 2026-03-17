// Parsers for /proc/buddyinfo, /proc/pagetypeinfo, /proc/meminfo, /proc/vmstat.

use anyhow::Context;
use serde::{Deserialize, Serialize};

/// One line from `/proc/buddyinfo`.
///
/// Example: `Node 0, zone   Normal   512  320  189  100  52  28  15  7  3  1  98`
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BuddyInfoEntry {
    pub node: u32,
    pub zone: String,
    /// Free chunk counts per order (index 0 = order 0, ..., typically up to order 10).
    pub free_counts: Vec<u64>,
}

/// One line from the "Free pages count per migrate type" section of `/proc/pagetypeinfo`.
///
/// Example: `Node    0, zone   Normal, type    Unmovable  100  50  30  10  5  2  1  0  0  0  0`
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PageTypeInfoEntry {
    pub node: u32,
    pub zone: String,
    /// Migration type: `Unmovable`, `Movable`, `Reclaimable`, `CMA`, etc.
    pub page_type: String,
    /// Free page counts per order.
    pub free_counts: Vec<u64>,
}

/// Parse `/proc/buddyinfo` content.
///
/// Each line has the format: `Node <n>, zone <name> <count0> <count1> ... <count10>`
pub fn parse_buddyinfo(content: &str) -> anyhow::Result<Vec<BuddyInfoEntry>> {
    let mut entries = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // "Node 0, zone   Normal   512  320  ..."
        let rest = line
            .strip_prefix("Node")
            .ok_or_else(|| anyhow::anyhow!("buddyinfo: unexpected line format: {line}"))?
            .trim_start();

        // Split at comma: "0" and "zone   Normal   512  320  ..."
        let (node_str, rest) = rest
            .split_once(',')
            .ok_or_else(|| anyhow::anyhow!("buddyinfo: missing comma: {line}"))?;
        let node: u32 = node_str.trim().parse()?;

        let rest = rest
            .trim_start()
            .strip_prefix("zone")
            .ok_or_else(|| anyhow::anyhow!("buddyinfo: missing 'zone' keyword: {line}"))?;

        // Remaining tokens: zone name followed by numbers.
        // Zone name is the first non-numeric token(s), but in practice it's a single word.
        let tokens: Vec<&str> = rest.split_whitespace().collect();
        if tokens.is_empty() {
            anyhow::bail!("buddyinfo: no tokens after zone: {line}");
        }

        // Find where numeric values start.
        let num_start = tokens
            .iter()
            .position(|t| t.parse::<u64>().is_ok())
            .ok_or_else(|| anyhow::anyhow!("buddyinfo: no numeric values: {line}"))?;

        let zone = tokens[..num_start].join(" ");
        let free_counts: Vec<u64> = tokens[num_start..]
            .iter()
            .map(|t| t.parse())
            .collect::<Result<_, _>>()?;

        entries.push(BuddyInfoEntry {
            node,
            zone,
            free_counts,
        });
    }
    Ok(entries)
}

/// Parse the "Free pages count per migrate type" section of `/proc/pagetypeinfo`.
///
/// Skips header lines. Parses lines matching:
/// `Node <n>, zone <name>, type <type> <count0> ... <count10>`
pub fn parse_pagetypeinfo(content: &str) -> anyhow::Result<Vec<PageTypeInfoEntry>> {
    let mut entries = Vec::new();
    let mut in_free_pages_section = false;

    for line in content.lines() {
        let trimmed = line.trim();

        // Detect section header.
        if trimmed.starts_with("Free pages count per migrate type") {
            in_free_pages_section = true;
            continue;
        }

        // Stop at next section or end.
        if in_free_pages_section && trimmed.starts_with("Number of blocks type") {
            break;
        }

        if !in_free_pages_section || trimmed.is_empty() {
            continue;
        }

        // Skip column header line (starts with "Node" but has no comma-separated values).
        if !trimmed.starts_with("Node") || !trimmed.contains(',') {
            continue;
        }

        // "Node    0, zone      DMA, type    Unmovable      1      0 ..."
        let rest = trimmed
            .strip_prefix("Node")
            .ok_or_else(|| anyhow::anyhow!("pagetypeinfo: unexpected format: {trimmed}"))?
            .trim_start();

        // Split at first comma: node number.
        let (node_str, rest) = rest
            .split_once(',')
            .ok_or_else(|| anyhow::anyhow!("pagetypeinfo: missing first comma: {trimmed}"))?;
        let node: u32 = node_str.trim().parse()?;

        // "zone      DMA, type    Unmovable      1      0 ..."
        let rest = rest
            .trim_start()
            .strip_prefix("zone")
            .ok_or_else(|| anyhow::anyhow!("pagetypeinfo: missing 'zone': {trimmed}"))?;

        // Split at second comma: zone name.
        let (zone_str, rest) = rest
            .split_once(',')
            .ok_or_else(|| anyhow::anyhow!("pagetypeinfo: missing second comma: {trimmed}"))?;
        let zone = zone_str.trim().to_string();

        // "type    Unmovable      1      0 ..."
        let rest = rest
            .trim_start()
            .strip_prefix("type")
            .ok_or_else(|| anyhow::anyhow!("pagetypeinfo: missing 'type': {trimmed}"))?;

        let tokens: Vec<&str> = rest.split_whitespace().collect();
        if tokens.is_empty() {
            anyhow::bail!("pagetypeinfo: no tokens after type: {trimmed}");
        }

        // Find where numeric values start.
        let num_start = tokens
            .iter()
            .position(|t| t.parse::<u64>().is_ok())
            .ok_or_else(|| anyhow::anyhow!("pagetypeinfo: no numeric values: {trimmed}"))?;

        let page_type = tokens[..num_start].join(" ");
        let free_counts: Vec<u64> = tokens[num_start..]
            .iter()
            .map(|t| t.parse())
            .collect::<Result<_, _>>()?;

        entries.push(PageTypeInfoEntry {
            node,
            zone,
            page_type,
            free_counts,
        });
    }
    Ok(entries)
}

/// Selected fields from `/proc/meminfo`. All values in kB.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[allow(clippy::struct_field_names)]
pub struct MemInfo {
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
    pub s_reclaimable_kb: Option<u64>,
    pub s_unreclaim_kb: Option<u64>,
    pub swap_total_kb: Option<u64>,
    pub swap_free_kb: Option<u64>,
    pub dirty_kb: Option<u64>,
    pub writeback_kb: Option<u64>,
    pub anon_pages_kb: Option<u64>,
    pub mapped_kb: Option<u64>,
    /// Compressed pool size in RAM (kernel 6.8+, requires `CONFIG_ZSWAP`).
    pub zswap_kb: Option<u64>,
    /// Uncompressed size of data stored in zswap.
    pub zswapped_kb: Option<u64>,
}

/// Selected fields from `/proc/vmstat`.
///
/// Counters are grouped by subsystem. All counters available since kernel 4.8+;
/// `kcompactd_wake` since 4.6, `oom_kill` since 4.13.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct VmStat {
    // -- Compaction --
    pub compact_stall: Option<u64>,
    pub compact_success: Option<u64>,
    pub compact_fail: Option<u64>,
    pub compact_isolated: Option<u64>,
    pub compactmigrate_scanned: Option<u64>,
    pub compactfree_scanned: Option<u64>,
    pub kcompactd_wake: Option<u64>,

    // -- Page allocation --
    pub pgalloc_normal: Option<u64>,
    pub pgfree: Option<u64>,

    // -- Direct reclaim (allocation-blocking path) --
    pub pgscan_direct: Option<u64>,
    pub pgsteal_direct: Option<u64>,
    pub allocstall_normal: Option<u64>,
    pub allocstall_movable: Option<u64>,

    // -- Background reclaim (kswapd) --
    pub pgscan_kswapd: Option<u64>,
    pub pgsteal_kswapd: Option<u64>,

    // -- Page migration --
    pub pgmigrate_success: Option<u64>,
    pub pgmigrate_fail: Option<u64>,

    // -- CMA --
    pub cma_alloc_success: Option<u64>,
    pub cma_alloc_fail: Option<u64>,
    pub nr_free_cma: Option<u64>,

    // -- OOM --
    pub oom_kill: Option<u64>,
}

/// Combined procfs snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcfsSnapshot {
    pub buddyinfo: Vec<BuddyInfoEntry>,
    pub pagetypeinfo: Vec<PageTypeInfoEntry>,
    pub meminfo: MemInfo,
    pub vmstat: VmStat,
}

/// Parse `/proc/meminfo` content. Extracts selected fields.
///
/// Required: `MemTotal`, `MemFree`, `MemAvailable`.
/// Optional: `CmaTotal`, `CmaFree`.
pub fn parse_meminfo(content: &str) -> anyhow::Result<MemInfo> {
    let mut mem_total_kb = None;
    let mut mem_free_kb = None;
    let mut mem_available_kb = None;
    let mut cma_total_kb = None;
    let mut cma_free_kb = None;
    let mut buffers_kb = None;
    let mut cached_kb = None;
    let mut active_kb = None;
    let mut inactive_kb = None;
    let mut shmem_kb = None;
    let mut slab_kb = None;
    let mut s_reclaimable_kb = None;
    let mut s_unreclaim_kb = None;
    let mut swap_total_kb = None;
    let mut swap_free_kb = None;
    let mut dirty_kb = None;
    let mut writeback_kb = None;
    let mut anon_pages_kb = None;
    let mut mapped_kb = None;
    let mut zswap_kb = None;
    let mut zswapped_kb = None;

    for line in content.lines() {
        let line = line.trim();
        if let Some((key, val)) = line.split_once(':') {
            let val_kb = val
                .trim()
                .strip_suffix("kB")
                .unwrap_or(val.trim())
                .trim()
                .parse::<u64>();

            match key {
                "MemTotal" => mem_total_kb = val_kb.ok(),
                "MemFree" => mem_free_kb = val_kb.ok(),
                "MemAvailable" => mem_available_kb = val_kb.ok(),
                "CmaTotal" => cma_total_kb = val_kb.ok(),
                "CmaFree" => cma_free_kb = val_kb.ok(),
                "Buffers" => buffers_kb = val_kb.ok(),
                "Cached" => cached_kb = val_kb.ok(),
                "Active" => active_kb = val_kb.ok(),
                "Inactive" => inactive_kb = val_kb.ok(),
                "Shmem" => shmem_kb = val_kb.ok(),
                "Slab" => slab_kb = val_kb.ok(),
                "SReclaimable" => s_reclaimable_kb = val_kb.ok(),
                "SUnreclaim" => s_unreclaim_kb = val_kb.ok(),
                "SwapTotal" => swap_total_kb = val_kb.ok(),
                "SwapFree" => swap_free_kb = val_kb.ok(),
                "Dirty" => dirty_kb = val_kb.ok(),
                "Writeback" => writeback_kb = val_kb.ok(),
                "AnonPages" => anon_pages_kb = val_kb.ok(),
                "Mapped" => mapped_kb = val_kb.ok(),
                "Zswap" => zswap_kb = val_kb.ok(),
                "Zswapped" => zswapped_kb = val_kb.ok(),
                _ => {}
            }
        }
    }

    Ok(MemInfo {
        mem_total_kb: mem_total_kb.ok_or_else(|| anyhow::anyhow!("meminfo: MemTotal not found"))?,
        mem_free_kb: mem_free_kb.ok_or_else(|| anyhow::anyhow!("meminfo: MemFree not found"))?,
        mem_available_kb: mem_available_kb
            .ok_or_else(|| anyhow::anyhow!("meminfo: MemAvailable not found"))?,
        cma_total_kb,
        cma_free_kb,
        buffers_kb,
        cached_kb,
        active_kb,
        inactive_kb,
        shmem_kb,
        slab_kb,
        s_reclaimable_kb,
        s_unreclaim_kb,
        swap_total_kb,
        swap_free_kb,
        dirty_kb,
        writeback_kb,
        anon_pages_kb,
        mapped_kb,
        zswap_kb,
        zswapped_kb,
    })
}

/// Parse `/proc/vmstat` content. Extracts selected compaction, reclaim, migration,
/// CMA and OOM fields relevant to DMA heap allocation diagnostics.
pub fn parse_vmstat(content: &str) -> VmStat {
    let mut vs = VmStat::default();

    for line in content.lines() {
        let mut parts = line.split_whitespace();
        if let (Some(key), Some(val_str)) = (parts.next(), parts.next()) {
            let val = val_str.parse::<u64>().ok();
            match key {
                // Compaction
                "compact_stall" => vs.compact_stall = val,
                "compact_success" => vs.compact_success = val,
                "compact_fail" => vs.compact_fail = val,
                "compact_isolated" => vs.compact_isolated = val,
                "compactmigrate_scanned" => vs.compactmigrate_scanned = val,
                "compactfree_scanned" => vs.compactfree_scanned = val,
                "kcompactd_wake" => vs.kcompactd_wake = val,
                // Page allocation
                "pgalloc_normal" => vs.pgalloc_normal = val,
                "pgfree" => vs.pgfree = val,
                // Direct reclaim
                "pgscan_direct" => vs.pgscan_direct = val,
                "pgsteal_direct" => vs.pgsteal_direct = val,
                "allocstall_normal" => vs.allocstall_normal = val,
                "allocstall_movable" => vs.allocstall_movable = val,
                // Background reclaim
                "pgscan_kswapd" => vs.pgscan_kswapd = val,
                "pgsteal_kswapd" => vs.pgsteal_kswapd = val,
                // Migration
                "pgmigrate_success" => vs.pgmigrate_success = val,
                "pgmigrate_fail" => vs.pgmigrate_fail = val,
                // CMA
                "cma_alloc_success" => vs.cma_alloc_success = val,
                "cma_alloc_fail" => vs.cma_alloc_fail = val,
                "nr_free_cma" => vs.nr_free_cma = val,
                // OOM
                "oom_kill" => vs.oom_kill = val,
                _ => {}
            }
        }
    }

    vs
}

// ---------------------------------------------------------------------------
// /proc/pressure/ (PSI — Pressure Stall Information)
// ---------------------------------------------------------------------------

/// One PSI line (either "some" or "full").
///
/// Format: `some avg10=X.XX avg60=X.XX avg300=X.XX total=NNNN`
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PsiLine {
    pub avg10: f64,
    pub avg60: f64,
    pub avg300: f64,
    /// Cumulative stall time in microseconds.
    pub total: u64,
}

/// Memory pressure from `/proc/pressure/memory`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PsiMemory {
    /// At least one task stalled on memory.
    pub some: PsiLine,
    /// All non-idle tasks stalled on memory.
    pub full: PsiLine,
}

/// I/O pressure from `/proc/pressure/io`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PsiIo {
    pub some: PsiLine,
    pub full: PsiLine,
}

/// Parse a single PSI line into key-value pairs.
fn parse_psi_line(line: &str) -> Option<PsiLine> {
    let mut avg10 = 0.0;
    let mut avg60 = 0.0;
    let mut avg300 = 0.0;
    let mut total = 0u64;

    // Skip the first token ("some" or "full")
    for token in line.split_whitespace().skip(1) {
        if let Some((key, val)) = token.split_once('=') {
            match key {
                "avg10" => avg10 = val.parse().ok()?,
                "avg60" => avg60 = val.parse().ok()?,
                "avg300" => avg300 = val.parse().ok()?,
                "total" => total = val.parse().ok()?,
                _ => {}
            }
        }
    }

    Some(PsiLine {
        avg10,
        avg60,
        avg300,
        total,
    })
}

/// Parse `/proc/pressure/memory` content.
pub fn parse_psi_memory(content: &str) -> PsiMemory {
    let mut psi = PsiMemory::default();
    for line in content.lines() {
        if let Some(parsed) = line
            .starts_with("some ")
            .then(|| parse_psi_line(line))
            .flatten()
        {
            psi.some = parsed;
        } else if let Some(parsed) = line
            .starts_with("full ")
            .then(|| parse_psi_line(line))
            .flatten()
        {
            psi.full = parsed;
        }
    }
    psi
}

/// Parse `/proc/pressure/io` content.
pub fn parse_psi_io(content: &str) -> PsiIo {
    let mut psi = PsiIo::default();
    for line in content.lines() {
        if let Some(parsed) = line
            .starts_with("some ")
            .then(|| parse_psi_line(line))
            .flatten()
        {
            psi.some = parsed;
        } else if let Some(parsed) = line
            .starts_with("full ")
            .then(|| parse_psi_line(line))
            .flatten()
        {
            psi.full = parsed;
        }
    }
    psi
}

/// Read and parse `/proc/pressure/memory`.
pub fn read_psi_memory() -> Option<PsiMemory> {
    let content = std::fs::read_to_string("/proc/pressure/memory").ok()?;
    Some(parse_psi_memory(&content))
}

/// Read and parse `/proc/pressure/io`.
pub fn read_psi_io() -> Option<PsiIo> {
    let content = std::fs::read_to_string("/proc/pressure/io").ok()?;
    Some(parse_psi_io(&content))
}

/// Read and parse `/proc/meminfo`.
pub fn read_meminfo() -> anyhow::Result<MemInfo> {
    let content =
        std::fs::read_to_string("/proc/meminfo").context("failed to read /proc/meminfo")?;
    parse_meminfo(&content)
}

/// Read and parse `/proc/vmstat`.
pub fn read_vmstat() -> anyhow::Result<VmStat> {
    let content = std::fs::read_to_string("/proc/vmstat").context("failed to read /proc/vmstat")?;
    Ok(parse_vmstat(&content))
}

/// Collect a full procfs snapshot (buddyinfo + pagetypeinfo + meminfo + vmstat).
pub fn snapshot() -> anyhow::Result<ProcfsSnapshot> {
    Ok(ProcfsSnapshot {
        buddyinfo: read_buddyinfo()?,
        pagetypeinfo: read_pagetypeinfo()?,
        meminfo: read_meminfo()?,
        vmstat: read_vmstat()?,
    })
}

/// Read and parse `/proc/buddyinfo`.
pub fn read_buddyinfo() -> anyhow::Result<Vec<BuddyInfoEntry>> {
    let content =
        std::fs::read_to_string("/proc/buddyinfo").context("failed to read /proc/buddyinfo")?;
    parse_buddyinfo(&content)
}

/// Read and parse `/proc/pagetypeinfo`.
pub fn read_pagetypeinfo() -> anyhow::Result<Vec<PageTypeInfoEntry>> {
    let content = std::fs::read_to_string("/proc/pagetypeinfo")
        .context("failed to read /proc/pagetypeinfo")?;
    parse_pagetypeinfo(&content)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BUDDYINFO_FIXTURE: &str = "\
Node 0, zone      DMA      1      1      0      0      0      0      0      0      1      1      3
Node 0, zone   Normal    512    320    189    100     52     28     15      7      3      1     98
";

    const PAGETYPEINFO_FIXTURE: &str = "\
Page block order: 9
Pages per block:  512

Free pages count per migrate type at order       0      1      2      3      4      5      6      7      8      9     10
Node    0, zone      DMA, type    Unmovable      1      0      0      0      0      0      0      0      0      0      0
Node    0, zone      DMA, type      Movable      0      1      0      0      0      0      0      0      1      1      3
Node    0, zone      DMA, type  Reclaimable      0      0      0      0      0      0      0      0      0      0      0
Node    0, zone   Normal, type    Unmovable    100     50     30     10      5      2      1      0      0      0      0
Node    0, zone   Normal, type      Movable    400    260    150     85     45     25     14      7      3      1     98
Node    0, zone   Normal, type          CMA     12     10      9      5      2      1      0      0      0      0      0

Number of blocks type     Unmovable      Movable  Reclaimable   HighAtomic          CMA      Isolate
Node 0, zone      DMA            1            7            0            0            0            0
";

    #[test]
    fn parse_buddyinfo_normal() {
        let entries = parse_buddyinfo(BUDDYINFO_FIXTURE).unwrap();
        assert_eq!(entries.len(), 2);

        assert_eq!(entries[0].node, 0);
        assert_eq!(entries[0].zone, "DMA");
        assert_eq!(
            entries[0].free_counts,
            vec![1, 1, 0, 0, 0, 0, 0, 0, 1, 1, 3]
        );

        assert_eq!(entries[1].zone, "Normal");
        assert_eq!(
            entries[1].free_counts,
            vec![512, 320, 189, 100, 52, 28, 15, 7, 3, 1, 98]
        );
    }

    #[test]
    fn parse_buddyinfo_empty() {
        let entries = parse_buddyinfo("").unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn parse_buddyinfo_single_zone() {
        let input = "Node 1, zone   HighMem   10  20  30\n";
        let entries = parse_buddyinfo(input).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].node, 1);
        assert_eq!(entries[0].zone, "HighMem");
        assert_eq!(entries[0].free_counts, vec![10, 20, 30]);
    }

    #[test]
    fn parse_buddyinfo_invalid_format() {
        assert!(parse_buddyinfo("garbage data").is_err());
    }

    #[test]
    fn parse_pagetypeinfo_normal() {
        let entries = parse_pagetypeinfo(PAGETYPEINFO_FIXTURE).unwrap();
        assert_eq!(entries.len(), 6);

        assert_eq!(entries[0].node, 0);
        assert_eq!(entries[0].zone, "DMA");
        assert_eq!(entries[0].page_type, "Unmovable");
        assert_eq!(
            entries[0].free_counts,
            vec![1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
        );

        assert_eq!(entries[4].zone, "Normal");
        assert_eq!(entries[4].page_type, "Movable");
        assert_eq!(
            entries[4].free_counts,
            vec![400, 260, 150, 85, 45, 25, 14, 7, 3, 1, 98]
        );

        assert_eq!(entries[5].page_type, "CMA");
        assert_eq!(
            entries[5].free_counts,
            vec![12, 10, 9, 5, 2, 1, 0, 0, 0, 0, 0]
        );
    }

    #[test]
    fn parse_pagetypeinfo_empty() {
        let entries = parse_pagetypeinfo("").unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn parse_pagetypeinfo_stops_at_next_section() {
        // Should only parse the "Free pages count" section, not "Number of blocks type".
        let entries = parse_pagetypeinfo(PAGETYPEINFO_FIXTURE).unwrap();
        // All entries should be from the free pages section (6 entries, not 8+).
        assert_eq!(entries.len(), 6);
    }

    const MEMINFO_FIXTURE: &str = "\
MemTotal:        8052444 kB
MemFree:         3145728 kB
MemAvailable:    5242880 kB
Buffers:          123456 kB
Cached:          1234567 kB
SwapCached:            0 kB
Active:          3355443 kB
Inactive:        1572864 kB
Dirty:             12288 kB
Writeback:           256 kB
AnonPages:       2097152 kB
Mapped:           524288 kB
Shmem:             65536 kB
Slab:             262144 kB
SReclaimable:     196608 kB
SUnreclaim:        65536 kB
SwapTotal:       2097152 kB
SwapFree:        1835008 kB
Zswap:             16384 kB
Zswapped:          65536 kB
CmaTotal:         262144 kB
CmaFree:          131072 kB
";

    #[test]
    fn parse_meminfo_all_fields() {
        let info = parse_meminfo(MEMINFO_FIXTURE).unwrap();
        assert_eq!(info.mem_total_kb, 8_052_444);
        assert_eq!(info.mem_free_kb, 3_145_728);
        assert_eq!(info.mem_available_kb, 5_242_880);
        assert_eq!(info.cma_total_kb, Some(262_144));
        assert_eq!(info.cma_free_kb, Some(131_072));
        assert_eq!(info.buffers_kb, Some(123_456));
        assert_eq!(info.cached_kb, Some(1_234_567));
        assert_eq!(info.active_kb, Some(3_355_443));
        assert_eq!(info.inactive_kb, Some(1_572_864));
        assert_eq!(info.shmem_kb, Some(65_536));
        assert_eq!(info.slab_kb, Some(262_144));
        assert_eq!(info.s_reclaimable_kb, Some(196_608));
        assert_eq!(info.s_unreclaim_kb, Some(65_536));
        assert_eq!(info.swap_total_kb, Some(2_097_152));
        assert_eq!(info.swap_free_kb, Some(1_835_008));
        assert_eq!(info.dirty_kb, Some(12_288));
        assert_eq!(info.writeback_kb, Some(256));
        assert_eq!(info.anon_pages_kb, Some(2_097_152));
        assert_eq!(info.mapped_kb, Some(524_288));
        assert_eq!(info.zswap_kb, Some(16_384));
        assert_eq!(info.zswapped_kb, Some(65_536));
    }

    #[test]
    fn parse_meminfo_no_cma() {
        let input = "\
MemTotal:        8052444 kB
MemFree:         3145728 kB
MemAvailable:    5242880 kB
";
        let info = parse_meminfo(input).unwrap();
        assert_eq!(info.cma_total_kb, None);
        assert_eq!(info.cma_free_kb, None);
    }

    #[test]
    fn parse_meminfo_missing_required() {
        let input = "MemFree:   1234 kB\n";
        assert!(parse_meminfo(input).is_err());
    }

    const VMSTAT_FIXTURE: &str = "\
nr_free_pages 789012
compact_stall 42
compact_success 30
compact_fail 12
compact_isolated 500
compactmigrate_scanned 8000
compactfree_scanned 12000
kcompactd_wake 15
pgalloc_normal 123456
pgfree 234567
pgscan_direct 3000
pgsteal_direct 2800
allocstall_normal 5
allocstall_movable 2
pgscan_kswapd 50000
pgsteal_kswapd 48000
pgmigrate_success 7500
pgmigrate_fail 300
nr_dirty 100
cma_alloc_success 100
cma_alloc_fail 2
nr_free_cma 32768
oom_kill 0
";

    #[test]
    fn parse_vmstat_selected_keys() {
        let stat = parse_vmstat(VMSTAT_FIXTURE);
        // Compaction
        assert_eq!(stat.compact_stall, Some(42));
        assert_eq!(stat.compact_success, Some(30));
        assert_eq!(stat.compact_fail, Some(12));
        assert_eq!(stat.compact_isolated, Some(500));
        assert_eq!(stat.compactmigrate_scanned, Some(8000));
        assert_eq!(stat.compactfree_scanned, Some(12_000));
        assert_eq!(stat.kcompactd_wake, Some(15));
        // Page allocation
        assert_eq!(stat.pgalloc_normal, Some(123_456));
        assert_eq!(stat.pgfree, Some(234_567));
        // Direct reclaim
        assert_eq!(stat.pgscan_direct, Some(3000));
        assert_eq!(stat.pgsteal_direct, Some(2800));
        assert_eq!(stat.allocstall_normal, Some(5));
        assert_eq!(stat.allocstall_movable, Some(2));
        // Background reclaim
        assert_eq!(stat.pgscan_kswapd, Some(50_000));
        assert_eq!(stat.pgsteal_kswapd, Some(48_000));
        // Migration
        assert_eq!(stat.pgmigrate_success, Some(7500));
        assert_eq!(stat.pgmigrate_fail, Some(300));
        // CMA
        assert_eq!(stat.cma_alloc_success, Some(100));
        assert_eq!(stat.cma_alloc_fail, Some(2));
        assert_eq!(stat.nr_free_cma, Some(32_768));
        // OOM
        assert_eq!(stat.oom_kill, Some(0));
    }

    #[test]
    fn parse_vmstat_missing_keys() {
        let input = "nr_free_pages 100\npgfree 200\n";
        let stat = parse_vmstat(input);
        assert_eq!(stat.compact_stall, None);
        assert_eq!(stat.compact_success, None);
        assert_eq!(stat.pgscan_direct, None);
        assert_eq!(stat.oom_kill, None);
        assert_eq!(stat.pgfree, Some(200));
    }

    #[test]
    fn parse_vmstat_empty() {
        let stat = parse_vmstat("");
        assert_eq!(stat.compact_stall, None);
        assert_eq!(stat.pgalloc_normal, None);
        assert_eq!(stat.pgscan_direct, None);
    }

    #[test]
    fn meminfo_serde_roundtrip() {
        let info = parse_meminfo(MEMINFO_FIXTURE).unwrap();
        let json = serde_json::to_string(&info).unwrap();
        let deserialized: MemInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(info, deserialized);
    }

    #[test]
    fn buddyinfo_serde_roundtrip() {
        let entries = parse_buddyinfo(BUDDYINFO_FIXTURE).unwrap();
        let json = serde_json::to_string(&entries).unwrap();
        let deserialized: Vec<BuddyInfoEntry> = serde_json::from_str(&json).unwrap();
        assert_eq!(entries, deserialized);
    }

    const PSI_MEMORY_FIXTURE: &str = "\
some avg10=1.23 avg60=4.56 avg300=7.89 total=123456
full avg10=0.10 avg60=0.20 avg300=0.30 total=7890
";

    #[test]
    fn parse_psi_memory_basic() {
        let psi = parse_psi_memory(PSI_MEMORY_FIXTURE);
        assert!((psi.some.avg10 - 1.23).abs() < f64::EPSILON);
        assert!((psi.some.avg60 - 4.56).abs() < f64::EPSILON);
        assert!((psi.some.avg300 - 7.89).abs() < f64::EPSILON);
        assert_eq!(psi.some.total, 123_456);
        assert!((psi.full.avg10 - 0.10).abs() < f64::EPSILON);
        assert_eq!(psi.full.total, 7890);
    }

    #[test]
    fn parse_psi_memory_empty() {
        let psi = parse_psi_memory("");
        assert!((psi.some.avg10 - 0.0).abs() < f64::EPSILON);
        assert_eq!(psi.some.total, 0);
    }

    #[test]
    fn parse_psi_io_basic() {
        let content = "some avg10=0.50 avg60=1.00 avg300=2.00 total=50000\n\
                        full avg10=0.01 avg60=0.05 avg300=0.10 total=1000\n";
        let psi = parse_psi_io(content);
        assert!((psi.some.avg10 - 0.50).abs() < f64::EPSILON);
        assert!((psi.full.avg300 - 0.10).abs() < f64::EPSILON);
    }

    #[test]
    fn psi_memory_serde_roundtrip() {
        let psi = parse_psi_memory(PSI_MEMORY_FIXTURE);
        let json = serde_json::to_string(&psi).unwrap();
        let deserialized: PsiMemory = serde_json::from_str(&json).unwrap();
        assert_eq!(psi, deserialized);
    }
}
