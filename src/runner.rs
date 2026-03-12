// Test execution engine and result aggregation.

use std::path::Path;
use std::time::Instant;

use anyhow::Context;

use serde::{Deserialize, Serialize};

/// Result of a single sub-test within a stage.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SubTestResult {
    pub name: String,
    pub passed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Result of a single test stage.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StageResult {
    pub name: String,
    pub passed: bool,
    pub duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

/// Aggregated test run results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunResult {
    pub heap: String,
    pub stages: Vec<StageResult>,
    pub total_passed: usize,
    pub total_failed: usize,
    pub total_duration_ms: u64,
}

impl RunResult {
    /// Create a new empty result set.
    #[must_use]
    pub fn new(heap: &str) -> Self {
        Self {
            heap: heap.to_string(),
            stages: Vec::new(),
            total_passed: 0,
            total_failed: 0,
            total_duration_ms: 0,
        }
    }

    /// Record a stage execution.
    pub fn record(
        &mut self,
        name: &str,
        result: anyhow::Result<()>,
        duration_ms: u64,
        details: Option<serde_json::Value>,
    ) {
        let (passed, error) = match result {
            Ok(()) => (true, None),
            Err(e) => (false, Some(e.to_string())),
        };

        if passed {
            self.total_passed += 1;
        } else {
            self.total_failed += 1;
        }
        self.total_duration_ms += duration_ms;

        self.stages.push(StageResult {
            name: name.to_string(),
            passed,
            duration_ms,
            error,
            details,
        });
    }

    /// Return true if all stages passed.
    #[must_use]
    pub fn all_passed(&self) -> bool {
        self.total_failed == 0
    }

    /// Write results as JSON to the given path.
    pub fn write_json(&self, path: &Path) -> anyhow::Result<()> {
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(path, &json)
            .with_context(|| format!("failed to write results to {}", path.display()))?;
        tracing::info!(path = %path.display(), "results written");
        Ok(())
    }
}

/// Run a stage and record the result.
pub fn run_stage<F>(results: &mut RunResult, name: &str, f: F)
where
    F: FnOnce() -> anyhow::Result<Option<serde_json::Value>>,
{
    tracing::info!(stage = name, "starting");
    let start = Instant::now();
    let result = f();
    #[allow(clippy::cast_possible_truncation)]
    let duration_ms = start.elapsed().as_millis() as u64;

    let (mapped, details) = match result {
        Ok(details) => {
            tracing::info!(stage = name, duration_ms, "PASS");
            (Ok(()), details)
        }
        Err(e) => {
            tracing::error!(stage = name, duration_ms, error = %e, "FAIL");
            (Err(e), None)
        }
    };

    results.record(name, mapped, duration_ms, details);
}

/// Convert sub-test results to a JSON value for stage details.
pub fn sub_tests_to_details(tests: &[SubTestResult]) -> serde_json::Value {
    serde_json::json!({ "tests": tests })
}

/// Return a `SELinux` hint suffix for permission-related errno values.
fn selinux_hint(errno: nix::errno::Errno) -> &'static str {
    match errno {
        nix::errno::Errno::EPERM | nix::errno::Errno::EACCES => {
            " (hint: likely SELinux denial. Try `adb shell setenforce 0` on userdebug builds. \
             Check `adb logcat | grep avc` for details)"
        }
        _ => "",
    }
}

/// Collect nix test results into `SubTestResult` entries, logging each.
/// Returns the collected results and the first error (if any).
pub fn collect_test_results(
    stage: &str,
    tests: &[(&str, nix::Result<()>)],
) -> (Vec<SubTestResult>, Option<anyhow::Error>) {
    let mut results = Vec::with_capacity(tests.len());
    let mut first_error: Option<anyhow::Error> = None;

    for (name, result) in tests {
        match result {
            Ok(()) => {
                tracing::info!(name, "PASS");
                results.push(SubTestResult {
                    name: (*name).to_string(),
                    passed: true,
                    error: None,
                });
            }
            Err(e) => {
                let hint = selinux_hint(*e);
                tracing::error!(name, error = %e, "FAIL");
                if !hint.is_empty() {
                    tracing::warn!(name, "{hint}");
                }
                results.push(SubTestResult {
                    name: (*name).to_string(),
                    passed: false,
                    error: Some(format!("{e}{hint}")),
                });
                if first_error.is_none() {
                    first_error = Some(anyhow::anyhow!("{stage} test '{name}' failed: {e}{hint}"));
                }
            }
        }
    }

    (results, first_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_pass() {
        let mut r = RunResult::new("system");
        r.record("test1", Ok(()), 100, None);
        assert_eq!(r.total_passed, 1);
        assert_eq!(r.total_failed, 0);
        assert!(r.all_passed());
        assert_eq!(r.stages[0].name, "test1");
        assert!(r.stages[0].passed);
        assert!(r.stages[0].error.is_none());
        assert!(r.stages[0].details.is_none());
    }

    #[test]
    fn record_fail() {
        let mut r = RunResult::new("system");
        r.record("test1", Err(anyhow::anyhow!("oops")), 50, None);
        assert_eq!(r.total_passed, 0);
        assert_eq!(r.total_failed, 1);
        assert!(!r.all_passed());
        assert_eq!(r.stages[0].error.as_deref(), Some("oops"));
    }

    #[test]
    fn record_mixed() {
        let mut r = RunResult::new("custom");
        r.record("a", Ok(()), 10, None);
        r.record("b", Err(anyhow::anyhow!("fail")), 20, None);
        r.record("c", Ok(()), 30, None);
        assert_eq!(r.total_passed, 2);
        assert_eq!(r.total_failed, 1);
        assert_eq!(r.total_duration_ms, 60);
        assert!(!r.all_passed());
    }

    #[test]
    fn record_with_details() {
        let mut r = RunResult::new("system");
        let details = serde_json::json!({"tests": [{"name": "t1", "passed": true}]});
        r.record("stage1", Ok(()), 100, Some(details.clone()));
        assert_eq!(r.stages[0].details, Some(details));
    }

    #[test]
    fn serde_roundtrip() {
        let mut r = RunResult::new("system");
        r.record("stage1", Ok(()), 100, None);
        r.record("stage2", Err(anyhow::anyhow!("error")), 50, None);

        let json = serde_json::to_string(&r).unwrap();
        let deserialized: RunResult = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.heap, "system");
        assert_eq!(deserialized.stages.len(), 2);
        assert_eq!(deserialized.total_passed, 1);
        assert_eq!(deserialized.total_failed, 1);
    }

    #[test]
    fn serde_skips_none_fields() {
        let mut r = RunResult::new("system");
        r.record("test", Ok(()), 42, None);

        let json = serde_json::to_string(&r).unwrap();
        assert!(!json.contains("details"));
        assert!(!json.contains("error"));
    }

    #[test]
    fn run_stage_records() {
        let mut r = RunResult::new("system");
        run_stage(&mut r, "ok_stage", || Ok(None));
        run_stage(&mut r, "err_stage", || Err(anyhow::anyhow!("boom")));
        assert_eq!(r.stages.len(), 2);
        assert!(r.stages[0].passed);
        assert!(!r.stages[1].passed);
    }

    #[test]
    fn run_stage_with_details() {
        let mut r = RunResult::new("system");
        run_stage(&mut r, "detailed", || {
            let details = serde_json::json!({"tests": []});
            Ok(Some(details))
        });
        assert!(r.stages[0].details.is_some());
    }

    #[test]
    fn sub_tests_to_details_converts() {
        let tests = vec![
            SubTestResult {
                name: "t1".into(),
                passed: true,
                error: None,
            },
            SubTestResult {
                name: "t2".into(),
                passed: false,
                error: Some("fail".into()),
            },
        ];
        let details = sub_tests_to_details(&tests);
        let tests_arr = details["tests"].as_array().unwrap();
        assert_eq!(tests_arr.len(), 2);
        assert_eq!(tests_arr[0]["name"], "t1");
        assert!(tests_arr[0]["passed"].as_bool().unwrap());
        assert!(!tests_arr[1]["passed"].as_bool().unwrap());
    }

    #[test]
    fn write_json_creates_file() {
        let mut r = RunResult::new("system");
        r.record("test", Ok(()), 42, None);

        let dir = std::env::temp_dir().join("dhp_test_runner");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test_output.json");

        r.write_json(&path).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let parsed: RunResult = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed.heap, "system");
        assert_eq!(parsed.stages.len(), 1);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn collect_eperm_includes_hint() {
        let tests: Vec<(&str, nix::Result<()>)> = vec![("t", Err(nix::errno::Errno::EPERM))];
        let (results, err) = collect_test_results("stage", &tests);
        assert!(!results[0].passed);
        let error_msg = results[0].error.as_ref().unwrap();
        assert!(
            error_msg.contains("SELinux"),
            "expected SELinux hint: {error_msg}"
        );
        assert!(error_msg.contains("setenforce"));
        let first = err.unwrap().to_string();
        assert!(first.contains("SELinux"));
    }

    #[test]
    fn collect_eacces_includes_hint() {
        let tests: Vec<(&str, nix::Result<()>)> = vec![("t", Err(nix::errno::Errno::EACCES))];
        let (results, _) = collect_test_results("stage", &tests);
        let error_msg = results[0].error.as_ref().unwrap();
        assert!(
            error_msg.contains("SELinux"),
            "expected SELinux hint: {error_msg}"
        );
    }

    #[test]
    fn collect_einval_no_hint() {
        let tests: Vec<(&str, nix::Result<()>)> = vec![("t", Err(nix::errno::Errno::EINVAL))];
        let (results, _) = collect_test_results("stage", &tests);
        let error_msg = results[0].error.as_ref().unwrap();
        assert!(
            !error_msg.contains("SELinux"),
            "unexpected SELinux hint: {error_msg}"
        );
    }
}
