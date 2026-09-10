//! Proves the library target exists and exports what a consumer needs.
//! An integration test links against the lib, not the binary, so this fails
//! to compile until `src/lib.rs` is present.

use vlanctl::config::Profile;
use vlanctl::net::{Cmd, RecordingRunner};

#[test]
fn a_consumer_can_build_a_cmd_and_a_recording_runner() {
    let c = Cmd::new("ifconfig", &["vlan10", "create"]);
    assert_eq!(c.display(), "ifconfig vlan10 create");
    let r = RecordingRunner::default();
    assert!(r.commands.is_empty());
}

#[test]
fn a_consumer_can_parse_a_profile_from_toml() {
    let toml = r#"
name = "t"
device = "en7"
[[interface]]
vlan = 11
address = "192.168.11.87/24"
"#;
    let p: Profile = toml::from_str(toml).expect("profile parses");
    assert_eq!(p.name, "t");
    assert_eq!(p.interfaces.len(), 1);
    p.validate().expect("profile validates");
}

/// The library must not require the CLI feature. Run this test with
/// `--no-default-features` to prove a consumer needs no argument parser:
///   cargo test --test lib_surface --no-default-features
#[test]
fn library_builds_without_the_cli_feature() {
    // Compiling this file at all under --no-default-features is the assertion.
    let _ = vlanctl::net::Cmd::new("true", &[]);
}

/// Compile-tests the README's "Library use" snippet: a consumer supplies a
/// `CommandRunner` and a `Platform` and drives `commands::apply` directly,
/// without shelling out to the `vlanctl` binary. `lum.toml` pins `device`,
/// so this needs no `ifconfig -l` seeding to resolve one.
#[test]
fn readme_library_use_snippet_applies_a_profile_through_a_recording_runner() {
    use vlanctl::plan::MacOs;
    use vlanctl::{commands, net::RecordingRunner};

    let profile =
        Profile::load("profiles/lum.toml".as_ref()).expect("the README's own example profile");
    let state_path = std::env::temp_dir().join("vlanctl-readme-library-snippet-state.json");
    let _ = std::fs::remove_file(&state_path);

    let mut runner = RecordingRunner::default(); // or SystemRunner to execute
    // A device override, exactly as the README shows: profiles no longer pin
    // one, and a `RecordingRunner` has no live system to auto-detect against.
    commands::apply(
        &mut runner,
        &MacOs,
        &profile,
        Some("en7"),
        &state_path,
        true,
    )
    .expect("dry-run apply of the README's own example profile");
    assert!(
        !runner.commands.is_empty(),
        "the README snippet's apply call should have recorded some commands"
    );
    for cmd in &runner.commands {
        println!("{}", cmd.display());
    }

    let _ = std::fs::remove_file(&state_path);
}
