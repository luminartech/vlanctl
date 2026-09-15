# Contributing

Pull requests are welcome, as are bug reports and questions in the issue
tracker.

`vlanctl` applies and tears down named VLAN profiles on a host. It is both a
CLI and a library: the library plans the commands, and the CLI runs them.

## Building and testing

The minimum supported Rust version is **1.88.0**, declared as `rust-version` in
`Cargo.toml`. That floor is real rather than conservative — `src/plan.rs` uses
let-chain expressions, which 1.85 and 1.87 reject with E0658.

`default = ["cli"]`, which pulls in `clap` and `libc` for argument parsing and
the root check. The library builds without them:

```sh
cargo test                        # default: the CLI and the library
cargo test --no-default-features  # the library alone
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

## The platform seam

`Platform` in `src/plan.rs` is the seam between planning a change and naming
the commands that carry it out. `MacOs` renders `ifconfig`/`route`, `Linux`
renders `ip`, and `host_platform()` picks one from the running host —
returning an error, not a default, on a host with no backend.

Two rules keep that seam honest, and both have tests that will tell you when
you break them:

- **A preview must render what an apply would actually run.**
  `preview_platform()` follows the host backend, so `--dry-run` on Linux prints
  `ip` commands. It was once pinned to `MacOs`, which made previews disagree
  with the applies they previewed.
- **Tests render through an explicit platform, never the host's.** The
  regression tests construct `&MacOs` or `&Linux` directly, so they assert the
  same thing on every developer's machine and in CI. A test that depends on
  where it runs proves nothing.

Commands are built as argv — program plus arguments — and never as a shell
string. `Cmd::display()` renders a shell-*like* string, but only for `show` and
`--dry-run` output. Keep it that way; see [`SECURITY.md`](SECURITY.md).

## Profiles

`profiles/example.toml` is the only profile in this repository. Bench profiles
-- ones that encode a particular sensor's VLAN layout, or pin a unit's MAC for
a static ARP entry -- are configuration for the bench that runs them, not
examples for a general reader, and they live with that bench rather than here.
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
