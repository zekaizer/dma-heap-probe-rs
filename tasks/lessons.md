# Lessons Learned

Patterns and corrections discovered during development. Updated after each mistake or user correction.

---

## Rust macro format args limitation

- `prop_assert!`, `prop_assert_ne!`, and similar macro wrappers around `format_args!` do NOT support inline variable capture (`{var:?}` syntax). Must use positional args: `"msg: {:?}", var`.
- Same applies to any macro that expands to `format_args!` internally.
- Always use explicit positional arguments in assertion macros from external crates.

## Test result verification

- Never report test results without actually reading the full output. If a tool execution was rejected or interrupted, re-run the test before claiming it passed.
- Always verify both compilation AND runtime results — a test that compiles doesn't mean it passes.

## Flat struct vs embedded struct for JSON serialization

- When a struct mirrors fields from two source structs (e.g. `MemoryContext` copying from `MemInfo` + `VmStat`), every new field requires 3 edits: source struct, target struct, and builder function.
- Embedding source structs directly (`MemoryContext { meminfo: MemInfo, vmstat: VmStat }`) eliminates duplication. New parser fields flow through automatically.
- Trade-off: changes the JSON output format from flat to nested. Acceptable for internal tools pre-1.0.

## macOS vs Linux procfs/sysfs compatibility in tests

- Smoke tests running on macOS CI will get `null` for `/proc/meminfo`, `/proc/pressure/`, etc.
- Use `json.get("field").is_some_and(|v| !v.is_null())` instead of `if let Some(v) = json.get("field")` — `serde_json` serializes `None` as JSON `null`, so `.get()` returns `Some(Value::Null)`, not `None`.

## Clippy pedantic patterns (edition 2024)

- `find(|(_, &c)| c > 0)` → use `find(|(_, c)| **c > 0)` — explicit dereference in pattern is not allowed in implicitly-borrowing iterator context.
- Nested `if let Some(..) { if cond { ... } }` → collapse with `.filter()`: `if let Some(v) = expr.filter(pred) { ... }`.

## ACK android15-6.6 dma_buf_stats_setup async regression

### Why: ACK 가 upstream 과 다르게 `dma_buf_stats_setup` 을 workqueue 로 분리

- **upstream v6.6** (`drivers/dma-buf/dma-buf-sysfs-stats.c:227`): `dma_buf_export` → `dma_buf_stats_setup` 에서 `kobject_init_and_add` 를 **동기** 호출. export 리턴 시점에 refcount = 1 (fd 소유권만).
- **ACK android15-6.6**: 동일 함수가 `get_dma_buf(dmabuf)` + `schedule_work(&sysfs_add_work)` 로 변경됨. export 리턴 시점에 refcount = 2 (fd + workqueue). workqueue (`sysfs_add_workfn`) 가 실행된 뒤에야 `dma_buf_put` 으로 drop.

### How to apply: close/release 비대칭 진단 시 이 경로부터 의심

- userspace `close(fd)` → `__fput_sync` → `atomic_long_dec_and_test(f_count)` → 2 → 1 (0 아님) → **`__fput` / `.release` 호출 안 됨**.
- heap 의 `.release` 는 그 후 workqueue 가 돌 때 (kthread) `dma_buf_put` 으로 refcount 0 도달 시 뒤늦게 호출됨 (확인 지점: ftrace 로 kthread context 확인).
- aging loop 가 workqueue 보다 빠르면 backlog 가 평형점 (~heap 용량 분량) 에서 머묾. 그 지점에서 heap exhaustion → `alloc error` (non-ENOMEM/EMFILE errno).

### 증상 체크리스트 (max-hold=0 기준)

1. app 측: `allocs == frees`, `held: 0(0B)` → userspace 무결
2. kernel 측: heap_size (= alloc tracepoint count) 가 증가하는데 heap release tracepoint 는 그만큼 안 뜸
3. release tracepoint 가 kthread context 에서 fire → `sysfs_add_workfn` workqueue 경로 확정
4. `CONFIG_DMABUF_SYSFS_STATS=y` 확인하면 더 단정적

### 대응

- **앱 측**: `--close-settle-us <N>` (50~200us 권장) 매 close 뒤 짧은 sleep 으로 workqueue drain 시간 확보. backlog 평형점을 충분히 낮춰 heap exhaustion 회피.
- **커널 측 (근본)**: ACK 에서 upstream 처럼 `dma_buf_stats_setup` 을 sync 로 되돌리거나, close path 에서 해당 workqueue 를 flush. Google 에 regression 제보 가치 있음.
- **우회**: `CONFIG_DMABUF_SYSFS_STATS=n` 빌드로 문제 경로 자체 제거 (sysfs stats 기능은 잃음).

### 연관 혼동 지점

- `aging::fuzz: alloc error` 로그가 **no-fuzz 모드에서도** 찍힘: `handle_alloc_error` 가 `src/cmd/aging/fuzz.rs` 에 정의됐기 때문. tracing span 이 소스 경로 기반이라 fuzz prefix 가 붙을 뿐, 실제 fuzz 동작은 아님. 이 로그 = non-ENOMEM/EMFILE alloc 실패 (heap exhaustion 신호).
