//! End-to-end tests driving the `exhume` binary.

use std::fs;

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::tempdir;

/// Deterministic pseudo-random bytes so tests don't depend on a RNG crate.
#[allow(
    clippy::cast_possible_truncation,
    reason = "intentionally taking the low byte"
)]
fn pattern(len: usize) -> Vec<u8> {
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    (0..len)
        .map(|_| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (state >> 33) as u8
        })
        .collect()
}

fn exhume() -> Command {
    Command::cargo_bin("exhume").expect("binary builds")
}

#[test]
fn copies_a_file_byte_for_byte() {
    let dir = tempdir().unwrap();
    let src = dir.path().join("src.img");
    let dst = dir.path().join("out.img");
    let data = pattern(300 * 1024); // spans several transfer blocks
    fs::write(&src, &data).unwrap();

    exhume()
        .arg(&src)
        .arg(&dst)
        .arg("--transfer-size")
        .arg("64K")
        .assert()
        .success()
        .stdout(predicate::str::contains("Done"));

    assert_eq!(fs::read(&dst).unwrap(), data, "target must match source");

    // An auto-named state file (no STATE argument) is removed after a clean,
    // error-free copy — there is nothing left to resume or inspect.
    let state_path = dir.path().join("out.img.state");
    assert!(
        !state_path.exists(),
        "auto-named state file should be removed on a clean copy"
    );
}

#[test]
fn rerunning_a_completed_copy_is_a_noop() {
    let dir = tempdir().unwrap();
    let src = dir.path().join("src.img");
    let dst = dir.path().join("out.img");
    let state = dir.path().join("run.state");
    fs::write(&src, pattern(128 * 1024)).unwrap();

    // With an explicit state file it is kept after a clean copy, so re-running
    // resumes from it (no --force needed) and is a no-op.
    exhume().arg(&src).arg(&dst).arg(&state).assert().success();
    assert!(
        state.exists(),
        "an explicit state file is kept after a clean copy"
    );
    exhume()
        .arg(&src)
        .arg(&dst)
        .arg(&state)
        .assert()
        .success()
        .stdout(predicate::str::contains("Done"));

    assert_eq!(fs::read(&dst).unwrap(), fs::read(&src).unwrap());
}

#[test]
fn refuses_to_overwrite_without_force() {
    let dir = tempdir().unwrap();
    let src = dir.path().join("src.img");
    let dst = dir.path().join("existing.img");
    fs::write(&src, pattern(4096)).unwrap();
    fs::write(&dst, b"important pre-existing data").unwrap();

    // No state file present + non-empty target => must fail.
    exhume()
        .arg(&src)
        .arg(&dst)
        .arg(dir.path().join("nostate.state"))
        .assert()
        .failure()
        .stderr(predicate::str::contains("already exists"));

    // The pre-existing data is untouched.
    assert_eq!(fs::read(&dst).unwrap(), b"important pre-existing data");

    // With --force it overwrites.
    exhume()
        .arg(&src)
        .arg(&dst)
        .arg(dir.path().join("nostate.state"))
        .arg("--force")
        .assert()
        .success();
    assert_eq!(fs::read(&dst).unwrap(), fs::read(&src).unwrap());
}

#[test]
fn honours_skip_seek_and_length() {
    let dir = tempdir().unwrap();
    let src = dir.path().join("src.img");
    let dst = dir.path().join("out.img");
    let data = pattern(8192);
    fs::write(&src, &data).unwrap();

    // Copy bytes [1024, 1024+2048) of the source to offset 512 of the target.
    exhume()
        .arg(&src)
        .arg(&dst)
        .arg("--skip")
        .arg("1K")
        .arg("--seek")
        .arg("512")
        .arg("--length")
        .arg("2K")
        .arg("--transfer-size")
        .arg("512")
        .assert()
        .success();

    let out = fs::read(&dst).unwrap();
    assert_eq!(out.len(), 512 + 2048);
    assert_eq!(&out[512..512 + 2048], &data[1024..1024 + 2048]);
}

#[test]
fn rejects_a_misaligned_offset_with_a_helpful_hint() {
    let dir = tempdir().unwrap();
    let src = dir.path().join("src.img");
    let dst = dir.path().join("out.img");
    fs::write(&src, pattern(4096)).unwrap();

    // A regular file detects the 512-byte fallback sector; --skip 100 is not a
    // multiple of it, so the run is refused with a suggestion of both neighbours.
    exhume()
        .arg(&src)
        .arg(&dst)
        .arg("--skip")
        .arg("100")
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "not a multiple of the 512-byte sector size",
        ))
        .stderr(predicate::str::contains("512 (up)"));

    // Nothing was written.
    assert!(!dst.exists());
}

#[test]
fn skip_unchanged_writes_only_changed_blocks() {
    let dir = tempdir().unwrap();
    let src = dir.path().join("src.img");
    let dst = dir.path().join("out.img");
    let state = dir.path().join("run.state");
    let data = pattern(16 * 1024);
    fs::write(&src, &data).unwrap();
    // Target identical to the source except for one clobbered 4 KiB block.
    let mut clone = data.clone();
    clone[8192..12288].fill(0);
    fs::write(&dst, &clone).unwrap();

    exhume()
        .arg(&src)
        .arg(&dst)
        .arg(&state)
        .arg("--skip-unchanged")
        .arg("--force")
        .arg("--transfer-size")
        .arg("4K")
        .assert()
        .success()
        .stdout(predicate::str::contains("already matched"));

    // The target now matches the source again ...
    assert_eq!(fs::read(&dst).unwrap(), data);
    // ... but only the single changed 4 KiB block was actually written.
    let s = fs::read_to_string(&state).unwrap();
    assert!(s.contains("bytes_written = 4096"), "state was:\n{s}");
}

#[test]
fn skip_zeros_leaves_zero_blocks_unwritten_but_correct() {
    let dir = tempdir().unwrap();
    let src = dir.path().join("src.img");
    let dst = dir.path().join("out.img");
    let state = dir.path().join("run.state");
    // Four 4 KiB blocks: non-zero, zero, non-zero, zero (trailing zero block).
    let mut data = pattern(16 * 1024);
    data[4096..8192].fill(0);
    data[12288..16384].fill(0);
    fs::write(&src, &data).unwrap();

    exhume()
        .arg(&src)
        .arg(&dst)
        .arg(&state)
        .arg("--skip-zeros")
        .arg("--transfer-size")
        .arg("4K")
        .assert()
        .success()
        .stdout(predicate::str::contains("zero blocks left sparse"));

    // Despite skipping the zero blocks (incl. the trailing one), the target is
    // the full size and byte-identical to the source (holes read back as zeros).
    assert_eq!(fs::read(&dst).unwrap(), data);
    // Only the two non-zero 4 KiB blocks were actually written.
    let s = fs::read_to_string(&state).unwrap();
    assert!(s.contains("bytes_written = 8192"), "state was:\n{s}");
}

#[test]
fn retry_recovers_bad_regions_from_a_readable_source() {
    let dir = tempdir().unwrap();
    let src = dir.path().join("src.img");
    let dst = dir.path().join("out.img");
    let state = dir.path().join("run.state");
    let data = pattern(16 * 1024);
    fs::write(&src, &data).unwrap();
    // The target already holds the correct data except [8K,12K), which the
    // seeded state records as `bad` and which is left zeroed here.
    let mut clone = data.clone();
    clone[8192..12288].fill(0);
    fs::write(&dst, &clone).unwrap();

    // Hand-seed a state file with one bad region (regular files can't produce
    // real read errors, so we inject the bad region directly).
    let seeded = format!(
        r#"[meta]
version = 1
program = "exhume"
program_version = "0.0.0"
created = "2026-06-18T08:00:00Z"
updated = "2026-06-18T08:00:00Z"

[params]
source = "{src}"
target = "{dst}"
sector_size = 512
transfer_size = 4096
skip = 0
seek = 0
length = 0
skip_unchanged = false
skip_zeros = false

[progress]
bytes_total = 16384
bytes_done = 12288
bytes_written = 12288
errors = 1

[[regions]]
start = 0
length = 8192
status = "done"

[[regions]]
start = 8192
length = 4096
status = "bad"

[[regions]]
start = 12288
length = 4096
status = "done"
"#,
        src = src.display(),
        dst = dst.display()
    );
    fs::write(&state, &seeded).unwrap();

    // Without --retry the bad region is left alone: exit code 2 (finished with
    // errors) and the target stays unrecovered.
    exhume().arg(&src).arg(&dst).arg(&state).assert().code(2);
    assert_eq!(&fs::read(&dst).unwrap()[8192..12288], &[0u8; 4096][..]);

    // Re-seed (the run above rewrote the state) and retry: the bad region is
    // re-read from the readable source and recovered.
    fs::write(&state, &seeded).unwrap();
    exhume()
        .arg(&src)
        .arg(&dst)
        .arg(&state)
        .arg("--retry")
        .assert()
        .success()
        .stdout(predicate::str::contains("Done"));

    assert_eq!(fs::read(&dst).unwrap(), data);
    let s = fs::read_to_string(&state).unwrap();
    assert!(!s.contains("\"bad\""), "no bad regions should remain:\n{s}");
}

#[test]
fn resuming_with_a_different_target_is_refused() {
    let dir = tempdir().unwrap();
    let src = dir.path().join("src.img");
    let target_a = dir.path().join("targetA.img");
    let target_b = dir.path().join("targetB.img");
    let state = dir.path().join("run.state");
    fs::write(&src, pattern(64 * 1024)).unwrap();

    exhume().arg(&src).arg(&target_a).arg(&state).assert().success();

    // The state file records targetA; reusing it with targetB would credit
    // targetA's progress to targetB (an all-zero file reported as "Done").
    exhume()
        .arg(&src)
        .arg(&target_b)
        .arg(&state)
        .assert()
        .failure()
        .stderr(predicate::str::contains("records target"));
    assert!(!target_b.exists(), "the refused run must not touch targetB");
}

#[test]
fn resuming_with_a_conflicting_skip_is_refused() {
    let dir = tempdir().unwrap();
    let src = dir.path().join("src.img");
    let dst = dir.path().join("out.img");
    let state = dir.path().join("run.state");
    fs::write(&src, pattern(64 * 1024)).unwrap();

    exhume().arg(&src).arg(&dst).arg(&state).assert().success();

    // The region map is relative to the recorded skip; a different value on
    // resume would shift the whole coordinate system.
    exhume()
        .arg(&src)
        .arg(&dst)
        .arg(&state)
        .arg("--skip")
        .arg("512")
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "conflicts with the resumed state's skip",
        ));
}

#[test]
fn resume_with_a_larger_length_copies_the_new_tail() {
    let dir = tempdir().unwrap();
    let src = dir.path().join("src.img");
    let dst = dir.path().join("out.img");
    let state = dir.path().join("run.state");
    let data = pattern(64 * 1024);
    fs::write(&src, &data).unwrap();

    exhume()
        .arg(&src)
        .arg(&dst)
        .arg(&state)
        .arg("--length")
        .arg("32K")
        .assert()
        .success();
    assert_eq!(fs::read(&dst).unwrap().len(), 32 * 1024);

    // The resumed map only covers the first 32K; the grown domain's tail must
    // be copied too, not silently reported as complete.
    exhume()
        .arg(&src)
        .arg(&dst)
        .arg(&state)
        .arg("--length")
        .arg("64K")
        .assert()
        .success()
        .stdout(predicate::str::contains("Done"));
    assert_eq!(fs::read(&dst).unwrap(), data);
}

#[test]
fn resumes_from_a_partial_state() {
    let dir = tempdir().unwrap();
    let src = dir.path().join("src.img");
    let dst = dir.path().join("out.img");
    let state = dir.path().join("run.state");
    let data = pattern(16 * 1024);
    fs::write(&src, &data).unwrap();
    // The interrupted run got the first 8 KiB onto the target.
    fs::write(&dst, &data[..8192]).unwrap();

    let seeded = format!(
        r#"[meta]
version = 1
program = "exhume"
program_version = "0.0.0"
created = "2026-06-18T08:00:00Z"
updated = "2026-06-18T08:00:00Z"

[params]
source = "{src}"
target = "{dst}"
sector_size = 512
transfer_size = 4096
skip = 0
seek = 0
length = 0
skip_unchanged = false
skip_zeros = false

[progress]
bytes_total = 16384
bytes_done = 8192
bytes_written = 8192
errors = 0

[[regions]]
start = 0
length = 8192
status = "done"

[[regions]]
start = 8192
length = 8192
status = "untried"
"#,
        src = src.display(),
        dst = dst.display()
    );
    fs::write(&state, &seeded).unwrap();

    exhume()
        .arg(&src)
        .arg(&dst)
        .arg(&state)
        .assert()
        .success()
        .stdout(predicate::str::contains("Done"));

    assert_eq!(fs::read(&dst).unwrap(), data);
    // Only the untried half was written; the write counter carries across.
    let s = fs::read_to_string(&state).unwrap();
    assert!(s.contains("bytes_written = 16384"), "state was:\n{s}");
}

#[test]
fn defaults_target_to_grave_img() {
    let dir = tempdir().unwrap();
    let src = dir.path().join("src.img");
    fs::write(&src, pattern(4096)).unwrap();

    exhume()
        .current_dir(dir.path())
        .arg(&src)
        .assert()
        .success();

    assert!(dir.path().join("grave.img").exists());
    // The auto-named state file is removed after the clean copy.
    assert!(!dir.path().join("grave.img.state").exists());
}

#[test]
fn json_flag_emits_a_machine_readable_summary() {
    let dir = tempdir().unwrap();
    let src = dir.path().join("src.img");
    let dst = dir.path().join("out.img");
    let data = pattern(128 * 1024);
    fs::write(&src, &data).unwrap();

    let output = exhume()
        .arg(&src)
        .arg(&dst)
        .arg("--json")
        .arg("--quiet")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let report: serde_json::Value = serde_json::from_slice(&output).expect("stdout is valid JSON");
    assert_eq!(report["status"], "completed");
    assert_eq!(report["bytes_total"], data.len() as u64);
    assert_eq!(report["bytes_done"], data.len() as u64);
    assert_eq!(report["bytes_written"], data.len() as u64);
    assert_eq!(report["bad_regions"], 0);
    assert_eq!(report["completed"], true);
    assert_eq!(report["interrupted"], false);
    assert_eq!(report["target"], dst.to_str().unwrap());
}
