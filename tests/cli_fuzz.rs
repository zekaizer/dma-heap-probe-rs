// Property-based CLI argument fuzzing — verify that arbitrary valid option
// combinations never crash and that invalid arguments exit gracefully.

use assert_cmd::Command;
use assert_cmd::cargo::cargo_bin_cmd;
use proptest::prelude::*;

fn dhp() -> Command {
    cargo_bin_cmd!("dhp")
}

// ---------------------------------------------------------------------------
// Strategy: global options
// ---------------------------------------------------------------------------

fn arb_global_opts() -> impl Strategy<Value = Vec<String>> {
    (
        proptest::option::of("[a-z_]{1,12}"),
        proptest::bool::ANY,
        proptest::bool::ANY,
        proptest::bool::ANY,
        0..4u8,
    )
        .prop_map(|(heap, trace, sysfs, procfs, verbose)| {
            let mut args: Vec<String> = Vec::new();
            if let Some(h) = heap {
                args.push("--heaps".into());
                args.push(h);
            }
            if trace {
                args.push("--trace".into());
            }
            if sysfs {
                args.push("--sysfs".into());
            }
            if procfs {
                args.push("--procfs".into());
            }
            match verbose {
                1 => args.push("-v".into()),
                2 => args.push("-vv".into()),
                3 => args.push("-vvv".into()),
                _ => {}
            }
            args
        })
}

// ---------------------------------------------------------------------------
// Strategy: valid subcommands with small parameters
// ---------------------------------------------------------------------------

fn arb_subcommand() -> impl Strategy<Value = Vec<String>> {
    prop_oneof![
        (1..4u32).prop_map(|r| vec![
            "basic".into(),
            "--sizes".into(),
            "4096".into(),
            "--repeat".into(),
            r.to_string(),
        ]),
        (1..8u32).prop_map(|t| vec![
            "basic".into(),
            "--sizes".into(),
            "4096".into(),
            "--threads".into(),
            t.to_string(),
        ]),
        Just(vec!["negative".into()]),
        (1..10u32, 0..3u32).prop_map(|(i, w)| vec![
            "perf".into(),
            "--iterations".into(),
            i.to_string(),
            "--warmup".into(),
            w.to_string(),
        ]),
        Just(vec![
            "pressure".into(),
            "--alloc-size".into(),
            "4096".into()
        ]),
        Just(vec!["info".into()]),
        (1..10u64).prop_map(|i| vec!["aging".into(), "--iterations".into(), i.to_string(),]),
        (1..10u64).prop_map(|i| vec![
            "aging".into(),
            "--fuzz".into(),
            "--iterations".into(),
            i.to_string(),
            "--seed".into(),
            "42".into(),
        ]),
    ]
}

// ---------------------------------------------------------------------------
// Strategy: invalid arguments
// ---------------------------------------------------------------------------

fn arb_invalid_args() -> impl Strategy<Value = Vec<String>> {
    prop_oneof![
        "[a-z]{1,8}".prop_map(|s| vec![s]),
        Just(vec![
            "basic".into(),
            "--sizes".into(),
            "not_a_number".into()
        ]),
        Just(vec!["basic".into(), "--threads".into(), "abc".into()]),
        "[a-z]{1,8}".prop_map(|s| vec![format!("--{s}"), "basic".into()]),
    ]
}

// ---------------------------------------------------------------------------
// Proptest: valid combinations must not crash
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]
    #[test]
    fn valid_combinations_do_not_crash(
        global in arb_global_opts(),
        subcmd in arb_subcommand(),
    ) {
        let output = dhp()
            .args(&global)
            .args(&subcmd)
            .output()
            .expect("failed to execute");
        // Must terminate normally (not killed by signal).
        prop_assert!(
            output.status.code().is_some(),
            "process killed by signal: args={:?} {:?}",
            global,
            subcmd,
        );
    }
}

// ---------------------------------------------------------------------------
// Proptest: --output with valid combinations produces valid JSON
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]
    #[test]
    fn output_produces_valid_json(
        global in arb_global_opts(),
        subcmd in arb_subcommand(),
    ) {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let output = dhp()
            .args(&global)
            .arg("--output")
            .arg(tmp.path())
            .args(&subcmd)
            .output()
            .expect("failed to execute");

        if output.status.success() {
            let content = std::fs::read_to_string(tmp.path())
                .expect("read output file");
            // Some subcommands (e.g. info --dump) ignore --output.
            if content.is_empty() {
                return Ok(());
            }
            let json: serde_json::Value = serde_json::from_str(&content)
                .expect("invalid JSON output");
            prop_assert!(json["heaps"].is_array());
            // info writes InfoReport (no stages), other cmds write RunResult.
            if json["stages"].is_array() {
                prop_assert!(json["total_passed"].is_u64());
                prop_assert!(json["total_failed"].is_u64());
                prop_assert!(json["total_duration_ms"].is_u64());
            } else {
                prop_assert!(json["total_buffers"].is_number());
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Proptest: invalid arguments exit gracefully (no crash, no exit 0)
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]
    #[test]
    fn invalid_args_exit_gracefully(args in arb_invalid_args()) {
        let output = dhp()
            .args(&args)
            .output()
            .expect("failed to execute");
        // Must terminate normally (not killed by signal).
        prop_assert!(
            output.status.code().is_some(),
            "process killed by signal: args={:?}",
            args,
        );
        // Must not succeed.
        let code = output.status.code();
        if code == Some(0) {
            let msg = format!("expected failure for args={args:?}");
            return Err(proptest::test_runner::TestCaseError::fail(msg));
        }
    }
}
