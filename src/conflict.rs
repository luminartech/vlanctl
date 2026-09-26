//! Ask the segment whether another host already holds an IPv4 address.
//!
//! `ip addr add` and `ifconfig inet` perform no duplicate-address detection:
//! an address another host already holds is assigned without complaint and
//! reads back correctly afterwards. Verifying the host after an apply cannot
//! see the problem, so it has to be asked of the wire.
//!
//! [`probe`] sends an RFC 5227 ARP probe for the address — sender IP
//! `0.0.0.0`, so a probe for an address that turns out to be taken poisons no
//! neighbor's ARP cache with our MAC — and listens for anyone else claiming
//! it. It works on an interface with or without the address assigned, so a
//! caller can ask before an apply claims the address or after.
//!
//! Needs a raw datalink channel: `CAP_NET_RAW` on Linux, read access to
//! `/dev/bpf*` on macOS. A probe that cannot run says so
//! ([`Conflict::Unavailable`]) rather than reporting the address free.

use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

/// How long [`probe`] listens when the caller has no reason to choose.
///
/// RFC 5227 waits a second or more before claiming an address. A host that
/// holds an address answers a probe for it in well under that on a local
/// segment, and a caller probing several interfaces pays this once per
/// address, so this trades a little of the RFC's margin for responsiveness.
pub const DEFAULT_WINDOW: Duration = Duration::from_millis(500);

/// What the segment said about an address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Conflict {
    /// The probe ran for the whole window and nobody else claimed the address.
    Free,
    /// Another host claimed the address; `mac` is the hardware address it
    /// claimed it from.
    InUse { mac: [u8; 6] },
    /// The probe could not run, so nothing is known about the address — no
    /// capture privilege, an interface with no hardware address, or a send
    /// or read failure. The string says which.
    Unavailable(String),
}

/// Probe `interface` for another host holding `address`, listening for
/// `window`.
///
/// Sends two ARP probes back to back (the second covers a momentarily full
/// transmit queue) and returns [`Conflict::InUse`] on the first ARP packet
/// from a foreign MAC that claims `address` — a reply, an announcement, or
/// another host's own probe for it. Frames from our own MAC are ignored,
/// because some kernels loop outgoing ARP back to the listening side.
///
/// Blocks for at most about `window` plus one 10 ms read interval.
pub fn probe(interface: &str, address: Ipv4Addr, window: Duration) -> Conflict {
    imp::probe(interface, address, window)
}

const ETHER_ARP_FRAME_LEN: usize = 42; // 14 Ethernet header + 28 ARP packet
const ETHERTYPE_ARP: u16 = 0x0806;
const ARP_HW_ETHERNET: u16 = 1;
const ARP_PROTO_IPV4: u16 = 0x0800;
const ARP_OPCODE_REQUEST: u16 = 1;

/// The 42-byte Ethernet + ARP frame for an RFC 5227 probe: a broadcast
/// "who has `address`?" whose sender IP is `0.0.0.0`.
#[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
fn build_probe(own_mac: [u8; 6], address: Ipv4Addr) -> [u8; ETHER_ARP_FRAME_LEN] {
    let mut buf = [0u8; ETHER_ARP_FRAME_LEN];
    buf[0..6].copy_from_slice(&[0xff; 6]);
    buf[6..12].copy_from_slice(&own_mac);
    buf[12..14].copy_from_slice(&ETHERTYPE_ARP.to_be_bytes());
    buf[14..16].copy_from_slice(&ARP_HW_ETHERNET.to_be_bytes());
    buf[16..18].copy_from_slice(&ARP_PROTO_IPV4.to_be_bytes());
    buf[18] = 6;
    buf[19] = 4;
    buf[20..22].copy_from_slice(&ARP_OPCODE_REQUEST.to_be_bytes());
    buf[22..28].copy_from_slice(&own_mac);
    // Sender protocol address stays 0.0.0.0 (RFC 5227 §2.1.1), and the
    // target hardware address stays zero.
    buf[38..42].copy_from_slice(&address.octets());
    buf
}

/// The MAC of a foreign host that `frame` shows claiming `address`, if any.
///
/// A claim is an ARP packet from a MAC other than `own_mac` that either
/// names `address` as its sender (a reply or an announcement) or is itself
/// a probe for it (sender `0.0.0.0`, target `address`) — RFC 5227 treats a
/// simultaneous probe from another host as a conflict too.
#[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
fn claimant(frame: &[u8], address: Ipv4Addr, own_mac: [u8; 6]) -> Option<[u8; 6]> {
    if frame.len() < ETHER_ARP_FRAME_LEN
        || u16::from_be_bytes([frame[12], frame[13]]) != ETHERTYPE_ARP
        || u16::from_be_bytes([frame[16], frame[17]]) != ARP_PROTO_IPV4
    {
        return None;
    }
    let mut sender_mac = [0u8; 6];
    sender_mac.copy_from_slice(&frame[22..28]);
    if sender_mac == own_mac {
        return None;
    }
    let sender_ip = Ipv4Addr::new(frame[28], frame[29], frame[30], frame[31]);
    let target_ip = Ipv4Addr::new(frame[38], frame[39], frame[40], frame[41]);
    let opcode = u16::from_be_bytes([frame[20], frame[21]]);
    let claims = sender_ip == address
        || (opcode == ARP_OPCODE_REQUEST && sender_ip.is_unspecified() && target_ip == address);
    claims.then_some(sender_mac)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod imp {
    use super::{Conflict, Duration, Instant, Ipv4Addr, build_probe, claimant};

    /// Per-read wait. Bounds how far past the window one non-matching frame
    /// on a busy segment can carry the probe.
    const READ_INTERVAL: Duration = Duration::from_millis(10);

    /// Cap on one send, so a stalled transmit queue cannot spend the whole
    /// window before listening starts.
    const WRITE_TIMEOUT: Duration = Duration::from_millis(20);

    pub(super) fn probe(interface: &str, address: Ipv4Addr, window: Duration) -> Conflict {
        // The clock starts before the channel opens, so slow setup is
        // spent from the window rather than added to it.
        let deadline = Instant::now() + window;

        let Some(iface) = pnet_datalink::interfaces()
            .into_iter()
            .find(|i| i.name == interface)
        else {
            return Conflict::Unavailable(format!("interface {interface} does not exist"));
        };
        let Some(own_mac) = iface.mac.map(|m| m.octets()).filter(|m| m != &[0u8; 6]) else {
            return Conflict::Unavailable(format!("interface {interface} has no hardware address"));
        };

        let config = pnet_datalink::Config {
            read_timeout: Some(READ_INTERVAL),
            write_timeout: Some(WRITE_TIMEOUT),
            ..Default::default()
        };
        let (mut tx, mut rx) = match pnet_datalink::channel(&iface, config) {
            Ok(pnet_datalink::Channel::Ethernet(tx, rx)) => (tx, rx),
            Ok(_) => {
                return Conflict::Unavailable(format!(
                    "{interface} did not offer an Ethernet channel"
                ));
            }
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                return Conflict::Unavailable(format!(
                    "no permission to open a raw channel on {interface} ({e}); \
                     this needs CAP_NET_RAW on Linux or /dev/bpf access on macOS"
                ));
            }
            Err(e) => {
                return Conflict::Unavailable(format!(
                    "could not open a raw channel on {interface}: {e}"
                ));
            }
        };

        let frame = build_probe(own_mac, address);
        for _ in 0..2 {
            match tx.send_to(&frame, None) {
                Some(Ok(())) => {}
                Some(Err(e)) => {
                    return Conflict::Unavailable(format!(
                        "could not send an ARP probe on {interface}: {e}"
                    ));
                }
                // pnet's "buffer too small for the frame". 42 bytes fits any
                // real MTU; the retry, or the read loop, still stands.
                None => {}
            }
        }

        while Instant::now() < deadline {
            match rx.next() {
                Ok(frame) => {
                    if let Some(mac) = claimant(frame, address, own_mac) {
                        return Conflict::InUse { mac };
                    }
                }
                // One read interval passing is not the window ending.
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(e) => {
                    return Conflict::Unavailable(format!(
                        "reading from {interface} failed before the window ended: {e}"
                    ));
                }
            }
        }
        Conflict::Free
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod imp {
    use super::{Conflict, Duration, Ipv4Addr};

    pub(super) fn probe(_interface: &str, _address: Ipv4Addr, _window: Duration) -> Conflict {
        Conflict::Unavailable(
            "address-conflict probing is implemented for Linux and macOS only".into(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OURS: [u8; 6] = [0x02, 0, 0, 0, 0, 0x01];
    const THEIRS: [u8; 6] = [0x74, 0x86, 0xe2, 0x19, 0xfa, 0xa4];
    const ADDR: Ipv4Addr = Ipv4Addr::new(192, 168, 10, 90);

    /// An ARP packet from `sender_mac`, as another host would put it on the
    /// wire.
    fn arp(opcode: u16, sender_mac: [u8; 6], sender_ip: Ipv4Addr, target_ip: Ipv4Addr) -> Vec<u8> {
        let mut f = build_probe(sender_mac, target_ip).to_vec();
        f[20..22].copy_from_slice(&opcode.to_be_bytes());
        f[28..32].copy_from_slice(&sender_ip.octets());
        f
    }

    #[test]
    fn the_probe_announces_nothing() {
        let f = build_probe(OURS, ADDR);
        assert_eq!(&f[0..6], &[0xff; 6], "broadcast");
        assert_eq!(&f[6..12], &OURS);
        assert_eq!(u16::from_be_bytes([f[12], f[13]]), ETHERTYPE_ARP);
        assert_eq!(u16::from_be_bytes([f[20], f[21]]), ARP_OPCODE_REQUEST);
        assert_eq!(&f[22..28], &OURS);
        // The whole point of a probe over a gratuitous request: if the
        // address is taken, no neighbor learns it at our MAC.
        assert_eq!(&f[28..32], &[0, 0, 0, 0], "sender IP must be 0.0.0.0");
        assert_eq!(&f[32..38], &[0; 6]);
        assert_eq!(&f[38..42], &ADDR.octets());
    }

    #[test]
    fn a_reply_from_another_host_is_a_claim() {
        let reply = arp(2, THEIRS, ADDR, Ipv4Addr::UNSPECIFIED);
        assert_eq!(claimant(&reply, ADDR, OURS), Some(THEIRS));
    }

    #[test]
    fn an_announcement_from_another_host_is_a_claim() {
        let announce = arp(1, THEIRS, ADDR, ADDR);
        assert_eq!(claimant(&announce, ADDR, OURS), Some(THEIRS));
    }

    #[test]
    fn another_hosts_probe_for_the_same_address_is_a_claim() {
        let their_probe = build_probe(THEIRS, ADDR);
        assert_eq!(claimant(&their_probe, ADDR, OURS), Some(THEIRS));
    }

    #[test]
    fn our_own_probe_looped_back_is_not_a_claim() {
        assert_eq!(claimant(&build_probe(OURS, ADDR), ADDR, OURS), None);
    }

    #[test]
    fn traffic_about_other_addresses_is_not_a_claim() {
        let other = Ipv4Addr::new(192, 168, 10, 149);
        assert_eq!(claimant(&arp(2, THEIRS, other, ADDR), ADDR, OURS), None);
        // Someone merely *asking* for our address, with their own address
        // as sender, is looking for the holder — not claiming to be it.
        assert_eq!(claimant(&arp(1, THEIRS, other, ADDR), ADDR, OURS), None);
    }

    #[test]
    fn non_arp_and_short_frames_are_not_claims() {
        let mut ipv4 = arp(2, THEIRS, ADDR, ADDR);
        ipv4[12..14].copy_from_slice(&0x0800u16.to_be_bytes());
        assert_eq!(claimant(&ipv4, ADDR, OURS), None);
        assert_eq!(claimant(&[0u8; 41], ADDR, OURS), None);
    }

    #[test]
    fn a_missing_interface_is_unavailable_not_free() {
        let got = probe("vlanctl-test-no-such-if0", ADDR, Duration::from_millis(10));
        assert!(matches!(got, Conflict::Unavailable(_)), "got {got:?}");
    }
}
