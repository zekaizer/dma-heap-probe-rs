# Perf 서브커맨드 측정 알고리즘

## 개요

`dhp perf` 서브커맨드는 DMA heap 할당 성능을 정량적으로 측정한다.
7개 벤치마크를 실행하며, 각 벤치마크는 동일한 통계 파이프라인을 거쳐 결과를 산출한다.

```
[samples] → sort → percentiles → variance → IQR filter → LatencyStats
```

---

## 1. 타이밍 메커니즘

### 클럭 소스

```rust
let start = Instant::now();
// ... measured operation ...
let elapsed = start.elapsed().as_micros() as u64;
```

- **`std::time::Instant`**: monotonic clock (`clock_gettime(CLOCK_MONOTONIC)`)
- **해상도**: 플랫폼 의존 (Linux/Android: 일반적으로 1~100ns)
- **저장 단위**: 마이크로초 (`u64`)
- **오버플로우 안전**: `u64` 최대값 ~18.4 × 10^18 µs ≈ 584,542년

### 측정 경계 (Timing Boundary)

각 벤치마크는 서로 다른 구간을 측정한다:

| 벤치마크 | 시작 | 종료 | 포함 연산 |
|----------|------|------|-----------|
| `alloc_only` | `heap.alloc()` 직전 | `alloc()` 반환 직후 | ioctl 호출만 |
| `full_pipeline` | `heap.alloc()` 직전 | `sync_end(READ)` 직후 | alloc → mmap → sync(W) → memwrite → sync(R) |
| `close` | `drop(buf)` 직전 | `drop(buf)` 직후 | munmap + close |
| `order_boundary` | `heap.alloc()` 직전 | `alloc()` 반환 직후 | ioctl 호출만 (15개 사이즈) |
| `pool_warmup` | `heap.alloc()` 직전 | `alloc()` 반환 직후 | cold vs warm 비교 |
| `size_switch` | `heap.alloc()` 직전 | `alloc()` 반환 직후 | 사이즈 전환 영향 |
| `internal_frag` | N/A | N/A | `llseek(SEEK_END)` 크기 비교 (타이밍 없음) |

**주의**: `alloc_only`에서 `drop(buf)`는 타이밍 구간 **밖**에서 실행된다.
`full_pipeline`에서 `drop(buf)`도 타이밍 구간 밖이다.
`close`에서만 `drop(buf)`가 타이밍 구간 **안**에 있다.

---

## 2. 워밍업 (Warmup)

```
for _ in 0..warmup {
    alloc → [pipeline] → drop    // 결과 폐기
}
for _ in 0..iterations {
    measure()                    // 결과 수집
}
```

- **목적**: 커널 페이지 풀, slab 캐시, TLB 프리밍
- **기본값**: `--warmup 10` (CLI 인자로 조정 가능)
- **메커니즘**: 워밍업 반복에서 alloc/free 사이클을 실행하여 커널이 deferred free pool을 채우도록 유도
- **한계**: 워밍업이 충분한지 자동 검증하지 않음 (CV로 간접 확인 가능)

---

## 3. 통계 계산 (`compute_stats`)

### 3.1 정렬

```rust
let mut sorted = samples.to_vec();
sorted.sort_unstable();      // O(n log n), in-place 불안정 정렬
```

- 원본 배열을 보존하기 위해 복사 후 정렬
- `sort_unstable()` 사용: 추가 메모리 할당 없음, 동일값 순서 보장 불필요

### 3.2 평균 (Mean)

```rust
let mean_f = sum as f64 / count as f64;
avg_us = mean_f.round() as u64;        // 반올림 (절삭 아님)
```

- **부동소수점 정밀도**: `f64` (53비트 가수부)로 정확한 평균 계산
- **반올림**: `round()` 사용 — 이전 `sum / count` 정수 나눗셈의 절삭 오류 제거
- **오버플로우**: `sum: u64`는 10^5 iterations × 10^9 µs = 10^14까지 안전 (`u64` 최대 1.8×10^19)

### 3.3 분산 및 표준편차 (Variance / Standard Deviation)

```rust
// Two-pass algorithm
let variance = sorted.iter()
    .map(|&x| (x as f64 - mean_f).powi(2))
    .sum::<f64>() / count as f64;
let stddev = variance.sqrt();
```

**알고리즘 선택: Two-pass vs Welford's**

| 방법 | 장점 | 단점 |
|------|------|------|
| **Two-pass** (채택) | 정수 데이터에 수치적으로 안정, 간단 | 두 번째 패스 필요 |
| Welford's online | 단일 패스 | 부동소수점 누적 오류 가능 |

- **모집단 분산** (N으로 나눔): 벤치마크 반복은 이론적 분포의 표본이 아니라 실제 측정의 전체 집합
- N=100에서 모집단(N) vs 표본(N-1) 표준편차 차이: ~0.5% — 실질적 무의미

### 3.4 변동계수 (Coefficient of Variation, CV)

```rust
cv_pct = stddev / mean * 100.0
```

- **의미**: 평균 대비 편차의 비율 (%)
- **판단 기준**:
  - CV < 5%: 매우 안정적 측정
  - CV 5-15%: 양호
  - CV > 15%: 노이즈 높음, `--iterations` 증가 권장
  - CV > 30%: 외부 간섭 의심 (다른 프로세스, thermal throttling 등)

### 3.5 백분위수 (Percentiles)

```rust
// Nearest-rank method (inclusive, NIST R-2)
fn percentile(sorted: &[u64], p: u32) -> u64 {
    let rank = ceil(p * n / 100);
    sorted[rank - 1]
}
```

**Nearest-rank 방식 선택 이유:**
- 결과가 항상 실제 관측값 — 보간(interpolation) 없음
- 정수 데이터에 적합: 보간 결과가 존재하지 않는 값을 생성하지 않음
- `p99`가 실제로 관측된 worst-case에 가까운 값을 반환

**분수 백분위수** (`p99.9`):

```rust
fn percentile_frac(sorted: &[u64], numer: u64, denom: u64) -> u64 {
    let rank = ceil(numer * n / denom);
    sorted[rank - 1]
}
```

- p99.9 = `percentile_frac(sorted, 999, 1000)`
- N=100 샘플에서 p99.9 = sorted[99] = 최대값 (rank = ceil(99.9) = 100)
- N=1000 이상에서 의미 있는 분별력 제공

**제공 백분위수:** p50 (중앙값), p95, p99, p99.9

### 3.6 백분위수 신뢰구간 (Percentile CI)

p99에 대한 95% 비모수 신뢰구간 (binomial order statistics):

```
rank = ceil(p × n / 100)
se_rank = sqrt(n × p/100 × (1 - p/100))
lower = sorted[max(1, floor(rank - 1.96 × se_rank)) - 1]
upper = sorted[min(n, ceil(rank + 1.96 × se_rank)) - 1]
```

- **분포 무관** (non-parametric): 정규 분포 가정 불필요
- **수학적 성질**: `se_rank`는 p=50에서 최대, p=99에서 최소 → 중앙값 CI가 가장 넓음
- **테이블 표시**: `p99[ci95]` 컬럼 (예: `38[32-45]`)
- **SLA/SLO 활용**: "p99 ≤ 45µs with 95% confidence"

---

## 4. IQR 이상치 탐지 (Outlier Detection)

### 4.1 알고리즘: Tukey's Fence

```
Q1 = percentile(sorted, 25)
Q3 = percentile(sorted, 75)
IQR = Q3 - Q1

lower_fence = Q1 - 1.5 × IQR
upper_fence = Q3 + 1.5 × IQR

outlier := sample < lower_fence OR sample > upper_fence
```

### 4.2 Trimmed Mean

```rust
trimmed_avg = mean(samples WHERE lower_fence ≤ sample ≤ upper_fence)
outlier_count = count(samples WHERE sample IS outlier)
```

### 4.3 설계 결정

| 결정 | 근거 |
|------|------|
| 1.5×IQR (표준) | 3×IQR은 너무 관대, 1×IQR은 너무 공격적 |
| N < 4일 때 비활성 | IQR 계산에 최소 4개 샘플 필요 |
| raw avg와 trimmed avg 모두 보고 | 사용자가 outlier 영향을 직접 비교 가능 |
| 모집단 stddev는 trimmed 아님 | stddev는 전체 분포의 특성을 반영해야 함 |

### 4.4 해석 가이드

```
[system]  perf::alloc_only (us)
           size  min  avg  tavg  sd  p50  p95  p99  p99.9  max  out
             4K    5   12     8   3    8   15   25     42   42    3
```

- `avg=12, tavg=8`: outlier 3개가 평균을 50% 끌어올림
- `tavg`가 `p50`에 가까울수록 분포가 대칭적
- `out > 0`이면 OS 스케줄러 jitter나 인터럽트 간섭이 존재

---

## 5. Throughput 및 신뢰구간 (Throughput & CI)

### 5.1 Throughput (ops/sec)

```
throughput_ops = round(1,000,000 / mean_us)
```

- 평균 지연시간의 역수로 초당 처리량 계산
- **테이블 표시**: `Kops/s` = `throughput_ops / 1000` (천 단위)
- `mean_us = 0`일 때 `throughput_ops = 0` (mock 백엔드 fast path)
- **용도**: 힙 간 성능 비교 ("system: 50Kops/s vs reserved: 30Kops/s")

### 5.2 95% 신뢰구간 (Confidence Interval)

```
ci95_us = ceil(1.96 × stddev / sqrt(n))
```

- **의미**: 진짜 평균은 95% 확률로 `[avg - ci95, avg + ci95]` 범위 안에 존재
- **테이블 표시**: `avg±ci95` (예: `12±2`)
- **z-score 1.96**: 정규분포 95% 양측 임계값
- **sqrt(n) 효과**: 반복 횟수를 4배로 늘리면 CI가 절반으로 줄어듦

**해석 가이드:**

| CI 상대 크기 | 의미 | 조치 |
|-------------|------|------|
| ci95 < avg × 5% | 정밀한 측정 | 그대로 사용 |
| ci95 = avg × 5~20% | 보통 | `--iterations` 증가 고려 |
| ci95 > avg × 20% | 부정확 | `--iterations` 증가 필수 또는 외부 간섭 확인 |

### 5.3 테이블 예시

```
[system]  perf::alloc_only (us)
     size  min  avg±ci95  tavg  sd  p50  p95   p99  p99.9   max  Kops/s  out
       4K    5      12±2     8   8    8   25    38     42    42      83    3
      64K   15     45±5    40  12   40   60    85    110   110      22    2
       1M   80   250±18   220  45  230  350   480    520   520       4    1
```

---

## 6. Size-Latency 선형 회귀 분석

### 6.1 알고리즘: Ordinary Least Squares (OLS)

`alloc_only` 벤치마크 종료 후, (size, avg_us) 쌍에 대해 선형 모델을 적합:

```
latency_us = base_us + slope_us_per_byte × size_bytes
```

**최소제곱법:**

```
slope = Σ(xi - x̄)(yi - ȳ) / Σ(xi - x̄)²
intercept = ȳ - slope × x̄
R² = 1 - SS_res / SS_tot
```

### 6.2 출력 필드

| 필드 | 의미 |
|------|------|
| `base_us` | 사이즈 무관 고정 비용 (y절편) — ioctl 오버헤드, 잠금 경합 등 |
| `us/KB` | KB당 추가 비용 (slope × 1024) — 페이지 할당, zeroing 비용 |
| `R²` | 결정계수 (0~1). 1에 가까울수록 사이즈-지연 관계가 선형적 |

### 6.3 해석 가이드

```
[system]  perf::alloc_model  base_us: 5.2  us/KB: 0.045  R²: 0.987
```

- **R² > 0.95**: 강한 선형 관계 — 지연은 사이즈에 비례
- **R² < 0.5**: 비선형 — buddy allocator order 경계, pool hit/miss 등의 영향
- **base_us 높음**: 고정 오버헤드가 큼 (잠금 경합, slab 경로)
- **us/KB ≈ 0**: 사이즈 무관 할당자 (pool 기반, 사전 할당)
- 최소 2개 사이즈 필요. 3개 이상에서 R²가 의미 있음

---

## 7. Drift Detection (시간적 편향 감지)

측정 도중 지연시간이 체계적으로 변하는지 자동 감지한다.

### 7.1 알고리즘

샘플 인덱스 `i`와 지연시간 `latency[i]`에 대해 선형 회귀:

```
slope = Σ(i - ī)(y_i - ȳ) / Σ(i - ī)²
drift_pct = slope × (n-1) / mean × 100
```

추가로 전반부/후반부 평균을 비교하여 직관적 해석을 제공한다.

### 7.2 출력 조건

`|drift_pct| > 10%`일 때만 경고를 출력한다:

```
[system]  perf::drift_warn (degrading)  drift: +25.3%  1st_half: 45  2nd_half: 58
```

| drift_pct | 의미 | 원인 |
|-----------|------|------|
| > +10% | 후반부가 느림 (degrading) | Thermal throttling, 메모리 압박 증가 |
| < -10% | 후반부가 빠름 (improving) | Warmup 부족, JIT/pool이 아직 안정화 안 됨 |
| -10% ~ +10% | 안정 | 정상적 측정 |

### 7.3 설계 결정

| 결정 | 근거 |
|------|------|
| 마지막(가장 큰) 사이즈만 분석 | 큰 할당이 thermal/pressure 영향에 가장 민감 |
| 임계값 10% | 5% 미만은 통계적 노이즈, 10% 이상은 체계적 편향 |
| 최소 10 샘플 | 그 이하에서는 회귀 slope가 불안정 |
| 경고만 출력 (자동 보정 없음) | 사용자가 원인 판단하여 `--warmup`/`--iterations` 조정 |

---

## 8. 벤치마크별 상세

### 8.1 `bench_alloc_only`

커널 `DMA_HEAP_IOCTL_ALLOC` 호출의 순수 지연시간.

- **측정 대상**: ioctl 시스콜만 (mmap, sync 제외)
- **사이즈**: `--sizes` (기본 4K, 64K, 1M)
- **용도**: 힙 할당자 성능의 기준선
- **회귀 분석**: 테이블 후 `perf::alloc_model` 라인으로 size-latency 모델 출력

### 8.2 `bench_full_pipeline`

실제 사용 시나리오: 할당 → 매핑 → 쓰기 → 읽기 → 해제.

- **측정 구간**: alloc → mmap → sync_start(W) → memwrite → sync_end(W) → sync_start(R) → sync_end(R)
- **memwrite**: `write_bytes(ptr, 0xAA, size)` — 전체 버퍼 쓰기
- **용도**: 캐시 유지보수 비용 포함한 end-to-end 지연
- **Stage Breakdown**: 사이즈별로 4개 단계의 개별 지연시간 + 비율 출력

```
[system]  perf::pipeline_breakdown@64K (us)
            stage  avg     %
            alloc   15  30.0
             mmap    8  16.0
           sync_w   22  44.0
           sync_r    5  10.0
```

| 단계 | 측정 구간 | 포함 연산 |
|------|-----------|-----------|
| `alloc` | t0→t1 | `DMA_HEAP_IOCTL_ALLOC` |
| `mmap` | t1→t2 | `mmap()` syscall |
| `sync_w` | t2→t3 | `sync_start(W)` + `write_bytes` + `sync_end(W)` |
| `sync_r` | t3→t4 | `sync_start(R)` + `sync_end(R)` |

- 타이머 오버헤드: `Instant::now()` 5회 × ~20ns = ~100ns/iter (<1% at µs scale)
- `sync_w`에 `write_bytes`가 포함됨 — 캐시 flush와 메모리 쓰기가 결합된 실제 비용

### 8.3 `bench_close`

버퍼 해제 경로 지연시간.

- **측정 대상**: `drop(DmaBuf)` = munmap + close
- **사전 조건**: 타이밍 전에 alloc 완료
- **용도**: deferred free pool로의 반환 지연, CMA 반환 비용
- **Close/Alloc Ratio** (`perf::close_ratio`): paired alloc+close 측정으로 효율 비교
  - 매 iteration에서 alloc과 close를 개별 타이밍
  - `ratio = close_avg / alloc_avg`
  - `<0.5` fast (deferred free), `0.5–2.0` balanced, `>2.0` expensive (CMA compaction)

```
[system]  perf::close_ratio
           size  alloc  close  ratio  verdict
             4K     12      3   0.25     fast
            64K     45     15   0.33     fast
             1M    250    800   3.20  expensive
```

### 8.4 `bench_order_boundary`

커널 buddy allocator의 order 경계에서 할당 비용 변화 측정.

- **사이즈 범위**: 4K → 8M (15개 포인트, 64K 경계 집중)
- **관찰 포인트**: `49152 (48K)` vs `65536 (64K)` — order 4 경계
- **용도**: buddy allocator의 order 승격/분할 비용 시각화
- **Step Detection**: 인접 사이즈 간 >20% 증가 시 `perf::order_step` 출력
  - `order N`: buddy allocator order (`ceil(log2(pages))`)
  - 크기순이 아닌 증가폭순으로 정렬 — 가장 큰 bottleneck 먼저 표시

```
[system]  perf::order_step (order 5)  from: 64K  to: 128K  avg: 15→25us  +%: 66.7
[system]  perf::order_step (order 4)  from: 48K  to: 64K   avg: 12→18us  +%: 50.0
```

### 8.5 `bench_internal_frag`

비정렬 요청 시 내부 단편화 비율.

- **방법**: `llseek(fd, 0, SEEK_END)` — 커널이 실제 할당한 크기 반환
- **사이즈**: 1, 4095, 4097, 65535, 65537, 100000 (의도적 비정렬)
- **출력**: `frag% = (actual - requested) / requested × 100`
- **용도**: 힙의 할당 그래뉼래리티(보통 4K) 확인

### 8.6 `bench_pool_warmup`

cold start vs warm state 할당 비용 비교.

- **cold**: 처음 100회 alloc (pool 미충전 상태)
- **warm**: 100회 alloc/free 사이클 후 100회 측정
- **출력**: `cold_p50, cold_p95, warm_p50, warm_p95`
- **용도**: 커널 deferred free pool의 효과 정량화
- **통계 검정** (`perf::pool_effect`): Welch's t-test + Cohen's d
  - `t`: t-통계량 (cold - warm). 양수이면 cold가 더 느림
  - `d`: Cohen's d 효과 크기. `<0.2` negligible, `0.2–0.5` small, `0.5–0.8` medium, `>0.8` large
  - `sig`: `***` (p<0.001), `**` (p<0.01), `*` (p<0.05), `ns` (not significant)
  - 정규 근사 사용 (n=100 → df≈198, 정규와 사실상 동일)

```
[system]  perf::pool_warmup  cold_p50: 45  cold_p95: 80  warm_p50: 12  warm_p95: 25
[system]  perf::pool_effect  t: 8.32  d: 1.85  sig: ***  effect: large
```

### 8.7 `bench_size_switch`

사이즈 전환이 할당 지연에 미치는 영향.

- **3 phase**: 64K×500 → 4K×500 → 64K×500
- **분석**: 각 phase의 처음 10회 vs 마지막 10회 p50 비교
- **용도**: 사이즈별 pool이 분리되었는지, 전환 비용이 있는지 확인
- **Hysteresis** (`perf::hysteresis`): phase 1 vs phase 3 (동일 사이즈) 평균 비교
  - `ratio = ph3_avg / ph1_avg`. 1.0 = 완전 복귀
  - `>1.1` degraded, `<0.9` improved, 나머지 recovered
  - 비가역적 성능 저하가 있는지 (pool fragmentation 등) 감지
- **Convergence** (`perf::converge_phN`): 사이즈 전환 후 안정화까지 필요한 alloc 수
  - 10-sample sliding window가 overall mean의 ±5% 이내에 도달하는 최초 지점
  - 이미 안정적이면 출력 안 함 (no transition cost)

```
[system]  perf::hysteresis  ph1_avg: 12  ph3_avg: 14  ratio: 1.17  verdict: degraded
[system]  perf::converge_ph2  after: 35 allocs
[system]  perf::converge_ph3  after: 22 allocs
```

---

## 9. JSON 출력 형식

`LatencyStats` 구조체가 그대로 직렬화된다:

```json
{
  "count": 100,
  "min_us": 5,
  "max_us": 42,
  "avg_us": 12,
  "stddev_us": 8,
  "p50_us": 8,
  "p95_us": 25,
  "p99_us": 38,
  "p99_9_us": 42,
  "cv_pct": 66.7,
  "trimmed_avg_us": 9,
  "outlier_count": 3,
  "throughput_ops": 83333,
  "ci95_us": 2
}
```

---

## 10. Measurement Quality Scorecard

`bench_alloc_only` 완료 후 4개 차원의 진단 신호를 종합한 품질 점수를 출력한다.

### 10.1 채점 체계

| 차원 | 신호 | 25점 | 20점 | 10점 | 5점 | 0점 |
|------|------|------|------|------|-----|-----|
| Stability | max CV (%) | <5 | <10 | <20 | <30 | >=30 |
| Precision | max CI/avg (%) | <3 | <10 | <20 | - | >=20 |
| Cleanliness | outlier rate (%) | <1 | <5 | <10 | - | >=10 |
| Stationarity | \|drift\| (%) | <5 | <10 | - | <20 | >=20 |

**등급**: >=90 EXCELLENT, >=75 GOOD, >=50 FAIR, <50 NOISY

### 10.2 출력 예시

```
[system]  perf::quality  score: 90/100  rating: EXCELLENT
```

점수가 낮은 차원에 대해 개선 권고를 출력:

```
[system]  perf::quality  score: 55/100  rating: FAIR
[system]  perf::quality  stability=10/25  hint: cv>10%: increase --iterations or reduce load
[system]  perf::quality  stationarity=5/25  hint: drift>10%: increase --warmup or shorten test
```

---

## 12. 정밀도 한계 및 주의사항

| 한계 | 영향 | 완화 방법 |
|------|------|-----------|
| 마이크로초 해상도 | fast op이 0µs로 측정될 수 있음 | `--iterations` 증가로 통계적 보상 |
| OS 스케줄러 jitter | p99+ 값에 spike 발생 | IQR trimmed mean으로 분리 |
| Thermal throttling | 긴 벤치마크에서 후반부 지연 증가 | CV 모니터링, 짧은 벤치마크 선호 |
| `Instant` 오버헤드 | 매 측정마다 ~20ns 추가 | 마이크로초 단위에서 무시 가능 (<1%) |
| 메모리 압박 | 대량 alloc 시 커널 경로 변경 | warmup으로 pool 안정화 |

---

## 13. 알고리즘 복잡도

| 단계 | 시간 복잡도 | 공간 복잡도 |
|------|-------------|-------------|
| 수집 | O(n) | O(n) |
| 정렬 | O(n log n) | O(1) in-place |
| 합계/평균 | O(n) | O(1) |
| 분산 (2nd pass) | O(n) | O(1) |
| 백분위수 | O(1) per query | O(1) |
| IQR trimmed mean | O(n) | O(1) |
| **전체** | **O(n log n)** | **O(n)** |

n = iterations (기본 100). 모든 통계 계산은 수집 + 정렬 이후 단일 패스로 완료 가능하며,
현재 구현은 가독성을 위해 별도 패스를 사용한다 (성능 차이 무시 가능).
