# Security policy

## Reporting a vulnerability

Report security issues through GitHub's private vulnerability reporting: open
the [Security tab](https://github.com/luminartech/vlanctl/security) and choose
**Report a vulnerability**. That opens a private advisory visible only to the
maintainers.

Please do not open a public issue for a security report.

A report is most useful with the crate version, the host OS, the profile that
triggered the behavior, and the output of `vlanctl show <profile>` or
`vlanctl apply <profile> --dry-run`, which print the exact commands vlanctl
would run without running them.

## Supported versions

This crate is pre-1.0. Fixes land on the latest published version; there are no
maintained release branches.

## Scope

`vlanctl` configures host network interfaces. `apply` and `down` refuse to run
unelevated and, when run under `sudo` or from an elevated Windows shell,
execute `ifconfig`/`route` on macOS, `ip` on Linux, or Hyper-V cmdlets and
`netsh` on Windows. Three properties follow from what the tool is, and are the
design rather than defects:

- **It is privileged by design.** Creating VLAN interfaces, assigning
  addresses, adding routes and adding static ARP entries as root is the
  purpose. That `sudo vlanctl apply` changes the host's networking is not a
  vulnerability.
- **Profiles are trusted local configuration.** A profile is read from
  `--profiles-dir`. Anyone who can write a profile that root then applies could
  configure the host's networking directly instead; treat profile files with
  the same care as any other root-applied config. Their *contents* are still in
  scope — see below.
- **Static ARP entries are a supported feature.** A gatewayless `/32` route may
  carry a `mac`, which adds an `arp -s` entry for an on-link host that will not
  answer ARP. Pinning an address to a MAC is the intent.

What is in scope:

- **Anything that escapes the argv boundary.** Commands are spawned
  argv-style — `Command::new(&cmd.program).args(&cmd.args)` in `src/net.rs` —
  never through a shell, so profile fields cannot inject shell syntax. A
  profile value that escapes its argument slot, introduces an additional
  argument, or reaches a shell is a real vulnerability. On Windows the
  Hyper-V cmdlets are reached through `powershell.exe -Command <script>`,
  the script being one argument; every value in it is a single-quoted
  PowerShell literal (`ps_literal` in `src/plan/windows.rs`), and a value that
  ends that literal or adds a statement is the same vulnerability.
- **Validation that admits what it should reject.** `Profile::validate` in
  `src/config.rs` enforces that a route's `gateway` and `mac` are mutually
  exclusive, that a `mac` appears only on a single-host `/32`, and that it
  parses as a MAC. A bypass belongs here.
- **Anything that lets an unprivileged user steer a privileged run.** vlanctl
  records what it created in a state file at
  `/usr/local/var/vlanctl/state.json` (`%ProgramData%\vlanctl\state.json` on
  Windows), which `down` later reads to decide what to tear down. A path by
  which a non-root user influences that teardown is in scope.
- **Panics or hangs on a malformed profile**, notwithstanding that profiles are
  trusted input — the parser should fail cleanly.

Out of scope: the reachability of a sensor or any other host once the VLANs are
up, and the security of the protocols carried over them. vlanctl configures
interfaces; it does not carry traffic, terminate sessions, or hold keys.
