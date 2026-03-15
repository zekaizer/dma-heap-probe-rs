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
cargo ndk -t arm64-v8a -P 35 build --release
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
- `src/cmd/` — subcommand implementations (basic, perf, pressure, negative, aging, histogram, info)
- `src/cmd/aging/` — sustained alloc/free aging tests (mod.rs, worker.rs, fuzz.rs)
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
- Test coverage targets: ioctl struct validation, errno branching, CLI parsing, procfs/sysfs parsing, latency statistics

## Dependencies

| Crate | Purpose |
|---|---|
| nix (0.31+) | ioctl, mmap, close, lseek, dup |
| clap (4.x, derive) | CLI parsing |
| serde + serde_json | JSON result output |
| libc | auxiliary constants (O_CLOEXEC, etc.) |
| tracing (0.1) | structured logging |
| tracing-subscriber (0.3) | log output formatting |
| assert_cmd (2.x) | CLI integration test runner (dev) |
| predicates (3.x) | Output assertion matchers (dev) |
| tempfile (3.x) | Temporary file for --output tests (dev) |
| proptest (1.x) | Property-based / fuzz testing (dev) |
| rand (0.8) | Random number generation for fuzz aging |

## Branching Strategy

- `main` — stable, always builds. Merge via PR only.
- `feat/<name>` — feature branches
- `fix/<name>` — bug fixes
- `refactor/<name>` — refactoring without behavior change

Each feature branch is based on `main` and merged back via PR after review.

## Task Management

- `tasks/todo.md` — implementation checklist with checkable items per phase. Update progress as tasks are completed.
- `tasks/lessons.md` — patterns and corrections discovered during development. Record mistakes and fixes here to avoid repeating them.

## Claude Code Hooks

Hooks are defined in `.claude/settings.json` and scripts live in `.claude/hooks/`.

| Hook | Event | Trigger | Action |
|---|---|---|---|
| `format-rs.sh` | PostToolUse (Write\|Edit) | `*.rs` modified | `rustfmt` on the file |
| `clippy-rs.sh` | PostToolUse (Write\|Edit) | `*.rs` modified | `cargo clippy` (full project) |
| `check-cargo.sh` | PostToolUse (Write\|Edit) | `Cargo.toml` modified | `cargo clippy` (full project) |
| `pre-git.sh` | PreToolUse (Bash) | `git commit` or `git push` | `cargo test` + `cargo clippy` |

All hooks exit 2 on failure, blocking the action and feeding errors back to Claude for auto-fix.

## CI/CD

- **CI** (`.github/workflows/ci.yml`): push/PR to `main`
  - `check` job: fmt, clippy, test (host x86_64)
  - `android` job: cross-compile `aarch64-linux-android` via `cargo ndk`
- **Release** (`.github/workflows/release.yml`): prerelease creation triggers workflow
  - Builds Android binary → uploads to release → promotes to full release
  - Changelog: manually written in release notes, no separate CHANGELOG file

### Release Process

```sh
# 1. Create prerelease (triggers build workflow)
#    --title: tag name, --notes: changelog
gh release create v<VERSION> --prerelease \
  --title "v<VERSION>" \
  --notes "## Changes

### Added
- new feature description

### Fixed
- bug fix description

### Changed
- behavior change description"

# 2. release.yml auto-runs: build → upload binary → promote to full release
```

## Deploy

```sh
# Option 1: Download from GitHub Release
gh release download v<VERSION> -p 'dhp-aarch64-linux-android'
adb push dhp-aarch64-linux-android /data/local/tmp/dhp

# Option 2: Build locally
cargo ndk -t arm64-v8a -P 35 build --release
adb push target/aarch64-linux-android/release/dhp /data/local/tmp/

# Run on device
adb shell chmod +x /data/local/tmp/dhp
adb shell su -c /data/local/tmp/dhp all --heaps system --trace --sysfs --procfs
```
