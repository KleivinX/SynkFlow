# Architecture and protocol

One Cargo package; a library (`synkflow`) holds everything testable, the binary is a thin launcher, and the Slint UI sits
behind the `gui` feature so the engine and all tests build without it.

```
 Slint UI (src/ui, ui/*.slint)  ──Command──►  Engine actor (src/engine) ──►  Platform backends (src/platform/*)
        ▲                                         │   owns ALL mutable state        macOS · Windows · Linux/X11
        └────────────── Snapshot (watch) ◄────────┤
                                                  ├─ Control state machine   (src/control.rs, pure)
                                                  ├─ Layout geometry         (src/geometry.rs, pure) + editor (layout_editor.rs)
                                                  ├─ Sessions / pairing      (src/session.rs, src/tls.rs, src/proto.rs)
                                                  ├─ Clipboard rules         (src/clipboard.rs) + engine/clip.rs worker
                                                  └─ File transfer           (src/transfer.rs) + engine/files.rs
```

* **One actor.** `engine::Core` owns peers, sessions, pairing, transfers and the control machine. Network tasks, OS capture
  threads, discovery and the UI send it messages; it publishes immutable `Snapshot`s. No locks on the hot path.
* **Pure core.** `control.rs` takes events and returns effects; it has no clock, I/O or threads, so every transition and every
  safe-failure path is a unit test.
* **Bounded everywhere.** Capture queue 4096 (pointer motion coalesces without losing deltas; keys/buttons never drop),
  session queue 1024 (overflow of anything but pointer position ends the session), clipboard chunk queue 8 (awaited),
  injector queue 1024.

## Transport

TCP + **TLS 1.3 only** (`rustls`, `ring`), mutual authentication, certificates are self-signed Ed25519; identity =
**SHA-256 of the certificate DER**. One listener; the client's ALPN picks the mode *before* any application data:

| ALPN | Verifier | Allowed |
|---|---|---|
| `synkflow/1` | **strict**: server side requires the peer fingerprint ∈ live trust set; client side requires exactly the expected fingerprint | everything below |
| `synkflow-pair/1` | key-possession only, grants nothing | `PairHello`, `PairDecision`, ping, bye |

Session resumption/tickets are disabled so revocation always bites at the next handshake.

After TLS: an 8-byte preamble `"SYNK" + major(u16) + minor(u16)`; a major mismatch is reported to the user, never parsed.
Frames are `u32` big-endian length + `postcard` message; the length is checked against the channel limit **before** allocation.
Enums are **append-only** within a major version.

| Limit | Value |
|---|---|
| Control/input/pairing frame | 64 KiB |
| Clipboard text / image payload | 1 MiB / 16 MiB (sent as 32 KiB chunks on the control channel's low-priority queue) |
| Image pixels (decoded) | 64 Mpx |
| File chunk (bulk channel) | 256 KiB |
| Displays per device / files per offer / name | 16 / 256 / 64 B (device) 200 B (file) |

### Channels

* **Control channel** (`Hello`/`HelloAck`, then `Input`, `Enter`/`EnterAck`/`Leave`, `Layout`, `Grants`, `State`, clipboard,
  file offers). Heartbeat every 2 s, dead after 8 s of silence. Two writer queues: high priority (input/control) always wins
  over low priority (clipboard chunks), so a large image never delays a keystroke.
* **Bulk channel** per accepted file transfer: a **separate TLS connection**, opened by whichever side opened the control
  connection (so the path that already works through a firewall is reused). Its first frame `Attach{session_id, token,
  transfer_id}` must match the control session (id **and** the secret `bulk_token` issued in `HelloAck`, constant-time
  compared), come from the same pinned identity, and refer to an accepted transfer. Large transfers cannot block input.
* **Reconnect**: bounded exponential backoff (0.5 s → 30 s) with ±25 % jitter; the lower-fingerprint device dials first; on a
  simultaneous dial the connection initiated by the lower fingerprint wins.

## Control model

`Disconnected → Local ⇄ Pending → RemoteActive`, `BeingControlled` on the receiving side, `Suspended(Paused|Panic|Locked|
CaptureUnavailable|Shutdown)`. The **source** tracks a virtual pointer across the shared layout, so the destination is dumb:
it receives `PointerAbs{display, x, y}` in that display's own logical points and injects.

* Edge crossing: pointer inside an activation zone *and* pushing outward, a neighbouring enabled monitor of another device
  covers that coordinate, corners are dead-zones by default, optional dwell (with hysteresis) and modifier requirement,
  per-edge overrides. Hot corners never cause a switch by default.
* Motion on a remote device is stepped ≤ 1 point at a time through *touching* tiles: it cannot teleport across a gap or slip
  between tiles that only meet at a corner; against a wall it slides.
* While `Pending`, input is queued (256) and flushed in order after the ack; an unanswered `Enter` times out in 1 s and a late
  accept is answered with `Leave`.
* **Held input.** The destination tracks what it pressed and releases everything (buttons, then keys, modifiers last) on
  leave, session loss, pause, panic, lock and permission withdrawal. The capture filter lets the local OS see the *release* of
  any key it saw go down, so nothing sticks locally either.
* One controller per receiver; a second `Enter` gets `Busy`.

## Layout

Each device keeps its own monitors' relative arrangement; the user places the **device** (a `Placement` offset). Units are
logical points; pixels appear only at the injection boundary (`DisplayInfo::to_pixels`). Windows reports pixels ÷ primary
scale so mixed-DPI monitors stay exactly adjacent. A layout carries a revision; the highest wins when peers sync, and only
devices with an input relationship may push one. The editor works on a **draft**; Apply sends it, Revert discards it.

## Shortcut translation (optional, per device)

On by default (Settings → Input; each device can override it), and only ever between a Mac and a non-Mac. The shortcut key (⌘ on macOS, Ctrl elsewhere)
is **held back** until a chord is known:

* chord with a mapped key → emit the equivalent (⌘C → Ctrl+C; ⌘Q → Alt+F4; ⌘Tab → Alt+Tab; ⌘←/→ → Home/End; ⌘↑/↓ →
  Ctrl+Home/End); the translated modifier stays down for the whole ⌘ hold so repeated chords and app switchers work;
* chord with an unmapped key (F-keys, ⌘Space) → the original modifier is delivered unchanged;
* a lone tap passes through; pointer actions with the key held (⌘+click) are translated too.

PC→Mac: Ctrl+letter/number → ⌘; Ctrl+←/→/Backspace/Delete → ⌥ variants; Ctrl+Home/End → ⌘↑/↓; Ctrl+Tab and F-keys untouched.
A property test proves every key-down the translator emits is balanced by a key-up. Terminals (where Ctrl+C means interrupt)
are why this is opt-in.

## Persistence

`config.toml` (settings + trusted devices + layout; atomic write, owner-only permissions; an unreadable file is **kept** and
defaults used). The identity key is in the OS credential store, else a private file. No database. Transfer history lives only
in memory; full paths are never stored.
