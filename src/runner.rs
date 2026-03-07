// Test execution engine and result aggregation.

use std::error::Error;
use std::path::Path;
use std::time::Instant;

use serde::{Deserialize, Serialize};

/// Result of a single test stage.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StageResult {
    pub name: String,
    pub passed: bool,
    pub duration_ms: u64,
    pub error: Option<String>,
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
    pub fn record(&mut self, name: &str, result: Result<(), Box<dyn Error>>, duration_ms: u64) {
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
        });
    }

    /// Return true if all stages passed.
    #[must_use]
    pub fn all_passed(&self) -> bool {
        self.total_failed == 0
    }

    /// Write results as JSON to the given path.
    pub fn write_json(&self, path: &Path) -> Result<(), Box<dyn Error>> {
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(path, json)?;
        tracing::info!(path = %path.display(), "results written");
        Ok(())
    }
}

/// Run a stage and record the result.
pub fn run_stage<F>(results: &mut RunResult, name: &str, f: F)
where
    F: FnOnce() -> Result<(), Box<dyn Error>>,
{
    tracing::info!(stage = name, "starting");
    let start = Instant::now();
    let result = f();
    #[allow(clippy::cast_possible_truncation)]
    let duration_ms = start.elapsed().as_millis() as u64;

    match &result {
        Ok(()) => tracing::info!(stage = name, duration_ms, "PASS"),
        Err(e) => tracing::error!(stage = name, duration_ms, error = %e, "FAIL"),
    }

    results.record(name, result, duration_ms);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_pass() {
        let mut r = RunResult::new("system");
        r.record("test1", Ok(()), 100);
        assert_eq!(r.total_passed, 1);
        assert_eq!(r.total_failed, 0);
        assert!(r.all_passed());
        assert_eq!(r.stages[0].name, "test1");
        assert!(r.stages[0].passed);
        assert!(r.stages[0].error.is_none());
    }

    #[test]
    fn record_fail() {
        let mut r = RunResult::new("system");
        r.record("test1", Err("oops".into()), 50);
        assert_eq!(r.total_passed, 0);
        assert_eq!(r.total_failed, 1);
        assert!(!r.all_passed());
        assert_eq!(r.stages[0].error.as_deref(), Some("oops"));
    }

    #[test]
    fn record_mixed() {
        let mut r = RunResult::new("custom");
        r.record("a", Ok(()), 10);
        r.record("b", Err("fail".into()), 20);
        r.record("c", Ok(()), 30);
        assert_eq!(r.total_passed, 2);
        assert_eq!(r.total_failed, 1);
        assert_eq!(r.total_duration_ms, 60);
        assert!(!r.all_passed());
    }

    #[test]
    fn serde_roundtrip() {
        let mut r = RunResult::new("system");
        r.record("stage1", Ok(()), 100);
        r.record("stage2", Err("error".into()), 50);

        let json = serde_json::to_string(&r).unwrap();
        let deserialized: RunResult = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.heap, "system");
        assert_eq!(deserialized.stages.len(), 2);
        assert_eq!(deserialized.total_passed, 1);
        assert_eq!(deserialized.total_failed, 1);
    }

    #[test]
    fn run_stage_records() {
        let mut r = RunResult::new("system");
        run_stage(&mut r, "ok_stage", || Ok(()));
        run_stage(&mut r, "err_stage", || Err("boom".into()));
        assert_eq!(r.stages.len(), 2);
        assert!(r.stages[0].passed);
        assert!(!r.stages[1].passed);
    }

    #[test]
    fn write_json_creates_file() {
        let mut r = RunResult::new("system");
        r.record("test", Ok(()), 42);

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
}
