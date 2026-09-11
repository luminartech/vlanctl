//! Apply and tear down named VLAN profiles.
//!
//! The library half of `vlanctl`. Callers supply a
//! [`net::CommandRunner`] and a [`plan::Platform`], so the same profile can
//! be rendered as a dry run, executed for real, or targeted at a different
//! operating system.
//!
//! The binary adds argument parsing and a root check; neither belongs here —
//! a consumer may hold `CAP_NET_ADMIN` without being root.

pub mod commands;
pub mod config;
pub mod device;
pub mod net;
pub mod plan;
pub mod state;
