//! Making a profile survive a reboot.
//!
//! Rendering a [`Profile`] to netplan v2 YAML, plus the one host question
//! that rendering depends on: which backend netplan will hand the file to.
//!
//! Deliberately separate from [`crate::commands`]. Applying a profile and
//! persisting one are different decisions with different blast radii —
//! `netplan apply` reapplies *every* file on the host, so a caller has to be
//! able to offer them independently.
//!
//! [`render_netplan`] never sets a top-level `renderer:`. That key is a
//! single *global* value across every file merged from `/etc/netplan`, not
//! a per-file setting — the last file wins by sort order, and a `90-` file
//! sorts after every distro default. A pinned renderer would silently
//! re-render *every* netdef on the host onto whichever backend was picked;
//! on a systemd-networkd server that backend may not even be installed.
//! Instead each netdef this module writes is pinned individually, and only
//! when [`renderer_for`] says NetworkManager owns the device. See
//! `netplan_generate_does_not_move_another_netdefs_backend`.
//!
//! One thing netplan cannot express: a route pinned to a static ARP entry
//! (`Route.mac`) has no netplan v2 key. [`render_netplan`] still renders
//! the route, and says so in the file's header comment rather than
//! silently dropping the guarantee.
//!
//! # When NetworkManager ignores a generated profile
//!
//! If NetworkManager ignores a generated keyfile — no error, no log line,
//! and `nmcli connection load <file>` returning *success* while the
//! connection never appears — look for a **tombstone** before suspecting
//! anything here:
//!
//! ```text
//! /etc/NetworkManager/system-connections/<uuid>.nmmeta -> /dev/null
//! ```
//!
//! That is how NM records "this connection was deleted" when the keyfile
//! lives in a read-only directory it cannot remove from, such as
//! `/run/NetworkManager/system-connections` where netplan writes. Because
//! netplan derives **stable UUIDs from the netdef name**, every later
//! `netplan apply` regenerates a connection with the same UUID and NM
//! discards it again — permanently, across reboots.
//!
//! The practical consequence: `nmcli con delete` on a netplan-generated
//! connection is destructive in a way that survives everything. The file
//! comes back; the connection does not. Remove the netplan source and
//! re-apply instead, and if a tombstone already exists, delete the
//! `.nmmeta` symlink and `nmcli con reload`.
//!
//! The symptom is easy to misattribute. A write/scan race, content
//! rejection, a missing `uuid=`, `unmanaged-devices` and a keyfile path
//! override all look plausible and are all irrelevant; the cause is debris
//! in `/etc` left by an earlier `nmcli con delete`, so check there first.

use crate::config::{Profile, Route};
use anyhow::{Result, bail};
use std::path::{Path, PathBuf};

/// Where a profile's netplan file lives.
///
/// The `90-` prefix orders it after a distro's own files so it wins on a
/// conflicting key. Rejects a `profile_name` outside `[A-Za-z0-9._-]`:
/// [`Profile::validate`] never inspects `name` at all, so nothing upstream
/// stops an operator-authored profile putting a `/` in it and escaping
/// `/etc/netplan` for whatever reads this path next.
pub fn netplan_path(profile_name: &str) -> Result<PathBuf> {
    let safe = !profile_name.is_empty()
        && profile_name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if !safe {
        bail!(
            "profile name '{profile_name}' is not a safe netplan filename component \
             (only letters, digits, '.', '_', '-' are allowed)"
        );
    }
    Ok(PathBuf::from(format!(
        "/etc/netplan/90-vlanctl-{profile_name}.yaml"
    )))
}

/// Which backend netplan will hand *our* netdefs to.
///
/// This decides only whether NM-specific keys are emitted, never whether
/// the parent stub is emitted — the stub is unconditional, because
/// networkd rejects the file without it and NetworkManager's backend
/// crashes without it.
///
/// A wrong answer is designed to be survivable: [`Renderer::Networkd`]
/// produces exactly the rendering that already works on both backends, so
/// guessing low costs the NM improvements rather than breaking the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Renderer {
    Networkd,
    NetworkManager,
}

/// Which backend will render `device`'s netdefs.
///
/// Asked **per device**, not per host, because the host-level question has
/// no answer: a machine can run NetworkManager and systemd-networkd at the
/// same time — this one does — while each individual device is managed by
/// exactly one of them.
///
/// NetworkManager writes a state file per managed device under
/// `/run/NetworkManager/devices`, named by ifindex. Its presence is a
/// direct statement that NM owns this device, and it needs no root and no
/// subprocess. Measured against `networkctl`, which reported the same
/// device as `carrier (unmanaged)` with `Network File: n/a`.
///
/// The heuristic this replaces compared the *counts* of
/// `/run/NetworkManager/devices` and `/run/systemd/netif/links` and got it
/// backwards on a real host: `netif/links` was empty in the morning and
/// carried seven stale entries by the afternoon, so a host NM was plainly
/// driving answered "networkd" and the NM-specific keys were never emitted.
///
/// Defaults to [`Renderer::Networkd`] when the answer is not clear, which
/// is the survivable direction — see the type's note.
pub fn renderer_for(device: &str) -> Renderer {
    renderer_for_in(
        device,
        Path::new("/sys/class/net"),
        Path::new("/run/NetworkManager/devices"),
    )
}

/// [`renderer_for`] with its two directories injected, so the rule can be
/// tested without a host that happens to be configured the right way.
fn renderer_for_in(device: &str, sys_class_net: &Path, nm_devices: &Path) -> Renderer {
    let Ok(ifindex) = std::fs::read_to_string(sys_class_net.join(device).join("ifindex")) else {
        return Renderer::Networkd;
    };
    if nm_devices.join(ifindex.trim()).exists() {
        Renderer::NetworkManager
    } else {
        Renderer::Networkd
    }
}

/// Render `profile` as netplan v2 YAML for `device`.
///
/// Pure and total so the output can be pinned by a test rather than
/// inspected on a host. Routes are emitted `scope: link`, matching the
/// `ip route add <dest>/32 dev <iface>` form the Linux backend uses for a
/// gatewayless host route.
///
/// Always emits an `ethernets:` netdef for `device`, even when the profile
/// has no untagged interface — see the comment at the stub for why both
/// backends require it and what it costs on NetworkManager.
pub fn render_netplan(profile: &Profile, device: &str, renderer: Renderer) -> String {
    let mut out = format!(
        "# Written by vlanctl for profile '{}'. Safe to delete.\n",
        first_line(&profile.name)
    );

    let mac_routes: Vec<&Route> = profile
        .interfaces
        .iter()
        .flat_map(|i| i.routes.iter())
        .filter(|r| r.mac.is_some())
        .collect();
    if !mac_routes.is_empty() {
        out.push_str(
            "# NOTE: netplan has no static-ARP-entry key, so the route(s) below\n\
             # will exist after reboot but will not resolve until Apply runs again:\n",
        );
        for route in &mac_routes {
            out.push_str(&format!(
                "#   {} (mac {})\n",
                first_line(&route.destination),
                first_line(route.mac.as_deref().expect("filtered to Some"))
            ));
        }
    }

    // No top-level `renderer:` — see the module doc comment for why a
    // pinned one is a host-wide hijack, not a local default.
    out.push_str("network:\n  version: 2\n");

    let untagged: Vec<_> = profile
        .interfaces
        .iter()
        .filter(|i| i.vlan.is_none())
        .collect();
    out.push_str("  ethernets:\n");
    out.push_str(&format!("    {device}:\n"));
    push_nm_renderer(&mut out, renderer, 6);
    if untagged.is_empty() {
        // No untagged interface in this profile: this netdef exists only so
        // the `vlans:` entries below have a defined `link:` to point at.
        //
        // Unconditional, and BOTH backends need it. networkd rejects the
        // file outright (`interface 'eth0' is not defined`). NetworkManager
        // is worse: its backend *crashes* — `write_nm_conf_access_point:
        // assertion failed: (def->vlan_link != NULL)` — as soon as more
        // than one VLAN links to an undefined parent, so `netplan apply`
        // writes nothing at all. Exactly one VLAN happens to survive, which
        // is how a single-interface fixture passes while every real
        // two-VLAN profile fails. This is observed netplan behavior.
        //
        // `link-local: []` is honored by networkd and **ignored by the NM
        // backend**, which gives the parent `ipv4.method=link-local` and a
        // persistent `169.254.0.0/16` regardless — as do `activation-mode:
        // manual` (ignored) and `activation-mode: off` (rejected by NM
        // outright). No netplan key prevents it, so on an NM host that
        // address is a known and accepted cost of making a profile
        // permanent, not something to engineer around here.
        out.push_str("      dhcp4: false\n      link-local: []\n");
        push_nm_parent_passthrough(&mut out, renderer);
    } else {
        out.push_str("      addresses:\n");
        for i in &untagged {
            out.push_str(&format!("        - {}\n", i.address));
        }
        if let Some(mtu) = untagged.iter().find_map(|i| i.mtu) {
            out.push_str(&format!("      mtu: {mtu}\n"));
        }
        push_routes(&mut out, untagged.iter().flat_map(|i| i.routes.iter()), 6);
    }

    let tagged: Vec<_> = profile
        .interfaces
        .iter()
        .filter(|i| i.vlan.is_some())
        .collect();
    if !tagged.is_empty() {
        out.push_str("  vlans:\n");
        for i in &tagged {
            let id = i.vlan.expect("filtered to Some");
            out.push_str(&format!("    {device}.{id}:\n"));
            push_nm_renderer(&mut out, renderer, 6);
            out.push_str(&format!("      id: {id}\n"));
            out.push_str(&format!("      link: {device}\n"));
            out.push_str("      addresses:\n");
            out.push_str(&format!("        - {}\n", i.address));
            if let Some(mtu) = i.mtu {
                out.push_str(&format!("      mtu: {mtu}\n"));
            }
            push_routes(&mut out, i.routes.iter(), 6);
        }
    }

    out
}

/// Take just the first line of `s`, so a newline embedded in
/// operator-authored data (a profile name, a route's MAC string, a route's
/// destination string — none of which `render_netplan` validates itself)
/// cannot inject a line at column 0 of the rendered YAML's `#` header
/// comments. [`netplan_path`]'s character-class check keeps this out of
/// the *filename*, but
/// `render_netplan` is a free function nothing stops from being called
/// directly on an unvalidated [`Profile`] — this is the same rule, applied
/// where the data is actually embedded.
fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or("")
}

/// Pin one netdef to the NetworkManager backend.
///
/// Per-netdef rather than the top-level `renderer:`, which is a single
/// *global* setting across every merged `/etc/netplan/*.yaml` — writing it
/// would re-render the whole host's networking onto our choice, the
/// hijack `netplan_generate_does_not_move_another_netdefs_backend` guards.
///
/// Emitted on **every** netdef this file owns, never just the parent: a
/// per-netdef renderer on the parent alone sends it to NetworkManager while
/// its own VLANs go to networkd, and a parent/child pair split across two
/// daemons is worse than the problem being solved. Measured.
fn push_nm_renderer(out: &mut String, renderer: Renderer, indent: usize) {
    if renderer == Renderer::NetworkManager {
        out.push_str(&format!(
            "{:indent$}renderer: NetworkManager\n",
            "",
            indent = indent
        ));
    }
}

/// Keys that decide the parent's fate on a NetworkManager host.
///
/// Both were checked against the generated keyfile, and each prevents a
/// failure that occurs in practice:
///
/// - `autoconnect-priority` outranks NM's stock `Wired connection 1`, which
///   ships at `-999` with `ipv4.method=auto`. Without this it wins the
///   parent after `netplan apply` restarts NetworkManager, then loops
///   forever on DHCP against a sensor link that has no DHCP server — and
///   the VLANs never activate because their parent never comes up.
/// - `ipv4.method=disabled` is the only thing that actually stops the
///   parent taking an address. `link-local: []` renders, generates cleanly
///   and is ignored by this backend; so is `activation-mode: manual`, and
///   `activation-mode: off` is rejected outright.
///
/// netplan rejects `networkmanager:` keys unless that netdef's renderer is
/// NetworkManager, which is why this is paired with [`push_nm_renderer`].
fn push_nm_parent_passthrough(out: &mut String, renderer: Renderer) {
    if renderer != Renderer::NetworkManager {
        return;
    }
    out.push_str(
        "      networkmanager:\n\
         \x20       passthrough:\n\
         \x20         connection.autoconnect-priority: \"999\"\n\
         \x20         ipv4.method: \"disabled\"\n",
    );
}

/// Emit a `routes:` block at `indent` spaces, or nothing when empty.
///
/// `Route.mac` — a static ARP entry — has no netplan v2 key and is
/// deliberately not rendered here; [`render_netplan`] surfaces that gap in
/// the file's header comment instead of silently dropping the guarantee.
fn push_routes<'a>(out: &mut String, routes: impl Iterator<Item = &'a Route>, indent: usize) {
    let pad = " ".repeat(indent);
    let mut any = false;
    for route in routes {
        if !any {
            out.push_str(&format!("{pad}routes:\n"));
            any = true;
        }
        out.push_str(&format!("{pad}  - to: {}\n", route.destination));
        match &route.gateway {
            Some(gw) => out.push_str(&format!("{pad}    via: {gw}\n")),
            None => out.push_str(&format!("{pad}    scope: link\n")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Interface, Profile, Route};

    fn tagged_profile() -> Profile {
        Profile {
            name: "lab".to_string(),
            description: None,
            device: Some("eth0".to_string()),
            interfaces: vec![Interface {
                vlan: Some(10),
                address: "192.168.10.90/24".parse().expect("valid CIDR"),
                mtu: None,
                routes: vec![Route {
                    destination: "192.168.10.150/32".to_string(),
                    gateway: None,
                    mac: None,
                }],
            }],
        }
    }

    fn untagged_profile() -> Profile {
        Profile {
            name: "lab-two".to_string(),
            description: None,
            device: Some("eth0".to_string()),
            interfaces: vec![Interface {
                vlan: None,
                address: "192.168.1.100/24".parse().expect("valid CIDR"),
                mtu: None,
                routes: Vec::new(),
            }],
        }
    }

    /// The rendered YAML is the whole contract with netplan, so pin it
    /// exactly rather than asserting on fragments. This exact string is
    /// also checked against real `netplan generate` below — see
    /// `netplan_generate_accepts_the_tagged_rendering`.
    #[test]
    fn path_is_namespaced_and_ordered_after_the_distro_defaults() {
        let p = netplan_path("lab").expect("a safe name");
        assert_eq!(p, PathBuf::from("/etc/netplan/90-vlanctl-lab.yaml"));
    }

    /// `Profile::validate` never inspects `name`, so this is the only thing
    /// standing between an operator-authored profile and an escape from
    /// `/etc/netplan`.
    #[test]
    fn path_rejects_a_name_outside_the_safe_character_set() {
        for bad in ["../evil", "a/b", "", "with space", "semi;colon"] {
            assert!(
                netplan_path(bad).is_err(),
                "{bad:?} should not produce a path"
            );
        }
    }

    #[test]
    fn renders_a_tagged_interface_with_its_host_route() {
        let yaml = render_netplan(&tagged_profile(), "eth0", Renderer::Networkd);
        let expected = "\
# Written by vlanctl for profile 'lab'. Safe to delete.
network:
  version: 2
  ethernets:
    eth0:
      dhcp4: false
      link-local: []
  vlans:
    eth0.10:
      id: 10
      link: eth0
      addresses:
        - 192.168.10.90/24
      routes:
        - to: 192.168.10.150/32
          scope: link
";
        assert_eq!(yaml, expected);
    }

    /// An untagged entry configures the parent device, which netplan
    /// addresses under `ethernets`, not `vlans`.
    #[test]
    fn renders_an_untagged_entry_under_ethernets() {
        let yaml = render_netplan(&untagged_profile(), "eth0", Renderer::Networkd);
        assert!(yaml.contains("ethernets:"), "got {yaml}");
        assert!(yaml.contains("eth0:"), "got {yaml}");
        assert!(!yaml.contains("vlans:"), "got {yaml}");
        // Real addresses are present, so the "no netdef to link against"
        // stub must not also appear.
        assert!(!yaml.contains("dhcp4: false"), "got {yaml}");
    }

    /// A `vlans:` netdef's `link:` must name a *defined* netdev or netplan
    /// rejects the whole file — see `netplan_generate_accepts_the_tagged_rendering`.
    #[test]
    fn stubs_the_parent_device_when_no_untagged_interface_exists() {
        let yaml = render_netplan(&tagged_profile(), "eth0", Renderer::Networkd);
        assert!(
            yaml.contains("  ethernets:\n    eth0:\n      dhcp4: false\n"),
            "got {yaml}"
        );
    }

    /// The stub netdef exists only to give a `vlans:` entry something to
    /// link against, so the parent must not acquire an address of its own.
    /// `dhcp4: false` alone does not achieve that: under the
    /// NetworkManager renderer a netdef with no addresses becomes
    /// `ipv4.method=link-local`, and NM autoconfigures a persistent
    /// `169.254.0.0/16` on a NIC that had no IPv4 at all, and Revert does
    /// not clean it up.
    #[test]
    fn stubbed_parent_does_not_take_a_link_local_address() {
        let yaml = render_netplan(&tagged_profile(), "eth0", Renderer::Networkd);
        assert!(
            yaml.contains("  ethernets:\n    eth0:\n      dhcp4: false\n      link-local: []\n"),
            "got {yaml}"
        );
    }

    #[test]
    fn renders_mtu_when_the_interface_sets_it() {
        let mut profile = tagged_profile();
        profile.interfaces[0].mtu = Some(1500);
        let yaml = render_netplan(&profile, "eth0", Renderer::Networkd);
        assert!(yaml.contains("      mtu: 1500\n"), "got {yaml}");
    }

    #[test]
    fn renders_mtu_on_an_untagged_interface() {
        let mut profile = untagged_profile();
        profile.interfaces[0].mtu = Some(9000);
        let yaml = render_netplan(&profile, "eth0", Renderer::Networkd);
        assert!(yaml.contains("      mtu: 9000\n"), "got {yaml}");
    }

    /// netplan has no static-ARP-entry key, so a `mac`-pinned route's ARP
    /// entry cannot be persisted. The route itself is still rendered — it
    /// works again once Apply runs — but the gap must be visible, not
    /// silent, or the operator believes something untrue about what
    /// "made permanent" covers.
    #[test]
    fn warns_in_the_header_when_a_route_carries_a_static_arp_entry() {
        let mut profile = untagged_profile();
        profile.interfaces[0].routes.push(Route {
            destination: "192.168.10.151/32".to_string(),
            gateway: None,
            mac: Some("00:00:5e:00:53:01".to_string()),
        });
        let yaml = render_netplan(&profile, "eth0", Renderer::Networkd);
        assert!(yaml.starts_with("# Written by vlanctl"), "got {yaml}");
        assert!(yaml.contains("NOTE"), "got {yaml}");
        assert!(yaml.contains("192.168.10.151/32"), "got {yaml}");
        assert!(yaml.contains("00:00:5e:00:53:01"), "got {yaml}");
        // The route itself is still rendered — only the ARP entry is not.
        assert!(yaml.contains("- to: 192.168.10.151/32"), "got {yaml}");
    }

    /// Resolves a working `netplan` invocation, preferring
    /// `/usr/sbin/netplan` — the package installs it there, and `sbin`
    /// directories are routinely absent from a non-root `PATH` — and
    /// falling back to plain `netplan` for whatever *is* on `PATH`.
    fn netplan_binary() -> Option<&'static str> {
        if std::path::Path::new("/usr/sbin/netplan").is_file() {
            return Some("/usr/sbin/netplan");
        }
        std::process::Command::new("netplan")
            .arg("info")
            .output()
            .ok()
            .map(|_| "netplan")
    }

    /// Resolves [`netplan_binary`], or explains why a test that needs it
    /// cannot run. Only reached when the `netplan-tests` feature is on —
    /// every caller is `#[cfg_attr(not(feature = "netplan-tests"), ignore)]`
    /// — so a plain `cargo nextest run` (what CI runs, on containers that
    /// do not ship `netplan.io`) never gets here at all. Once opted in,
    /// though, this is a hard failure on Linux, not a skip: an
    /// `eprintln!`-and-return skip is captured by `nextest` and shown
    /// nowhere, so enabling the feature on a Linux image that happens to
    /// lack `netplan` would silently lose every one of this module's
    /// real-netplan tests and report all green — exactly the gap this
    /// feature exists to let someone deliberately open. Off Linux (where
    /// `netplan` will never be present) it stays a skip, so enabling the feature there — which
    /// nothing in this crate does — still doesn't gate on a tool it has no
    /// reason to require.
    fn require_netplan(label: &str) -> Option<&'static str> {
        match netplan_binary() {
            Some(bin) => Some(bin),
            None if cfg!(target_os = "linux") => panic!(
                "`netplan` not found (checked /usr/sbin/netplan and PATH) — \
                 refusing to silently skip {label}; install netplan.io or \
                 fix PATH rather than let this test go green with no coverage"
            ),
            None => {
                eprintln!("skipping {label}: not on Linux and `netplan` not found");
                None
            }
        }
    }

    /// Runs `netplan generate --root-dir <root>` (already populated with
    /// `.yaml` fixtures under `<root>/etc/netplan`) and returns the raw
    /// output. Deliberately not `.output()` through a shell pipe: piping
    /// through e.g. `| head` reports *that* command's exit status, not
    /// netplan's, and hides the very failure these tests exist to catch.
    fn netplan_generate(bin: &str, root: &Path) -> std::process::Output {
        std::process::Command::new(bin)
            .arg("generate")
            .arg("--root-dir")
            .arg(root)
            .output()
            .expect("run `netplan generate`")
    }

    fn write_fixture(dir: &Path, name: &str, contents: &str) {
        let file = dir.join(name);
        std::fs::write(&file, contents).expect("write fixture");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600))
                .expect("chmod 600 the staged file");
        }
    }

    /// `serde`/YAML well-formedness (checked in the tests above via
    /// `assert_eq!` against a string that is itself valid YAML) proves the
    /// file *parses* — it does not prove netplan *accepts* it. This is
    /// what caught the missing `ethernets:` netdef: `render_netplan` used
    /// to emit `vlans: { eth0.10: { link: eth0 } }` with no `eth0` netdef
    /// anywhere in the file, which is well-formed YAML that netplan
    /// rejects with "interface 'eth0' is not defined".
    fn assert_netplan_generate_accepts(yaml: &str, label: &str) {
        let Some(bin) = require_netplan(label) else {
            return;
        };

        let root = std::env::temp_dir().join(format!(
            "vlanctl-netplan-generate-test-{label}-{}",
            std::process::id()
        ));
        let netplan_dir = root.join("etc/netplan");
        std::fs::create_dir_all(&netplan_dir).expect("create temp netplan root");
        write_fixture(&netplan_dir, "90-vlanctl-test.yaml", yaml);

        let out = netplan_generate(bin, &root);
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        let _ = std::fs::remove_dir_all(&root);

        assert!(
            out.status.success(),
            "netplan generate exited {:?} for {label}: {stderr}",
            out.status.code()
        );
        assert!(
            !stderr.contains("Error in network definition"),
            "netplan rejected the {label} rendering: {stderr}"
        );
        // A backend *crash* prints neither that string nor a zero exit:
        // `write_nm_conf_access_point: assertion failed: (def->vlan_link !=
        // NULL)` arrives as `**` plus an ERROR line. Checking only for the
        // rejection string is how an omitted parent stub passed this suite
        // while `netplan apply` wrote nothing on a real NM host.
        assert!(
            !stderr.contains("assertion failed") && !stderr.contains("ERROR:"),
            "netplan's backend crashed on the {label} rendering: {stderr}"
        );
    }

    #[cfg_attr(not(feature = "netplan-tests"), ignore)]
    #[test]
    fn netplan_generate_accepts_the_tagged_rendering() {
        assert_netplan_generate_accepts(
            &render_netplan(&tagged_profile(), "eth0", Renderer::Networkd),
            "tagged",
        );
    }

    /// A directory under the system temp dir, removed on drop. Unique per
    /// test and process, so parallel tests never share one.
    struct ScratchDir(std::path::PathBuf);

    impl ScratchDir {
        fn new(label: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("vlanctl-renderer-{label}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("mkdir scratch dir");
            ScratchDir(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A device NetworkManager has a state file for is one NM manages.
    #[test]
    fn a_device_network_manager_has_claimed_renders_as_network_manager() {
        let root = ScratchDir::new("claimed");
        let sys = root.path().join("sys");
        let nm = root.path().join("nm");
        std::fs::create_dir_all(sys.join("eth0")).expect("mkdir");
        std::fs::write(sys.join("eth0").join("ifindex"), "324\n").expect("write");
        std::fs::create_dir_all(&nm).expect("mkdir");
        std::fs::write(nm.join("324"), "").expect("write");
        assert_eq!(renderer_for_in("eth0", &sys, &nm), Renderer::NetworkManager);
    }

    /// Both daemons can be running at once — this was measured on a real
    /// host — so the absence of an NM state file for *this* device is what
    /// decides, not any host-wide count.
    #[test]
    fn a_device_network_manager_has_not_claimed_renders_as_networkd() {
        let root = ScratchDir::new("unclaimed");
        let sys = root.path().join("sys");
        let nm = root.path().join("nm");
        std::fs::create_dir_all(sys.join("eth0")).expect("mkdir");
        std::fs::write(sys.join("eth0").join("ifindex"), "324\n").expect("write");
        // NM manages other devices, just not this one.
        std::fs::create_dir_all(&nm).expect("mkdir");
        std::fs::write(nm.join("2"), "").expect("write");
        assert_eq!(renderer_for_in("eth0", &sys, &nm), Renderer::Networkd);
    }

    /// An unreadable ifindex is "not clear", which lands on the rendering
    /// that works under both backends rather than guessing.
    #[test]
    fn an_unknown_device_falls_back_to_networkd() {
        let root = ScratchDir::new("unknown");
        assert_eq!(
            renderer_for_in("nope0", &root.path().join("sys"), &root.path().join("nm")),
            Renderer::Networkd
        );
    }

    /// The NM rendering pins every netdef this file owns, never just the
    /// parent: a split sends the parent to NetworkManager and its own VLANs
    /// to networkd, which is worse than the problem.
    #[test]
    fn the_network_manager_rendering_pins_every_netdef_it_owns() {
        let yaml = render_netplan(&two_vlan_profile(), "eth0", Renderer::NetworkManager);
        assert_eq!(
            yaml.matches("renderer: NetworkManager").count(),
            3,
            "parent + both VLANs, got {yaml}"
        );
        assert!(
            !yaml.contains("\n  renderer:"),
            "never the top-level renderer — that re-renders the whole host: {yaml}"
        );
    }

    /// `ipv4.method=disabled` is the only key that stops the parent taking
    /// an address, and the priority is what keeps NM's stock `Wired
    /// connection 1` from capturing it after a netplan apply.
    #[test]
    fn the_network_manager_parent_disables_ipv4_and_outranks_autoconnect() {
        let yaml = render_netplan(&two_vlan_profile(), "eth0", Renderer::NetworkManager);
        assert!(
            yaml.contains("connection.autoconnect-priority: \"999\""),
            "got {yaml}"
        );
        assert!(yaml.contains("ipv4.method: \"disabled\""), "got {yaml}");
    }

    /// The networkd rendering is untouched by any of this — it is the
    /// fallback a wrong detection lands on, so it has to stay exactly what
    /// already works on both backends.
    #[test]
    fn the_networkd_rendering_carries_no_network_manager_keys() {
        let yaml = render_netplan(&two_vlan_profile(), "eth0", Renderer::Networkd);
        assert!(!yaml.contains("renderer:"), "got {yaml}");
        assert!(!yaml.contains("networkmanager:"), "got {yaml}");
        assert!(
            yaml.contains("      dhcp4: false\n      link-local: []\n"),
            "got {yaml}"
        );
    }

    /// Two tagged interfaces on one parent — a common real-world shape,
    /// and the one that exposes the undefined-parent crash below.
    fn two_vlan_profile() -> Profile {
        let mut p = tagged_profile();
        p.interfaces.push(Interface {
            vlan: Some(11),
            address: "192.168.11.87/24".parse().expect("valid CIDR"),
            mtu: None,
            routes: vec![Route {
                destination: "192.168.11.102/32".to_string(),
                gateway: None,
                mac: None,
            }],
        });
        p
    }

    /// Guards a regression that unit fixtures alone do not catch.
    ///
    /// One tagged interface survives an undefined parent netdef; two do
    /// not. Omitting the `ethernets:` stub therefore passed every
    /// one-interface fixture and then crashed netplan's NM backend on the
    /// first real profile, so Make permanent wrote nothing at all.
    /// The decisive check for the NetworkManager path: not that generation
    /// succeeds, but what it produced.
    ///
    /// All three netdefs must land as NM keyfiles (a `.network` file among
    /// them means the backends split), the parent must carry the priority
    /// that outranks `Wired connection 1`, and its ipv4 method must be
    /// `disabled` rather than `link-local` or `auto`.
    #[cfg_attr(not(feature = "netplan-tests"), ignore)]
    #[test]
    fn network_manager_keyfiles_disable_the_parent_and_outrank_autoconnect() {
        let Some(bin) = require_netplan("nm-passthrough") else {
            return;
        };
        let root =
            std::env::temp_dir().join(format!("vlanctl-netplan-nm-pass-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let netplan_dir = root.join("etc/netplan");
        std::fs::create_dir_all(&netplan_dir).expect("create temp netplan root");
        write_fixture(
            &netplan_dir,
            "90-vlanctl-test.yaml",
            &render_netplan(&two_vlan_profile(), "eth0", Renderer::NetworkManager),
        );

        let out = netplan_generate(bin, &root);
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        let nm_dir = root.join("run/NetworkManager/system-connections");
        let names: Vec<String> = std::fs::read_dir(&nm_dir)
            .map(|e| {
                e.flatten()
                    .map(|f| f.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        let parent =
            std::fs::read_to_string(nm_dir.join("netplan-eth0.nmconnection")).unwrap_or_default();
        let networkd: Vec<String> = std::fs::read_dir(root.join("run/systemd/network"))
            .map(|e| {
                e.flatten()
                    .map(|f| f.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        let _ = std::fs::remove_dir_all(&root);

        assert!(
            !stderr.contains("Error in network definition")
                && !stderr.contains("assertion failed")
                && !stderr.contains("ERROR:"),
            "netplan did not accept the NM rendering: {stderr}"
        );
        assert_eq!(names.len(), 3, "parent + both VLANs, got {names:?}");
        assert!(
            networkd.is_empty(),
            "a networkd unit means the backends split: {networkd:?}"
        );
        assert!(
            parent.contains("autoconnect-priority=999"),
            "parent must outrank NM's stock wired profile: {parent}"
        );
        assert!(
            parent.contains("method=disabled"),
            "parent must take no address at all: {parent}"
        );
    }

    #[cfg_attr(not(feature = "netplan-tests"), ignore)]
    #[test]
    fn netplan_generate_accepts_a_two_vlan_rendering() {
        assert_netplan_generate_accepts(
            &render_netplan(&two_vlan_profile(), "eth0", Renderer::Networkd),
            "two-vlan",
        );
    }

    #[cfg_attr(not(feature = "netplan-tests"), ignore)]
    #[test]
    fn netplan_generate_accepts_the_untagged_rendering() {
        assert_netplan_generate_accepts(
            &render_netplan(&untagged_profile(), "eth0", Renderer::Networkd),
            "untagged",
        );
    }

    /// The Critical regression this guards: `render_netplan` used to pin
    /// `renderer: NetworkManager` at the top level, which is a single
    /// *global* setting across every merged `/etc/netplan/*.yaml`, last
    /// file wins by sort order — and `90-vlanctl-*` sorts after every
    /// distro default. Writing one permanence file therefore
    /// silently re-rendered the *entire host's* network configuration,
    /// including interfaces this module never touched, onto whichever
    /// backend our file said. On a systemd-networkd server or cloud image
    /// that backend may not even be installed.
    ///
    /// Fixture: an installer-style file with its own explicit `renderer:
    /// networkd` and an unrelated `ens3` NIC, alongside our rendering of a
    /// tagged profile. Proves our file does not move `ens3` off networkd by
    /// checking it still gets a systemd-networkd unit after `generate`,
    /// rather than ending up under NetworkManager instead.
    #[cfg_attr(not(feature = "netplan-tests"), ignore)]
    #[test]
    fn netplan_generate_does_not_move_another_netdefs_backend() {
        let Some(bin) = require_netplan("hijack regression") else {
            return;
        };

        let root = std::env::temp_dir().join(format!(
            "vlanctl-netplan-hijack-test-{}",
            std::process::id()
        ));
        let netplan_dir = root.join("etc/netplan");
        std::fs::create_dir_all(&netplan_dir).expect("create temp netplan root");
        write_fixture(
            &netplan_dir,
            "00-installer-config.yaml",
            "network:\n  version: 2\n  renderer: networkd\n  ethernets:\n    ens3:\n      dhcp4: true\n",
        );
        write_fixture(
            &netplan_dir,
            "90-vlanctl-lab.yaml",
            &render_netplan(&tagged_profile(), "eth0", Renderer::Networkd),
        );

        let out = netplan_generate(bin, &root);
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();

        let ens3_kept_its_networkd_unit = std::fs::read_dir(root.join("run/systemd/network"))
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .any(|e| e.file_name().to_string_lossy().contains("ens3"))
            })
            .unwrap_or(false);

        let _ = std::fs::remove_dir_all(&root);

        assert!(
            out.status.success(),
            "netplan generate exited {:?}: {stderr}",
            out.status.code()
        );
        assert!(
            !stderr.contains("Error in network definition"),
            "netplan rejected the fixture: {stderr}"
        );
        assert!(
            ens3_kept_its_networkd_unit,
            "installing our permanence file moved 'ens3' off systemd-networkd — \
             our rendering is leaking a renderer choice beyond its own netdefs"
        );
    }
}
