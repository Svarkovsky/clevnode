//! The binary, driven the way a stranger drives it.
//!
//! Everything here runs against the fixture sysfs tree, so no device is
//! enumerated, touched or written — these tests are safe on a host with the
//! rig attached, and they pass on a host with nothing attached at all.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

const EXE: &str = env!("CARGO_BIN_EXE_lnflash");

#[path = "fixtures/materialize.rs"]
mod materialize;

fn fixture_sysfs() -> PathBuf {
    materialize::materialized().to_path_buf()
}

/// An unpacked bundle: binary at the top, payload under `firmware/`, exactly
/// the layout `just lnflash-bundle` produces.
fn unpacked_bundle() -> TempDir {
    let dir = TempDir::new().unwrap();
    let firmware = dir.path().join("firmware/t114");
    fs::create_dir_all(&firmware).unwrap();

    let hex = include_str!("../payload/t114/s140_nrf52_7.3.0_softdevice.hex");
    let licence = include_str!("../payload/t114/s140_nrf52_7.3.0_license-agreement.txt");
    // A minimal but real UF2 at the application base.
    let app = lnflash::uf2::Image::from_spans(
        &[lnflash::ihex::Span {
            start: 0x2_7000,
            data: vec![0x5A; 0x800],
        }],
        lnflash::uf2::FAMILY_NRF52840_APP,
    )
    .encode()
    .unwrap();

    fs::write(firmware.join("s140_nrf52_7.3.0_softdevice.hex"), hex).unwrap();
    fs::write(
        firmware.join("s140_nrf52_7.3.0_license-agreement.txt"),
        licence,
    )
    .unwrap();
    fs::write(firmware.join("leviculum-t114-0.8.0.uf2"), &app).unwrap();
    fs::write(
        dir.path().join("firmware/manifest.toml"),
        manifest_text(
            &lnflash::manifest::hex_digest(&app),
            &lnflash::manifest::hex_digest(hex.as_bytes()),
        ),
    )
    .unwrap();
    dir
}

/// The same bundle with the RAK4631 image beside the T114's, which is what
/// `scripts/lnflash-bundle.sh` stages since Codeberg #261. No SoftDevice
/// remedy for the RAK: the bundle carries none, by decision.
fn unpacked_bundle_with_rak() -> TempDir {
    let dir = unpacked_bundle();
    let firmware = dir.path().join("firmware/rak4631");
    fs::create_dir_all(&firmware).unwrap();
    let app = lnflash::uf2::Image::from_spans(
        &[lnflash::ihex::Span {
            start: 0x2_7000,
            data: vec![0xCD; 0x800],
        }],
        lnflash::uf2::FAMILY_NRF52840_APP,
    )
    .encode()
    .unwrap();
    fs::write(firmware.join("leviculum-rak4631-0.8.0.uf2"), &app).unwrap();

    let path = dir.path().join("firmware/manifest.toml");
    let text = format!(
        "{}\n[board.rak4631.app]\nfile    = \"rak4631/leviculum-rak4631-0.8.0.uf2\"\n\
         sha256  = \"{}\"\ngit_sha = \"bb7c4f64\"\n",
        fs::read_to_string(&path).unwrap(),
        lnflash::manifest::hex_digest(&app),
    );
    fs::write(&path, text).unwrap();
    dir
}

/// The manifest `scripts/lnflash-bundle.sh` writes: the release, and one
/// image per board. The board facts are the compiled-in catalogue's since
/// Codeberg #342.
fn manifest_text(app_sha: &str, sd_sha: &str) -> String {
    format!(
        r#"
[bundle]
version = "0.8.0"
built   = "2026-08-10"

[board.t114.app]
file    = "t114/leviculum-t114-0.8.0.uf2"
sha256  = "{app_sha}"
git_sha = "bb7c4f64"

[board.t114.remedy.softdevice]
file    = "t114/s140_nrf52_7.3.0_softdevice.hex"
sha256  = "{sd_sha}"
license = "t114/s140_nrf52_7.3.0_license-agreement.txt"
convert = "hex-to-uf2"
"#
    )
}

/// The board-fact sections a pre-#342 bundle carried. Kept here so the
/// backwards-compatibility test states what an old manifest looked like
/// rather than pointing at a git revision.
const PRE_342_BOARD_FACTS: &str = r#"
[board.t114]
family    = "nrf52840"
transport = "uf2-msc"
entry     = ["touch-1200", "double-tap"]

[board.t114.identify]
info_uf2_board_id = "HT-n5262"
bootloader_usb    = ["239a:0071"]
candidate_usb     = ["1209:0001", "239a:8071"]
msc_label         = "HT-n5262"

[board.t114.flash]
family_id      = 0xADA52840
writable_start = 0x1000
writable_end   = 0xEA000
app_base       = 0x27000

[board.t114.requires]
softdevice = ">=7.0.1, <8.0.0"
"#;

/// Run the binary with no `$LNFLASH_BUNDLE` unless one is given, so the
/// developer's own environment cannot decide a test's outcome.
fn run(args: &[&str], env: Option<(&str, &Path)>) -> Output {
    let mut cmd = Command::new(EXE);
    cmd.args(args).env_remove(lnflash::manifest::BUNDLE_ENV);
    if let Some((key, value)) = env {
        cmd.env(key, value);
    }
    cmd.output().unwrap()
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn an_unpacked_bundle_is_found_by_pointing_at_its_root() {
    let bundle = unpacked_bundle();
    let out = run(
        &[
            "--bundle",
            &bundle.path().display().to_string(),
            "--check-bundle",
        ],
        None,
    );
    assert!(out.status.success(), "{}", stdout(&out));
    assert!(stdout(&out).contains("matches its recorded checksum"));
    assert!(stdout(&out).contains("carrying t114"));
}

#[test]
fn a_bundle_carrying_both_boards_loads_and_checks_both_images() {
    // Codeberg #261. The tarball a stranger downloads now has firmware for a
    // RAK4631 in it, and --check-bundle is the one command that reads every
    // image without a board attached — so it is the one that proves both
    // arrived intact.
    let bundle = unpacked_bundle_with_rak();
    let out = run(
        &[
            "--bundle",
            &bundle.path().display().to_string(),
            "--check-bundle",
        ],
        None,
    );
    let said = stdout(&out);
    assert!(out.status.success(), "{said}");
    assert!(said.contains("carrying rak4631, t114"), "{said}");
    assert!(said.contains("matches its recorded checksum"), "{said}");
}

#[test]
fn a_two_board_bundle_whose_rak_image_was_corrupted_fails_the_check() {
    // The positive control for the test above: with only the RAK image
    // touched, the check has to fail. Without this, "both verified" is
    // indistinguishable from "the second one was never read".
    let bundle = unpacked_bundle_with_rak();
    fs::write(
        bundle
            .path()
            .join("firmware/rak4631/leviculum-rak4631-0.8.0.uf2"),
        b"not the image the manifest recorded",
    )
    .unwrap();
    let out = run(
        &[
            "--bundle",
            &bundle.path().display().to_string(),
            "--check-bundle",
        ],
        None,
    );
    assert!(!out.status.success(), "{}", stdout(&out));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("sha256"), "{err}");
    assert!(err.contains("leviculum-rak4631-0.8.0.uf2"), "{err}");
}

#[test]
fn a_t114_only_bundle_still_loads_and_still_names_only_the_board_it_carries() {
    // The control for #261: a tarball built before the RAK entry existed is
    // still a valid bundle. It loads, it checks, and it advertises exactly one
    // board — the catalogue growing must not make old bundles look broken.
    let bundle = unpacked_bundle();
    let out = run(
        &[
            "--bundle",
            &bundle.path().display().to_string(),
            "--check-bundle",
        ],
        None,
    );
    let said = stdout(&out);
    assert!(out.status.success(), "{said}");
    assert!(said.contains("carrying t114"), "{said}");
    assert!(!said.contains("rak4631"), "{said}");
}

#[test]
fn a_bundle_from_before_the_catalogue_split_still_loads() {
    // A tarball downloaded before #342 carries the board facts in its own
    // manifest. They are now the catalogue's, and the catalogue is compiled
    // into the binary reading them — so the old sections are ignored rather
    // than refused, and the tarball keeps working.
    let bundle = unpacked_bundle();
    let path = bundle.path().join("firmware/manifest.toml");
    let old = format!(
        "{}{PRE_342_BOARD_FACTS}",
        fs::read_to_string(&path).unwrap()
    );
    fs::write(&path, old).unwrap();

    let out = run(
        &[
            "--bundle",
            &bundle.path().display().to_string(),
            "--check-bundle",
        ],
        None,
    );
    assert!(out.status.success(), "{}", stdout(&out));
    assert!(stdout(&out).contains("matches its recorded checksum"));
}

#[test]
fn the_environment_variable_finds_the_bundle_too() {
    let bundle = unpacked_bundle();
    let out = run(
        &["--check-bundle"],
        Some((lnflash::manifest::BUNDLE_ENV, bundle.path())),
    );
    assert!(out.status.success(), "{}", stdout(&out));
    assert!(stdout(&out).contains("matches its recorded checksum"));
}

#[test]
fn an_explicit_bundle_beats_the_environment_variable() {
    let good = unpacked_bundle();
    let broken = TempDir::new().unwrap();
    fs::create_dir_all(broken.path().join("firmware")).unwrap();
    fs::write(
        broken.path().join("firmware/manifest.toml"),
        "not a manifest",
    )
    .unwrap();

    let out = run(
        &[
            "--bundle",
            &good.path().display().to_string(),
            "--check-bundle",
        ],
        Some((lnflash::manifest::BUNDLE_ENV, broken.path())),
    );
    assert!(out.status.success(), "{}", stdout(&out));
}

#[test]
fn a_tampered_payload_fails_the_bundle_check_and_the_exit_code_says_so() {
    let bundle = unpacked_bundle();
    fs::write(
        bundle.path().join("firmware/t114/leviculum-t114-0.8.0.uf2"),
        b"not the image the manifest recorded",
    )
    .unwrap();
    let out = run(
        &[
            "--bundle",
            &bundle.path().display().to_string(),
            "--check-bundle",
        ],
        None,
    );
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("sha256"));
}

#[test]
fn a_bundle_missing_the_nordic_licence_will_not_even_load() {
    let bundle = unpacked_bundle();
    fs::remove_file(
        bundle
            .path()
            .join("firmware/t114/s140_nrf52_7.3.0_license-agreement.txt"),
    )
    .unwrap();
    let out = run(
        &[
            "--bundle",
            &bundle.path().display().to_string(),
            "--check-bundle",
        ],
        None,
    );
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("s140_nrf52_7.3.0_license-agreement.txt"));
}

#[test]
fn no_bundle_anywhere_names_every_place_it_looked() {
    let nowhere = TempDir::new().unwrap();
    let out = run(
        &[
            "--bundle",
            &nowhere.path().join("absent").display().to_string(),
        ],
        None,
    );
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("absent"), "{err}");
    assert!(err.contains("/usr/share/lnflash"), "{err}");
}

#[test]
fn a_dry_run_reports_every_board_on_the_bus_and_writes_nothing() {
    let bundle = unpacked_bundle();
    let out = run(
        &[
            "--bundle",
            &bundle.path().display().to_string(),
            "--dry-run",
            "--sysfs",
            &fixture_sysfs().display().to_string(),
        ],
        None,
    );
    let said = stdout(&out);
    assert!(out.status.success(), "{said}");
    // The T114 application on 3-2.3.1, its bootloader on 3-2.4, and — since
    // Codeberg #261 — the RAK4631 application on 3-2.3.4.4. That third line is
    // the ticket's other end: before the catalogue entry existed, a Pocket V2
    // on the same hub was not a device lnflash could see at all.
    assert!(said.contains("Found 3 device(s)"), "{said}");
    assert!(
        said.contains("3-2.3.1 [1209:0001] 183004F712B4A7FE"),
        "{said}"
    );
    assert!(
        said.contains("3-2.3.4.4 [1209:0002] DEC9947DAD9D2869"),
        "{said}"
    );
    assert!(
        said.contains("3-2.4 [239a:0071] 12B4A7FE183004F7"),
        "{said}"
    );
    // Each is hinted at its own board, never at whichever the catalogue lists
    // first — the hint decides which bootloader IDs get waited for.
    assert!(
        said.contains("leviculum RAK4631 — probably a rak4631"),
        "{said}"
    );
    assert!(said.contains("leviculum T114 — probably a t114"), "{said}");
    // And it stops before doing anything to any of them.
    assert!(
        said.contains("rebooting a board is already a change"),
        "{said}"
    );
    assert!(!said.contains("copied"), "{said}");
}

#[test]
fn an_empty_bus_is_a_clean_report_rather_than_an_error_message() {
    let bundle = unpacked_bundle();
    let empty = TempDir::new().unwrap();
    let out = run(
        &[
            "--bundle",
            &bundle.path().display().to_string(),
            "--dry-run",
            "--sysfs",
            &empty.path().display().to_string(),
        ],
        None,
    );
    assert!(out.status.success());
    let said = stdout(&out);
    // "lnflash knows", not "this bundle knows": since #342 the boards are the
    // compiled-in catalogue's, and the bundle only says which of them it has
    // an image for.
    assert!(
        said.contains("No board lnflash knows is attached"),
        "{said}"
    );
    // The board that produces this is usually attached, just dark: nothing to
    // enumerate and nothing to touch. Saying so is the whole use of the run.
    assert!(said.contains("Double-tap RESET"), "{said}");
}

#[test]
fn a_run_that_was_supposed_to_write_and_did_not_exits_non_zero() {
    // Without --dry-run and with no board it can write to, the tool must not
    // report success: a CI job that flashes nothing has not flashed.
    let bundle = unpacked_bundle();
    let empty = TempDir::new().unwrap();
    let out = run(
        &[
            "--bundle",
            &bundle.path().display().to_string(),
            "--yes",
            "--sysfs",
            &empty.path().display().to_string(),
        ],
        None,
    );
    assert!(!out.status.success());
}

#[test]
fn the_help_text_says_it_needs_root_and_never_uses_the_network() {
    let out = run(&["--help"], None);
    assert!(out.status.success());
    let help = stdout(&out);
    assert!(help.contains("Needs root"), "{help}");
    assert!(help.contains("No network access"), "{help}");
    for flag in [
        "--dry-run",
        "--yes",
        "--bundle",
        "--board",
        "--check-bundle",
        "--radio-freq",
        "--radio-bw",
        "--radio-sf",
        "--radio-cr",
        "--radio-txpower",
        "--radio-preset",
        "--no-radio",
        "--telemetry",
        "--telemetry-profile",
        "--telemetry-key",
        "--no-telemetry",
        "--set-time",
        "--set-telemetry",
    ] {
        assert!(help.contains(flag), "{flag} missing from --help:\n{help}");
    }
    // What the radio flags do has to be readable without the source.
    assert!(help.contains("Spreading factor, 7 to 12"), "{help}");
    assert!(help.contains("comes back up on that frequency"), "{help}");
    // And so does the telemetry decision: default no, one input on yes.
    assert!(help.contains("The default is no"), "{help}");
    assert!(help.contains("32 hex characters"), "{help}");
}

#[test]
fn an_unavailable_preset_is_refused_before_anything_is_enumerated() {
    // eu433 is decided but needs 10 dBm, which the PA driver cannot produce
    // yet. Refusing beats transmitting 4 dB over the limit, and it has to
    // happen before a board is touched.
    let bundle = unpacked_bundle();
    let out = run(
        &[
            "--bundle",
            &bundle.path().display().to_string(),
            "--yes",
            "--radio-preset",
            "eu433",
            "--sysfs",
            &fixture_sysfs().display().to_string(),
        ],
        None,
    );
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("10 dBm"), "{err}");
    assert!(err.contains("not offered"), "{err}");
    assert!(
        stdout(&out).is_empty(),
        "nothing should have run: {}",
        stdout(&out)
    );
}

#[test]
fn a_preset_and_explicit_values_together_stop_the_run() {
    let bundle = unpacked_bundle();
    let out = run(
        &[
            "--bundle",
            &bundle.path().display().to_string(),
            "--yes",
            "--radio-preset",
            "eu868",
            "--radio-freq",
            "867100000",
            "--sysfs",
            &fixture_sysfs().display().to_string(),
        ],
        None,
    );
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("pick one"), "{err}");
    assert!(stdout(&out).is_empty(), "{}", stdout(&out));
}

#[test]
fn a_mistyped_lxmf_address_stops_the_run_at_the_command_line() {
    // The flash has not happened yet when the flags are parsed, so a typo in
    // the one input telemetry needs costs nothing but the retype.
    let bundle = unpacked_bundle();
    let out = run(
        &[
            "--bundle",
            &bundle.path().display().to_string(),
            "--yes",
            "--telemetry",
            "a7b2c3",
            "--sysfs",
            &fixture_sysfs().display().to_string(),
        ],
        None,
    );
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("32 hex characters"), "{err}");
    assert!(err.contains("16 bytes"), "{err}");
    assert!(stdout(&out).is_empty(), "{}", stdout(&out));
}

#[test]
fn switching_telemetry_on_and_off_at_once_stops_the_run() {
    let bundle = unpacked_bundle();
    let out = run(
        &[
            "--bundle",
            &bundle.path().display().to_string(),
            "--yes",
            "--no-telemetry",
            "--telemetry",
            "a7b2c3d4e5f60718293a4b5c6d7e8f90",
            "--sysfs",
            &fixture_sysfs().display().to_string(),
        ],
        None,
    );
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("pick one"), "{err}");
    assert!(stdout(&out).is_empty(), "{}", stdout(&out));
}

#[test]
fn the_config_only_session_needs_a_running_board_and_says_so() {
    // --set-telemetry configures without flashing, so a bus with nothing
    // running on it is a reported fact rather than a wait.
    let bundle = unpacked_bundle();
    let empty = TempDir::new().unwrap();
    let out = run(
        &[
            "--bundle",
            &bundle.path().display().to_string(),
            "--yes",
            "--set-telemetry",
            "--telemetry",
            "a7b2c3d4e5f60718293a4b5c6d7e8f90",
            "--sysfs",
            &empty.path().display().to_string(),
        ],
        None,
    );
    assert!(!out.status.success());
    let said = stdout(&out);
    assert!(said.contains("No running LNode on the bus"), "{said}");
    assert!(said.contains("--set-telemetry"), "{said}");
    // It must not have gone anywhere near a bootloader or a write.
    assert!(!said.contains("copied"), "{said}");
}

#[test]
fn the_config_only_sessions_run_with_no_bundle_anywhere() {
    // Codeberg #342. Activation is configuration, not firmware (#236/#238):
    // somebody pointing a node they already own at an LXMF address has no use
    // for a firmware bundle, and the board facts these sessions need — which
    // USB IDs are LNodes — are in the compiled-in catalogue, not in a bundle.
    //
    // Reaching "No running LNode on the bus" is what proves the session
    // started: that message comes from `flow::set_time`/`set_telemetry` after
    // the bus has been enumerated.
    let empty = TempDir::new().unwrap();
    let nowhere = TempDir::new().unwrap();
    for args in [
        vec!["--set-time"],
        vec![
            "--set-telemetry",
            "--telemetry",
            "a7b2c3d4e5f60718293a4b5c6d7e8f90",
        ],
        vec!["--set-tx-spacing", "60"],
    ] {
        // --bundle names a directory that does not exist, so no bundle can be
        // found by any of the four resolution steps.
        let bundle = nowhere.path().join("absent").display().to_string();
        let mut full = vec!["--bundle", &bundle, "--yes"];
        full.extend_from_slice(&args);
        let sysfs = empty.path().display().to_string();
        full.extend_from_slice(&["--sysfs", &sysfs]);
        let out = run(&full, None);
        let said = stdout(&out);
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            said.contains("No running LNode on the bus"),
            "{args:?}\nstdout: {said}\nstderr: {err}"
        );
        // And it must not have complained about a bundle it has no use for.
        assert!(!err.contains("no bundle found"), "{args:?}: {err}");
    }
}

#[test]
fn the_transmit_spacing_session_needs_a_running_board_and_says_so() {
    // #345. Like the other configure-only sessions it never flashes, so an
    // empty bus is a reported fact rather than a wait — and the message has
    // to name the flag, because a sweep script reads this and nothing else.
    let empty = TempDir::new().unwrap();
    let out = run(
        &[
            "--yes",
            "--set-tx-spacing",
            "60",
            "--sysfs",
            &empty.path().display().to_string(),
        ],
        None,
    );
    assert!(!out.status.success());
    let said = stdout(&out);
    assert!(said.contains("No running LNode on the bus"), "{said}");
    assert!(said.contains("--set-tx-spacing"), "{said}");
}

#[test]
fn a_transmit_spacing_that_does_not_fit_the_wire_stops_at_the_command_line() {
    // The frame carries a u16, so a value the board could never take has
    // to be refused before a board is touched, not truncated into a
    // different sweep point.
    let empty = TempDir::new().unwrap();
    for value in ["-1", "70000", "sixty"] {
        let out = run(
            &[
                "--yes",
                "--set-tx-spacing",
                value,
                "--sysfs",
                &empty.path().display().to_string(),
            ],
            None,
        );
        assert!(!out.status.success(), "{value} was accepted");
        // The refusal names the value that was refused, so a sweep script
        // that mistypes a point is told which one.
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains(value), "{value}: {err}");
        // And nothing was enumerated: the run stopped at the command line.
        assert!(
            !stdout(&out).contains("No running LNode on the bus"),
            "{value}: the session started before the value was checked"
        );
    }
}

#[test]
fn a_flash_still_refuses_to_run_without_a_bundle_and_says_so() {
    // The other half of #342: the config sessions stopped needing images, the
    // flashing paths did not. The error has to name what is missing for the
    // operation that was asked for.
    let nowhere = TempDir::new().unwrap();
    let empty = TempDir::new().unwrap();
    let out = run(
        &[
            "--bundle",
            &nowhere.path().join("absent").display().to_string(),
            "--yes",
            "--sysfs",
            &empty.path().display().to_string(),
        ],
        None,
    );
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("no bundle found"), "{err}");
    assert!(err.contains("absent"), "{err}");
}

#[test]
fn a_bundle_missing_its_image_still_stops_a_flash_before_any_board_is_touched() {
    // The guard does not get weaker on the flashing paths. A checksum
    // mismatch is only reachable once the image is actually read, which needs
    // root and a mounted bootloader drive (`--check-bundle` above covers that
    // half, and flow::prepare covers it at the unit level). What a flash can
    // be held to with no hardware is the load-time half: an image the
    // manifest names and the bundle does not carry stops the run before the
    // bus is even enumerated.
    let bundle = unpacked_bundle();
    fs::remove_file(bundle.path().join("firmware/t114/leviculum-t114-0.8.0.uf2")).unwrap();
    let out = run(
        &[
            "--bundle",
            &bundle.path().display().to_string(),
            "--yes",
            "--sysfs",
            &fixture_sysfs().display().to_string(),
        ],
        None,
    );
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("leviculum-t114-0.8.0.uf2"), "{err}");
    assert!(err.contains("not in the bundle"), "{err}");
    // Nothing was enumerated: the bundle was refused first.
    assert!(!stdout(&out).contains("Found"), "{}", stdout(&out));
}

#[test]
fn the_prompt_reaches_a_piped_stdin_and_a_piped_stdin_answers_no() {
    // The real Console on a real pipe, not a test double: `Command::output`
    // gives the child an empty stdin, which is exactly the shape of a
    // scripted run. It must print the question, read end-of-input as "no",
    // and finish — a tool that blocks here hangs somebody's CI job.
    let bundle = unpacked_bundle();
    let out = run(
        &[
            "--bundle",
            &bundle.path().display().to_string(),
            "--set-telemetry",
            "--sysfs",
            &fixture_sysfs().display().to_string(),
        ],
        None,
    );
    let said = stdout(&out);
    assert!(said.contains("Send telemetry? [y/N]"), "{said}");
    assert!(said.contains("Telemetry left as it is"), "{said}");
    // No address was asked for, because the first question was answered no.
    assert!(!said.contains("LXMF address"), "{said}");
    assert!(out.status.success(), "{said}");
}

#[test]
fn an_impossible_radio_value_stops_the_run_at_the_command_line() {
    // A board written and then handed a configuration its firmware refuses
    // is the outcome this prevents: the check happens before enumeration.
    let bundle = unpacked_bundle();
    let out = run(
        &[
            "--bundle",
            &bundle.path().display().to_string(),
            "--yes",
            "--radio-sf",
            "3",
            "--sysfs",
            &fixture_sysfs().display().to_string(),
        ],
        None,
    );
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("SF3 is outside SF7-SF12"), "{err}");
    assert!(stdout(&out).is_empty(), "{}", stdout(&out));
}
