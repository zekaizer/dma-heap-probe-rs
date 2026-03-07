// Parsers for /proc/buddyinfo, /proc/pagetypeinfo, /proc/meminfo, /proc/vmstat.

use std::error::Error;

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
pub fn parse_buddyinfo(content: &str) -> Result<Vec<BuddyInfoEntry>, Box<dyn Error>> {
    let mut entries = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // "Node 0, zone   Normal   512  320  ..."
        let rest = line
            .strip_prefix("Node")
            .ok_or_else(|| format!("buddyinfo: unexpected line format: {line}"))?
            .trim_start();

        // Split at comma: "0" and "zone   Normal   512  320  ..."
        let (node_str, rest) = rest
            .split_once(',')
            .ok_or_else(|| format!("buddyinfo: missing comma: {line}"))?;
        let node: u32 = node_str.trim().parse()?;

        let rest = rest
            .trim_start()
            .strip_prefix("zone")
            .ok_or_else(|| format!("buddyinfo: missing 'zone' keyword: {line}"))?;

        // Remaining tokens: zone name followed by numbers.
        // Zone name is the first non-numeric token(s), but in practice it's a single word.
        let tokens: Vec<&str> = rest.split_whitespace().collect();
        if tokens.is_empty() {
            return Err(format!("buddyinfo: no tokens after zone: {line}").into());
        }

        // Find where numeric values start.
        let num_start = tokens
            .iter()
            .position(|t| t.parse::<u64>().is_ok())
            .ok_or_else(|| format!("buddyinfo: no numeric values: {line}"))?;

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
pub fn parse_pagetypeinfo(content: &str) -> Result<Vec<PageTypeInfoEntry>, Box<dyn Error>> {
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
            .ok_or_else(|| format!("pagetypeinfo: unexpected format: {trimmed}"))?
            .trim_start();

        // Split at first comma: node number.
        let (node_str, rest) = rest
            .split_once(',')
            .ok_or_else(|| format!("pagetypeinfo: missing first comma: {trimmed}"))?;
        let node: u32 = node_str.trim().parse()?;

        // "zone      DMA, type    Unmovable      1      0 ..."
        let rest = rest
            .trim_start()
            .strip_prefix("zone")
            .ok_or_else(|| format!("pagetypeinfo: missing 'zone': {trimmed}"))?;

        // Split at second comma: zone name.
        let (zone_str, rest) = rest
            .split_once(',')
            .ok_or_else(|| format!("pagetypeinfo: missing second comma: {trimmed}"))?;
        let zone = zone_str.trim().to_string();

        // "type    Unmovable      1      0 ..."
        let rest = rest
            .trim_start()
            .strip_prefix("type")
            .ok_or_else(|| format!("pagetypeinfo: missing 'type': {trimmed}"))?;

        let tokens: Vec<&str> = rest.split_whitespace().collect();
        if tokens.is_empty() {
            return Err(format!("pagetypeinfo: no tokens after type: {trimmed}").into());
        }

        // Find where numeric values start.
        let num_start = tokens
            .iter()
            .position(|t| t.parse::<u64>().is_ok())
            .ok_or_else(|| format!("pagetypeinfo: no numeric values: {trimmed}"))?;

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

/// Read and parse `/proc/buddyinfo`.
pub fn read_buddyinfo() -> Result<Vec<BuddyInfoEntry>, Box<dyn Error>> {
    let content = std::fs::read_to_string("/proc/buddyinfo")?;
    parse_buddyinfo(&content)
}

/// Read and parse `/proc/pagetypeinfo`.
pub fn read_pagetypeinfo() -> Result<Vec<PageTypeInfoEntry>, Box<dyn Error>> {
    let content = std::fs::read_to_string("/proc/pagetypeinfo")?;
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
}
