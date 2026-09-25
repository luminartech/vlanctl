//! Making a profile survive a reboot.
//!
//! Rendering a [`Profile`] to netplan v2 YAML, plus the one host question
//! that rendering depends on: which backend netplan will hand the file to.
//!
//! Deliberately separate from [`crate::commands`]. Applying a profile and
//! persisting one are different decisions with different blast radii —
//! `netplan apply` reapplies *every* file on the host, so a caller has to be
//! able to offer them independently.

use crate::config::{Profile, Route};
use anyhow::{Result, bail};
use std::path::PathBuf;

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

/// Which backend netplan will hand this file to.
///
/// Only one thing turns on it, and it is not cosmetic: the `ethernets:`
/// stub that gives a `vlans:` entry a defined `link:` to point at.
/// **networkd requires it** — without it `netplan generate` fails with
/// `interface 'eth0' is not defined` and rejects the whole file, which then
/// breaks every subsequent `netplan generate` including the one at boot.
/// **NetworkManager does not need it**, and emitting it there is actively
/// harmful: NM turns an address-less netdef into `ipv4.method=link-local`
/// and autoconfigures a persistent `169.254.0.0/16` on a parent that had no
/// IPv4 at all — which survives reboot and which a teardown does not undo.
///
/// `link-local: []` does **not** prevent that. It renders, `netplan
/// generate` accepts it, and the NM backend ignores it. Measured directly
/// against the generated keyfile, along with `activation-mode: manual`
/// (also ignored) and `activation-mode: off` (rejected outright by NM).
///
/// The practical lesson for anyone extending this: asserting that `netplan
/// generate` *succeeds* proves nothing here. The broken rendering generated
/// cleanly. Read what it produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Renderer {
    Networkd,
    NetworkManager,
}

/// Which backend this host's netplan will use.
///
/// NetworkManager records a state file per device it manages under
/// `/run/NetworkManager/devices`; systemd-networkd does the same under
/// `/run/systemd/netif/links`. Reading a directory beats spawning
/// `systemctl`, and unlike `netplan get renderer` it needs no root (that
/// command reads `/lib/netplan/00-network-manager-all.yaml`, which is
/// root-only).
///
/// Defaults to [`Renderer::Networkd`] when neither is conclusive. That is
/// the conservative answer: the stub it implies is *required* by networkd
/// and merely unwanted under NM, so guessing wrong costs a link-local
/// address rather than a file netplan refuses to parse.
pub fn host_renderer() -> Renderer {
    let managed = |dir: &str| {
        std::fs::read_dir(dir)
            .map(|entries| entries.flatten().next().is_some())
            .unwrap_or(false)
    };
    if managed("/run/NetworkManager/devices") && !managed("/run/systemd/netif/links") {
        Renderer::NetworkManager
    } else {
        Renderer::Networkd
    }
}

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
    // The parent netdef is emitted when it carries real addresses, or when
    // the renderer needs it defined for a `vlans:` entry to link against.
    // Under NetworkManager an address-less stub is not needed and not
    // harmless — see [`Renderer`].
    let stub_only = untagged.is_empty();
    if !stub_only || renderer == Renderer::Networkd {
        out.push_str("  ethernets:\n");
        out.push_str(&format!("    {device}:\n"));
    }
    if untagged.is_empty() {
        // No untagged interface: under networkd this netdef exists only
        // so a `vlans:` entry below has a defined `link:` to point at.
        // Under NetworkManager it is not emitted at all, because NM would
        // give it a link-local address (see [`Renderer`]) and no netplan
        // key prevents that.
        if renderer == Renderer::Networkd {
            out.push_str("      dhcp4: false\n      link-local: []\n");
        }
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
/// the *filename* for anything that goes through [`PermanenceWriter`], but
/// `render_netplan` is a free function nothing stops from being called
/// directly on an unvalidated [`Profile`] — this is the same rule, applied
/// where the data is actually embedded.
fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or("")
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

    fn iris_profile() -> Profile {
        let p: Profile = toml::from_str(
            "name=\"derived-iris\"\ndevice=\"eth0\"\n\
             [[interface]]\nvlan=10\naddress=\"192.168.10.90/24\"\n\
             [[interface.route]]\ndestination=\"192.168.10.150/32\"\n",
        )
        .expect("fixture parses");
        p.validate().expect("fixture is valid");
        p
    }

    fn untagged_profile() -> Profile {
        let p: Profile = toml::from_str(
            "name=\"halo\"\ndevice=\"eth0\"\n\
             [[interface]]\naddress=\"192.168.1.100/24\"\n",
        )
        .expect("fixture parses");
        p.validate().expect("fixture is valid");
        p
    }

    /// The whole rendering, pinned. This exact string is also fed to a real
    /// `netplan generate` below.
    #[test]
    fn renders_a_tagged_interface_with_its_host_route() {
        let yaml = render_netplan(&iris_profile(), "eth0", Renderer::Networkd);
        let expected = "\
# Written by vlanctl for profile 'derived-iris'. Safe to delete.
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

    /// NM needs no parent netdef, and an address-less one is exactly what it
    /// turns into `ipv4.method=link-local`.
    #[test]
    fn network_manager_gets_no_parent_stub() {
        let yaml = render_netplan(&iris_profile(), "eth0", Renderer::NetworkManager);
        assert!(!yaml.contains("ethernets:"), "got {yaml}");
        assert!(yaml.contains("vlans:"), "the VLAN is still configured");
        assert!(yaml.contains("eth0.10:"), "got {yaml}");
    }

    /// networkd is the opposite: without the stub the file is rejected
    /// outright.
    #[test]
    fn networkd_still_gets_the_parent_stub() {
        let yaml = render_netplan(&iris_profile(), "eth0", Renderer::Networkd);
        assert!(
            yaml.contains("  ethernets:\n    eth0:\n      dhcp4: false\n      link-local: []\n"),
            "got {yaml}"
        );
    }

    /// A real untagged interface configures the parent for its own sake, so
    /// it is emitted under either renderer.
    #[test]
    fn an_untagged_interface_is_emitted_under_either_renderer() {
        for renderer in [Renderer::Networkd, Renderer::NetworkManager] {
            let yaml = render_netplan(&untagged_profile(), "eth0", renderer);
            assert!(yaml.contains("ethernets:"), "{renderer:?}: {yaml}");
            assert!(!yaml.contains("dhcp4: false"), "{renderer:?}: {yaml}");
        }
    }

    #[test]
    fn path_is_namespaced_and_ordered_after_the_distro_defaults() {
        let p = netplan_path("derived-iris").expect("a safe name");
        assert_eq!(
            p,
            PathBuf::from("/etc/netplan/90-vlanctl-derived-iris.yaml")
        );
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

    /// Resolves a working `netplan`, preferring `/usr/sbin/netplan` — the
    /// package installs it there and `sbin` is routinely absent from a
    /// non-root `PATH`.
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

    /// Only reached with `netplan-tests` on, where a missing binary is a
    /// hard failure: an `eprintln!`-and-return skip is swallowed by the test
    /// harness and would silently lose every real-netplan test on an image
    /// that happens to lack the tool — exactly the gap the feature exists to
    /// let someone deliberately open.
    fn require_netplan(label: &str) -> &'static str {
        match netplan_binary() {
            Some(bin) => bin,
            None if cfg!(target_os = "linux") => {
                panic!("netplan-tests is enabled but no `netplan` binary is present ({label})")
            }
            None => unreachable!("netplan-tests is Linux-only"),
        }
    }

    fn generate_into(root: &std::path::Path, yaml: &str, bin: &str) -> std::process::Output {
        let dir = root.join("etc/netplan");
        std::fs::create_dir_all(&dir).expect("temp netplan root");
        let file = dir.join("90-vlanctl-test.yaml");
        std::fs::write(&file, yaml).expect("write fixture");
        // netplan refuses world-readable configuration.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600))
                .expect("chmod fixture");
        }
        std::process::Command::new(bin)
            .arg("generate")
            .arg("--root-dir")
            .arg(root)
            .output()
            .expect("run `netplan generate`")
    }

    /// The networkd rendering has to survive a real parse: an invalid file
    /// left in `/etc/netplan` breaks every later `netplan generate`,
    /// including the one at boot.
    #[cfg_attr(not(feature = "netplan-tests"), ignore)]
    #[test]
    fn netplan_generate_accepts_the_networkd_rendering() {
        let bin = require_netplan("networkd");
        let root = std::env::temp_dir().join(format!("vlanctl-np-networkd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let yaml = render_netplan(&iris_profile(), "eth0", Renderer::Networkd);
        let out = generate_into(&root, &yaml, bin);
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        let _ = std::fs::remove_dir_all(&root);
        assert!(
            !stderr.contains("Error in network definition"),
            "netplan rejected the networkd rendering: {stderr}"
        );
    }

    /// The check the `link-local: []` attempt was missing.
    ///
    /// That rendering was accepted by `netplan generate`, so a test asking
    /// only whether generation *succeeds* passed — and on hardware the
    /// parent still came up with a `169.254.0.0/16`, because the NM backend
    /// ignores the key. This reads what generation actually produced.
    #[cfg_attr(not(feature = "netplan-tests"), ignore)]
    #[test]
    fn network_manager_generates_no_parent_connection() {
        let bin = require_netplan("nm-keyfile");
        let root = std::env::temp_dir().join(format!("vlanctl-np-nm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        // The renderer is pinned in the fixture so the generated backend is
        // NM's regardless of what this host runs.
        let yaml = render_netplan(&iris_profile(), "eth0", Renderer::NetworkManager).replace(
            "network:\n  version: 2\n",
            "network:\n  version: 2\n  renderer: NetworkManager\n",
        );
        let out = generate_into(&root, &yaml, bin);
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        let keyfiles: Vec<String> =
            std::fs::read_dir(root.join("run/NetworkManager/system-connections"))
                .map(|entries| {
                    entries
                        .flatten()
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .collect()
                })
                .unwrap_or_default();
        let _ = std::fs::remove_dir_all(&root);

        assert!(
            !stderr.contains("Error in network definition"),
            "netplan rejected the NM rendering: {stderr}"
        );
        assert!(
            keyfiles.iter().any(|f| f.contains("eth0.10")),
            "the VLAN must still be configured, got {keyfiles:?}"
        );
        assert!(
            !keyfiles.iter().any(|f| f == "netplan-eth0.nmconnection"),
            "a parent connection is what takes the link-local address, got {keyfiles:?}"
        );
    }
}
