// End-to-end CLI smoke tests — verify every subcommand and key option
// combination runs successfully with the mock backend.

use std::path::Path;

use assert_cmd::Command;
use assert_cmd::cargo::cargo_bin_cmd;
use predicates::prelude::*;

fn dhp() -> Command {
    cargo_bin_cmd!("dhp")
}

fn read_json(path: &Path) -> serde_json::Value {
    let content = std::fs::read_to_string(path).expect("read output file");
    serde_json::from_str(&content).expect("parse JSON")
}

// ---------------------------------------------------------------------------
// A. Subcommand default execution — exit 0
// ---------------------------------------------------------------------------

#[test]
fn basic_defaults() {
    dhp().args(["basic", "--sizes", "4096"]).assert().success();
}

#[test]
fn negative_defaults() {
    dhp().arg("negative").assert().success();
}

#[test]
fn perf_defaults() {
    dhp()
        .args(["perf", "--iterations", "5", "--warmup", "1"])
        .assert()
        .success();
}

#[test]
fn perf_pool_bypass() {
    dhp()
        .args([
            "perf",
            "--iterations",
            "3",
            "--warmup",
            "1",
            "--pool-bypass",
            "--drain-count",
            "32",
        ])
        .assert()
        .success();
}

#[test]
fn pressure_defaults() {
    dhp()
        .args(["pressure", "--alloc-size", "4096"])
        .assert()
        .success();
}

#[test]
fn info_defaults() {
    dhp().arg("info").assert().success();
}

#[test]
fn info_with_detail() {
    dhp().args(["info", "--detail"]).assert().success();
}

#[test]
fn info_with_procfs() {
    dhp().args(["info", "--procfs"]).assert().success();
}

#[test]
fn info_json_output() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    dhp()
        .args(["info", "--output", tmp.path().to_str().unwrap()])
        .assert()
        .success();
    let json = read_json(tmp.path());
    assert!(json["heaps"].is_array());
    assert!(json["total_buffers"].is_number());
    // Memory context uses embedded meminfo + vmstat structure (absent on non-Linux)
    if json.get("memory").is_some_and(|v| !v.is_null()) {
        let mem = &json["memory"];
        assert!(mem["meminfo"]["mem_total_kb"].is_number());
        assert!(mem["meminfo"]["mem_free_kb"].is_number());
        assert!(mem["vmstat"].is_object());
    }
}

#[test]
fn info_json_output_has_psi_fields() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    dhp()
        .args(["info", "--output", tmp.path().to_str().unwrap()])
        .assert()
        .success();
    let json = read_json(tmp.path());
    // PSI fields are optional (absent on non-Linux or when /proc/pressure not mounted)
    if json.get("psi_memory").is_some_and(|v| !v.is_null()) {
        let psi = &json["psi_memory"];
        assert!(psi["some"]["avg10"].is_f64());
        assert!(psi["some"]["total"].is_u64());
        assert!(psi["full"]["avg10"].is_f64());
    }
}

#[test]
fn info_dump() {
    dhp().args(["info", "--dump"]).assert().success();
}

#[test]
fn all_subcommand() {
    dhp().arg("all").assert().success();
}

#[test]
fn microbench_defaults() {
    dhp()
        .args([
            "microbench",
            "--ops",
            "alloc,close",
            "--sizes",
            "4096",
            "--iterations",
            "5",
            "--warmup",
            "1",
            "--no-env-control",
        ])
        .assert()
        .success();
}

#[test]
fn microbench_json_output() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    dhp()
        .args([
            "microbench",
            "--ops",
            "alloc",
            "--sizes",
            "4096",
            "--iterations",
            "5",
            "--warmup",
            "1",
            "--no-env-control",
            "--output",
            tmp.path().to_str().unwrap(),
        ])
        .assert()
        .success();
    let json = read_json(tmp.path());
    // Verify 2-tier environment snapshot is in details.
    let details = &json["stages"][0]["details"];
    assert!(details["bench_context"]["timestamp"].is_string());
    assert!(details["device_identity"]["cpu_arch"].is_string());
    // Benchmarks are keyed by heap name, then op name, then size.
    let benchmarks = details["benchmarks"]
        .as_object()
        .expect("benchmarks object");
    assert!(!benchmarks.is_empty(), "at least one heap's benchmarks");
    for heap_bench in benchmarks.values() {
        assert!(heap_bench["alloc"]["4096"]["avg_us"].is_u64());
    }
}

#[test]
fn container_defaults() {
    dhp().arg("container").assert().success();
}

#[test]
fn container_custom_heaps() {
    dhp()
        .args(["container", "--heaps", "system,restricted"])
        .assert()
        .success();
}

#[test]
fn aging_defaults() {
    dhp()
        .args(["aging", "--iterations", "10"])
        .assert()
        .success();
}

#[test]
fn aging_fuzz() {
    dhp()
        .args(["aging", "--fuzz", "--iterations", "10", "--seed", "42"])
        .assert()
        .success();
}

#[test]
fn aging_fuzz_with_size_cap() {
    dhp()
        .args([
            "aging",
            "--fuzz",
            "--size",
            "64K",
            "--iterations",
            "10",
            "--seed",
            "42",
        ])
        .assert()
        .success();
}

#[test]
fn aging_with_duration() {
    dhp()
        .args(["aging", "--duration", "2", "--report-interval", "1"])
        .assert()
        .success();
}

// ---------------------------------------------------------------------------
// B. JSON output validation
// ---------------------------------------------------------------------------

#[test]
fn output_basic_json() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    dhp()
        .args([
            "--output",
            tmp.path().to_str().unwrap(),
            "basic",
            "--sizes",
            "4096",
        ])
        .assert()
        .success();

    let json = read_json(tmp.path());
    let heaps = json["heaps"].as_array().expect("heaps is an array");
    assert!(
        heaps.iter().any(|h| h == "system"),
        "heaps should contain 'system'"
    );
    assert!(json["stages"].is_array());
    assert!(json["stages"][0]["passed"].as_bool().unwrap());
    assert!(json["total_passed"].as_u64().unwrap() >= 1);
}

#[test]
fn output_custom_heaps_json() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    dhp()
        .args([
            "--heaps",
            "system-uncached",
            "--output",
            tmp.path().to_str().unwrap(),
            "basic",
            "--sizes",
            "4096",
        ])
        .assert()
        .success();

    let json = read_json(tmp.path());
    assert_eq!(json["heaps"][0], "system-uncached");
}

#[test]
fn output_perf_json() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    dhp()
        .args([
            "--output",
            tmp.path().to_str().unwrap(),
            "perf",
            "--iterations",
            "5",
            "--warmup",
            "1",
        ])
        .assert()
        .success();

    let json = read_json(tmp.path());
    assert!(json["stages"][0]["duration_ms"].as_u64().unwrap() > 0);
    assert!(json["total_duration_ms"].as_u64().unwrap() > 0);
}

#[test]
fn output_all_json() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    dhp()
        .args(["--output", tmp.path().to_str().unwrap(), "all"])
        .assert()
        .success();

    let json = read_json(tmp.path());
    let stages = json["stages"].as_array().unwrap();
    assert!(
        stages.len() >= 4,
        "expected >= 4 stages, got {}",
        stages.len()
    );
    let total = json["total_passed"].as_u64().unwrap() + json["total_failed"].as_u64().unwrap();
    assert_eq!(total, stages.len() as u64);
}

// ---------------------------------------------------------------------------
// C. Error cases — non-zero exit
// ---------------------------------------------------------------------------

#[test]
fn error_no_subcommand() {
    dhp()
        .assert()
        .failure()
        .stderr(predicate::str::contains("Usage"));
}

#[test]
fn error_invalid_subcommand() {
    dhp().arg("nonexistent").assert().failure();
}

#[test]
fn error_invalid_sizes_value() {
    dhp().args(["basic", "--sizes", "abc"]).assert().failure();
}

#[test]
fn error_unknown_global_option() {
    dhp().args(["--nonexistent", "basic"]).assert().failure();
}

#[test]
fn error_nonexistent_heap() {
    dhp()
        .args(["--heaps", "nonexistent", "basic", "--sizes", "4096"])
        .assert()
        .failure();
}

#[test]
fn error_nonexistent_heap_aging() {
    dhp()
        .args(["--heaps", "nonexistent", "aging", "--iterations", "1"])
        .assert()
        .failure();
}

// ---------------------------------------------------------------------------
// D. Global option combinations
// ---------------------------------------------------------------------------

#[test]
fn global_all_flags_combined() {
    dhp()
        .args(["--trace", "--sysfs", "--procfs", "basic", "--sizes", "4096"])
        .assert()
        .success();
}

#[test]
fn global_options_after_subcommand() {
    dhp()
        .args([
            "basic", "--heaps", "system", "--trace", "-vv", "--sizes", "4096",
        ])
        .assert()
        .success();
}

// ---------------------------------------------------------------------------
// E. Log file output
// ---------------------------------------------------------------------------

#[test]
fn log_file_captures_output() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    dhp()
        .args([
            "--log",
            tmp.path().to_str().unwrap(),
            "basic",
            "--sizes",
            "4096",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("PASS"));

    let content = std::fs::read_to_string(tmp.path()).expect("read log file");
    assert!(
        content.contains("PASS"),
        "log file should contain PASS lines"
    );
    // Walltime prefix check
    assert!(
        content.contains("[20"),
        "log file lines should have walltime prefix"
    );
}

#[test]
fn log_file_with_log_level() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    dhp()
        .args([
            "--log",
            tmp.path().to_str().unwrap(),
            "--log-level",
            "debug",
            "basic",
            "--sizes",
            "4096",
        ])
        .assert()
        .success();

    let content = std::fs::read_to_string(tmp.path()).expect("read log file");
    assert!(
        content.contains("PASS"),
        "log file should contain tee output"
    );
}
