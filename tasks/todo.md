# Implementation TODO

## Phase 1: `feat/ioctl-backend` — ioctl definitions + backend trait + real/mock

- [ ] `src/ioctl/mod.rs` — module declarations
- [ ] `src/ioctl/dma_heap.rs` — `dma_heap_allocation_data` struct + `DMA_HEAP_IOCTL_ALLOC`
- [ ] `src/ioctl/dma_buf.rs` — `dma_buf_sync`, `dma_buf_export_sync_file`, `dma_buf_import_sync_file` structs + ioctl macros
- [ ] `src/backend/mod.rs` — `HeapBackend` + `DmaBufBackend` trait definitions
- [ ] `src/backend/real.rs` — real ioctl/mmap implementation (`cfg(target_os = "android")`)
- [ ] `src/backend/mock.rs` — mock implementation for host `cargo test`
- [ ] Unit tests for ioctl struct layout/size validation
- [ ] Unit tests for mock backend (alloc, sync flags, errno paths)

## Phase 2: `feat/cli-core` — clap CLI + heap.rs + dmabuf.rs

- [ ] `src/cli.rs` — clap derive subcommand definitions (all commands + global options)
- [ ] `src/main.rs` — CLI entry point wiring
- [ ] `src/heap.rs` — `/dev/dma_heap/<name>` open + alloc wrapper (trait-based)
- [ ] `src/dmabuf.rs` — mmap, sync, llseek, sync_file, set_name wrappers (trait-based)
- [ ] Unit tests for CLI parsing
- [ ] Unit tests for heap/dmabuf wrappers with mock backend

## Phase 3: `feat/cmd-basic` — stage 1 tests

- [ ] `src/cmd/mod.rs` — module declarations
- [ ] `src/cmd/basic.rs::test_alloc_and_map()` — size-based alloc → mmap → sync → verify → close
- [ ] `src/cmd/basic.rs::test_alloc_zeroed()` — 16 fd pattern write → realloc → zero check
- [ ] `src/cmd/basic.rs::test_repeated_alloc()` — 1024x alloc/close loop + leak detection
- [ ] `src/cmd/basic.rs::test_llseek_size()` — llseek(SEEK_END) size verification
- [ ] Unit tests with mock backend for all basic tests

## Phase 4: `feat/infra` — trace + sysfs + procfs

- [ ] `src/trace.rs` — Perfetto atrace marker (trace_marker write, no-op when disabled)
- [ ] `src/sysfs.rs` — `/sys/kernel/dmabuf/buffers/` parsing
- [ ] `src/procfs.rs` — buddyinfo, pagetypeinfo, meminfo, vmstat parsing
- [ ] Unit tests for procfs/sysfs parsers with sample data
- [ ] Unit tests for trace marker format generation

## Phase 5: `feat/cmd-sync-edge` — stage 2 tests

- [ ] `src/cmd/sync_file.rs::test_export_sync_file()` — export + poll signaled
- [ ] `src/cmd/sync_file.rs::test_import_sync_file()` — import + poll state
- [ ] `src/cmd/edge.rs::test_concurrent_alloc()` — 100 threads concurrent alloc/sync/close
- [ ] `src/cmd/edge.rs::test_dup_fd()` — dup → close original → verify dup works
- [ ] `src/cmd/edge.rs::test_set_name()` — DMA_BUF_SET_NAME_B + fdinfo verify
- [ ] Unit tests for sync_file and edge logic

## Phase 6: `feat/cmd-negative` — negative tests

- [ ] Layer 1: Heap device access (nonexistent, not-a-heap, permission)
- [ ] Layer 2: Alloc ioctl (zero size, overflow, huge, invalid flags, closed fd, wrong ioctl, reserved, garbage)
- [ ] Layer 3: dma-buf fd ops (sync flags, non-dmabuf, end-without-start, double-start, llseek, set_name, read/write)
- [ ] Layer 4: mmap (beyond size, invalid offset, close-then-access, prot_exec, access-after-munmap)
- [ ] Layer 5: sync_file (invalid flags, bad fd, non-sync import, closed dmabuf)
- [ ] Layer 6: Resource leaks (fd leak, mmap leak, cloexec, rapid alloc/close)
- [ ] Layer 7: Concurrency (double close, sync+close, mmap+munmap, alloc exhaust)
- [ ] Unit tests for errno validation with mock backend

## Phase 7: `feat/cmd-perf` — stage 3 performance

- [ ] `src/cmd/perf.rs::bench_alloc_only()` — alloc latency p50/p95/p99
- [ ] `src/cmd/perf.rs::bench_full_pipeline()` — full path latency
- [ ] `src/cmd/perf.rs::bench_close()` — close/release latency
- [ ] `src/cmd/perf.rs::bench_order_boundary()` — size sweep around 64K boundary
- [ ] `src/cmd/perf.rs::bench_fallback_path()` — pool exhaustion + fallback
- [ ] `src/cmd/perf.rs::bench_internal_frag()` — unaligned size → actual size ratio
- [ ] Unit tests for latency statistics calculation (p50/p95/p99)

## Phase 8: `feat/cmd-pressure-frag-pool` — pressure + fragmentation + pool

- [ ] `src/cmd/pressure.rs` — gradual_exhaust, recovery, pressure_concurrent
- [ ] `src/cmd/fragmentation.rs` — buddyinfo_track, interleave_pattern, pagetypeinfo_track
- [ ] `src/cmd/pool.rs` — pool_warmup, pool_drain, size_switch, release_order, deferred_free
- [ ] Unit tests for pressure/fragmentation/pool logic

## Phase 9: Scenarios

### `feat/scenario-npu`
- [ ] `src/cmd/scenario/mod.rs` — common patterns (BufferPool, BulkAlloc, SizeSwitch, MixedAlloc, LongHold)
- [ ] `src/cmd/scenario/npu.rs` — model_load, inference_loop, model_switch, sustained, concurrent, pressure

### `feat/scenario-camera`
- [ ] `src/cmd/scenario/camera.rs` — preview, capture, switch, multi_stream

### `feat/scenario-display`
- [ ] `src/cmd/scenario/display.rs` — flip, rotation, multi_layer

### `feat/scenario-codec`
- [ ] `src/cmd/scenario/codec.rs` — decode, adaptive, transcode

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
