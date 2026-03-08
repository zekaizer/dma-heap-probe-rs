# dma-heap-probe-rs (`dhp`)

A comprehensive userspace testing tool for DMA-Heap on Android 16+ (kernel 6.12+), written in Rust.

## Features

- **Basic validation** — alloc, mmap, sync, zero-fill, repeated alloc, llseek
- **sync_file** — export/import sync_file fence operations
- **Edge cases** — concurrent alloc, dup fd, set_name, error paths
- **Performance** — alloc latency (p50/p95/p99), order boundary profiling, internal fragmentation
- **Negative testing** — invalid inputs, race conditions, resource leak detection
- **Memory pressure** — gradual exhaustion, recovery, concurrent pressure
- **Fragmentation** — buddyinfo/pagetypeinfo tracking across alloc/free cycles
- **Pool/cache** — warmup, drain, size switch, release order, deferred free
- **Workload simulation** — NPU, camera, display, codec, GPU, multi-subsystem pipelines
- **Observability** — Perfetto trace markers, sysfs/procfs snapshot collection

## Quick Start

```sh
# Build for Android
rustup target add aarch64-linux-android
cargo install cargo-ndk
cargo ndk -t arm64-v8a -P 35 build --release

# Deploy
adb push target/aarch64-linux-android/release/dhp /data/local/tmp/
adb shell chmod +x /data/local/tmp/dhp

# Run
adb shell su -c /data/local/tmp/dhp basic --heap system
adb shell su -c /data/local/tmp/dhp perf --heap system --trace
adb shell su -c /data/local/tmp/dhp scenario npu --heap system
adb shell su -c /data/local/tmp/dhp all --heap system --sysfs --procfs --output /data/local/tmp/results.json
```

## License

MIT
