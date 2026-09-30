# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0](https://github.com/luminartech/vlanctl/compare/v0.1.0...v0.2.0) - 2026-09-30

### Added

- `--report` for a caller that cannot read vlanctl's output, and Serialize on Profile
- a second Windows backend on the adapter driver's VLAN keyword, chosen by profile shape
- Windows backend through Hyper-V virtual adapters and netsh
- *(commands)* tell "could not check" apart from "failed" after a clean run
- *(commands)* let a caller recover which interfaces failed verification
- *(conflict)* say why a probe could not run, as something to match on
- *(conflict)* ask the wire whether an address is already held
- *(permanence)* render a profile to netplan, renderer-aware
- *(commands)* prove an apply against the host, not the exit code
- *(commands)* prove a teardown against the host, not the exit code

### Documentation

- describe the Windows backend for readers of the crate

### Fixed

- *(conflict)* import `Instant` only where the ARP probe uses it
- *(windows)* don't report the driver backend's own parent as a teardown survivor
- *(windows)* put the parent's own address back after the switch is removed
- *(windows)* fail loudly when New-VMSwitch can't actually bind the adapter
- *(windows)* leave the stack's multicast and broadcast neighbors alone on teardown
- *(windows)* let the driver backend re-apply, and say why netsh failed
- tear the previous profile down through the backend that applied it
- *(permanence)* take the renderer rules the bench proved, not the ones it disproved
