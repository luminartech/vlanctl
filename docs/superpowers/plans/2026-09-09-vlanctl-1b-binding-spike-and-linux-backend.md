# Phase 1b — Binding Spike, then the Linux Backend

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Find out whether application-level socket binding removes the need for host route/ARP provisioning, settle three factual questions the design is currently guessing at, then build the Linux `Platform` backend that the answers justify.

**Architecture:** Tasks 1-3 are a **spike**: throwaway code, and the deliverable is *answers*, not commits. A decision gate follows. Tasks 4-5 are unconditional — two silent failures inherited from 1a break teardown for any non-macOS backend, and the Linux `Platform` impl is needed either way. Task 6 exists only if the spike says host routes are still required.

**Tech Stack:** Rust edition 2024; `vlanctl` (`Cmd`/`CommandRunner`/`Platform`); `tokio` sockets; `socket2` for `bind_device`; `ip`/`tcpdump` on the bench Linux box.

**Spec:** `docs/superpowers/specs/2026-09-08-cross-platform-host-network-autoconfig-design.md` — read §2.0 (the unresolved datapath contradiction), §2.1 (shared subnet), §7 (Linux, including the corrected capability note), and §12 (the binding question). Note the spec carries three **CORRECTION** blocks dated 2026-09-09; those corrections are binding, the text they correct is not.

## Global Constraints

- **Tasks 1-3 produce no commits to any product repo.** Spike patches are local and reverted. If you find yourself wanting to keep spike code, stop and say so — keeping it is a new decision, not part of this plan.
- **`crates/simple_doip` and `crates/uds_protocol` are git submodules** (`git@github.com:luminartech/simple_doip.git`). A real change there is a separate PR plus a two-part pin bump in `dft` (pointer + version requirement, in ONE commit). For the spike, patch the submodule working tree and **do not commit inside it**.
- `~/dev/dft` is a **shared checkout on a stale branch** used by other sessions. Do not `git checkout`, reset, or rebase in it. Read canonical files with `git show origin/main:<path>`. For any dft-side commits, use a fresh worktree under `~/dev/`.
- vlanctl work happens in `~/dev/vlanctl`. Its `main` is behind PR #1 (`feat/lib-target-and-platform-seam`); **branch 1b work off that PR branch**, not `main`, or the `Platform` trait will not exist.
- CI in `dft` runs `RUSTFLAGS=-Dwarnings`. `vlanctl` has **no CI at all** — local gates are the only gates.
- Use `cargo nextest run` in `dft`; `cargo test` in `vlanctl` (no nextest config there).
- Run `cargo fmt` and `cargo clippy --all-targets -- -D warnings` before every commit; read exit codes, do not pipe to `tail`. In `vlanctl`, bare `cargo fmt -- --check` fails on pre-existing `src/config.rs` drift — do not "fix" it; stage only your own files.
- US English. No naked TODOs. No named attribution. No plan-phase labels ("1b", "Task 4") in shipped code or comments.
- Never `git add -A`.
- **Do not enable, disable, or reconfigure anything on the corporate network.** The bench NIC is a dedicated port; never bind or reconfigure the interface carrying the host's default route.

## Bench facts you will need

- Sensor (factory defaults): SOME/IP `192.168.11.151` VLAN 11; DoIP `192.168.10.150` VLAN 10; telnet `192.168.12.152` VLAN 12 (Iris). Host side: `192.168.11.87`, `192.168.10.90` (any address in the `/24` works — the sensor's gateway is unspecified), `192.168.12.x`.
- SD multicast `239.255.0.255:30490`. Datapath UDP 4370/4371.
- **The bench sensor `.102` is REPROVISIONED** (untagged, multicasting to `239.11.7.7`) and cannot answer the tagging question. Tasks 1-2 need a **factory-config** sensor.
- The Linux recipe that already works: `~/dev/iris_vlan_up.sh` (`ip link add link <p> name vlan11 type vlan id 11`, `ip link set vlan11 up`, `ip addr add 192.168.11.87/24 dev vlan11`, `ip route add 239.255.0.255/32 dev vlan11`).
- Tagged-traffic generator, if a second host is needed: `~/dev/iris_vlan_tagsrc.sh` (self-elevates; disables `txvlan`/`rxvlan` offload first, because hardware offload hides tags from local capture).

---

## PHASE 0 — THE SPIKE (Tasks 1-3, no commits)

### Task 1: Does socket binding remove the need for host routes?

**Files (all changes are LOCAL and REVERTED at the end):**
- Patch: `crates/simple_doip/src/client.rs:36` (`pub struct ClientOptions`) and `crates/simple_doip/src/connection.rs` (`establish_connection`, which does `TcpSocket::new_v4()?` then `.connect(gateway_address)` with **no local bind**)
- Patch: `crates/envision_drivers/src/telnet/transport.rs:289` (`TcpStream::connect((host, port))`)
- Reference, no change: `crates/iris_someip_client/src/client.rs` — already takes `interface: Ipv4Addr` and passes it to `simple-someip`, which binds it and sets `IP_MULTICAST_IF`. **SOME/IP already binds; do not re-do it.**

**Interfaces:**
- Produces: an answer, recorded in the report. No code survives.

**THE QUESTION.** vlanctl's field guide says application-level binding is "the most robust cross-platform way to force same-subnet traffic out a specific interface, sidestepping routing-table ambiguity entirely." If that holds, the entire route/ARP layer in the design (`/32` host routes, `wants_onlink_host_route`, `arp -s`, the macOS self-MAC trap, the never-specified Windows route mechanism) is unnecessary for traffic EnVision originates, and the Linux backend becomes two commands. If it does not hold, Task 6 exists.

- [ ] **Step 1: Stand up the VLANs with NO routes at all**

```bash
PARENT=eth0   # confirm with: ip -brief link show; must NOT be the default-route NIC
sudo ip link add link $PARENT name vlan10 type vlan id 10
sudo ip link add link $PARENT name vlan11 type vlan id 11
sudo ip link set vlan10 up && sudo ip link set vlan11 up
sudo ip addr add 192.168.10.90/24 dev vlan10
sudo ip addr add 192.168.11.87/24 dev vlan11
# DELIBERATELY NO `ip route add` — that is the thing under test.
ip route show dev vlan10; ip route show dev vlan11   # expect only the connected /24s
```

- [ ] **Step 2: Establish the unbound baseline — confirm the problem is real**

```bash
# DoIP: TCP 13400 to the VLAN 10 sensor.
timeout 5 bash -c 'echo > /dev/tcp/192.168.10.150/13400' && echo "DoIP connect OK" || echo "DoIP connect FAILED"
# Telnet: TCP 23 to the VLAN 12 sensor — no vlan12 interface exists yet, so expect failure.
timeout 5 bash -c 'echo > /dev/tcp/192.168.12.152/23' && echo "telnet OK" || echo "telnet FAILED"
```

Record both. If DoIP already connects with no `/32` route, that is itself a finding: it means `.10.0/24` is not shared on a factory sensor and §2.1's premise does not apply to this layout — check `crates/iris_config/src/default.rs` for how many interfaces the config model actually defines.

- [ ] **Step 3: Add the bind option to `simple_doip` (local patch only)**

In `crates/simple_doip/src/client.rs`, add a field to `ClientOptions`:

```rust
    /// Spike: local interface to pin the DoIP TCP socket to. `None` keeps
    /// today's behavior (kernel picks the egress interface by route lookup).
    pub bind_device: Option<String>,
```

In `crates/simple_doip/src/connection.rs`, in `establish_connection`, immediately after the socket is created and before `.connect(...)`:

```rust
    // Spike: pin egress to a named interface. `SO_BINDTODEVICE` needs
    // CAP_NET_RAW on Linux; the process already holds it for capture.
    if let Some(dev) = options.bind_device.as_deref() {
        use std::os::fd::AsFd;
        socket2::SockRef::from(&tcp_socket.as_fd())
            .bind_device(Some(dev.as_bytes()))?;
    }
```

Add `socket2 = "0.5"` to that crate's `[dependencies]` if absent. If `ClientOptions` is not threaded into `establish_connection`, hardcode the device name there for the spike — this code is thrown away.

- [ ] **Step 4: Test DoIP bound vs unbound**

Write `/tmp/spike_doip.rs` (or a scratch `examples/` file in the submodule) that opens a DoIP client twice — once with `bind_device: None`, once with `Some("vlan10")` — against `192.168.10.150:13400`, and prints whether each connected. Run it. Record both outcomes and any error text verbatim.

Expected if binding works: both connect (there is no ambiguity to resolve on a factory sensor), OR the bound one connects where the unbound one failed.

- [ ] **Step 5: Test the case binding is supposed to fix — a shared subnet**

This is the load-bearing test. Manufacture the ambiguity the design is worried about:

```bash
# A second interface claiming the SAME /24 as vlan10, so a route lookup
# for 192.168.10.150 is genuinely ambiguous.
sudo ip link add link $PARENT name vlan12 type vlan id 12
sudo ip link set vlan12 up
sudo ip addr add 192.168.10.91/24 dev vlan12
ip route show | grep 192.168.10   # two connected /24s now
```

Re-run Step 4. **The question: with two interfaces in one subnet and no `/32` routes, does `bind_device: Some("vlan10")` reach the sensor while unbound fails or picks wrong?** Confirm which interface traffic actually leaves on:

```bash
sudo timeout 10 tcpdump -i vlan10 -nn "host 192.168.10.150 and tcp port 13400" -c 5 &
sudo timeout 10 tcpdump -i vlan12 -nn "host 192.168.10.150 and tcp port 13400" -c 5 &
# then re-run the bound client
```

- [ ] **Step 6: Test telnet bound (VLAN 12, its own subnet)**

```bash
sudo ip addr add 192.168.12.1/24 dev vlan12   # vlan12 now serves its real purpose
timeout 5 bash -c 'echo > /dev/tcp/192.168.12.152/23' && echo "telnet OK, no route needed" || echo "telnet FAILED"
```

Then patch `crates/envision_drivers/src/telnet/transport.rs:289` to bind, using the same `socket2` shape, and confirm.

- [ ] **Step 7: Tear down and revert every patch**

```bash
sudo ip link del vlan10; sudo ip link del vlan11; sudo ip link del vlan12
cd ~/dev/dft/crates/simple_doip && git checkout -- . && git status --short   # must be empty
cd ~/dev/dft && git checkout -- crates/envision_drivers/src/telnet/transport.rs
git status --short   # only pre-existing modifications, none of yours
```

- [ ] **Step 8: Record the verdict**

In the report, answer exactly these, each with the command output that supports it:
1. Does DoIP reach a factory sensor on VLAN 10 with **no** `/32` route? (yes/no)
2. With two interfaces in one `/24`, does `bind_device` reach it where unbound does not? (yes/no — and which interface the packets left on)
3. Does telnet reach VLAN 12 with no route once the interface has an address? (yes/no)
4. **Verdict: does binding remove the need for host routes for EnVision-originated traffic?** (yes / no / partially — and if partially, exactly which cases still need a route)
5. What binding does NOT fix, confirmed rather than assumed: an out-of-subnet gatewayless destination (Halo's host `192.168.1.100/24` vs sensor `192.168.10.151`). Do not test this unless a Halo is on the bench; if untested, say so.

---

### Task 2: Settle the datapath-tagging contradiction

**Files:** none. Output is one capture and a verdict.

**Interfaces:**
- Produces: an answer that determines whether design §6.3's trunk-vNIC-with-`NativeVlanId 0` topology is solving a real problem.

**THE CONTRADICTION.** The design's §2 says the point cloud is always untagged. `crates/iris_config/src/lib.rs` says:

```rust
pub const ETHIF1_VLAN_ID_ABOUT: &str = "Virtual LAN (VLAN) ID for UDP data and SOME/IP";
```

with `ethif1_vlan_id_default()` returning **11**. So the sensor's own configuration model puts UDP data on the VLAN-11 interface. Both cannot be true. The only evidence for "untagged" is the **reprovisioned** bench sensor `.102` and a field-guide table describing the Halo layout.

**This needs a FACTORY-CONFIG sensor.** `.102` cannot answer it.

- [ ] **Step 1: Capture the raw wire on the parent NIC, no VLAN interfaces**

VLAN sub-interfaces must NOT exist during this capture — the kernel strips tags when they do, which is exactly how this question got confused in the first place.

```bash
ip -brief link show type vlan   # must be empty; delete any with `sudo ip link del <name>`
sudo ethtool -K $PARENT rxvlan off 2>/dev/null || echo "note: could not disable rx offload"
sudo timeout 60 tcpdump -i $PARENT -nn -e -s 128 -w /tmp/factory-wire.pcap
```

Set the sensor to **Active** so the point cloud actually flows.

- [ ] **Step 2: Classify every stream by tag**

```bash
cp /tmp/factory-wire.pcap ~/   # tcpdump here is apparmor-confined; read from $HOME
cd ~ && for f in "udp port 4370 or udp port 4371" "udp port 30490" "port 13400" "tcp port 23"; do
  echo "--- $f"
  echo -n "    tagged:   "; tcpdump -r factory-wire.pcap -nn "vlan and ($f)" 2>/dev/null | wc -l
  echo -n "    untagged: "; tcpdump -r factory-wire.pcap -nn "not vlan and ($f)" 2>/dev/null | wc -l
  echo -n "    vlan ids: "; tcpdump -r factory-wire.pcap -nn -e "vlan and ($f)" 2>/dev/null | grep -oE 'vlan [0-9]+' | sort -u | tr '\n' ' '; echo
done
```

- [ ] **Step 3: Record the verdict**

Answer, with the counts above:
1. Datapath (4370/4371): tagged or untagged? If tagged, which VLAN id?
2. SOME/IP-SD (30490): tagged? which id?
3. Telnet (23): tagged? which id? (The user stated VLAN 12 for Iris; vlanctl's `halo.toml` says VLAN 12 telnet for **Halo** while the user said Halo telnet is untagged — if a Halo is available, capture it too and say which is right.)
4. **Does design §6.3's `NativeVlanId 0` trunk exist for a real reason?** If the datapath is tagged VLAN 11, that native-VLAN-0 path is solving a non-problem and §6.3 should be simplified.
5. A caveat you must state: zero tagged frames is only evidence about the wire if offload was actually off. Report whether `ethtool -K rxvlan off` succeeded.

---

### Task 3: Can we run `ip` under EnVision's capabilities, or do we need rtnetlink?

**Files:** none. Output is a verdict that determines the Linux backend's mechanism.

**THE QUESTION.** Design §7 originally claimed VLAN links can be created "via netlink **or `ip`** with no sudo/pkexec at all" because EnVision holds `CAP_NET_ADMIN`. The `ip` half is wrong: **file capabilities do not survive `exec`** — a child's permitted set is `F(inheritable) ∩ P(inheritable)`, and `/sbin/ip` carries no file capabilities. Since vlanctl's entire model is shelling out through `Cmd`/`SystemRunner`, this decides whether 1b can reuse vlanctl's executor at all.

- [ ] **Step 1: Confirm the capability is present on a real build**

```bash
getcap /usr/bin/envision /usr/bin/envision-internal 2>/dev/null
# Expect cap_net_admin,cap_net_raw=eip on a .deb-installed build.
# A `cargo run` build has NONE — note which you are testing.
```

- [ ] **Step 2: Prove the exec problem empirically**

```bash
cat > /tmp/capexec.c <<'EOF'
#include <stdio.h>
#include <stdlib.h>
int main(void) { return system("ip link add link eth0 name captest type vlan id 99"); }
EOF
gcc -o /tmp/capexec /tmp/capexec.c
sudo setcap cap_net_admin=eip /tmp/capexec
/tmp/capexec; echo "exit: $?"
ip link show captest 2>/dev/null && echo "CREATED — exec kept the cap" || echo "NOT created — cap lost across exec"
```

- [ ] **Step 3: Test the ambient-capability workaround**

```bash
cat > /tmp/capambient.c <<'EOF'
#include <stdio.h>
#include <stdlib.h>
#include <sys/prctl.h>
#include <linux/capability.h>
int main(void) {
    if (prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_RAISE, CAP_NET_ADMIN, 0, 0) != 0)
        perror("PR_CAP_AMBIENT_RAISE");
    return system("ip link add link eth0 name captest2 type vlan id 98");
}
EOF
gcc -o /tmp/capambient /tmp/capambient.c
sudo setcap cap_net_admin=eip /tmp/capambient
/tmp/capambient; echo "exit: $?"
ip link show captest2 2>/dev/null && echo "CREATED — ambient works" || echo "NOT created"
sudo ip link del captest 2>/dev/null; sudo ip link del captest2 2>/dev/null; rm -f /tmp/capexec* /tmp/capambient*
```

Raising ambient caps requires the capability to be in the inheritable set too; if `PR_CAP_AMBIENT_RAISE` fails with `EPERM`, record that — it means `setcap cap_net_admin=eip` alone is insufficient and the install would need `+i`.

- [ ] **Step 4: Record the verdict**

1. Does `ip` inherit `CAP_NET_ADMIN` across `exec`? (expected: no)
2. Does `PR_CAP_AMBIENT_RAISE` make it work? (yes/no, with the errno if not)
3. **Verdict: mechanism for the Linux backend** — (a) shell out via vlanctl's `Cmd`/`SystemRunner` plus an ambient-cap raise before spawn, or (b) `rtnetlink` in-process, which bypasses vlanctl's executor and means the Linux `Platform` cannot express itself as `Vec<Cmd>` at all.
4. If (b), say plainly what that costs: `RecordingRunner`-based dry-run and `show` stop working for Linux, and the `Platform` trait's `Vec<Cmd>` return type is wrong for one of its backends — which is a trait-shape problem the design has not considered.

---

## DECISION GATE — stop here and report

Do not start Task 4 until Tasks 1-3 are reported and their consequences agreed. The answers change the remaining scope:

| Answer | Consequence |
|---|---|
| Binding removes the route layer | **Task 6 is cancelled.** Delete §2.1's routing machinery and `wants_onlink_host_route` from the design. Linux backend = `ip link add` + `ip addr add`. |
| Binding does not, or only partly | Task 6 proceeds, scoped to exactly the cases Task 1 Step 8 item 4 named. |
| Datapath is tagged VLAN 11 | §6.3's `NativeVlanId 0` trunk is unnecessary; simplify the Windows topology before Phase 3. |
| `ip` needs ambient caps and they work | Linux backend reuses vlanctl's `Cmd` executor. Proceed as planned. |
| Ambient caps do not work | **Stop.** `rtnetlink` breaks the `Vec<Cmd>` trait contract — that is a design change, not a task, and needs its own brainstorm. |

---

## PHASE 1 — UNCONDITIONAL WORK

### Task 4: Fix the two silent failures inherited from 1a

**Repository:** `~/dev/vlanctl`, branched off `feat/lib-target-and-platform-seam`.

**Files:**
- Modify: `src/commands.rs` (the `is_create` test)
- Modify: `src/plan.rs` (`interface_names`, and the `Platform` trait)
- Test: inline tests in both

**Interfaces:**
- Produces: `Platform::records_created_interface(&self, cmd: &Cmd) -> bool` and `Platform::managed_interface_names(&self, profile: &Profile) -> Vec<String>`; `interface_names` becomes macOS-specific and moves behind the trait.

**WHY THIS IS FIRST AND UNCONDITIONAL.** Both bugs are inert on macOS and fatal on any other backend, and both fail **silently** — no error, no failing test, just lost safety:

1. `src/commands.rs` records a created interface only when the emitted command's second argument is literally `create`. Under `ip link add link eth0 name eth0.11 type vlan id 11`, `args[1]` is `link`. So `created` stays empty: **rollback destroys nothing, the state file records nothing, and `down` becomes a no-op.**
2. `src/plan.rs`'s `interface_names()` builds `format!("vlan{id}")` independently of `Platform::iface_name`, and it feeds the pre-apply collision guard. With a backend naming interfaces `eth0.11`, the guard looks for `vlan11`, never matches, and **never fires** — so `apply` will happily reconfigure an interface it did not create.

- [ ] **Step 1: Write the failing tests**

Add to `src/plan.rs`'s test module. `Contrarian` already exists there and names interfaces `<device>.<id>`; give it the two new methods too.

```rust
    #[test]
    fn a_non_ifconfig_platform_still_records_what_it_created() {
        // The bug: recording keyed off `args[1] == "create"`, which is
        // ifconfig-shaped. A Linux-shaped `ip link add link eth0 name
        // eth0.11 ...` has `link` there, so nothing was recorded and
        // rollback/down silently became no-ops.
        let create = Cmd::new(
            "ip",
            &["link", "add", "link", "eth0", "name", "eth0.11", "type", "vlan", "id", "11"],
        );
        assert!(
            Contrarian.records_created_interface(&create),
            "a platform must be able to recognize its own creation command"
        );
        let not_create = Cmd::new("ip", &["addr", "add", "192.168.11.87/24", "dev", "eth0.11"]);
        assert!(!Contrarian.records_created_interface(&not_create));
    }

    #[test]
    fn macos_still_recognizes_ifconfig_create_and_nothing_else() {
        assert!(MacOs.records_created_interface(&Cmd::new("ifconfig", &["vlan11", "create"])));
        assert!(!MacOs.records_created_interface(&Cmd::new(
            "ifconfig",
            &["vlan11", "inet", "192.168.11.87", "netmask", "255.255.255.0"]
        )));
    }

    #[test]
    fn the_collision_guard_uses_the_platforms_own_interface_names() {
        // The bug: the guard hardcoded vlan<id>, so a platform naming
        // interfaces eth0.11 was never guarded at all.
        let p = Profile {
            name: "t".to_owned(),
            description: String::new(),
            device: Some("eth0".to_owned()),
            interfaces: vec![Interface {
                vlan: Some(11),
                address: "192.168.11.87/24".parse().unwrap(),
                mtu: None,
                routes: vec![],
            }],
        };
        assert_eq!(Contrarian.managed_interface_names(&p), vec!["eth0.11"]);
        assert_eq!(MacOs.managed_interface_names(&p), vec!["vlan11"]);
    }
```

Construct `Profile` exactly as the neighbouring tests in `src/config.rs` / `src/commands.rs` already do if the literal above does not match the real field set.

- [ ] **Step 2: Run to verify they fail**

```bash
cd ~/dev/vlanctl && cargo test --lib plan::tests
```

Expected: FAIL to compile — the two methods do not exist.

- [ ] **Step 3: Add the two trait methods and the macOS implementations**

In `src/plan.rs`, add to `trait Platform` (no default bodies — a default would let a backend silently inherit macOS shape, which is the whole failure this fixes):

```rust
    /// Whether `cmd` is this platform's *interface creation* command, and so
    /// the interface it names must be recorded for teardown and rollback.
    ///
    /// This exists because the recording test used to match `ifconfig`
    /// argument shape directly, which silently recorded nothing on any other
    /// platform — leaving rollback with nothing to undo and `down` a no-op.
    fn records_created_interface(&self, cmd: &Cmd) -> bool;

    /// The interface names this profile will manage on this platform, used by
    /// the pre-apply collision guard. Must agree with
    /// [`Platform::iface_name`] — a guard built on a different naming scheme
    /// never fires.
    fn managed_interface_names(&self, profile: &Profile) -> Vec<String>;
```

Implement for `MacOs`:

```rust
    fn records_created_interface(&self, cmd: &Cmd) -> bool {
        // `ifconfig <name> create`
        cmd.program == "ifconfig" && cmd.args.get(1).map(|a| a == "create").unwrap_or(false)
    }

    fn managed_interface_names(&self, profile: &Profile) -> Vec<String> {
        interface_names(profile)
    }
```

- [ ] **Step 4: Route the call sites through the trait**

In `src/commands.rs`, replace the inline `is_create` test with `platform.records_created_interface(cmd)`, and replace the `interface_names(profile)` call feeding the collision guard with `platform.managed_interface_names(profile)`. Update `Contrarian` in the test module with both methods (`records_created_interface` matching `program == "ip" && args[0] == "link" && args[1] == "add"`; `managed_interface_names` mapping to `format!("{device}.{id}")`).

- [ ] **Step 5: Verify macOS behaviour is unchanged**

```bash
cd ~/dev/vlanctl
for p in lum lum_legacy halo setup example; do ./target/debug/vlanctl apply $p --dry-run > /tmp/after-$p.txt 2>&1; done
git stash && cargo build --quiet && for p in lum lum_legacy halo setup example; do ./target/debug/vlanctl apply $p --dry-run > /tmp/before-$p.txt 2>&1; done && git stash pop && cargo build --quiet
for p in lum lum_legacy halo setup example; do diff -q /tmp/before-$p.txt /tmp/after-$p.txt && echo "  $p identical" || echo "  $p DIFFERS"; done
cargo test && cargo clippy --all-targets -- -D warnings
```

Expected: all five identical, tests pass, clippy exit 0.

- [ ] **Step 6: Commit**

```bash
git add src/plan.rs src/commands.rs
git commit -m "fix: put interface recording and the collision guard behind Platform

Both were ifconfig-shaped and failed silently on any other backend:
recording keyed off args[1] == \"create\", so a Linux \"ip link add\" was
never recorded and rollback/down became no-ops; and the collision guard
hardcoded vlan<id> independently of Platform::iface_name, so it never
fired for a backend using another naming scheme.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

### Task 5: The Linux `Platform` implementation

**Repository:** `~/dev/vlanctl`, same branch as Task 4.

**Files:**
- Modify: `src/plan.rs` (add `pub struct Linux;` and its `impl Platform`; update `host_platform`)
- Test: inline tests in `src/plan.rs`

**Interfaces:**
- Consumes: Task 4's `records_created_interface` / `managed_interface_names`; the existing 10-method `Platform` trait.
- Produces: `pub struct Linux;` implementing `Platform`; `host_platform()` returns `Ok(Box::new(Linux))` under `#[cfg(target_os = "linux")]`.

**MECHANISM DEPENDS ON TASK 3.** This task assumes Task 3's verdict was "shell out with an ambient-cap raise". If it was "rtnetlink", **stop** — a `Vec<Cmd>`-returning trait cannot express an in-process netlink backend, and that is a design change requiring its own brainstorm, not this task.

The commands come from `~/dev/iris_vlan_up.sh`, which is known to work on this hardware.

- [ ] **Step 1: Write the failing tests**

```rust
    #[test]
    fn linux_names_vlan_interfaces_after_the_parent() {
        let i = Interface {
            vlan: Some(11),
            address: "192.168.11.87/24".parse().unwrap(),
            mtu: None,
            routes: vec![],
        };
        assert_eq!(Linux.iface_name(&i, "eth0"), "eth0.11");
        let untagged = Interface { vlan: None, ..i.clone() };
        assert_eq!(Linux.iface_name(&untagged, "eth0"), "eth0");
    }

    #[test]
    fn linux_tagged_bringup_is_add_then_up_then_address() {
        let i = Interface {
            vlan: Some(11),
            address: "192.168.11.87/24".parse().unwrap(),
            mtu: Some(9000),
            routes: vec![],
        };
        let rendered: Vec<String> =
            Linux.bringup_commands(&i, "eth0").iter().map(|c| c.display()).collect();
        assert_eq!(
            rendered,
            vec![
                "ip link add link eth0 name eth0.11 type vlan id 11",
                "ip link set eth0.11 up",
                "ip addr add 192.168.11.87/24 dev eth0.11",
                "ip link set eth0.11 mtu 9000",
            ]
        );
    }

    #[test]
    fn linux_untagged_addresses_the_parent_and_creates_nothing() {
        let i = Interface {
            vlan: None,
            address: "192.168.1.100/24".parse().unwrap(),
            mtu: None,
            routes: vec![],
        };
        let rendered: Vec<String> =
            Linux.bringup_commands(&i, "eth0").iter().map(|c| c.display()).collect();
        assert!(!rendered.iter().any(|c| c.contains("link add")), "got {rendered:?}");
        assert_eq!(rendered, vec!["ip addr add 192.168.1.100/24 dev eth0"]);
    }

    #[test]
    fn linux_teardown_deletes_the_link() {
        assert_eq!(
            Linux.teardown_commands("eth0.11").iter().map(|c| c.display()).collect::<Vec<_>>(),
            vec!["ip link del eth0.11"]
        );
    }

    #[test]
    fn linux_wants_the_onlink_host_route_that_macos_refuses() {
        // The platform split that justifies the seam: on Linux
        // `ip route add X/32 dev Y` makes X genuinely on-link and the kernel
        // ARPs for it, which is how several hosts sharing one subnet across
        // different VLANs are reached at all. macOS answers the opposite
        // because an interface-scoped host route there installs a self-MAC
        // entry and black-holes the traffic.
        assert!(Linux.wants_onlink_host_route(true));
        assert!(Linux.wants_onlink_host_route(false));
        assert!(!MacOs.wants_onlink_host_route(true));
    }

    #[test]
    fn linux_reverts_parent_config_because_it_can_delete_one_address() {
        // `ip addr del <cidr> dev <parent>` removes exactly the address we
        // added, unlike macOS where an alias teardown needs `-alias` the
        // generic path cannot express.
        assert!(Linux.reverts_parent_config());
    }

    #[test]
    fn linux_recognizes_its_own_creation_command() {
        assert!(Linux.records_created_interface(&Cmd::new(
            "ip",
            &["link", "add", "link", "eth0", "name", "eth0.11", "type", "vlan", "id", "11"]
        )));
        assert!(!Linux.records_created_interface(&Cmd::new(
            "ip",
            &["addr", "add", "192.168.11.87/24", "dev", "eth0.11"]
        )));
    }

    #[test]
    fn linux_host_state_reads_use_ip_json_not_ifconfig() {
        let mut r = RecordingRunner::default();
        r.stdout.insert(
            "ip -json link show".to_string(),
            r#"[{"ifname":"lo"},{"ifname":"eth0"}]"#.to_string(),
        );
        assert_eq!(Linux.list_devices(&mut r).unwrap(), vec!["lo", "eth0"]);
    }
```

- [ ] **Step 2: Run to verify they fail**

```bash
cd ~/dev/vlanctl && cargo test --lib plan::tests::linux
```

Expected: FAIL to compile — `Linux` does not exist.

- [ ] **Step 3: Implement `Linux`**

```rust
/// Linux. Commands mirror the recipe already proven on this hardware by
/// `iris_vlan_up.sh`.
pub struct Linux;

impl Platform for Linux {
    fn name(&self) -> &'static str {
        "linux"
    }

    fn iface_name(&self, interface: &Interface, device: &str) -> String {
        match interface.vlan {
            // 8021q convention: `<parent>.<id>`.
            Some(id) => format!("{device}.{id}"),
            None => device.to_string(),
        }
    }

    fn bringup_commands(&self, interface: &Interface, device: &str) -> Vec<Cmd> {
        let name = self.iface_name(interface, device);
        let addr = interface.address.to_string();
        let mut cmds = Vec::new();
        if let Some(id) = interface.vlan {
            let id = id.to_string();
            cmds.push(Cmd::new(
                "ip",
                &["link", "add", "link", device, "name", &name, "type", "vlan", "id", &id],
            ));
            cmds.push(Cmd::new("ip", &["link", "set", &name, "up"]));
        }
        // `ip addr add` takes CIDR directly — no netmask conversion needed.
        cmds.push(Cmd::new("ip", &["addr", "add", &addr, "dev", &name]));
        if let Some(mtu) = interface.mtu {
            cmds.push(Cmd::new("ip", &["link", "set", &name, "mtu", &mtu.to_string()]));
        }
        cmds
    }

    fn teardown_commands(&self, iface: &str) -> Vec<Cmd> {
        // Deleting the link drops its addresses and routes with it.
        vec![Cmd::new("ip", &["link", "del", iface])]
    }

    fn wants_onlink_host_route(&self, _in_subnet: bool) -> bool {
        // Unlike macOS, `ip route add X/32 dev Y` makes X genuinely on-link
        // and the kernel ARPs for it. This is the only way to reach several
        // hosts that share one subnet across different VLANs.
        true
    }

    fn reverts_parent_config(&self) -> bool {
        // `ip addr del <cidr> dev <parent>` removes exactly what was added.
        true
    }

    fn records_created_interface(&self, cmd: &Cmd) -> bool {
        cmd.program == "ip"
            && cmd.args.first().map(|a| a == "link").unwrap_or(false)
            && cmd.args.get(1).map(|a| a == "add").unwrap_or(false)
    }

    fn managed_interface_names(&self, profile: &Profile) -> Vec<String> {
        let device = profile.device.as_deref().unwrap_or_default();
        profile
            .interfaces
            .iter()
            .filter_map(|i| i.vlan.map(|id| format!("{device}.{id}")))
            .collect()
    }

    fn list_devices(&self, runner: &mut dyn CommandRunner) -> Result<Vec<String>> {
        let out = runner.run(&Cmd::new("ip", &["-json", "link", "show"]))?;
        let entries: Vec<serde_json::Value> = serde_json::from_str(&out)?;
        Ok(entries
            .iter()
            .filter_map(|e| e.get("ifname")?.as_str().map(str::to_owned))
            .collect())
    }

    fn addresses_on(&self, runner: &mut dyn CommandRunner, device: &str) -> Result<Vec<IpNet>> {
        let out = runner.run(&Cmd::new("ip", &["-json", "addr", "show", "dev", device]))?;
        let entries: Vec<serde_json::Value> = serde_json::from_str(&out)?;
        let mut nets = Vec::new();
        for e in &entries {
            for a in e.get("addr_info").and_then(|v| v.as_array()).into_iter().flatten() {
                if a.get("family").and_then(|f| f.as_str()) != Some("inet") {
                    continue;
                }
                let (Some(local), Some(len)) = (
                    a.get("local").and_then(|l| l.as_str()),
                    a.get("prefixlen").and_then(|p| p.as_u64()),
                ) else {
                    continue;
                };
                if let Ok(net) = format!("{local}/{len}").parse::<IpNet>() {
                    nets.push(net);
                }
            }
        }
        Ok(nets)
    }

    fn is_wireless(&self, runner: &mut dyn CommandRunner, device: &str) -> Result<bool> {
        // `/sys/class/net/<dev>/wireless` exists only for wireless devices.
        let out = runner.run(&Cmd::new(
            "test",
            &["-d", &format!("/sys/class/net/{device}/wireless")],
        ));
        Ok(out.is_ok())
    }

    fn link_is_active(&self, runner: &mut dyn CommandRunner, device: &str) -> Result<bool> {
        let out = runner.run(&Cmd::new("cat", &[&format!("/sys/class/net/{device}/carrier")]))?;
        Ok(out.trim() == "1")
    }
}
```

Add `serde_json` to `[dependencies]` if absent (the crate already depends on it for the state file). Then update `host_platform()`:

```rust
    #[cfg(target_os = "linux")]
    {
        return Ok(Box::new(Linux));
    }
```

- [ ] **Step 4: Run the tests**

```bash
cd ~/dev/vlanctl && cargo test && cargo clippy --all-targets -- -D warnings
```

Expected: all pass, clippy exit 0. Note `host_platform()` now succeeds on Linux, so any test that asserted it errors here must be updated — that test was added by the previous phase and may be amended.

- [ ] **Step 5: Verify macOS output is still untouched**

```bash
cd ~/dev/vlanctl
for p in lum lum_legacy halo setup example; do
  diff <(git show HEAD~1:src/plan.rs > /dev/null; ./target/debug/vlanctl apply $p --dry-run 2>&1) /tmp/after-$p.txt \
    && echo "  $p unchanged" || echo "  $p DIFFERS — investigate"
done
```

`preview_platform()` still returns `MacOs`, so preview output is expected to be macOS-shaped even on Linux. **Repointing `preview_platform()` at `host_platform()` is now possible and should be its own commit** — its doc comment says exactly this. Do it in this task as a second commit, and update the `show_plan` doc comment that currently promises preview and apply agree.

- [ ] **Step 6: Commit**

```bash
git add src/plan.rs Cargo.toml
git commit -m "feat(plan): add the Linux Platform implementation

Commands mirror iris_vlan_up.sh, which is proven on this hardware.
wants_onlink_host_route is true here, unlike macOS: ip route add X/32 dev Y
makes the destination genuinely on-link and the kernel ARPs for it, which
is how hosts sharing one subnet across different VLANs are reached.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

## PHASE 2 — CONDITIONAL

### Task 6: Per-platform route emission — ONLY IF Task 1 says routes are still needed

**Do not start this task if Task 1's verdict was that binding removes the need for host routes.** In that case, the correct action is to delete §2.1's routing machinery and `wants_onlink_host_route` from the design instead, and this task is cancelled.

**Files:**
- Modify: `src/plan.rs` (add `Platform::route_commands`, move `append_route_commands`/`interface_route_command` behind it, implement for `MacOs` and `Linux`)
- Test: inline tests in `src/plan.rs`

**Interfaces:**
- Produces: `Platform::route_commands(&self, route: &Route, in_subnet: bool, iface: &str) -> Vec<Cmd>`; `wants_onlink_host_route` is **removed** — it becomes redundant once the platform emits the whole route, which is the correction the previous phase's seam doc names.

**WHY.** The current seam delegates the route *decision* but emits BSD `route add -host/-net … -interface` and `arp -s` from shared code for every platform, so a Linux backend silently emits BSD syntax. The pinning test `route_syntax_is_currently_shared_and_bsd_shaped_a_known_limitation` documents this and **must be deleted** by this task — it asserts the behaviour being fixed.

- [ ] **Step 1: Write the failing tests**

```rust
    #[test]
    fn linux_emits_ip_route_and_ip_neigh_not_bsd_route_and_arp() {
        let route = Route {
            destination: "192.168.11.151/32".to_string(),
            gateway: None,
            mac: Some("3a:42:f7:79:32:2e".to_string()),
        };
        let rendered: Vec<String> = Linux
            .route_commands(&route, true, "eth0.11")
            .iter()
            .map(|c| c.display())
            .collect();
        assert_eq!(
            rendered,
            vec![
                "ip route add 192.168.11.151/32 dev eth0.11",
                "ip neigh add 192.168.11.151 lladdr 3a:42:f7:79:32:2e dev eth0.11",
            ],
            "got {rendered:?}"
        );
        assert!(!rendered.iter().any(|c| c.contains("arp -s")));
    }

    #[test]
    fn macos_route_emission_is_unchanged_by_the_move() {
        let route = Route {
            destination: "192.168.11.151/32".to_string(),
            gateway: None,
            mac: Some("3a:42:f7:79:32:2e".to_string()),
        };
        // In-subnet on macOS: no route at all (self-MAC black-hole), arp only.
        let rendered: Vec<String> = MacOs
            .route_commands(&route, true, "vlan11")
            .iter()
            .map(|c| c.display())
            .collect();
        assert!(!rendered.iter().any(|c| c.starts_with("route ")), "got {rendered:?}");
        assert!(rendered.iter().any(|c| c == "arp -s 192.168.11.151 3a:42:f7:79:32:2e"));
    }

    #[test]
    fn macos_out_of_subnet_still_emits_host_route_then_arp_in_that_order() {
        let route = Route {
            destination: "192.168.10.151/32".to_string(),
            gateway: None,
            mac: Some("3a:42:f7:79:32:2e".to_string()),
        };
        let rendered: Vec<String> = MacOs
            .route_commands(&route, false, "en7")
            .iter()
            .map(|c| c.display())
            .collect();
        assert_eq!(
            rendered,
            vec![
                "route add -host 192.168.10.151 -interface en7",
                "arp -s 192.168.10.151 3a:42:f7:79:32:2e",
            ],
            "order matters: arp -s overwrites the self-MAC entry the route installs, \
             and an `arp -d` between them deletes the route"
        );
    }
```

- [ ] **Step 2: Run to verify they fail**

```bash
cd ~/dev/vlanctl && cargo test --lib route_commands
```

Expected: FAIL to compile — the method does not exist.

- [ ] **Step 3: Move route emission behind the trait**

Add `fn route_commands(&self, route: &Route, in_subnet: bool, iface: &str) -> Vec<Cmd>;` to `Platform` (no default body). Move the body of the existing `append_route_commands`/`interface_route_command` into `MacOs::route_commands` **verbatim**, preserving the in-subnet skip, the `-host`/`-net` split, the `default` case, and the `route add` → `arp -s` order with no `arp -d`. Implement `Linux::route_commands` with `ip route add <dest> dev <iface>` (plus `via <gw>` when a gateway is set) and `ip neigh add <ip> lladdr <mac> dev <iface>`. Then change `bringup_commands_for` to call `platform.route_commands(...)` instead of the shared function, delete `wants_onlink_host_route` from the trait and both impls, and **delete** `route_syntax_is_currently_shared_and_bsd_shaped_a_known_limitation`.

- [ ] **Step 4: Verify macOS output is byte-identical**

```bash
cd ~/dev/vlanctl
for p in lum lum_legacy halo setup example; do ./target/debug/vlanctl apply $p --dry-run > /tmp/t6-$p.txt 2>&1; done
for p in lum lum_legacy halo setup example; do diff -q /tmp/after-$p.txt /tmp/t6-$p.txt && echo "  $p identical" || echo "  $p DIFFERS — FAIL"; done
cargo test && cargo clippy --all-targets -- -D warnings
```

All five must be identical; this is a pure move for macOS.

- [ ] **Step 5: Update the seam doc and commit**

Remove gaps 1 and 2 from `docs/specs/2026-09-08-platform-seam.md`'s gap list (route syntax is now behind the seam; the host-state methods were wired in Task 5), and remove the reference to the deleted pinning test.

```bash
git add src/plan.rs docs/specs/2026-09-08-platform-seam.md
git commit -m "feat(plan): put route and ARP emission behind Platform

Replaces wants_onlink_host_route, which could express the decision but not
the syntax, so every platform emitted BSD route/arp commands. macOS output
is byte-identical for all five profiles; Linux now emits ip route/ip neigh.

Co-Authored-By: Claude Opus 5 <noreply@anthropic.com>"
```

---

## Self-Review

**1. Spec coverage.** Against the design as corrected on 2026-09-09:

| Spec element | Task |
|---|---|
| §12 binding question — the open item the design "built past" | 1 |
| §2.0 datapath-tagging contradiction | 2 |
| §7 corrected capability note (caps do not survive `exec`) | 3 |
| §6.3 `NativeVlanId 0` justification | 2 (verdict item 4) |
| §2.1 shared subnet — per-host `/32` on Linux | 1 (Step 5 tests it), 6 (implements it, if needed) |
| §11 phase 1b "Linux `Platform` impl" | 5 |
| 1a's two silent failures (rollback, collision guard) | 4 |
| 1a's route-syntax gap | 6 (conditional) |
| 1a's dead host-state methods | 5 (`Linux` implements them; wiring the macOS call sites is separate) |
| `preview_platform()` repoint | 5, Step 5 |

**Deliberately not here:** Windows (Phase 3), EnVision-side `WireProfile` and UI (Phase 1c/2), macOS integration (Phase 4). Wiring `apply`/`down`/`status` over to the host-state methods for **macOS** is not included — Task 5 gives `Linux` its implementations, but converting the existing macOS call sites is a behaviour-risk change that deserves its own task once a second backend proves the methods' shape.

**2. Placeholder scan.** No "TBD"/"TODO"/"implement later". Tasks 1-3 are a spike, so their steps are commands and verdict questions rather than code — that is the correct shape for work whose output is an answer, not the placeholder pattern. Three steps direct the implementer to match existing constructors rather than guess (`Profile`/`Interface` literals in Tasks 4-6), each naming the file to copy from. Task 5 Step 3 says "add `serde_json` if absent" — a conditional on a verifiable fact, not a blank.

**3. Type consistency.** `Platform` gains `records_created_interface(&Cmd) -> bool` and `managed_interface_names(&Profile) -> Vec<String>` in Task 4; both are implemented for `Linux` in Task 5 and unchanged in Task 6. `Linux` (Task 5) is consumed by Tasks 5-6. Task 6's `route_commands(&Route, bool, &str) -> Vec<Cmd>` is new and **removes** `wants_onlink_host_route`, which Task 5's tests assert — so Task 6 must update those two assertions. **Flagged here because a later task invalidating an earlier task's test is exactly the failure that bit the previous phase twice**; the executor should carry this into Task 6's dispatch.
