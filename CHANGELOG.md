# Changelog

## [1.0.0] - 2026-03-08

Initial release of `dhp` — a comprehensive DMA-Heap userspace test tool for Android 16+ (kernel 6.12+).

### Features

- **ioctl / backend abstraction** — `DMA_HEAP_IOCTL_ALLOC`, `DMA_BUF_IOCTL_SYNC`, `DMA_BUF_SET_NAME_B`, `EXPORT/IMPORT_SYNC_FILE` via nix crate macros; trait-based backend (real for Android, mock for host testing)
- **CLI** — clap-derive subcommand structure with global options (`--heap`, `--trace`, `--sysfs`, `--procfs`, `--verbose`, `--output`, `--config`)
- **basic** — alloc+mmap+sync validation, zero-fill check, repeated alloc leak detection, llseek size verification
- **sync-file** — export/import sync_file fence operations
- **edge** — concurrent alloc, dup fd, set_name (short + max length)
- **negative** — 7-layer negative testing (heap access, alloc ioctl, dma-buf fd ops, mmap, sync_file, resource leaks, concurrency)
- **perf** — alloc/close/full-pipeline latency with p50/p95/p99 statistics, order boundary profiling, internal fragmentation measurement
- **pressure** — gradual exhaustion, recovery, concurrent pressure
- **fragmentation** — buddyinfo/pagetypeinfo tracking across alloc/free cycles
- **pool** — warmup, drain, size switch, release order, deferred free
- **scenario** — workload simulations for NPU, camera, display, codec, GPU, and multi-subsystem pipeline
- **scenario JSON config** — customizable workload parameters via JSON file with CLI override priority (Default → JSON → CLI)
- **runner** — test execution engine with `RunResult`/`StageResult` aggregation, JSON output via `--output`
- **sysfs-dump** — standalone `/sys/kernel/dmabuf/buffers/` + meminfo/vmstat JSON dump
- **observability** — Perfetto atrace markers (`--trace`), sysfs/procfs snapshot collection
- **procfs** — buddyinfo, pagetypeinfo, meminfo, vmstat parsers

### Testing

- 295 tests (255 unit + 40 integration)
- CLI smoke tests for all subcommands, JSON output, error cases
- proptest-based argument combination fuzzing (448 iterations)
- Mock backend enables full host testing without Android device

### Dependencies

- nix 0.31, clap 4, serde/serde_json, libc, tracing/tracing-subscriber
