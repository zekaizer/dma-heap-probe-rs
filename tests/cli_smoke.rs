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
    dhp()
        .args(["basic", "--sizes", "4096", "--repeat", "2"])
        .assert()
        .success();
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
            "--repeat",
            "2",
        ])
        .assert()
        .success();

    let json = read_json(tmp.path());
    assert_eq!(json["heaps"][0], "system");
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
            "myheap",
            "--output",
            tmp.path().to_str().unwrap(),
            "basic",
            "--sizes",
            "4096",
            "--repeat",
            "2",
        ])
        .assert()
        .success();

    let json = read_json(tmp.path());
    assert_eq!(json["heaps"][0], "myheap");
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
fn error_invalid_threads_value() {
    dhp()
        .args(["basic", "--threads", "notanum"])
        .assert()
        .failure();
}

#[test]
fn error_unknown_global_option() {
    dhp().args(["--nonexistent", "basic"]).assert().failure();
}

// ---------------------------------------------------------------------------
// D. Global option combinations
// ---------------------------------------------------------------------------

#[test]
fn global_all_flags_combined() {
    dhp()
        .args([
            "--trace", "--sysfs", "--procfs", "basic", "--sizes", "4096", "--repeat", "1",
        ])
        .assert()
        .success();
}

#[test]
fn global_options_after_subcommand() {
    dhp()
        .args([
            "basic", "--heaps", "myheap", "--trace", "-vv", "--sizes", "4096", "--repeat", "1",
        ])
        .assert()
        .success();
}
