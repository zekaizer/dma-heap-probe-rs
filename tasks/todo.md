# Implementation TODO

## Phase 1: `feat/ioctl-backend` — ioctl definitions + backend trait + real/mock

- [x] `src/ioctl/mod.rs` — module declarations
- [x] `src/ioctl/dma_heap.rs` — `DmaHeapAllocationData` struct + `DMA_HEAP_IOCTL_ALLOC`
- [x] `src/ioctl/dma_buf.rs` — `DmaBufSync`, `DmaBufExportSyncFile`, `DmaBufImportSyncFile` structs + ioctl macros
- [x] `src/backend/mod.rs` — `HeapBackend` + `DmaBufBackend` trait definitions
- [x] `src/backend/real.rs` — real ioctl/mmap implementation (`cfg(target_os = "android")`)
- [x] `src/backend/mock.rs` — mock implementation for host `cargo test`
- [x] Unit tests for ioctl struct layout/size validation (15 tests)
- [x] Unit tests for mock backend: alloc, sync flags, errno paths (29 tests)

## Phase 2: `feat/cli-core` — clap CLI + heap.rs + dmabuf.rs

- [x] `src/cli.rs` — clap derive subcommand definitions (all commands + global options)
- [x] `src/main.rs` — CLI entry point wiring
- [x] `src/heap.rs` — `/dev/dma_heap/<name>` open + alloc wrapper (trait-based)
- [x] `src/dmabuf.rs` — mmap, sync, llseek, sync_file, set_name wrappers (trait-based)
- [x] Unit tests for CLI parsing
- [x] Unit tests for heap/dmabuf wrappers with mock backend

## Phase 3: `feat/cmd-basic` — stage 1 tests

- [x] `src/cmd/mod.rs` — module declarations
- [x] `src/cmd/basic.rs::test_alloc_and_map()` — size-based alloc → mmap → sync → verify → close
- [x] `src/cmd/basic.rs::test_alloc_zeroed()` — 16 fd pattern write → realloc → zero check
- [x] `src/cmd/basic.rs::test_repeated_alloc()` — 1024x alloc/close loop + leak detection
- [x] `src/cmd/basic.rs::test_llseek_size()` — llseek(SEEK_END) size verification
- [x] Unit tests with mock backend for all basic tests

## Phase 4: `feat/infra` — trace + sysfs + procfs

- [x] `src/trace.rs` — Perfetto atrace marker (trace_marker write, no-op when disabled)
- [x] `src/sysfs.rs` — `/sys/kernel/dmabuf/buffers/` parsing
- [x] `src/procfs.rs` — buddyinfo, pagetypeinfo, meminfo, vmstat parsing
- [x] Unit tests for procfs/sysfs parsers with sample data (22 tests)
- [x] Unit tests for trace marker format generation (5 tests)

## Phase 5: `feat/cmd-sync-edge` — stage 2 tests

- [x] `src/cmd/sync_file.rs::test_export_sync_file()` — export with READ/WRITE/RW flags
- [x] `src/cmd/sync_file.rs::test_import_sync_file()` — export→import roundtrip
- [x] `src/cmd/edge.rs::test_concurrent_alloc()` — N threads concurrent alloc/sync/close
- [x] `src/cmd/edge.rs::test_dup_fd()` — dup → close original → verify dup works
- [x] `src/cmd/edge.rs::test_set_name()` — short name + max length validation
- [x] Unit tests for sync_file (4) and edge (6) logic

## Phase 6: `feat/cmd-negative` — negative tests

- [x] Layer 1: Heap device access (nonexistent → ENOENT)
- [x] Layer 2: Alloc ioctl (zero size, overflow, invalid fd_flags/heap_flags, closed heap)
- [x] Layer 3: dma-buf fd ops (sync invalid flags, closed fd, llseek invalid, set_name too long)
- [x] Layer 4: mmap (beyond size)
- [x] Layer 5: sync_file (invalid flags, bad sync_file fd)
- [x] Layer 6: Resource leaks (rapid 1000x alloc/close)
- [x] Layer 7: Concurrency (double close same fd)
- [x] Unit tests for errno validation with mock backend (16 tests)

## Phase 7: `feat/cmd-perf` — stage 3 performance

- [x] `src/cmd/perf.rs::bench_alloc_only()` — alloc latency p50/p95/p99
- [x] `src/cmd/perf.rs::bench_full_pipeline()` — full path latency
- [x] `src/cmd/perf.rs::bench_close()` — close/release latency
- [x] `src/cmd/perf.rs::bench_order_boundary()` — size sweep 4K-8M around 64K boundary
- [ ] `src/cmd/perf.rs::bench_fallback_path()` — pool exhaustion + fallback (deferred to device testing)
- [x] `src/cmd/perf.rs::bench_internal_frag()` — unaligned size → actual size ratio
- [x] Unit tests for latency statistics calculation (p50/p95/p99) — 12 tests

## Phase 8: `feat/cmd-pressure-frag-pool` — pressure + fragmentation + pool

- [x] `src/cmd/pressure.rs` — gradual_exhaust, recovery, pressure_concurrent
- [x] `src/cmd/fragmentation.rs` — buddyinfo_track, interleave_pattern, pagetypeinfo_track
- [x] `src/cmd/pool.rs` — pool_warmup, pool_drain (merged into deferred_free), size_switch, release_order, deferred_free
- [x] Unit tests for pressure/fragmentation/pool logic (21 tests)

## Phase 9: Scenarios

### `feat/scenario-npu`
- [x] `src/cmd/scenario/mod.rs` — common patterns (BufferPool, bulk_alloc, fill_buffer, read_sync)
- [x] `src/cmd/scenario/npu.rs` — model_load, inference_loop, model_switch, sustained, concurrent (13 tests)

### `feat/scenario-camera`
- [x] `src/cmd/scenario/camera.rs` — preview, capture, switch, multi_stream (10 tests)

### `feat/scenario-display`
- [x] `src/cmd/scenario/display.rs` — flip, rotation, multi_layer (8 tests)

### `feat/scenario-codec`
- [x] `src/cmd/scenario/codec.rs` — decode, adaptive, transcode (8 tests)

### `feat/scenario-gpu`
- [ ] `src/cmd/scenario/gpu.rs` — app_launch, app_switch, game_texture

### `feat/scenario-pipeline`
- [ ] `src/cmd/scenario/pipeline.rs` — camera_preview, video_call, ai_camera, heavy
- [ ] `--workload <role>:<heap>` multi-heap support

### Scenario unit tests
- [ ] Buffer size calculations
- [ ] Pool rotation logic
- [ ] Scenario parameter validation

## Phase 10: `feat/runner-output` — runner + JSON output

- [ ] `src/runner.rs` — test execution engine + result aggregation
- [ ] JSON output format + `--output` path writing
- [ ] `all` command implementation (sequential full run)
- [ ] `sysfs-dump` standalone command
- [ ] Integration tests for runner + JSON serialization

---

## Review Checklist

- [ ] All `cargo test --all-targets` pass
- [ ] `cargo fmt --check` clean
- [ ] `cargo clippy --all-targets -- -D warnings` clean
- [ ] Device test on aarch64-linux-android (system heap)
- [ ] Perfetto trace integration verified
- [ ] JSON output format validated
