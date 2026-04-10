// Data structures for info report serialization.

use serde::{Deserialize, Serialize};

use crate::procfs::{BuddyInfoEntry, MemInfo, PageTypeInfoEntry, PsiIo, PsiMemory, VmStat};
use crate::sysfs::CmaAreaStats;

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
    pub meminfo: MemInfo,
    pub vmstat: VmStat,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub heap_caps: Option<Vec<crate::probe::HeapCaps>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cma_areas: Option<Vec<CmaAreaStats>>,
    /// DMA heap page pool size in kB (Android-specific).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dma_heap_pool_kb: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub psi_memory: Option<PsiMemory>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub psi_io: Option<PsiIo>,
}

/// Aggregate per-process DMA-BUF usage for display.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessSummary {
    pub pid: u32,
    pub comm: String,
    pub fd_count: usize,
    pub total_size_bytes: u64,
}
