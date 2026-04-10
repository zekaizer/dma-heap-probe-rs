// System DMA heap information and buffer status display.

mod collect;
mod follow;
mod format;
mod types;

use std::path::PathBuf;

use anyhow::Context;

use crate::procfs::{self, VmStat};
use crate::sysfs;
use crate::{tee_print, tee_println};

use collect::{
    DMA_HEAP_BASE, aggregate_debugfs, aggregate_sysfs, enumerate_heaps, read_debugfs_bufinfo,
    read_vm_params, read_zoneinfo, scan_process_dmabufs,
};
use format::format_human;
use types::{InfoReport, MemoryContext};

pub use follow::run_follow;
pub use format::format_size;

/// Run the info subcommand.
///
/// - `backend`/`heap_names`: needed for `--probe` capability probing.
/// - `detail`: show individual buffer list and per-process usage.
/// - `heap_filter`: when detail is true, filter buffers by heap/exporter name(s).
/// - `show_procfs`: include extended memory info (zoneinfo, buddyinfo, pagetypeinfo, vm params).
/// - `show_probe`: probe heap capabilities (alloc, mmap, sync, etc.).
/// - `output`: if Some, write JSON report to file instead of human-readable stdout.
#[allow(clippy::too_many_arguments)]
pub fn run<B: crate::backend::HeapBackend + crate::backend::DmaBufBackend>(
    backend: &B,
    heap_names: &[String],
    detail: bool,
    heap_filter: Option<&[&str]>,
    show_procfs: bool,
    show_probe: bool,
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
        (Ok(meminfo), Ok(vmstat)) => Some(MemoryContext { meminfo, vmstat }),
        (Ok(meminfo), Err(_)) => Some(MemoryContext {
            meminfo,
            vmstat: VmStat::default(),
        }),
        _ => None,
    };

    // 5. Extended procfs (--procfs)
    let vm_params = show_procfs.then(read_vm_params);
    let zones = show_procfs.then(|| read_zoneinfo().ok()).flatten();
    let buddyinfo = show_procfs.then(|| procfs::read_buddyinfo().ok()).flatten();
    let pagetypeinfo = show_procfs
        .then(|| procfs::read_pagetypeinfo().ok())
        .flatten();

    // 6. Heap capability probe (--probe)
    let heap_caps = if show_probe {
        Some(crate::probe::discover_and_probe(backend, Some(heap_names)))
    } else {
        None
    };

    // 7. CMA per-area stats
    let cma_areas = if show_procfs {
        let areas = sysfs::read_cma_areas();
        if areas.is_empty() { None } else { Some(areas) }
    } else {
        None
    };

    // 8. DMA heap page pool size (Android-specific)
    let dma_heap_pool_kb = sysfs::read_dma_heap_pool_kb();

    // 9. PSI (Pressure Stall Information)
    let psi_memory = procfs::read_psi_memory();
    let psi_io = procfs::read_psi_io();

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
        heap_caps,
        cma_areas,
        dma_heap_pool_kb,
        psi_memory,
        psi_io,
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
