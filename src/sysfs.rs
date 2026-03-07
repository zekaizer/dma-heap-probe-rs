// Parser for /sys/kernel/dmabuf/buffers/ directory.

use std::error::Error;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// Information about a single dma-buf buffer from sysfs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DmaBufInfo {
    /// Buffer inode number (directory name under `/sys/kernel/dmabuf/buffers/`).
    pub ino: u64,
    /// Buffer size in bytes.
    pub size: u64,
    /// Exporter name (e.g. "system").
    pub exporter_name: String,
}

/// Snapshot of all active dma-buf buffers visible in sysfs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SysfsSnapshot {
    pub buffers: Vec<DmaBufInfo>,
}

/// Parse a buffer entry from raw sysfs file contents.
///
/// `ino` is the directory name (buffer inode number).
/// `size_content` is the content of the `size` file.
/// `exporter_content` is the content of the `exporter_name` file.
pub fn parse_buffer_entry(
    ino: u64,
    size_content: &str,
    exporter_content: &str,
) -> Result<DmaBufInfo, Box<dyn Error>> {
    let size: u64 = size_content
        .trim()
        .parse()
        .map_err(|e| format!("sysfs: invalid size for ino {ino}: {e}"))?;
    let exporter_name = exporter_content.trim().to_string();
    if exporter_name.is_empty() {
        return Err(format!("sysfs: empty exporter_name for ino {ino}").into());
    }
    Ok(DmaBufInfo {
        ino,
        size,
        exporter_name,
    })
}

const SYSFS_DMABUF_PATH: &str = "/sys/kernel/dmabuf/buffers";

/// Read all buffer entries from `/sys/kernel/dmabuf/buffers/`.
///
/// Returns an empty snapshot if the path does not exist (e.g. on host).
pub fn snapshot() -> Result<SysfsSnapshot, Box<dyn Error>> {
    let base = Path::new(SYSFS_DMABUF_PATH);
    if !base.exists() {
        return Ok(SysfsSnapshot {
            buffers: Vec::new(),
        });
    }

    let mut buffers = Vec::new();
    for entry in std::fs::read_dir(base)? {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        let ino: u64 = match name_str.parse() {
            Ok(v) => v,
            Err(_) => continue, // skip non-numeric entries
        };

        let dir = entry.path();
        let size_content = std::fs::read_to_string(dir.join("size"))?;
        let exporter_content = std::fs::read_to_string(dir.join("exporter_name"))?;

        buffers.push(parse_buffer_entry(ino, &size_content, &exporter_content)?);
    }

    buffers.sort_by_key(|b| b.ino);
    Ok(SysfsSnapshot { buffers })
}

/// Count total active buffers in a snapshot.
pub fn buffer_count(snap: &SysfsSnapshot) -> usize {
    snap.buffers.len()
}

/// Total size of all active buffers in bytes.
pub fn total_size(snap: &SysfsSnapshot) -> u64 {
    snap.buffers.iter().map(|b| b.size).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_buffer_entry_valid() {
        let info = parse_buffer_entry(1234, "65536\n", "system\n").unwrap();
        assert_eq!(info.ino, 1234);
        assert_eq!(info.size, 65536);
        assert_eq!(info.exporter_name, "system");
    }

    #[test]
    fn parse_buffer_entry_no_trailing_newline() {
        let info = parse_buffer_entry(42, "4096", "my_heap").unwrap();
        assert_eq!(info.size, 4096);
        assert_eq!(info.exporter_name, "my_heap");
    }

    #[test]
    fn parse_buffer_entry_invalid_size() {
        assert!(parse_buffer_entry(1, "not_a_number\n", "system\n").is_err());
    }

    #[test]
    fn parse_buffer_entry_empty_exporter() {
        assert!(parse_buffer_entry(1, "4096\n", "\n").is_err());
    }

    #[test]
    fn buffer_count_and_total_size() {
        let snap = SysfsSnapshot {
            buffers: vec![
                DmaBufInfo {
                    ino: 1,
                    size: 4096,
                    exporter_name: "system".into(),
                },
                DmaBufInfo {
                    ino: 2,
                    size: 65536,
                    exporter_name: "system".into(),
                },
                DmaBufInfo {
                    ino: 3,
                    size: 1_048_576,
                    exporter_name: "custom".into(),
                },
            ],
        };
        assert_eq!(buffer_count(&snap), 3);
        assert_eq!(total_size(&snap), 4096 + 65536 + 1_048_576);
    }

    #[test]
    fn empty_snapshot() {
        let snap = SysfsSnapshot {
            buffers: Vec::new(),
        };
        assert_eq!(buffer_count(&snap), 0);
        assert_eq!(total_size(&snap), 0);
    }

    #[test]
    fn sysfs_snapshot_serde_roundtrip() {
        let snap = SysfsSnapshot {
            buffers: vec![DmaBufInfo {
                ino: 99,
                size: 8192,
                exporter_name: "test_heap".into(),
            }],
        };
        let json = serde_json::to_string(&snap).unwrap();
        let deserialized: SysfsSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(snap, deserialized);
    }
}
