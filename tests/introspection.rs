//! Integration tests for the introspection commands: `--list-outputs` and
//! `--dry-run`. The sandbox has no compositor, so the compositor-present
//! happy path stays a documented manual check; what is pinned here is the
//! no-compositor behaviour: clean errors for `--list-outputs`, and
//! `--dry-run` degrading to file validation + configuration printing with
//! a zero exit status.

use assert_cmd::Command;
use predicates::str::contains;

fn wally() -> Command {
    Command::cargo_bin("wally").expect("wally binary builds")
}

#[test]
fn list_outputs_without_a_compositor_fails_cleanly() {
    // No WAYLAND_DISPLAY in the test environment; the command must fail
    // with the connect error, not panic or hang.
    wally()
        .arg("--list-outputs")
        .assert()
        .failure()
        .stderr(contains("could not connect to a Wayland compositor"));
}

#[test]
fn dry_run_validates_and_describes_a_still_image() {
    wally()
        .args(["--dry-run", "-i", "tests/fixtures/photo.png"])
        .assert()
        .success()
        .stdout(contains("tests/fixtures/photo.png"))
        .stdout(contains("png"))
        .stdout(contains("static"))
        .stdout(contains("no Wayland compositor"));
}

#[test]
fn dry_run_recognizes_an_animation() {
    wally()
        .args(["--dry-run", "-i", "tests/fixtures/anim.gif"])
        .assert()
        .success()
        .stdout(contains("animated"));
}

#[test]
fn dry_run_fails_on_a_missing_file() {
    wally()
        .args(["--dry-run", "-i", "tests/fixtures/does-not-exist.png"])
        .assert()
        .failure()
        .stderr(contains("could not load"));
}

#[test]
fn dry_run_lists_configured_targets_without_a_compositor() {
    wally()
        .args([
            "--dry-run",
            "-i",
            "tests/fixtures/photo.png",
            "-o",
            "DP-1",
            "-i",
            "tests/fixtures/photo.png",
            "-m",
            "tile",
        ])
        .assert()
        .success()
        .stdout(contains("default (all outputs): tests/fixtures/photo.png"))
        .stdout(contains("DP-1: tests/fixtures/photo.png"));
}

#[test]
fn dry_run_still_requires_something_to_show() {
    wally()
        .arg("--dry-run")
        .assert()
        .failure()
        .stderr(contains("nothing to show"));
}

#[test]
fn dry_run_reports_an_empty_target_group_as_skipped() {
    // `-A` alone creates a target group with no image and no color —
    // exactly what run() skips; dry-run must say so instead of
    // inventing a source. (`-o DP-1` with no following option creates
    // no group at all.)
    wally()
        .args(["--dry-run", "-c", "#ffffff", "-o", "DP-1", "-A"])
        .assert()
        .success()
        .stdout(contains("default (all outputs): solid #ffffff"))
        .stdout(contains("DP-1: nothing configured"));
}

#[test]
fn dry_run_accepts_a_solid_color() {
    wally()
        .args(["--dry-run", "-c", "#1e1e2e"])
        .assert()
        .success()
        .stdout(contains("solid #1e1e2e"));
}
