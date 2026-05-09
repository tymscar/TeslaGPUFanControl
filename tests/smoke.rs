//! Smoke tests for the `--check-config` CLI surface.
//!
//! These run the actual built binary (cargo provides its path via
//! `CARGO_BIN_EXE_<name>`) so they cover clap parsing, file I/O, the
//! parse + validate pipeline, and the exit-code contract end to end.

#![allow(
    clippy::unwrap_used,
    clippy::indexing_slicing,
    clippy::expect_used,
    clippy::panic
)]

use std::path::PathBuf;
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_tesla_fan_control");
const MANIFEST: &str = env!("CARGO_MANIFEST_DIR");

fn manifest_path(rel: &str) -> PathBuf {
    PathBuf::from(MANIFEST).join(rel)
}

fn run_check_config(rel_path: &str) -> Output {
    let cfg = manifest_path(rel_path);
    Command::new(BIN)
        .args(["--check-config", "-c"])
        .arg(&cfg)
        .output()
        .expect("failed to spawn tesla_fan_control")
}

fn assert_rule_in_stderr(out: &Output, needle: &str) {
    assert!(
        !out.status.success(),
        "expected non-zero exit; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(needle),
        "expected stderr to mention {needle}; got:\n{stderr}"
    );
}

#[test]
fn check_config_passes_on_canonical() {
    let out = run_check_config("config/tesla_fan_control.conf");
    assert!(
        out.status.success(),
        "canonical config should validate cleanly. stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn s4_orphan_gpu_fixture_surfaces_rule_code() {
    let out = run_check_config("tests/fixtures/s4_orphan_gpu.conf");
    assert_rule_in_stderr(&out, "[S4]");
}

#[test]
fn g2_descending_temp_fixture_surfaces_rule_code() {
    // Curve errors are caught at parse-time and Display includes "(G2)".
    let out = run_check_config("tests/fixtures/g2_temps_not_ascending.conf");
    assert_rule_in_stderr(&out, "G2");
}

#[test]
fn f1_max_le_min_rpm_fixture_surfaces_rule_code() {
    let out = run_check_config("tests/fixtures/f1_max_le_min_rpm.conf");
    assert_rule_in_stderr(&out, "[F1]");
}

#[test]
fn w2_timeout_out_of_range_fixture_surfaces_rule_code() {
    let out = run_check_config("tests/fixtures/w2_timeout_out_of_range.conf");
    assert_rule_in_stderr(&out, "[W2]");
}

#[test]
fn w7_unknown_config_version_fixture_surfaces_rule_code() {
    let out = run_check_config("tests/fixtures/w7_unknown_config_version.conf");
    assert_rule_in_stderr(&out, "[W7]");
}

#[test]
fn p1_enabled_without_value_surfaces_rule_code() {
    let out = run_check_config("tests/fixtures/p1_enabled_without_value.conf");
    assert_rule_in_stderr(&out, "[P1]");
}

#[test]
fn p2_power_above_validate_max_surfaces_rule_code() {
    let out = run_check_config("tests/fixtures/p2_power_above_validate_max.conf");
    assert_rule_in_stderr(&out, "[P2]");
}

#[test]
fn p4_interval_out_of_range_surfaces_rule_code() {
    let out = run_check_config("tests/fixtures/p4_interval_out_of_range.conf");
    assert_rule_in_stderr(&out, "[P4]");
}

#[test]
fn p5_validate_max_out_of_range_surfaces_rule_code() {
    let out = run_check_config("tests/fixtures/p5_validate_max_out_of_range.conf");
    assert_rule_in_stderr(&out, "[P5]");
}

#[test]
fn missing_config_file_exits_nonzero() {
    let out = Command::new(BIN)
        .args([
            "--check-config",
            "-c",
            "/nonexistent/tesla_fan_control.conf",
        ])
        .output()
        .expect("failed to spawn tesla_fan_control");
    assert!(!out.status.success());
}
