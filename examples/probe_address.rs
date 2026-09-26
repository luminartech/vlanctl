//! Probe an interface for another host holding an IPv4 address.
//!
//! ```text
//! cargo build --example probe_address
//! sudo setcap cap_net_raw=eip target/debug/examples/probe_address
//! target/debug/examples/probe_address eth0.10 192.168.10.149
//! ```

use std::net::Ipv4Addr;
use vlanctl::conflict::{self, Conflict};

fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(interface), Some(address)) = (args.next(), args.next()) else {
        eprintln!("usage: probe_address <interface> <ipv4-address>");
        std::process::exit(2);
    };
    let address: Ipv4Addr = match address.parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("not an IPv4 address: {address} ({e})");
            std::process::exit(2);
        }
    };
    match conflict::probe(&interface, address, conflict::DEFAULT_WINDOW) {
        Conflict::Free => println!("{address} on {interface}: free"),
        Conflict::InUse { mac } => {
            let mac = mac.map(|b| format!("{b:02x}")).join(":");
            println!("{address} on {interface}: in use by {mac}");
            std::process::exit(1);
        }
        Conflict::Unavailable(why) => {
            println!("{address} on {interface}: could not probe ({why})");
            std::process::exit(3);
        }
    }
}
