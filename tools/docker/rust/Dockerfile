# syntax=docker/dockerfile:1
#
# This image, not `dtolnay/rust-toolchain@stable` fetched at job time, is
# what CI builds vlanctl with. The reason is not speed: it's that we do not
# control what is on our self-hosted runners. A toolchain action pulled at
# job start depends on the runner's network egress reaching wherever it
# fetches from, and on nothing else on that runner having drifted; a
# container pins rustc, every cargo subcommand, and the apt packages behind
# them to one content-addressed image, built once and reused by every job
# that needs it via the `image-lifecycle` job in
# .github/workflows/ci.yml (see luminartech/fw_ci's image-lifecycle.yml).
# luminartech/beacon and luminartech/dft both run every Rust job in a
# container for exactly this reason — this Dockerfile mirrors their
# tools/docker/rust/Dockerfile, trimmed to what vlanctl's own CI jobs
# (build, test, clippy, fmt, and `cargo release publish`) actually invoke.
# Do not "simplify" this back to a toolchain action: that reintroduces the
# runner-state dependency this file exists to remove.
#
# vlanctl is one small crate (lib + bin, no FFI beyond `libc` bindings, no
# cross-compilation, no docs generation), so this deliberately leaves out
# most of beacon's/dft's tool list: no nightly toolchain (miri, sanitizers),
# no cargo-fuzz, cargo-vet, cargo-udeps, cargo-cyclonedx, or cargo-criterion
# (nothing here fuzzes, audits supply chain, or benchmarks), and no Python
# (no docs/tooling in this repo invokes it). cargo-llvm-cov and
# cargo-semver-checks ARE included below — both have jobs in
# .github/workflows/ci.yml that invoke them. If vlanctl grows any of the
# excluded needs, add the corresponding tool here rather than reaching for a
# bare runner. This repo has no Python of its own, but it does carry the `uv`
# binaries and a `uv python install`-ed interpreter below — that's a
# dependency of a shared fw_ci action the `coverage` job calls, not of
# vlanctl; see that COPY line's comment before removing it.

FROM harbor-eq-dc.sw-ci.kraken.as/dockerhub-proxy/ubuntu:22.04

ENV DEBIAN_FRONTEND=noninteractive

# build-essential: a C linker/compiler, which `rustc` shells out to for
#   every binary it links, including vlanctl's own `cli` binary target.
# ca-certificates: TLS roots — rustup's install script, cargo-binstall's
#   download, and every Kellnr request in the jobs below are HTTPS.
# curl: fetches the rustup and cargo-binstall installers below.
# git, git-lfs, openssh-client: required by
#   luminartech/fw_ci/.github/actions/lum-checkout, which every job in
#   .github/workflows/ci.yml uses to check out this repo.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential \
        ca-certificates \
        curl \
        git \
        git-lfs \
        openssh-client \
    && rm -rf /var/lib/apt/lists/*

# Rust stable toolchain, plus the components its CI jobs need:
#   clippy, rustfmt  - the `test` job's lint/format steps.
#   llvm-tools-preview - cargo-llvm-cov links against this to turn the
#     instrumented binaries `cargo test` produces into a coverage report.
#     No nightly toolchain here: basic line coverage via `-C
#     instrument-coverage` has been stable since Rust 1.60, and the
#     `coverage` job below only needs a report, not the nightly-only
#     `#[coverage(off)]` test-module exclusion beacon's larger workspace
#     uses (see that job's comment in ci.yml for the full reasoning). No
#     miri or sanitizers run here either, which is the other usual reason
#     to carry nightly.
ENV RUSTUP_HOME=/opt/rustup
ENV CARGO_HOME=/opt/cargo
ENV PATH="/opt/cargo/bin:${PATH}"
ARG RUST_TOOLCHAIN=stable
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --profile minimal --default-toolchain ${RUST_TOOLCHAIN} \
    && rustup component add clippy rustfmt llvm-tools-preview

# cargo-binstall from its own prebuilt release (not `cargo install
# cargo-binstall`, which compiles it) — same bootstrap dft and envision use.
# Pinned so a new cargo-binstall release can't change what lands in this
# image out from under a rebuild.
ARG CARGO_BINSTALL_VERSION=1.22.0
RUN curl -fsSL "https://github.com/cargo-bins/cargo-binstall/releases/download/v${CARGO_BINSTALL_VERSION}/cargo-binstall-x86_64-unknown-linux-musl.tgz" \
        | tar -C /opt/cargo/bin -xz cargo-binstall \
    && cargo-binstall -V

# cargo-release: the `publish` job runs `cargo release publish` to push a
#   tagged version to Kellnr.
# cargo-llvm-cov: the `coverage` job's line-coverage report (informational —
#   see that job in ci.yml for why it does not gate anything).
# cargo-semver-checks: the `semver-checks` job compares a tag build against
#   the previous release (or the default branch on PR/branch builds) and, on
#   a tag, gates `publish` on the result — see that job for the first-release
#   bootstrap problem (no previous tag to compare against) and how it's
#   handled. Pinned to 0.50.0, not beacon's 0.47.0: 0.47.0 only understands
#   rustdoc JSON formats v56/v57 and this image's rustc (1.98.1) emits v60,
#   so `semver-checks` failed outright with "unsupported rustdoc format v60"
#   before ever comparing an API. v60 support landed in 0.49.0 (trustfall_rustdoc
#   upgrade, obi1kenobi/cargo-semver-checks#1661); 0.50.0 is the latest past
#   that point (it drops v56 support and adds v61, neither of which affects
#   v60) so it's pinned here rather than the bare minimum 0.49.0.
RUN cargo-binstall cargo-release --locked --version 0.25.17 \
    && cargo-binstall cargo-llvm-cov --locked --version 0.8.7 \
    && cargo-binstall cargo-semver-checks --locked --version 0.50.0 \
    && rm -rf /opt/cargo/registry /opt/cargo/git

# actions/checkout (via lum-checkout) expects to write into /__w and
# /runner/_work; neither exists in a stock Ubuntu image and checkout fails
# without them.
RUN mkdir -m 1777 /__w && mkdir -m 1777 -p /runner/_work

# uv (+ its `uvx` sibling): NOT a dependency of vlanctl itself, which has no
# Python anywhere in this repo — this is here solely because
# luminartech/fw_ci/.github/actions/lum-upload-artifact (used by the
# `coverage` job to push the LCOV/HTML report to S3) is implemented as a
# Python package and shells out to `uv run` internally. Without it that step
# fails with "uv: command not found" before it ever gets to argument
# validation. Same reason beacon and dft both carry uv in their rust images.
# If you are doing a lean-image pass and this looks unused, it is not: remove
# it and the `coverage` job's upload step breaks again.
#
# fw_ci's pyproject.toml pins `requires-python = ">=3.12,<4"`, which is newer
# than Ubuntu 22.04's system Python (3.10), so `uv python install` bakes a
# matching interpreter into the image at build time — same as beacon. This is
# the minimum the action needs: no venv, no pytest/ruff (that's dft's
# python-ci job, which vlanctl has no equivalent of).
COPY --from=harbor-eq-dc.sw-ci.kraken.as/dockerhub-proxy/astral/uv:latest /uv /uvx /bin/
ARG PYTHON_VERSION=3.12
RUN uv python install ${PYTHON_VERSION}
