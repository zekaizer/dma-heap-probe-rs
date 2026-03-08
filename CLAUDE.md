# dma-heap-probe-rs (dhp)

Android 16+ (kernel 6.12+) dma-heap userspace test tool written in Rust.

## Build

```sh
# Host (for testing with mock backend)
cargo build
cargo test

# Android target (requires NDK)
rustup target add aarch64-linux-android
cargo install cargo-ndk
cargo ndk -t arm64-v8a -p 35 build --release
```

## Project Structure

- `src/main.rs` — CLI entry point (clap)
- `src/cli.rs` — subcommand definitions
- `src/ioctl/` — DMA_HEAP_IOCTL_ALLOC, DMA_BUF_IOCTL_SYNC, etc.
- `src/heap.rs` — /dev/dma_heap/<name> open + alloc
- `src/dmabuf.rs` — mmap, sync, llseek, sync_file, set_name
- `src/backend/` — HeapBackend / DmaBufBackend trait (real.rs + mock.rs)
- `src/trace.rs` — Perfetto atrace marker
- `src/sysfs.rs` — /sys/kernel/dmabuf/buffers/ parsing
- `src/procfs.rs` — buddyinfo, pagetypeinfo, meminfo, vmstat parsing
- `src/runner.rs` — test runner + result aggregation
- `src/cmd/` — subcommand implementations (basic, sync_file, edge, perf, negative, pressure, fragmentation, pool)
- `src/cmd/scenario/` — workload simulations (npu, camera, display, codec, gpu, pipeline)
- `src/config.rs` — JSON config file loading, ScenarioConfigs, resolve functions
- `tests/cli_smoke.rs` — CLI end-to-end smoke tests (assert_cmd)
- `tests/cli_fuzz.rs` — CLI argument combination fuzzing (proptest)

## Conventions

- Code comments, variable names, and commit messages in English
- Commit messages use [Conventional Commits](https://www.conventionalcommits.org/) format: `type(scope): description`
  - Types: `feat`, `fix`, `refactor`, `test`, `docs`, `chore`, `style`, `ci`
  - Scope is optional but encouraged (e.g., `feat(ioctl)`, `fix(mock)`, `refactor(backend)`)
- Commits must be atomic — one logical change per commit. Do not bundle unrelated changes.
- Suppress warnings with `-w` for simple builds; review warnings when fixing them
- Backend abstraction: `cfg(target_os = "android")` for real, `cfg(test)` for mock
- ioctl definitions via `nix` crate macros (ioctl_readwrite!, ioctl_write_ptr!)
- No external C library dependencies — pure Rust + syscall wrappers

## Testing Policy

- Every new module/function MUST have unit tests (mock backend for ioctl/mmap paths)
- `cargo test` must pass before every commit
- PR merge to `main` requires ALL tests passing (`cargo test --all-targets`)
- Integration tests go in `tests/` directory (mock backend based)
  - `tests/cli_smoke.rs` — end-to-end smoke tests for all subcommands, JSON output, error cases
  - `tests/cli_fuzz.rs` — proptest-based argument combination fuzzing (448 iterations)
- Test coverage targets: ioctl struct validation, errno branching, CLI parsing, procfs/sysfs parsing, scenario buffer calculations, latency statistics

## Dependencies

| Crate | Purpose |
|---|---|
| nix (0.29+) | ioctl, mmap, close, lseek, dup |
| clap (4.x, derive) | CLI parsing |
| serde + serde_json | JSON result output + config loading |
| libc | auxiliary constants (O_CLOEXEC, etc.) |
| assert_cmd (2.x) | CLI integration test runner (dev) |
| predicates (3.x) | Output assertion matchers (dev) |
| tempfile (3.x) | Temporary file for --output tests (dev) |
| proptest (1.x) | Property-based / fuzz testing (dev) |

## Branching Strategy

- `main` — stable, always builds. Merge via PR only.
- `feat/<name>` — feature branches, one per implementation phase:
  - `feat/ioctl-backend` — ioctl definitions + backend trait + real/mock
  - `feat/cli-core` — clap CLI + heap.rs + dmabuf.rs
  - `feat/cmd-basic` — cmd/basic.rs (stage 1 tests)
  - `feat/infra` — trace.rs + sysfs.rs + procfs.rs
  - `feat/cmd-sync-edge` — cmd/sync_file.rs + cmd/edge.rs (stage 2)
  - `feat/cmd-negative` — cmd/negative.rs
  - `feat/cmd-perf` — cmd/perf.rs (stage 3)
  - `feat/cmd-pressure-frag-pool` — pressure.rs + fragmentation.rs + pool.rs
  - `feat/scenario-npu` — cmd/scenario/npu.rs
  - `feat/scenario-camera` — cmd/scenario/camera.rs
  - `feat/scenario-display` — cmd/scenario/display.rs
  - `feat/scenario-codec` — cmd/scenario/codec.rs
  - `feat/scenario-gpu` — cmd/scenario/gpu.rs
  - `feat/scenario-pipeline` — cmd/scenario/pipeline.rs
  - `feat/runner-output` — runner.rs + JSON output integration
  - `feat/cli-integration-test` — CLI smoke + proptest fuzz tests
  - `feat/scenario-json-config` — JSON config file for scenario workloads
- `fix/<name>` — bug fixes
- `refactor/<name>` — refactoring without behavior change

Each feature branch is based on `main` and merged back via PR after review.

## Task Management

- `tasks/todo.md` — implementation checklist with checkable items per phase. Update progress as tasks are completed.
- `tasks/lessons.md` — patterns and corrections discovered during development. Record mistakes and fixes here to avoid repeating them.

## Scenario JSON Configuration

Scenario workload parameters can be customized via a JSON config file.

```sh
# Dump default config to file
dhp scenario dump-config > config.json

# Edit config.json, then run with custom settings
dhp --config config.json scenario npu

# CLI args override JSON values (priority: Default -> JSON -> CLI)
dhp --config config.json scenario npu --iterations 10

# Works with 'all' and top-level 'all' command
dhp --config config.json scenario all
dhp --config config.json all
```

Config file supports partial specification — omitted fields use defaults:
```json
{
  "npu": { "iterations": 50, "clients": 2 },
  "camera": { "width": 3840, "height": 2160, "format": "raw16" }
}
```

## Deploy

```sh
adb push target/aarch64-linux-android/release/dhp /data/local/tmp/
adb shell chmod +x /data/local/tmp/dhp
adb shell su -c /data/local/tmp/dhp all --heap system --trace --sysfs --procfs
```
