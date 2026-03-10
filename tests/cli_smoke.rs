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
fn sync_file_defaults() {
    dhp().arg("sync-file").assert().success();
}

#[test]
fn edge_defaults() {
    dhp().args(["edge", "--threads", "4"]).assert().success();
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
fn fragmentation_defaults() {
    dhp().arg("fragmentation").assert().success();
}

#[test]
fn pool_defaults() {
    dhp().arg("pool").assert().success();
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
fn sysfs_dump_defaults() {
    dhp().arg("sysfs-dump").assert().success();
}

#[test]
fn scenario_npu_defaults() {
    dhp()
        .args(["scenario", "npu", "--iterations", "5", "--clients", "1"])
        .assert()
        .success();
}

#[test]
fn scenario_camera_defaults() {
    dhp()
        .args([
            "scenario", "camera", "--width", "64", "--height", "64", "--frames", "2",
        ])
        .assert()
        .success();
}

#[test]
fn scenario_display_defaults() {
    dhp()
        .args([
            "scenario", "display", "--width", "64", "--height", "64", "--frames", "2",
        ])
        .assert()
        .success();
}

#[test]
fn scenario_codec_defaults() {
    dhp()
        .args([
            "scenario", "codec", "--width", "64", "--height", "64", "--frames", "2",
        ])
        .assert()
        .success();
}

#[test]
fn scenario_gpu_defaults() {
    dhp()
        .args([
            "scenario",
            "gpu",
            "--buffer-count",
            "5",
            "--texture-size",
            "4096",
        ])
        .assert()
        .success();
}

#[test]
fn scenario_pipeline_defaults() {
    dhp()
        .args(["scenario", "pipeline", "--frames", "3"])
        .assert()
        .success();
}

#[test]
fn all_subcommand() {
    dhp().arg("all").assert().success();
}

#[test]
fn scenario_all_subcommand() {
    dhp().args(["scenario", "all"]).assert().success();
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
    assert_eq!(json["heap"], "system");
    assert!(json["stages"].is_array());
    assert!(json["stages"][0]["passed"].as_bool().unwrap());
    assert!(json["total_passed"].as_u64().unwrap() >= 1);
}

#[test]
fn output_custom_heap_json() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    dhp()
        .args([
            "--heap",
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
    assert_eq!(json["heap"], "myheap");
}

#[test]
fn output_scenario_json() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    dhp()
        .args([
            "--output",
            tmp.path().to_str().unwrap(),
            "scenario",
            "npu",
            "--iterations",
            "5",
            "--clients",
            "1",
        ])
        .assert()
        .success();

    let json = read_json(tmp.path());
    let stage = &json["stages"][0];
    assert!(stage["name"].as_str().unwrap().contains("npu"));
    assert!(stage["details"]["tests"].is_array());
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
        stages.len() >= 14,
        "expected >= 14 stages, got {}",
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
        .args(["edge", "--threads", "notanum"])
        .assert()
        .failure();
}

#[test]
fn error_scenario_no_subcommand() {
    dhp().arg("scenario").assert().failure();
}

#[test]
fn error_invalid_frag_pattern() {
    dhp()
        .args(["fragmentation", "--pattern", "invalid"])
        .assert()
        .failure();
}

#[test]
fn error_unknown_global_option() {
    dhp().args(["--nonexistent", "basic"]).assert().failure();
}

// ---------------------------------------------------------------------------
// D. --config / dump-config
// ---------------------------------------------------------------------------

#[test]
fn config_file_not_found() {
    dhp()
        .args(["--config", "/nonexistent/config.json", "scenario", "npu"])
        .assert()
        .failure();
}

#[test]
fn dump_config_outputs_json() {
    let output = dhp()
        .args(["scenario", "dump-config"])
        .output()
        .expect("run dump-config");
    assert!(output.status.success());
    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("parse dump-config JSON");
    assert!(json.get("npu").is_some());
    assert!(json.get("camera").is_some());
    assert!(json.get("display").is_some());
    assert!(json.get("codec").is_some());
    assert!(json.get("gpu").is_some());
    assert!(json.get("pipeline").is_some());
}

#[test]
fn config_file_loads_and_runs() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let config_path = dir.path().join("config.json");
    std::fs::write(
        &config_path,
        r#"{ "npu": { "iterations": 10, "clients": 1 } }"#,
    )
    .expect("write config");
    dhp()
        .args(["--config", config_path.to_str().unwrap(), "scenario", "npu"])
        .assert()
        .success();
}

#[test]
fn config_with_cli_override() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let config_path = dir.path().join("config.json");
    std::fs::write(
        &config_path,
        r#"{ "npu": { "iterations": 50, "clients": 2 } }"#,
    )
    .expect("write config");
    // CLI override: --iterations 10
    dhp()
        .args([
            "--config",
            config_path.to_str().unwrap(),
            "scenario",
            "npu",
            "--iterations",
            "10",
        ])
        .assert()
        .success();
}

#[test]
fn dump_config_roundtrip() {
    // dump -> write to file -> load with --config -> run
    let output = dhp()
        .args(["scenario", "dump-config"])
        .output()
        .expect("run dump-config");
    assert!(output.status.success());

    let dir = tempfile::tempdir().expect("create tempdir");
    let config_path = dir.path().join("default.json");
    std::fs::write(&config_path, &output.stdout).expect("write dumped config");

    dhp()
        .args([
            "--config",
            config_path.to_str().unwrap(),
            "scenario",
            "npu",
            "--iterations",
            "10",
            "--clients",
            "1",
        ])
        .assert()
        .success();
}

#[test]
fn config_invalid_json() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let config_path = dir.path().join("bad.json");
    std::fs::write(&config_path, "{ not valid json }").expect("write bad config");
    dhp()
        .args(["--config", config_path.to_str().unwrap(), "scenario", "npu"])
        .assert()
        .failure();
}

// ---------------------------------------------------------------------------
// E. Global option combinations
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
            "basic", "--heap", "myheap", "--trace", "-vv", "--sizes", "4096", "--repeat", "1",
        ])
        .assert()
        .success();
}
