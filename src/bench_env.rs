// Benchmark environment control and snapshot collection.
//
// Provides RAII-based CPU affinity, frequency governor, and scheduling priority
// management for reproducible micro-benchmarks. Captures 2-tier environment
// snapshots (dynamic bench context + static device identity) for cross-run
// comparison.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Tier 1: BenchContext — dynamic state that directly affects benchmark results
// ---------------------------------------------------------------------------

/// Dynamic system state captured at benchmark time. Changes between runs on the
/// same device; differences here explain result differences.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchContext {
    /// ISO 8601 timestamp of measurement start.
    pub timestamp: String,
    /// dhp binary version.
    pub dhp_version: String,

    /// Environment control application results.
    pub env_control: EnvControlStatus,

    /// CPU core pinned for benchmarking.
    pub bench_cpu: u32,
    /// Current frequency of bench CPU (kHz) at capture time.
    pub bench_cpu_cur_freq_khz: Option<u64>,
    /// Governor of bench CPU at capture time.
    pub bench_cpu_governor: Option<String>,

    /// Available memory (kB) from `/proc/meminfo`.
    pub mem_available_kb: Option<u64>,
    /// Free CMA pages (kB).
    pub cma_free_kb: Option<u64>,
    /// Free swap (kB).
    pub swap_free_kb: Option<u64>,

    /// DMA heap page pool size (kB).
    pub pool_size_kb: Option<u64>,

    /// Thermal zone temperatures at capture time.
    pub thermal: Vec<ThermalZone>,

    /// PSI memory pressure avg10 (percentage, 0.0–100.0).
    pub psi_memory_avg10: Option<f64>,
}

/// Status of each environment control attempt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvControlStatus {
    pub affinity_applied: bool,
    pub governor_applied: bool,
    /// Original governor before override (for restore + documentation).
    pub governor_original: Option<String>,
    pub nice_applied: bool,
    /// Actual nice value after `setpriority`.
    pub nice_value: i32,
}

/// Single thermal zone reading.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThermalZone {
    /// Zone type name (e.g. "cpu-0-0-0", "battery").
    pub zone_type: String,
    /// Temperature in milli-Celsius.
    pub temp_mc: i32,
}

// ---------------------------------------------------------------------------
// Tier 2: DeviceIdentity — static attributes (stable until reboot/reflash)
// ---------------------------------------------------------------------------

/// Fixed device/kernel attributes for cross-device comparison. Identify the
/// hardware and software platform; changes only on reboot or flash.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceIdentity {
    // Android device (None on host/non-Android)
    pub model: Option<String>,
    pub board: Option<String>,
    pub fingerprint: Option<String>,
    pub sdk: Option<String>,

    // Kernel
    /// Full `/proc/version` string.
    pub kernel_version: Option<String>,
    /// `cma=` parameter extracted from `/proc/cmdline`.
    pub cmdline_cma: Option<String>,

    // CPU static attributes
    /// CPU model from `/proc/cpuinfo` ("Hardware" on ARM, "model name" on x86).
    pub cpu_model: Option<String>,
    /// Architecture string (e.g. `aarch64`, `x86_64`).
    pub cpu_arch: String,
    /// Online CPU core indices.
    pub cores_online: Vec<u32>,
    /// Max frequency of bench CPU (kHz).
    pub bench_cpu_max_freq_khz: Option<u64>,

    // Memory static attributes
    pub mem_total_kb: Option<u64>,
    pub cma_total_kb: Option<u64>,
    pub swap_total_kb: Option<u64>,

    // DMA Heap
    /// Discovered heap names.
    pub heaps: Vec<String>,
}

// ---------------------------------------------------------------------------
// BenchEnvConfig — user-facing configuration for environment control
// ---------------------------------------------------------------------------

/// Configuration for `BenchEnv::setup`.
pub struct BenchEnvConfig {
    /// CPU core to pin (None = auto-detect fastest core).
    pub cpu: Option<u32>,
    /// Whether to apply environment controls (false = capture snapshot only).
    pub enabled: bool,
}

// ---------------------------------------------------------------------------
// BenchEnv — RAII environment control + snapshot capture
// ---------------------------------------------------------------------------

/// RAII guard that applies environment controls on creation and restores
/// original state on drop.
pub struct BenchEnv {
    bench_cpu: u32,
    #[cfg(target_os = "linux")]
    original_governor: Option<String>,
    #[cfg(target_os = "linux")]
    original_nice: i32,
    env_control: EnvControlStatus,
}

impl BenchEnv {
    /// Set up benchmark environment. Applies controls on Linux; no-op on other
    /// platforms. Always captures the selected CPU core index.
    #[allow(unused_variables)]
    pub fn setup(config: &BenchEnvConfig) -> Self {
        let bench_cpu = config
            .cpu
            .unwrap_or_else(|| detect_fastest_core().unwrap_or(0));

        let env_control = EnvControlStatus {
            affinity_applied: false,
            governor_applied: false,
            governor_original: None,
            nice_applied: false,
            nice_value: 0,
        };

        #[cfg(target_os = "linux")]
        let original_governor;
        #[cfg(target_os = "linux")]
        let original_nice;

        #[cfg(target_os = "linux")]
        {
            original_nice = get_nice();

            if config.enabled {
                // CPU affinity
                env_control.affinity_applied = set_affinity(bench_cpu);
                if !env_control.affinity_applied {
                    tracing::warn!(cpu = bench_cpu, "failed to set CPU affinity");
                }

                // CPU frequency governor
                original_governor = read_governor(bench_cpu);
                env_control.governor_original = original_governor.clone();
                if let Some(ref orig) = original_governor {
                    if orig != "performance" {
                        env_control.governor_applied = write_governor(bench_cpu, "performance");
                        if !env_control.governor_applied {
                            tracing::warn!(
                                cpu = bench_cpu,
                                "failed to set performance governor (need root?)"
                            );
                        }
                    } else {
                        // Already performance — mark as applied.
                        env_control.governor_applied = true;
                    }
                }

                // Scheduling priority (nice -20)
                let ok = set_nice(-20);
                env_control.nice_applied = ok;
                env_control.nice_value = if ok { -20 } else { original_nice };
                if !ok {
                    tracing::warn!("failed to set nice -20 (need root?)");
                }
            } else {
                original_governor = read_governor(bench_cpu);
                env_control.governor_original = original_governor.clone();
                env_control.nice_value = original_nice;
            }
        }

        #[cfg(not(target_os = "linux"))]
        {
            // No environment control on non-Linux (macOS host builds).
            let _ = config.enabled;
        }

        Self {
            bench_cpu,
            #[cfg(target_os = "linux")]
            original_governor,
            #[cfg(target_os = "linux")]
            original_nice,
            env_control,
        }
    }

    /// Capture `BenchContext` (Tier 1) at this moment.
    pub fn capture_context(&self) -> BenchContext {
        let meminfo = crate::procfs::read_meminfo().ok();
        let psi = crate::procfs::read_psi_memory();

        BenchContext {
            timestamp: now_iso8601(),
            dhp_version: env!("CARGO_PKG_VERSION").to_string(),
            env_control: self.env_control.clone(),
            bench_cpu: self.bench_cpu,
            bench_cpu_cur_freq_khz: read_cur_freq(self.bench_cpu),
            bench_cpu_governor: read_governor(self.bench_cpu),
            mem_available_kb: meminfo.as_ref().map(|m| m.mem_available_kb),
            cma_free_kb: meminfo.as_ref().and_then(|m| m.cma_free_kb),
            swap_free_kb: meminfo.as_ref().and_then(|m| m.swap_free_kb),
            pool_size_kb: crate::sysfs::read_dma_heap_pool_kb(),
            thermal: read_thermal_zones(),
            psi_memory_avg10: psi.map(|p| p.some.avg10),
        }
    }

    /// Capture `DeviceIdentity` (Tier 2).
    pub fn capture_identity(&self, heaps: &[String]) -> DeviceIdentity {
        let meminfo = crate::procfs::read_meminfo().ok();

        DeviceIdentity {
            model: getprop("ro.product.model"),
            board: getprop("ro.product.board"),
            fingerprint: getprop("ro.build.fingerprint"),
            sdk: getprop("ro.build.version.sdk"),
            kernel_version: read_file_line("/proc/version"),
            cmdline_cma: read_cmdline_cma(),
            cpu_model: read_cpu_model(),
            cpu_arch: std::env::consts::ARCH.to_string(),
            cores_online: read_cores_online(),
            bench_cpu_max_freq_khz: read_max_freq(self.bench_cpu),
            mem_total_kb: meminfo.as_ref().map(|m| m.mem_total_kb),
            cma_total_kb: meminfo.as_ref().and_then(|m| m.cma_total_kb),
            swap_total_kb: meminfo.as_ref().and_then(|m| m.swap_total_kb),
            heaps: heaps.to_vec(),
        }
    }

    /// The CPU core selected for benchmarking.
    #[must_use]
    pub fn bench_cpu(&self) -> u32 {
        self.bench_cpu
    }

    /// The environment control status.
    #[must_use]
    pub fn env_control(&self) -> &EnvControlStatus {
        &self.env_control
    }
}

impl Drop for BenchEnv {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        {
            // Restore governor.
            if self.env_control.governor_applied {
                if let Some(ref orig) = self.original_governor {
                    if orig != "performance" {
                        let _ = write_governor(self.bench_cpu, orig);
                    }
                }
            }

            // Restore nice.
            if self.env_control.nice_applied {
                let _ = set_nice(self.original_nice);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Platform helpers — sysfs / procfs / getprop readers
// ---------------------------------------------------------------------------

/// ISO 8601 timestamp using system time (no chrono dependency).
fn now_iso8601() -> String {
    use std::time::SystemTime;
    match SystemTime::now().duration_since(SystemTime::UNIX_EPOCH) {
        Ok(d) => {
            let secs = d.as_secs();
            // Simple UTC format without timezone offset.
            let days = secs / 86400;
            let time_secs = secs % 86400;
            let hours = time_secs / 3600;
            let minutes = (time_secs % 3600) / 60;
            let seconds = time_secs % 60;

            // Days since epoch → year/month/day (simplified Gregorian).
            let (year, month, day) = epoch_days_to_date(days);
            format!("{year:04}-{month:02}-{day:02}T{hours:02}:{minutes:02}:{seconds:02}Z")
        }
        Err(_) => "unknown".to_string(),
    }
}

/// Convert days since Unix epoch to (year, month, day).
#[allow(clippy::cast_possible_truncation)]
fn epoch_days_to_date(days: u64) -> (u64, u64, u64) {
    // Algorithm from Howard Hinnant's `chrono`-compatible date library.
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// Read a single line from a file, trimmed.
fn read_file_line(path: &str) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Read `cma=` parameter from `/proc/cmdline`.
fn read_cmdline_cma() -> Option<String> {
    let content = std::fs::read_to_string("/proc/cmdline").ok()?;
    content
        .split_whitespace()
        .find(|tok| tok.starts_with("cma="))
        .map(ToString::to_string)
}

/// Read CPU model name from `/proc/cpuinfo`.
fn read_cpu_model() -> Option<String> {
    let content = std::fs::read_to_string("/proc/cpuinfo").ok()?;
    for line in content.lines() {
        // ARM: "Hardware : ..."
        if let Some(val) = line.strip_prefix("Hardware") {
            return val
                .trim_start_matches(|c: char| c == ':' || c.is_whitespace())
                .to_string()
                .into();
        }
        // x86: "model name : ..."
        if let Some(val) = line.strip_prefix("model name") {
            return val
                .trim_start_matches(|c: char| c == ':' || c.is_whitespace())
                .to_string()
                .into();
        }
    }
    None
}

/// Parse `/sys/devices/system/cpu/online` (e.g. "0-7" or "0-3,6-7").
fn read_cores_online() -> Vec<u32> {
    let Ok(content) = std::fs::read_to_string("/sys/devices/system/cpu/online") else {
        return Vec::new();
    };
    parse_cpu_range(content.trim())
}

/// Parse CPU range string like "0-3,6-7" into sorted Vec of core indices.
fn parse_cpu_range(s: &str) -> Vec<u32> {
    let mut cores = Vec::new();
    for part in s.split(',') {
        let part = part.trim();
        if let Some((a, b)) = part.split_once('-') {
            if let (Ok(start), Ok(end)) = (a.parse::<u32>(), b.parse::<u32>()) {
                cores.extend(start..=end);
            }
        } else if let Ok(n) = part.parse::<u32>() {
            cores.push(n);
        }
    }
    cores.sort_unstable();
    cores
}

/// Read current frequency (kHz) for a CPU core.
fn read_cur_freq(cpu: u32) -> Option<u64> {
    read_file_line(&format!(
        "/sys/devices/system/cpu/cpu{cpu}/cpufreq/scaling_cur_freq"
    ))
    .and_then(|s| s.parse().ok())
}

/// Read max frequency (kHz) for a CPU core.
fn read_max_freq(cpu: u32) -> Option<u64> {
    read_file_line(&format!(
        "/sys/devices/system/cpu/cpu{cpu}/cpufreq/cpuinfo_max_freq"
    ))
    .and_then(|s| s.parse().ok())
}

/// Read current governor for a CPU core.
fn read_governor(cpu: u32) -> Option<String> {
    read_file_line(&format!(
        "/sys/devices/system/cpu/cpu{cpu}/cpufreq/scaling_governor"
    ))
}

/// Write governor for a CPU core. Returns true on success.
#[cfg(target_os = "linux")]
fn write_governor(cpu: u32, governor: &str) -> bool {
    std::fs::write(
        format!("/sys/devices/system/cpu/cpu{cpu}/cpufreq/scaling_governor"),
        governor,
    )
    .is_ok()
}

/// Detect the fastest CPU core by `cpuinfo_max_freq`.
fn detect_fastest_core() -> Option<u32> {
    let cores = read_cores_online();
    cores
        .into_iter()
        .filter_map(|c| read_max_freq(c).map(|f| (c, f)))
        .max_by_key(|&(_, f)| f)
        .map(|(c, _)| c)
}

/// Set CPU affinity to a single core. Returns true on success.
#[cfg(target_os = "linux")]
fn set_affinity(cpu: u32) -> bool {
    use nix::sched::{CpuSet, sched_setaffinity};
    use nix::unistd::Pid;

    let mut cpuset = CpuSet::new();
    if cpuset.set(cpu as usize).is_err() {
        return false;
    }
    sched_setaffinity(Pid::from_raw(0), &cpuset).is_ok()
}

/// Get current nice value.
#[cfg(target_os = "linux")]
fn get_nice() -> i32 {
    // getpriority returns the nice value; on error it returns -1 but that's also
    // a valid nice value. We use errno to disambiguate, but for simplicity just
    // accept the value.
    unsafe { libc::getpriority(libc::PRIO_PROCESS, 0) }
}

/// Set nice value. Returns true on success.
#[cfg(target_os = "linux")]
fn set_nice(nice: i32) -> bool {
    unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, nice) == 0 }
}

/// Read all thermal zones.
fn read_thermal_zones() -> Vec<ThermalZone> {
    let mut zones = Vec::new();
    let base = "/sys/class/thermal";
    let Ok(entries) = std::fs::read_dir(base) else {
        return zones;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("thermal_zone") {
            continue;
        }
        let path = entry.path();
        let zone_type = std::fs::read_to_string(path.join("type"))
            .unwrap_or_default()
            .trim()
            .to_string();
        let temp_mc = std::fs::read_to_string(path.join("temp"))
            .ok()
            .and_then(|s| s.trim().parse::<i32>().ok())
            .unwrap_or(0);
        if !zone_type.is_empty() {
            zones.push(ThermalZone { zone_type, temp_mc });
        }
    }
    zones.sort_by(|a, b| a.zone_type.cmp(&b.zone_type));
    zones
}

/// Run `getprop <key>` on Android. Returns None on non-Android or failure.
fn getprop(key: &str) -> Option<String> {
    #[cfg(target_os = "android")]
    {
        std::process::Command::new("getprop")
            .arg(key)
            .output()
            .ok()
            .and_then(|o| {
                let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if s.is_empty() { None } else { Some(s) }
            })
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = key;
        None
    }
}

// ---------------------------------------------------------------------------
// Display helpers for human-readable output
// ---------------------------------------------------------------------------

impl BenchContext {
    /// Print the bench context header to stdout.
    pub fn print_header(&self) {
        println!("── bench context ───────────────────────────────────────");

        let freq_str = self
            .bench_cpu_cur_freq_khz
            .map_or("? MHz".to_string(), |f| format!("{} MHz", f / 1000));
        let gov_str = self.bench_cpu_governor.as_deref().unwrap_or("?");
        println!(
            "  cpu{} @ {} (governor: {})",
            self.bench_cpu, freq_str, gov_str
        );

        let ec = &self.env_control;
        let aff = if ec.affinity_applied { "✓" } else { "✗" };
        let gov = if ec.governor_applied { "✓" } else { "✗" };
        let gov_orig = ec
            .governor_original
            .as_deref()
            .map_or(String::new(), |g| format!(" (was: {g})"));
        let nice = if ec.nice_applied { "✓" } else { "✗" };
        println!(
            "  affinity: {} | governor: {}{} | nice: {} {}",
            aff, gov, gov_orig, ec.nice_value, nice
        );

        let mem_avail = self
            .mem_available_kb
            .map_or("?".to_string(), |kb| format!("{} MB", kb / 1024));
        let cma_free = self
            .cma_free_kb
            .map_or(String::new(), |kb| format!(" | CMA free: {} MB", kb / 1024));
        let pool = self
            .pool_size_kb
            .map_or(String::new(), |kb| format!(" | pool: {} MB", kb / 1024));
        println!("  mem avail: {mem_avail}{cma_free}{pool}");

        if !self.thermal.is_empty() {
            let temps: Vec<String> = self
                .thermal
                .iter()
                .filter(|z| z.zone_type.contains("cpu"))
                .take(4)
                .map(|z| {
                    let deg = f64::from(z.temp_mc) / 1000.0;
                    format!("{} {deg:.1}°C", z.zone_type)
                })
                .collect();
            if !temps.is_empty() {
                println!("  thermal: {}", temps.join(", "));
            }
        }

        if let Some(psi) = self.psi_memory_avg10 {
            println!("  PSI mem avg10: {psi:.2}%");
        }
        println!("────────────────────────────────────────────────────────");
    }
}

impl DeviceIdentity {
    /// Print the device identity header to stdout.
    pub fn print_header(&self) {
        println!("── device ──────────────────────────────────────────────");

        let model = self.model.as_deref().unwrap_or("unknown");
        let board = self
            .board
            .as_deref()
            .map_or(String::new(), |b| format!(" ({b})"));
        let sdk = self
            .sdk
            .as_deref()
            .map_or(String::new(), |s| format!(" | SDK {s}"));
        println!("  {model}{board}{sdk}");

        let kernel = self.kernel_version.as_deref().unwrap_or("unknown kernel");
        // Truncate kernel version for readability (first ~60 chars).
        let kernel_short = if kernel.len() > 60 {
            &kernel[..60]
        } else {
            kernel
        };
        let n_cores = self.cores_online.len();
        println!("  {} | {} {} cores", kernel_short, self.cpu_arch, n_cores);

        let mem = self
            .mem_total_kb
            .map_or("?".to_string(), |kb| format!("{} GB", kb / 1024 / 1024));
        let cma = self
            .cma_total_kb
            .map_or(String::new(), |kb| format!(" | CMA {} MB", kb / 1024));
        let heaps_str = self.heaps.join(", ");
        println!("  {mem} RAM{cma} | {heaps_str}");
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_cpu_range_simple() {
        assert_eq!(parse_cpu_range("0-3"), vec![0, 1, 2, 3]);
    }

    #[test]
    fn parse_cpu_range_multi() {
        assert_eq!(parse_cpu_range("0-3,6-7"), vec![0, 1, 2, 3, 6, 7]);
    }

    #[test]
    fn parse_cpu_range_single() {
        assert_eq!(parse_cpu_range("5"), vec![5]);
    }

    #[test]
    fn parse_cpu_range_mixed() {
        assert_eq!(parse_cpu_range("0,2-4,7"), vec![0, 2, 3, 4, 7]);
    }

    #[test]
    fn parse_cpu_range_empty() {
        assert_eq!(parse_cpu_range(""), Vec::<u32>::new());
    }

    #[test]
    fn epoch_days_to_date_known() {
        // 2026-04-17 = day 20560 since epoch (approx).
        let (y, m, d) = epoch_days_to_date(20560);
        assert_eq!(y, 2026);
        assert_eq!(m, 4);
        assert_eq!(d, 17);
    }

    #[test]
    fn now_iso8601_format() {
        let ts = now_iso8601();
        // Should look like "2026-04-17T12:34:56Z"
        assert!(ts.contains('T'), "missing T separator: {ts}");
        assert!(ts.ends_with('Z'), "missing Z suffix: {ts}");
        assert!(ts.len() >= 20, "too short: {ts}");
    }

    #[test]
    fn bench_env_setup_noop() {
        // On non-Linux (macOS CI), setup should succeed as no-op.
        let env = BenchEnv::setup(&BenchEnvConfig {
            cpu: Some(0),
            enabled: false,
        });
        assert_eq!(env.bench_cpu(), 0);
    }

    #[test]
    fn bench_context_serialize() {
        let ctx = BenchContext {
            timestamp: "2026-04-17T00:00:00Z".to_string(),
            dhp_version: "2.7.0".to_string(),
            env_control: EnvControlStatus {
                affinity_applied: true,
                governor_applied: true,
                governor_original: Some("schedutil".to_string()),
                nice_applied: true,
                nice_value: -20,
            },
            bench_cpu: 7,
            bench_cpu_cur_freq_khz: Some(3_200_000),
            bench_cpu_governor: Some("performance".to_string()),
            mem_available_kb: Some(8_000_000),
            cma_free_kb: Some(500_000),
            swap_free_kb: Some(0),
            pool_size_kb: Some(32_768),
            thermal: vec![ThermalZone {
                zone_type: "cpu-0-0".to_string(),
                temp_mc: 38_200,
            }],
            psi_memory_avg10: Some(0.0),
        };
        let json = serde_json::to_value(&ctx).unwrap();
        assert_eq!(json["bench_cpu"], 7);
        assert_eq!(json["env_control"]["affinity_applied"], true);
    }

    #[test]
    fn device_identity_serialize() {
        let id = DeviceIdentity {
            model: Some("Pixel 9 Pro".to_string()),
            board: Some("husky".to_string()),
            fingerprint: None,
            sdk: Some("36".to_string()),
            kernel_version: Some("Linux version 6.12.1".to_string()),
            cmdline_cma: Some("cma=512M".to_string()),
            cpu_model: Some("Cortex-A725".to_string()),
            cpu_arch: "aarch64".to_string(),
            cores_online: vec![0, 1, 2, 3, 4, 5, 6, 7],
            bench_cpu_max_freq_khz: Some(3_200_000),
            mem_total_kb: Some(12_000_000),
            cma_total_kb: Some(524_288),
            swap_total_kb: Some(0),
            heaps: vec!["system".to_string()],
        };
        let json = serde_json::to_value(&id).unwrap();
        assert_eq!(json["model"], "Pixel 9 Pro");
        assert_eq!(json["cpu_arch"], "aarch64");
        assert_eq!(json["heaps"], serde_json::json!(["system"]));
    }

    #[test]
    fn getprop_returns_none_on_host() {
        // On macOS/Linux host, getprop is not available.
        assert!(getprop("ro.product.model").is_none());
    }
}
