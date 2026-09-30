# Contributing

Pull requests are welcome, as are bug reports and questions in the issue
tracker.

`vlanctl` applies and tears down named VLAN profiles on a host. It is both a
CLI and a library: the library plans the commands, and the CLI runs them.

## Building and testing

The minimum supported Rust version is **1.88.0**, declared as `rust-version` in
`Cargo.toml`. That floor is real rather than conservative — `src/plan.rs` uses
let-chain expressions, which 1.85 and 1.87 reject with E0658.

`default = ["cli"]`, which pulls in `clap` for argument parsing and, for the
elevation check, `libc` on Unix or `windows-sys` on Windows. The library builds
without them:

```sh
cargo test                        # default: the CLI and the library
cargo test --no-default-features  # the library alone
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

## The platform seam

`Platform` in `src/plan.rs` is the seam between planning a change and naming
the commands that carry it out. `MacOs` renders `ifconfig`/`route`, `Linux`
renders `ip`, and Windows has two: `WindowsHyperV` (`src/plan/windows.rs`)
renders Hyper-V cmdlets through `powershell.exe` plus `netsh`, and
`WindowsDriverVlan` (`src/plan/windows_driver_vlan.rs`) sets the adapter
driver's `VlanID` keyword instead and holds one VLAN per adapter.
`host_platform()` picks a backend from the running host — returning an error,
not a default, on a host with no backend — and `host_platform_for(profile)`
refines that on Windows by the profile's shape. The state file records the
backend's `name()` so `down` runs through the one that applied
(`platform_named`); a backend that cannot take a profile at all says so in
`validate_profile`, which `apply` checks before touching anything.

Two rules keep that seam honest, and both have tests that will tell you when
you break them:

- **A preview must render what an apply would actually run.**
  `preview_platform()` follows the host backend, so `--dry-run` on Linux prints
  `ip` commands. It was once pinned to `MacOs`, which made previews disagree
  with the applies they previewed.
- **Tests render through an explicit platform, never the host's.** The
  regression tests construct `&MacOs`, `&Linux`, `&WindowsHyperV` or
  `&WindowsDriverVlan` directly, so they assert the same thing on every
  developer's machine and in CI. A test that depends on where it runs proves
  nothing.

Commands are built as argv — program plus arguments — and never as a shell
string. `Cmd::display()` renders a shell-*like* string, but only for `show` and
`--dry-run` output. Keep it that way; see [`SECURITY.md`](SECURITY.md).

The Windows backends are the one place a command carries a script: a
PowerShell command line is one argv element, `-Command <script>`, because
Hyper-V and the adapter keyword have no executable to call. Two rules keep
that inside the argv boundary. Every value
that enters a script goes through `ps_literal`, which renders a single-quoted
PowerShell literal — the one context in PowerShell where nothing is
interpolated and the only special character is `'`. And the dry-run allowlist
for those commands lives with the backend (`WindowsHyperV::is_read_only_probe`),
statement by statement, because `argv` alone cannot tell a probe from a
mutation there.

## Profiles

`profiles/example.toml` is the only profile in this repository. Profiles for a
specific test setup -- ones that encode a particular sensor's VLAN layout, or
pin a unit's MAC for a static ARP entry -- are configuration for that setup,
not examples for a general reader, and they belong with that setup rather than
here.
A new profile added to this repository should be one anybody can read and learn
the schema from.

Every TOML block in `README.md` is a profile a reader will copy, so each one
should parse and validate against the real deserializer. The README once
documented `[[vlan]]` with an `id` field while `src/config.rs` expected
`[[interface]]` with a `vlan` field, and every example in it failed.

## Commits and pull requests

Commit subjects follow [Conventional Commits](https://www.conventionalcommits.org/)
(`feat:`, `fix:`, `docs:`, `build:`, `chore:`, with a `!` for a breaking
change), because the changelog is organized around them. Say what changed and
why in the body.

This repository merges with **merge commits** — the `main` ruleset allows no
other method — so the subjects of the commits on your branch are what land on
`main`, and those are what release-plz reads to build the changelog and compute
the next version. A tidy PR title does not stand in for them.

**Do not bump the version in `Cargo.toml`, write `CHANGELOG.md`, or tag a
release.** release-plz owns all three. It opens a release PR against `main`
with the version bump and changelog already written; merging that PR publishes
to crates.io.

`main` requires a pull request with one approving review, resolved review
threads, and the CI checks in the `Main Gate` ruleset. `ci / Semver Checks` is
not among them only because `cargo-semver-checks` has no published baseline to
compare against until the first release; it should be added once one exists.
