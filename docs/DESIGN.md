# dma-heap-probe-rs (`dhp`) 설계
## 개요

Android 16+ (kernel 6.12+) 환경에서 dma-heap (system heap + custom heap)을 유저스페이스에서 종합 테스트하기 위한 Rust CLI 앱.

- **프로젝트명**: `dma-heap-probe-rs`
- **바이너리명**: `dhp`

| 항목 | 결정 |
|---|---|
| 실행 방식 | CLI only (clap 서브커맨드) |
| 테스트 범위 | 전체 3단계 + 커스텀 힙 전용 |
| 추가 기능 | Perfetto trace (trace_marker) + sysfs/procfs 수집 |
| 빌드 | 독립 NDK cross-compile (Cargo 기반) |
| 타겟 | `aarch64-linux-android`, kernel 6.12+ |
| 코드베이스 참조 | `android16-6.12`, `android17-6.18` |

---

## 프로젝트 구조

```
dma-heap-probe-rs/
├── Cargo.toml
├── .cargo/
│   └── config.toml              # NDK cross-compile 설정
├── src/
│   ├── main.rs                  # CLI entry (clap)
│   ├── cli.rs                   # 서브커맨드 정의
│   ├── ioctl/
│   │   ├── mod.rs
│   │   ├── dma_heap.rs          # DMA_HEAP_IOCTL_ALLOC
│   │   └── dma_buf.rs           # SYNC, EXPORT/IMPORT_SYNC_FILE, SET_NAME
│   ├── heap.rs                  # /dev/dma_heap/<name> open + alloc
│   ├── dmabuf.rs                # mmap, sync, llseek, sync_file, set_name
│   ├── trace.rs                 # Perfetto atrace marker (trace_marker 직접 write)
│   ├── sysfs.rs                 # /sys/kernel/dmabuf/buffers/ 파싱
│   ├── procfs.rs                # buddyinfo, pagetypeinfo, meminfo, vmstat 파싱
│   ├── runner.rs                # 테스트 실행 엔진 + 결과 집계
│   ├── backend/
│   │   ├── mod.rs               # HeapBackend / DmaBufBackend trait 정의
│   │   ├── real.rs              # 실제 ioctl/mmap (aarch64-linux-android)
│   │   └── mock.rs              # mock 구현 (호스트 cargo test)
│   └── cmd/
│       ├── mod.rs
│       ├── basic.rs             # 1단계: alloc, mmap, sync, llseek, zeroed, repeated
│       ├── sync_file.rs         # 2단계: export/import sync_file
│       ├── edge.rs              # 2단계: 에러 경로, dup, set_name, concurrent
│       ├── perf.rs              # 3단계: latency, order 경계, 내부 단편화
│       ├── negative.rs          # 네거티브: 에러 경로, 잘못된 입력, 경합 조건
│       ├── pressure.rs          # 메모리 압박 하 동작
│       ├── fragmentation.rs     # 단편화 관찰
│       ├── pool.rs              # pool/cache 동작, 해제 경로
│       └── scenario/
│           ├── mod.rs
│           ├── npu.rs           # NPU 워크로드
│           ├── camera.rs        # 카메라 캡처/프리뷰
│           ├── display.rs       # 디스플레이/컴포지터
│           ├── codec.rs         # 비디오 코덱 (decode/encode)
│           ├── gpu.rs           # GPU 렌더링 (gralloc 패턴)
│           └── pipeline.rs      # 복합 파이프라인
```

---

## CLI 인터페이스

```
dhp <COMMAND> [OPTIONS]

공통 옵션:
  --heap <name>          힙 이름 (default: "system")
  --trace                Perfetto atrace marker 활성화
  --sysfs                테스트 전후 sysfs 통계 수집
  --procfs               buddyinfo/pagetypeinfo/meminfo 수집
  --output <path>        결과 JSON 출력 경로

COMMANDS:
  basic                  1단계 기본 기능 검증
    --sizes <list>       할당 크기 목록 (e.g. "4096,65536,1048576")
    --repeat <n>         반복 할당 횟수 (default: 1024)

  sync-file              2단계 sync_file export/import

  edge                   2단계 경계 조건 및 에러 경로
    --threads <n>        동시 할당 스레드 수 (default: 100)

  perf                   3단계 성능 측정
    --sizes <list>       측정할 크기 목록
    --iterations <n>     반복 횟수 (default: 100)
    --warmup <n>         워밍업 횟수 (default: 10)

  pressure               메모리 압박 하 동작
    --alloc-size <bytes> 점진 소진 할당 크기 (default: 1048576)

  fragmentation          단편화 관찰
    --pattern <type>     interleave / sequential (default: interleave)

  pool                   pool/cache 동작 검증

  negative               네거티브 테스트 (에러 경로, 잘못된 입력, 경합)

  scenario <CATEGORY> <SUBCOMMAND>   워크로드 시뮬레이션

    npu model-load       대용량 모델 로딩
      --model-size <MB>    (default: 512)
      --chunk-size <MB>    (default: 64, 0이면 단일)
    npu inference        추론 루프
      --model-size <MB>    (default: 256)
      --input-size <bytes> (default: 602112)
      --output-size <bytes> (default: 4096)
      --iterations <n>     (default: 1000)
      --interval-ms <ms>   (default: 33)
    npu model-switch     모델 전환
      --model-sizes <list> (e.g. "256,512,256")
    npu sustained        장시간 안정성
      --duration-sec <sec> (default: 300)
    npu concurrent       멀티 클라이언트
      --clients <n>        (default: 4)
    npu pressure         압박 하 추론
      --pressure-pct <n>   (default: 80)

    camera preview       프리뷰 버퍼 풀 순환
      --resolution <WxH>   (default: 1920x1080)
      --format <fmt>       nv12 / raw10 / raw16 (default: nv12)
      --pool-size <n>      버퍼 풀 크기 (default: 8)
      --fps <n>            (default: 30)
      --duration-sec <sec> (default: 30)
    camera capture       고해상도 버스트 캡처
      --resolution <WxH>   (default: 4000x3000)
      --burst-count <n>    (default: 10)
    camera switch        해상도/모드 전환
      --resolutions <list> (e.g. "1920x1080,4000x3000,1920x1080")
    camera multi-stream  동시 스트림
      --streams <list>     (e.g. "1920x1080:nv12,1280x720:nv12,640x480:nv12")

    display flip         double/triple buffer 순환
      --resolution <WxH>   (default: 1440x3200)
      --buffers <n>        2 / 3 (default: 3)
      --fps <n>            (default: 60)
      --duration-sec <sec> (default: 30)
    display rotation     화면 회전 전환
      --cycles <n>         회전 횟수 (default: 20)
    display multi-layer  다중 레이어 composition
      --layers <n>         (default: 6)

    codec decode         DPB 풀 + 프레임 순환
      --resolution <WxH>   (default: 3840x2160)
      --dpb-size <n>       참조 프레임 수 (default: 16)
      --fps <n>            (default: 30)
      --duration-sec <sec> (default: 30)
    codec adaptive       적응형 스트리밍 해상도 전환
      --resolutions <list> (e.g. "1280x720,1920x1080,3840x2160,1920x1080")
    codec transcode      동시 decode + encode

    gpu app-launch       앱 시작 burst 할당
      --buffer-count <n>   (default: 50)
    gpu app-switch       앱 전환 (해제→재할당)
      --switch-count <n>   (default: 10)
    gpu game-texture     텍스처 스트리밍 (load/evict)
      --texture-size <KB>  (default: 1024)
      --pool-size <n>      (default: 100)
      --evict-pct <n>      evict 비율 (default: 30)

    pipeline camera-preview  camera + display + GPU 동시
    pipeline video-call      camera + encode + decode + display
    pipeline ai-camera       camera + NPU + display
    pipeline heavy           전체 최대 부하

    pipeline 공통 옵션:
      --workload <role>:<heap>   워크로드별 힙 지정 (반복 가능)
                                 role: camera, display, gpu, npu, codec
                                 미지정 role은 --heap 값을 fallback
      예시:
        dhp scenario pipeline ai-camera \
          --heap system \
          --workload camera:samsung_camera_heap \
          --workload npu:npu_bulk_heap

  all                    전체 테스트 순차 실행

  sysfs-dump             sysfs/procfs 스냅샷 단독 수집
```

---

## ioctl 정의

커널 헤더 기준 (`include/uapi/linux/dma-heap.h`, `include/uapi/linux/dma-buf.h`):

| ioctl | type | nr | direction | struct |
|---|---|---|---|---|
| `DMA_HEAP_IOCTL_ALLOC` | `'H'` | 0x00 | WR | `dma_heap_allocation_data` |
| `DMA_BUF_IOCTL_SYNC` | `'b'` | 0x00 | WR | `dma_buf_sync` |
| `DMA_BUF_SET_NAME_B` | `'b'` | 0x01 | W | `const char *` |
| `DMA_BUF_IOCTL_EXPORT_SYNC_FILE` | `'b'` | 0x02 | WR | `dma_buf_export_sync_file` |
| `DMA_BUF_IOCTL_IMPORT_SYNC_FILE` | `'b'` | 0x03 | WR | `dma_buf_import_sync_file` |

`nix::ioctl_readwrite!` / `ioctl_write_ptr!` 매크로로 정의.

---

## 의존성

| 크레이트 | 용도 |
|---|---|
| `nix` (0.29+) | ioctl, mmap, close, lseek, dup |
| `clap` (4.x, derive) | CLI 파싱 |
| `serde` + `serde_json` | 결과 JSON 출력 |
| `libc` | 보조 상수 (O_CLOEXEC 등) |
| `tracing` (0.1) | 구조화 로깅 |
| `tracing-subscriber` (0.3) | 로그 출력 포맷터 |

외부 C 라이브러리 의존 없음. pure Rust + syscall wrapper.

---

## 구조화 트레이싱 (Structured Tracing)

`tracing` 크레이트 기반 구조화 로깅. `--verbose` / `-v` 로 레벨 제어.

### 레벨 기준

| 레벨 | 용도 | 예시 |
|------|------|------|
| `ERROR` | 복구 불가능한 실패 | ioctl errno, 힙 open 실패 |
| `WARN` | 예상 가능한 이상 | 비정상 크기 할당, 리소스 누수 의심 |
| `INFO` | 주요 동작 마일스톤 | 테스트 시작/완료, 결과 요약 |
| `DEBUG` | 상세 동작 추적 | alloc/mmap/sync/close 개별 호출, fd 값, 크기 |
| `TRACE` | 최저 수준 세부사항 | Drop 경로, sync flags 비트값 |

### CLI 플래그

| 플래그 | 레벨 |
|--------|------|
| (없음) | `WARN` |
| `-v` | `INFO` |
| `-vv` | `DEBUG` |
| `-vvv` | `TRACE` |

`--trace`는 Perfetto atrace marker 전용 (`trace.rs`). `tracing` 크레이트와 독립.

### 필드 컨벤션

| 필드명 | 타입 | 설명 |
|--------|------|------|
| `heap` | `&str` | 힙 이름 |
| `fd` | `RawFd` | 파일 디스크립터 |
| `size` / `len` | `u64` / `usize` | 바이트 단위 크기 |
| `flags` | `u64` / `u32` | ioctl 플래그 (hex 포맷) |
| `name` | `&str` | dma-buf 디버그 이름 |
| `elapsed_us` | `u64` | 경과 시간 (마이크로초, perf 전용) |

### span 컨벤션

- 테스트 함수 단위: `#[tracing::instrument]` 또는 수동 `info_span!("test_name")`
- cmd 모듈 진입: `info_span!("basic")`, `info_span!("edge")` 등
- 개별 ioctl 호출: span 없이 event만 (오버헤드 최소화)

### 테스트에서의 tracing

- 단위 테스트에서 subscriber 초기화 불필요 (이벤트 무시됨)
- 필요 시 `tracing_subscriber::fmt().with_test_writer().init()` 사용

---

## 테스트 시나리오 전체 목록

### 1단계: 기본 기능 검증 (`cmd/basic.rs`)

#### `test_alloc_and_map()`
- 크기별 alloc → mmap → SYNC_START(WRITE) → pattern write → SYNC_END → SYNC_START(READ) → read verify → SYNC_END → munmap → close
- 크기 세트: PAGE_SIZE, 32K, 48K, 64K, 128K, 1M, 2M
- cached/uncached 양쪽 (`--heap system` / `--heap system-uncached`)

#### `test_alloc_zeroed()`
- **libdmabufheap Zeroed 테스트 차용**
- 16개 fd를 연속 alloc → mmap → 0xaa 패턴 write → close
- 동일 크기 재할당 → 전 바이트 0 확인
- heap의 zeroed page 반환 보안 요구사항 검증

#### `test_repeated_alloc()`
- **libdmabufheap RepeatedAllocate 차용**
- 1024회 반복 alloc/close 루프 (크기별)
- sysfs 활성 시 루프 전후 버퍼 수 변화 검증 (릭 감지)

#### `test_llseek_size()`
- `llseek(fd, 0, SEEK_END)` → size 확인 → `SEEK_SET(0)` 복원
- 할당 요청 크기와 llseek 결과 비교

---

### 2단계: sync_file 및 경계 조건

#### `cmd/sync_file.rs`

##### `test_export_sync_file()`
- alloc → `DMA_BUF_IOCTL_EXPORT_SYNC_FILE` (READ/WRITE 플래그)
- 반환된 sync_file fd 유효성 확인
- poll()로 즉시 signaled 확인 (attach된 fence 없으므로)

##### `test_import_sync_file()`
- signaled sync_file 생성 → `DMA_BUF_IOCTL_IMPORT_SYNC_FILE`
- import 후 poll(POLLOUT) 상태 확인

#### `cmd/edge.rs`

##### `test_concurrent_alloc()`
- **libdmabufheap ConcurrentAccessTest 차용**
- 100 스레드 동시 alloc → mmap → sync → munmap → close
- `std::thread::scope` 사용
- 각 스레드에서 cached + uncached 양쪽

##### `test_dup_fd()`
- alloc → dup(fd) → 원본 close → dup fd로 mmap+read 정상 확인
- dup fd close → sysfs에서 해당 버퍼 소멸 확인

##### `test_set_name()`
- `DMA_BUF_SET_NAME_B` → `/proc/<pid>/fdinfo/<fd>` 에서 이름 확인

---

### 3단계: 성능 측정 (`cmd/perf.rs`)

#### `bench_alloc_only()`
- **dmabuf-heap-bench 패턴 차용 + 확장**
- alloc-only latency (ioctl 호출 ~ fd 반환)
- 워밍업 후 N회 반복 → min/max/avg/p50/p95/p99

#### `bench_full_pipeline()`
- alloc + mmap + sync + write + unmap 전체 경로 latency

#### `bench_close()`
- close (해제 경로, pool 반환 포함) latency

#### `bench_order_boundary()` — 커스텀 힙 전용
- 크기 스윕: 4K, 8K, 16K, 32K, 48K, **60K, 64K, 68K**, 128K, 256K, 512K, 1M, 2M, 4M, 8M
- 각 크기별 N회 반복 → latency 분포
- **핵심 관찰**: 64K 경계 전후 latency 급변 여부, fallback 발생 시 추가 지연

#### `bench_fallback_path()` — 커스텀 힙 전용
- 64KB 버퍼를 대량 할당 (close 안 함) → order-4 pool 소진
- 추가 64KB 할당 시도 → fallback 경로 진입 여부
- latency 측정 + `/proc/buddyinfo` order-4 칼럼 변화 관찰

#### `bench_internal_frag()` — 커스텀 힙 전용
- 비정렬 크기 할당: 1 byte, 4095, 4097, 65535, 65537, 100000 등
- `llseek(SEEK_END)`로 실제 할당 크기 확인
- 요청 대비 실제 크기 비율 → 내부 단편화율 산출
- sysfs `size`와 llseek 결과 대조

---

### 메모리 압박 (`cmd/pressure.rs`)

#### `test_gradual_exhaust()`
- 고정 크기(1MB) 버퍼를 계속 할당 (fd 보관, close 안 함)
- 각 할당마다 latency 기록
- ENOMEM 시점까지 총 할당량 기록
- 병렬로 `/proc/buddyinfo`, `/proc/meminfo` MemFree/MemAvailable 스냅샷 수집

#### `test_recovery()`
- `test_gradual_exhaust`에서 ENOMEM 도달 후
- 보유 버퍼의 50% close → 재할당 시도 → 성공 여부 + latency
- 커스텀 힙의 pool 회수/재활용 경로 동작 확인

#### `test_pressure_concurrent()`
- 워커 스레드: `mmap(MAP_ANONYMOUS|MAP_POPULATE)` 으로 대량 메모리 점유
- 테스트 스레드: 커스텀 힙 할당 반복
- 압박 스레드 수 / 점유량 파라미터화

---

### 단편화 관찰 (`cmd/fragmentation.rs`)

#### `test_buddyinfo_track()`
- `/proc/buddyinfo` 파싱: zone별 order 0~10 free chunk 수
- 테스트 전 스냅샷 → 대량 alloc/free 사이클 → 테스트 후 스냅샷
- **핵심 지표**: order-4(64KB) 이상 free chunk 수의 감소 패턴
- 고order가 0 수렴 → 단편화 심각 → 커스텀 힙의 compaction 유도 능력 평가

#### `test_interleave_pattern()`
- Phase 1: 100개 버퍼 할당 (크기 혼합: 4K, 64K, 1M)
- Phase 2: 짝수 인덱스만 close (홀수 유지)
- Phase 3: 해제된 것과 다른 크기로 재할당
- Phase 4: 전체 close
- 각 phase에서 buddyinfo + sysfs 스냅샷

#### `test_pagetypeinfo_track()`
- `/proc/pagetypeinfo`에서 Movable/Unmovable/CMA 타입별 free page 분포 추적
- CMA 기반 힙이면 CMA 행, buddy 기반이면 Movable 행이 핵심

---

### Pool/Cache 동작 (`cmd/pool.rs`)

#### `test_pool_warmup()`
- Cold 상태: 부팅 직후 또는 pool flush 후 첫 N회 할당 latency
- Warm 상태: 동일 크기 alloc→close를 100회 수행 후 측정
- Cold vs Warm latency 비교 → pool 효과 정량화

#### `test_pool_drain()`
- 64KB × 1000회 alloc→close (pool 채움)
- `echo 3 > /proc/sys/vm/drop_caches` (page cache flush)
- 직후 64KB 할당 latency → pool 유지 여부 확인
- `echo 1 > /proc/sys/vm/compact_memory` 후 동일 측정

#### `test_size_switch()`
- Phase 1: 64KB × 500회 alloc→close (pool을 64KB로 채움)
- Phase 2: 4KB × 500회 alloc→close (크기 전환)
- Phase 3: 다시 64KB × 500회
- 각 phase 첫 10회 vs 마지막 10회 latency 비교

#### `test_release_order()`
- 100개 버퍼 할당 → LIFO/FIFO/랜덤 순서 close → 다시 100개 할당 latency 비교
- Pool이 stack 기반이면 LIFO 유리, queue 기반이면 FIFO 유리

#### `test_deferred_free()`
- 대량 할당 후 전체 close → 즉시 `/proc/meminfo` MemFree 확인
- MemFree 미증가 시 deferred free 동작 중
- 일정 대기 후 재확인 → 실제 해제 시점 측정

---

### 워크로드 시뮬레이션 (`cmd/scenario/`)

각 시나리오는 실제 디바이스 서브시스템이 dma-heap에 가하는 할당/해제 패턴을 유저스페이스에서 모사한다. 공통 추상화 패턴을 재활용하여 구현.

**공통 패턴 (scenario/mod.rs)**:

| 패턴 | 설명 | 사용 시나리오 |
|---|---|---|
| `BufferPool` | N개 고정 크기 버퍼 할당 → 인덱스로 순환 사용 | camera preview, display flip, codec DPB |
| `BulkAlloc` | 일괄 할당 → 일괄 해제 | camera capture, model load, app launch |
| `SizeSwitch` | 해제 → 다른 크기 재할당 | camera switch, codec adaptive, display rotation |
| `MixedAlloc` | 다양한 크기가 동시 존재 | gpu texture, pipeline, multi-stream |
| `LongHold` | 장기 점유 + 소규모 반복 | NPU inference, video playback |

---

#### NPU (`cmd/scenario/npu.rs`)

NPU가 dma-heap에 가하는 할당 패턴을 유저스페이스에서 모사. 실제 DMA 전송이나 디바이스 동작은 불가하나, 할당/해제 패턴과 메모리 압박 특성은 재현 가능.

#### `npu_model_load()` — 모델 로딩
- Phase 1: 대용량 버퍼 할당 (단일 또는 chunk 분할)
  - 예: 64MB × 8 = 512MB, 또는 512MB 단일
  - alloc latency 기록 + mmap → SYNC_START(WRITE) → 패턴 write → SYNC_END
  - fd 보관 (해제 안 함)
- Phase 2: 모델 로드 상태에서 시스템 상태 수집
  - buddyinfo, meminfo, sysfs 스냅샷
- Phase 3: 모델 유지한 채 소규모 추가 할당 가능 여부
  - 4KB, 64KB, 1MB 시도 → 성공/실패 + latency

#### `npu_inference_loop()` — 추론 반복
- Setup: 모델 버퍼 할당 (백그라운드 점유)
- Loop (N회, 간격 조절):
  1. 입력 버퍼 alloc (예: 224×224×3×fp32 = ~600KB)
  2. mmap → SYNC_START(WRITE) → write → SYNC_END
  3. 출력 버퍼 alloc (예: 1000×fp32 = 4KB ~ 수 MB)
  4. simulated inference delay (usleep)
  5. 출력 SYNC_START(READ) → read → SYNC_END
  6. 입출력 munmap → close
  7. per-iteration latency 기록
- Teardown: 모델 close
- **핵심 관찰**: pool warming에 의한 latency 안정화, p99 스파이크 패턴

#### `npu_model_switch()` — 모델 전환
- Phase 1: 모델 A 로드 (256MB)
- Phase 2: 모델 A 유지한 채 모델 B 로드 (512MB) → 압박 상태 할당 latency
- Phase 3: 모델 A 해제 → close latency + MemFree 회복 속도
- Phase 4: 모델 C 로드 (모델 A와 동일 크기) → 해제 공간 재활용 여부, pool/cache 효과
- 각 phase에서 buddyinfo 추적

#### `npu_sustained_inference()` — 장시간 안정성
- 모델 로드 후 inference_loop를 수천~수만 회 반복
- 매 N회(snapshot-interval)마다:
  - rolling window latency 통계 (min/avg/p50/p95/p99)
  - buddyinfo + meminfo 스냅샷
  - sysfs dmabuf 버퍼 수 추이 (릭 감지)
- **핵심 관찰**: latency drift (시간 경과에 따른 성능 저하), 단편화 누적, fd/mmap 릭

#### `npu_concurrent_clients()` — 멀티 클라이언트
- N 스레드가 각각 독립 모델 로드 + 동시 inference_loop
- 관찰: 스레드 간 latency 간섭, 총 메모리 사용량, 동시 할당 경합에 의한 tail latency

#### `npu_pressure_inference()` — 압박 하 추론
- 시스템 메모리 70~90% 점유 상태에서 모델 로드 + inference loop
- reclaim/compaction 발생 시 latency 스파이크 관찰
- `/proc/vmstat` compact_stall, pgalloc_* 변화 추적

---

#### Camera (`cmd/scenario/camera.rs`)

V4L2 캡처 파이프라인의 dma-buf 할당 패턴 모사.

##### `camera_preview()` — 프리뷰 버퍼 풀 순환
- 버퍼 크기 계산: W × H × bpp (NV12 = 1.5, RAW10 = 1.25, RAW16 = 2.0)
- N개(pool-size) 일괄 할당 → mmap
- fps 주기로 순환: SYNC_START(WRITE) → write → SYNC_END → 다음 버퍼
- duration 경과 후 전체 munmap → close
- **핵심 관찰**: 순환 중 latency 안정성, pool 크기 부족 시 추가 alloc 필요 여부

##### `camera_capture()` — 고해상도 버스트 캡처
- 고해상도 버퍼 (예: 4000×3000×RAW16 = ~24MB) × burst-count 연속 할당
- 각 alloc latency 기록 — burst 중 뒤쪽 프레임에서 latency 증가 여부
- 전체 close → 해제 latency

##### `camera_switch()` — 해상도/모드 전환
- 해상도 A로 풀 할당 → 전체 해제 → 해상도 B로 재할당 → 반복
- 전환마다 alloc latency + buddyinfo 스냅샷
- **핵심 관찰**: 해제 후 즉시 다른 크기 재할당 시 pool 재활용 효율

##### `camera_multi_stream()` — 동시 스트림
- 복수 스트림을 독립 스레드로 동시 실행 (예: preview 1080p + record 720p + AI 480p)
- 각 스트림이 독립 풀 운영
- 관찰: 스트림 간 할당 경합, 총 메모리 사용량

---

#### Display (`cmd/scenario/display.rs`)

DRM/compositor 프레임버퍼 패턴 모사.

##### `display_flip()` — double/triple buffer 순환
- 해상도 기반 버퍼 크기 (W × H × 4 ARGB8888)
- 2~3개 버퍼 할당 → vsync 주기(1/fps)로 순환
- 각 프레임: SYNC_START(WRITE) → write → SYNC_END
- **핵심 관찰**: vsync 주기 내 alloc/sync 완료 여부 (jank 예측)

##### `display_rotation()` — 화면 회전
- portrait (W×H) 풀 전체 해제 → landscape (H×W) 풀 재할당 → 반복
- 전환 latency = 해제 + 재할당 총 시간

##### `display_multi_layer()` — 다중 레이어
- N개 레이어 × 각각 다른 크기 (전체화면, status bar, nav bar, overlay 등)
- 일괄 할당 → 순환 사용 → 일괄 해제

---

#### Video Codec (`cmd/scenario/codec.rs`)

H.264/H.265 디코더/인코더의 DPB 및 비트스트림 버퍼 패턴 모사.

##### `codec_decode()` — DPB 풀 + 프레임 순환
- DPB 버퍼: W × H × 1.5 (NV12) × dpb-size개 일괄 할당
- fps 주기로 순환 사용 (SYNC_START/END)
- **핵심 관찰**: 4K×16장 = ~192MB 일괄 할당 latency, 순환 중 안정성

##### `codec_adaptive()` — 적응형 스트리밍 해상도 전환
- 해상도 목록 순회: 각 해상도에서 DPB 전체 해제 → 새 해상도로 재할당
- 실제 adaptive bitrate 스트리밍 시나리오
- **핵심 관찰**: DPB 재구성 latency, 전환 중 일시적 메모리 피크 (old + new 공존)

##### `codec_transcode()` — 동시 decode + encode
- 디코더 DPB 풀 + 인코더 입력/출력 풀 동시 운영
- 디코더 출력 → 인코더 입력으로 전달 (크기 동일, fd 재활용 시뮬레이션)

---

#### GPU Rendering (`cmd/scenario/gpu.rs`)

Gralloc/BufferQueue 패턴 모사.

##### `gpu_app_launch()` — 앱 시작 burst 할당
- 다양한 크기의 버퍼를 짧은 시간에 대량 할당 (텍스처, 렌더타겟, 오프스크린)
- 크기 분포: 4KB~16MB 범위에서 랜덤 또는 프리셋
- burst 완료까지 총 시간 + 개별 alloc latency

##### `gpu_app_switch()` — 앱 전환
- 앱 A 버퍼 전체 해제 → 앱 B 버퍼 할당 → 반복
- 해제→재할당 전환 속도 측정
- buddyinfo로 해제 후 단편화 상태 확인

##### `gpu_game_texture()` — 텍스처 스트리밍
- 텍스처 풀 할당 (예: 1MB × 100개)
- 주기적으로 evict-pct만큼 해제 → 동일 크기 재할당 (LRU eviction 패턴)
- pool hit 효과, 단편화 누적 관찰

---

#### 복합 파이프라인 (`cmd/scenario/pipeline.rs`)

복수 서브시스템이 동시에 dma-heap을 사용하는 실제 시나리오 모사. 각 서브시스템을 독립 스레드로 실행.

**워크로드별 힙 지정**: `--workload <role>:<heap>` 옵션으로 각 워크로드가 사용할 힙을 개별 지정 가능. 미지정 워크로드는 공통 `--heap` 값을 fallback. 이를 통해 멀티 힙 환경(예: camera는 전용 CMA 힙, NPU는 bulk 힙, display는 system 힙)을 단일 명령으로 테스트할 수 있다.

```
dhp scenario pipeline heavy \
  --heap system \
  --workload camera:samsung_camera_heap \
  --workload codec:system \
  --workload npu:npu_bulk_heap \
  --workload display:system-uncached \
  --workload gpu:system
```

##### `pipeline_camera_preview()`
- camera preview (1080p, 30fps) + display flip (60fps) + GPU composition
- 3개 스레드 동시 실행, 각각 독립 풀 운영

##### `pipeline_video_call()`
- camera capture (720p) + codec encode + codec decode + display flip
- 4개 스레드, 총 메모리 사용량 + 개별 latency 추적

##### `pipeline_ai_camera()`
- camera preview + NPU inference loop + display flip
- camera 출력 크기 = NPU 입력 크기로 맞춤

##### `pipeline_heavy()`
- 전체 최대 부하: camera + codec + GPU + NPU + display 동시
- 시스템 한계 탐색, ENOMEM 발생 시점 기록
- **핵심 관찰**: 어느 서브시스템에서 먼저 할당 실패가 발생하는지

---

### 네거티브 테스트 (`cmd/negative.rs`)

에러 경로, 잘못된 입력, 경합 조건을 체계적으로 검증. 각 테스트는 커널이 올바른 errno를 반환하고 크래시/패닉 없이 처리하는지를 확인한다.

#### 레이어 1: Heap 디바이스 접근

##### `neg_open_nonexistent_heap()`
- `/dev/dma_heap/this_heap_does_not_exist` open → `ENOENT`

##### `neg_open_not_a_heap()`
- `/dev/dma_heap/` 디렉토리 자체를 open → ioctl 실패
- `/dev/null`에 `DMA_HEAP_IOCTL_ALLOC` → `ENOTTY`
- 일반 파일 fd에 heap ioctl → `ENOTTY`

##### `neg_open_permission_denied()`
- SELinux enforcing + 비특권 shell에서 open → `EACCES`
- root 실행 시 skip

#### 레이어 2: 할당 ioctl

##### `neg_alloc_zero_size()`
- `len = 0` → `EINVAL`
- 커널 `dma_heap_ioctl_allocate()`에서 len == 0 체크

##### `neg_alloc_overflow_size()`
- `len = u64::MAX`, `len = u64::MAX - PAGE_SIZE + 1`
- `PAGE_ALIGN` 오버플로우 유도 → `EINVAL` 또는 `ENOMEM`

##### `neg_alloc_huge_size()`
- 물리 메모리 × 2 크기 → `ENOMEM`
- **실패 후 시스템 안정성 확인**: 추가 정상 크기 alloc이 성공하는지 검증

##### `neg_alloc_invalid_fd_flags()`
- `fd_flags`에 `O_APPEND`, `O_TRUNC`, `O_CREAT` 등 → `EINVAL`
- 커널은 `O_CLOEXEC | O_ACCMODE` 외 거부

##### `neg_alloc_invalid_heap_flags()`
- `heap_flags != 0` → `EINVAL`
- 현재 커널에서 reserved 필드 역할

##### `neg_alloc_on_closed_fd()`
- heap fd close 후 ioctl → `EBADF`

##### `neg_wrong_ioctl_on_heap()`
- heap fd에 `DMA_BUF_IOCTL_SYNC` → `ENOTTY` (heap fd ≠ dma-buf fd)

##### `neg_alloc_reserved_nonzero()`
- `dma_heap_allocation_data`의 reserved 필드에 비0 값 → `EINVAL`
- 커널 ABI 안정성 검증

##### `neg_ioctl_garbage_data()`
- 구조체 전체를 0xFF로 채워서 ioctl → `EINVAL`

#### 레이어 3: dma-buf fd 연산

##### `neg_sync_invalid_flags()`
- flags = 0 (START/END 미지정) → `EINVAL`
- START + END 동시 설정 → `EINVAL`
- READ/WRITE 없이 START만 → `EINVAL`

##### `neg_sync_on_non_dmabuf_fd()`
- 일반 파일 fd / pipe fd에 `DMA_BUF_IOCTL_SYNC` → `ENOTTY`

##### `neg_sync_end_without_start()`
- mmap 후 SYNC_START 없이 SYNC_END → 에러 또는 no-op 확인

##### `neg_sync_double_start()`
- SYNC_START(READ) → SYNC_START(WRITE) (END 없이 연속) → 커널 동작 확인

##### `neg_llseek_invalid_whence()`
- `lseek(fd, 0, SEEK_CUR)` → `EINVAL` (SEEK_SET/SEEK_END만 지원)
- `lseek(fd, 1, SEEK_SET)` → `EINVAL` (offset != 0)
- `lseek(fd, 1, SEEK_END)` → `EINVAL` (offset != 0)

##### `neg_set_name_too_long()`
- `DMA_BUF_NAME_LEN` (32바이트) 초과 → `ENAMETOOLONG`

##### `neg_set_name_null()`
- NULL 포인터 → `EFAULT`

##### `neg_read_write_on_dmabuf()`
- dma-buf fd에 `read()` / `write()` 시스콜 → 예상: `EINVAL` 또는 `-1`

#### 레이어 4: mmap 관련

##### `neg_mmap_beyond_size()`
- 버퍼 크기보다 큰 length로 mmap → `EINVAL`

##### `neg_mmap_invalid_offset()`
- pgoff != 0으로 mmap → `EINVAL` 또는 구현 의존

##### `neg_mmap_close_then_access()`
- fd close 후 기존 mmap 영역 접근 → 정상 접근 가능해야 함 (close ≠ munmap)
- 커스텀 힙이 이를 위반하면 구현 버그

##### `neg_mmap_prot_exec()`
- `PROT_EXEC`으로 mmap → 거부 가능

##### `neg_access_after_munmap()`
- munmap 후 해당 주소 접근 → SIGSEGV
- signal handler로 포착하여 올바른 시그널인지 확인

#### 레이어 5: sync_file 관련

##### `neg_export_sync_file_invalid_flags()`
- flags = 0 → `EINVAL`
- 유효하지 않은 비트 → `EINVAL`

##### `neg_import_sync_file_bad_fd()`
- fd = -1 또는 9999 → `EBADF`

##### `neg_import_sync_file_non_sync()`
- 일반 파일 fd를 sync_file로 import → `EINVAL`

##### `neg_import_sync_file_invalid_flags()`
- flags = 0 → `EINVAL`

##### `neg_export_on_closed_dmabuf()`
- close된 dma-buf fd에 export → `EBADF`

#### 레이어 6: 리소스 누수 및 라이프사이클

##### `neg_fd_leak_detection()`
- N개 alloc 후 의도적 close 누락 → `/proc/self/fd/` 카운트 증가 확인
- 전체 close → fd 카운트 원복 확인

##### `neg_mmap_leak_detection()`
- mmap 후 munmap 누락 → `/proc/self/maps`에서 매핑 잔존 확인
- close만으로 매핑이 자동 해제되지 않음 확인

##### `neg_cloexec_inheritance()`
- `O_CLOEXEC` 포함 alloc → fork+exec → 자식에서 fd 접근 불가 확인
- `O_CLOEXEC` 미포함 시 자식에서 fd 접근 가능 확인 (보안 위험 시나리오)

##### `neg_rapid_alloc_close_no_leak()`
- 10000회 alloc→close 고속 반복
- 전후 `/proc/self/fd/` 카운트 동일 + sysfs 버퍼 카운트 원복

#### 레이어 7: 동시성 / 경합 조건

##### `neg_concurrent_close_same_fd()`
- 두 스레드에서 동일 fd 동시 close → 하나 성공, 하나 `EBADF`
- double-close가 크래시 유발하지 않는지 확인

##### `neg_concurrent_sync_and_close()`
- 스레드 A: mmap + SYNC_START → sleep → SYNC_END
- 스레드 B: 중간에 fd close
- 패닉 없이 에러 반환 확인

##### `neg_concurrent_mmap_munmap()`
- 스레드 A: mmap → write
- 스레드 B: 동일 fd 매핑 munmap
- 크래시 없이 graceful 처리 확인

##### `neg_concurrent_alloc_exhaust()`
- N 스레드 동시 대량 할당 → 일부 `ENOMEM`
- ENOMEM 스레드 정상 종료 + 성공 스레드 데이터 오염 없음 확인

---

## Perfetto 연동

### 동작 흐름
1. 테스트 앱 시작 시 `/sys/kernel/tracing/trace_marker` fd를 한 번 open
2. 각 측정 구간에서 `B|<pid>|<section_name>` / `E|<pid>` write
3. 카운터: `C|<pid>|<counter_name>|<value>`
4. 별도 터미널에서 perfetto 수집 (또는 background)
5. `--trace` 비활성 시 no-op

### Perfetto Config 예시
```
buffers { size_kb: 65536 }
data_sources {
  config {
    name: "linux.ftrace"
    ftrace_config {
      ftrace_events: "print"
      ftrace_events: "dma_heap/dma_heap_alloc"
      ftrace_events: "dma_buf/dma_buf_export"
      atrace_apps: "*"
    }
  }
}
```

userspace trace 구간 + 커널 tracepoint를 하나의 타임라인에서 대조 가능.

---

## procfs 수집 모듈 (`procfs.rs`)

| 파일 | 수집 항목 | 용도 |
|---|---|---|
| `/proc/buddyinfo` | zone별 order 0~10 free chunk 수 | 단편화 추적 |
| `/proc/pagetypeinfo` | migration type별 free page 분포 | CMA/Movable 추적 |
| `/proc/meminfo` | MemFree, MemAvailable, CmaFree, CmaTotal | 메모리 압박 상태 |
| `/proc/vmstat` | compact_*, pgalloc_*, pgfree_* | compaction/alloc 통계 |

---

## 기존 테스트 대비 차별점

| 영역 | kernel selftest | libdmabufheap test | VTS | **본 앱** |
|---|---|---|---|---|
| 기본 alloc/mmap/sync | O | O | O | O |
| zero-fill 검증 | O | O | X | O |
| sync_file export/import | X | X | X | **O** |
| edge case | X | 일부 | X | **O** |
| 커스텀 힙 이름 지정 | X | X | X | **O** |
| latency (p50/p95/p99) | X | X | X | **O** |
| order 경계 프로파일링 | X | X | X | **O** |
| 메모리 압박 테스트 | X | X | X | **O** |
| 단편화 추적 (buddyinfo) | X | X | X | **O** |
| pool 동작 검증 | X | X | X | **O** |
| 워크로드 시뮬레이션 (NPU/camera/display/codec/GPU/pipeline) | X | X | X | **O** |
| 네거티브 (에러경로/경합) | X | X | X | **O** |
| Perfetto trace 연동 | X | X | X | **O** |
| sysfs/procfs 수집 | X | X | X | **O** |
| 독립 바이너리 (NDK) | X | X | X | **O** |

---

## libdmabufheap에서 차용한 패턴

| 원본 테스트 | 차용 위치 | 비고 |
|---|---|---|
| `DmaBufHeapTest::Zeroed` | `cmd/basic.rs::test_alloc_zeroed()` | 0xaa → 재할당 → 0 확인 (16개 fd) |
| `DmaBufHeapTest::RepeatedAllocate` | `cmd/basic.rs::test_repeated_alloc()` | 1024회 반복, 릭 감지 추가 |
| `DmaBufHeapTest::Allocate` | `cmd/basic.rs::test_alloc_and_map()` | 크기 세트 확장 (order-4 경계 추가) |
| `DmaBufHeapConcurrentAccessTest` | `cmd/edge.rs::test_concurrent_alloc()` | 100 스레드, cached+uncached |
| `dmabuf-heap-bench` | `cmd/perf.rs::bench_alloc_only()` | 통계 확장 (p50/p95/p99) + trace marker |

ION 호환 코드(`CheckIonSupport`, `MapNameToIonHeap`)는 Android 16+ 대상이므로 전부 제거. `BufferAllocator` 추상화 대신 ioctl 직접 호출.

---

## 호스트 유닛 테스트 및 Mocking 전략

개발 중에는 디바이스 없이 호스트(x86_64 Linux)에서 `cargo test`로 로직을 검증해야 한다. dma-heap/dma-buf ioctl과 `/dev/dma_heap/*` 디바이스는 호스트에 존재하지 않으므로, 커널 인터페이스 계층을 trait으로 추상화하고 mock 구현을 제공한다.

### 추상화 계층 설계

```
src/
├── backend/
│   ├── mod.rs           # trait 정의
│   ├── real.rs          # 실제 ioctl/mmap (디바이스 빌드)
│   └── mock.rs          # mock 구현 (호스트 테스트)
```

**`backend/mod.rs`** — 핵심 trait:

- `trait HeapBackend`: heap open, alloc (fd 반환)
- `trait DmaBufBackend`: mmap, munmap, sync, llseek, export/import sync_file, set_name, close

`heap.rs`와 `dmabuf.rs`는 이 trait에 의존하고, 구체 구현은 컴파일 타겟에 따라 선택한다.

### 빌드 타겟 분기

| 타겟 | backend | 동작 |
|---|---|---|
| `aarch64-linux-android` | `real.rs` | 실제 `/dev/dma_heap/*` ioctl |
| `x86_64-unknown-linux-gnu` (테스트) | `mock.rs` | 메모리 기반 시뮬레이션 |

`cfg(target_os = "android")` 또는 `cfg(test)` feature flag로 분기.

### Mock 구현 범위

**`mock.rs`가 시뮬레이션하는 것**:
- `DMA_HEAP_IOCTL_ALLOC`: 요청 크기를 `PAGE_ALIGN` → `Vec<u8>` 할당 → mock fd (파일이 아닌 내부 인덱스) 반환
- `DMA_BUF_IOCTL_SYNC`: START/END 상태 추적, flags 유효성 검증 (실제 캐시 관리 없음)
- `mmap`: 내부 `Vec<u8>`의 슬라이스 포인터 반환
- `llseek`: 할당된 크기 반환
- `DMA_BUF_SET_NAME_B`: 이름 저장, 길이 검증
- `export/import sync_file`: mock sync_file fd 반환, flags 검증
- `close`: 내부 버퍼 해제, refcount 관리

**Mock이 검증하는 것**:
- ioctl 구조체 필드 유효성 (flags, reserved, size)
- 호출 순서 (예: SYNC_END before SYNC_START → 에러)
- errno 반환 경로 (EINVAL, ENOMEM, EBADF 등)
- 리소스 해제 완전성 (Drop 시 미해제 fd 경고)

**Mock이 시뮬레이션하지 않는 것** (디바이스 테스트에서만 확인):
- 실제 커널 메모리 할당 경로 (buddy allocator, CMA, page pool)
- DMA 캐시 coherency
- 커널 내부 locking / 동시성
- 실제 latency 특성

### 테스트 구조

```
tests/                          # integration tests (호스트)
├── test_basic.rs               # basic 로직 검증 (mock backend)
├── test_negative.rs            # 네거티브 케이스 errno 검증
├── test_edge.rs                # dup, set_name, concurrent 로직
└── test_scenario_logic.rs      # 시나리오 버퍼 크기 계산, 풀 순환 로직

src/cmd/basic.rs 등 내부:
#[cfg(test)]
mod tests {
    // mock backend 주입하여 유닛 테스트
}
```

### 호스트에서 테스트 가능한 범위

| 영역 | 호스트 유닛 테스트 | 디바이스 테스트 |
|---|---|---|
| ioctl 구조체 직렬화/역직렬화 | **O** | O |
| flags 유효성 검증 로직 | **O** | O |
| errno 분기 처리 | **O** | O |
| CLI 파싱/라우팅 | **O** | O |
| JSON 결과 직렬화 | **O** | O |
| procfs/sysfs 파싱 로직 | **O** (샘플 데이터) | O |
| trace marker 포맷 생성 | **O** | O |
| 시나리오 버퍼 크기 계산 | **O** | O |
| 풀 순환/인터리브 로직 | **O** | O |
| latency 통계 (p50/p95/p99) 계산 | **O** | O |
| 실제 할당 성공/실패 | X | **O** |
| 실제 latency 측정 | X | **O** |
| 커널 에러 경로 (실제 errno) | X | **O** |
| 메모리 압박/단편화 관찰 | X | **O** |

### 개발 워크플로우

```
1. 호스트에서 로직 구현 + cargo test (mock backend)
   → 빠른 피드백 루프, CI 연동 가능

2. cargo build --target aarch64-linux-android --release
   → 디바이스용 바이너리 생성

3. adb push → 디바이스에서 실행
   → 실제 커널 인터페이스 검증
```

호스트 테스트는 **로직 정확성**을, 디바이스 테스트는 **커널 동작 정확성**을 각각 담당한다. 두 계층이 분리되어야 개발 속도와 검증 신뢰성을 동시에 확보할 수 있다.

---

## 빌드 및 배포

Rust 컴파일러가 codegen을 자체 LLVM으로 처리하므로 NDK의 clang을 컴파일러로 쓸 필요는 없다. 단, 최종 링킹 시 bionic libc sysroot + 링커가 필요하므로 NDK 설치 자체는 필요하다. `cargo-ndk`가 `ANDROID_NDK_HOME`에서 링커/sysroot를 자동 감지하므로 `.cargo/config.toml`에 경로를 하드코딩할 필요가 없다.

### 사전 준비

```sh
# Rust 타겟 추가
rustup target add aarch64-linux-android

# cargo-ndk 설치
cargo install cargo-ndk

# NDK 경로 설정 (Android Studio 또는 독립 NDK)
export ANDROID_NDK_HOME=/path/to/android-ndk-r27c
```

### 빌드

```sh
# API level 35 = Android 16
cargo ndk -t arm64-v8a -p 35 build --release
```

출력: `target/aarch64-linux-android/release/dhp`

### 배포 및 실행

```sh
adb push target/aarch64-linux-android/release/dhp /data/local/tmp/
adb shell chmod +x /data/local/tmp/dhp
adb shell su -c /data/local/tmp/dhp all --heap system --trace --sysfs --procfs --output /data/local/tmp/results.json
```

SELinux permissive 또는 userdebug 빌드 필요.

### CI 환경

CI에서는 `cross`를 대안으로 사용 가능 (NDK 포함 Docker 이미지 기반, 호스트 NDK 설치 불필요):

```sh
cargo install cross
cross build --target aarch64-linux-android --release
```

### `.cargo/config.toml`

`cargo-ndk` 사용 시 링커 설정은 불필요. 호스트 테스트와 Android 빌드 모두 추가 설정 없이 동작한다. 수동 설정이 필요한 경우에만:

```toml
[target.aarch64-linux-android]
# cargo-ndk가 자동 처리하므로 보통 불필요
# linker = "... /aarch64-linux-android35-clang"
```

---

## 구현 순서

1. `ioctl/` 정의 + `backend/` trait 설계 + `real.rs` + `mock.rs` 기본 구조
2. `heap.rs` + `dmabuf.rs` (trait 기반 핵심 래퍼)
3. `cmd/basic.rs` + 호스트 유닛 테스트 (mock backend으로 로직 검증)
4. `trace.rs` + `sysfs.rs` + `procfs.rs` (인프라, 파싱 로직은 호스트에서 테스트 가능)
5. `cmd/sync_file.rs` + `cmd/edge.rs`
6. `cmd/negative.rs` (에러 경로 검증 — mock으로 errno 분기 먼저, 이후 디바이스 확인)
7. `cmd/perf.rs` (bench_alloc_only 먼저 → order_boundary 확장)
8. `cmd/pressure.rs` + `cmd/fragmentation.rs` + `cmd/pool.rs`
9. `cmd/scenario/` (npu → camera → codec → display → gpu → pipeline 순)
10. `runner.rs` + JSON 출력 통합
