# Testing

## Automated (run: `cargo test --no-default-features`; plain `cargo test` also builds the UI)

| Suite | What it proves |
|---|---|
| `src/*` unit tests (142) | identity/fingerprints, pinned-TLS verifiers (untrusted client, wrong server, revocation, pairing isolation), bounded protocol decoding, layout geometry (gaps, corners, spans, snapping, mixed-DPI), control state machine and held-input cleanup, key table, Cmd/Ctrl translator (incl. a property test), capture filter, clipboard guard/limits/PNG bombs, safe file names, capability-based inbox, streaming transfer (hash, cancel, overrun, symlinks, 96 MiB), config persistence/recovery/migration, address ranking and concurrent dialing, session heartbeat/handshake/pairing channel, layout editor |
| `tests/engine.rs` (12) | **two full engines over loopback TLS**: reciprocal pairing, rejection, closed window, pointer crossing and return with clean release, emergency stop, pause, peer shutdown, revocation, permission withdrawal, locked receiver, one-way permissions, automatic reconnect |
| `tests/engine_data.rs` (15) | clipboard (permissions, pause, private items, no echo, images in chunks), file offers (acceptance, decline/cancel, auto-accept without overwrite, folders refused), and a **hand-driven hostile peer** (unsafe names, control without permission, oversize/garbage frames, bulk attach without the secret) |
| `packaging/windows/installer` (4) | installer logic runs natively: folder validation, atomic install without temp files, uninstall removes only its own files, self-delete of the running uninstaller (`cargo test --manifest-path packaging/windows/installer/Cargo.toml`) |
| `tests/design.rs` | WCAG contrast of the real tokens in both themes; status never colour-only |
| `discovery` (ignored by default) | two mDNS instances find each other: `cargo test discovery -- --ignored` |

These use the `dev-backend` fake input/clipboard (a dev-only dependency on the crate itself). **They do not prove that real OS
capture or injection works.** Skipped/ignored tests are reported as such.

Large-file streaming memory: `/usr/bin/time -l cargo test transfer::tests::large_file` → peak resident set ≈ 10 MB for a 96 MiB
transfer (macOS; see `docs/BENCHMARKS.md`).

## Manual two-machine acceptance checklist

Run on two real computers on one LAN (repeat per OS pair). ☐ = tick when verified.

**Install & first run**
- ☐ App opens; five onboarding screens appear in order; no network call is made (see packet capture below).
- ☐ Permissions screen shows real state; granting then **Check again** turns it green.

**Pairing**
- ☐ Computer B: *Pair a device* → window opens, countdown visible. Computer A lists B as **Not paired**; also works by typing B's address.
- ☐ Both screens show the other's *full* fingerprint, identical to what the other shows as its own.
- ☐ Approve on A only → neither trusts. Approve on B → both list each other. Decline on either → nothing trusted.
- ☐ Pairing window closed on B → A reports "not accepting pairing requests".

**Input**
- ☐ Arrange screens to match the physical desk; Apply; B shows "layout updated".
- ☐ Push the pointer through the shared edge: it appears on B at the same height; keyboard types on B.
- ☐ Return across the edge; modifiers/keys held during the crossing do not stick on either machine.
- ☐ Emergency stop while controlling B: control returns, sharing paused, B has no stuck key/button.
- ☐ Unplug B's network mid-control: A recovers within ~8 s with a clear notice.
- ☐ Lock B's screen: B reports locked; A cannot enter; after unlock sharing stays paused.
- ☐ Revoke A on B while A is controlling B: input stops immediately; A cannot reconnect.
- ☐ Mixed DPI / multi-monitor: pointer lands where expected; excluded monitor is never entered; corners don't switch.
- ☐ Scroll direction and Cmd/Ctrl translation behave as documented for Mac↔PC.

**Clipboard** — ☐ off by default; ☐ text and an image sync once each way and don't bounce; ☐ password-manager copy is skipped;
☐ *Pause clipboard* stops it immediately.

**Files** — ☐ offer prompts on the receiver; ☐ accept → file appears in a new folder, SHA verified, original untouched; ☐ cancel mid-transfer removes partial files;
☐ same name twice never overwrites; ☐ dropping on the window works.

**Cross-platform matrix** — mark each cell *works / limited / n/a* and note OS versions:

| Source ↓ / Target → | macOS | Windows | Linux X11 | Linux Wayland |
|---|---|---|---|---|
| macOS | | | | |
| Windows | | | | |
| Linux X11 | | | | |
| Linux Wayland | (clipboard/files only) | | | |

## Offline-LAN verification (no internet)

1. Put both computers on a switch/router with **no uplink** (or disable the WAN). 2. Pair, share input, clipboard and a file as above.
3. Everything must work. 4. Check for hidden internet reliance: no update prompt, no spinner waiting on a host, no DNS lookups (below).

## Packet capture: confirm no unintended external connections

```bash
# macOS/Linux — capture everything the machine sends while Synkflow runs, then list distinct destinations:
sudo tcpdump -i any -w synkflow.pcap &            # start; use Synkflow for a few minutes; then stop tcpdump
tcpdump -nr synkflow.pcap 'not (net 10.0.0.0/8 or net 172.16.0.0/12 or net 192.168.0.0/16 or net 169.254.0.0/16 or net 224.0.0.0/4 or net ff00::/8 or net fe80::/10)'
# Expected from Synkflow: empty. (Other applications may appear: filter by process with `lsof -iTCP -sTCP:ESTABLISHED -p $(pgrep synkflow)`.)
lsof -nP -a -p $(pgrep synkflow) -i                # sockets owned by Synkflow only: the listener, mDNS (UDP 5353), LAN peers
```
On Windows use Wireshark with the display filter `!(ip.dst==10.0.0.0/8 || ip.dst==172.16.0.0/12 || ip.dst==192.168.0.0/16 || ip.dst==224.0.0.0/4)` or `Get-NetTCPConnection -OwningProcess (Get-Process synkflow).Id`.
Expected traffic: mDNS multicast (UDP 5353) and TCP to the paired computers' Synkflow port. Nothing else.

## OS-permission and session-lock tests

- macOS: revoke Accessibility while sharing → within seconds sharing pauses with the "Keyboard and mouse sharing stopped" notice and no key stays held; grant again → **Check again** resumes.
- Lock/unlock each OS while controlling and while controlled → pause, release, no automatic resume (default).
- Windows: focus an elevated window while controlled → input is ignored by that window (documented limit); the lock screen/UAC likewise.

## Fuzzing

Fuzz targets were **not** added in this environment (a property test covers the translator; the decoder and name sanitiser are
covered by table tests). `cargo fuzz` targets for `proto::decode` and `transfer::safe_name` are recommended next.
