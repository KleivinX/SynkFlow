# Threat model

## Assets

1. **Control of a computer's keyboard and pointer** (the most sensitive: it is remote control).
2. **Clipboard contents** (often secrets).
3. **Files received** and the folder they land in.
4. **The device identity key** (impersonation).
5. **The list of trusted devices.**

## Actors and assumptions

| Actor | Assumed capability |
|---|---|
| Other LAN device | Can send arbitrary packets, spoof discovery, connect to open ports, replay or tamper. **Hostile by default.** |
| A previously trusted device | May later be compromised; its permissions must be revocable instantly. |
| Local user of a participating computer | Trusted. Can pause/panic at any time. |
| Local privileged malware / physical access | **Out of scope.** Transport encryption cannot help. |

## Boundaries and controls

| Threat | Control | Where tested |
|---|---|---|
| Stranger connects and injects input | Strict pinned mutual TLS; unknown certificate fails the handshake | `tls::tests::untrusted_client_is_refused`, `engine_data::an_unpaired_identity_cannot_open_a_session` |
| Imposter answers at a known address | Client pins the *expected* fingerprint; mismatch fails | `tls::tests::wrong_server_identity_is_refused_by_client` |
| MITM during pairing | Both people compare the full fingerprint of the certificate each side *saw*; a relay shows different certificates on the two screens | `engine::pairing_needs_both_approvals…` |
| Pairing request spam | Window must be opened by the user, expires (2 min), one at a time, ≤ 5 attempts/min/address, flood limit before TLS | `engine::pairing_requests_are_ignored_unless…` |
| Fake "discovery" announcement | Announcement creates only an untrusted candidate; identity is checked by the pinned handshake | `discovery` tests, design |
| Revoked device reconnects | Trust set is shared live with the verifier; session ended and bound transfers failed first | `tls::…revocation…`, `engine::revoking_a_device…` |
| Session resumption bypasses revocation | Server session cache and tickets disabled; client resumption disabled | `tls::server_config` |
| Cross-session channel hijack | Bulk channels must present the control session's id **and** a secret token, from the same pinned identity, for an accepted transfer; compared in constant time | `engine_data::a_bulk_channel_cannot_attach…` |
| Oversized / malformed frames | Length checked before allocation (64 KiB control, 256 KiB bulk); structural validation; trailing bytes rejected | `proto` tests |
| Compression/decoder bombs | PNG dimensions capped (64 Mpx) before any pixel buffer is allocated | `clipboard::png_bomb_header…` |
| Malicious file names | Flat names only; traversal, separators, control/bidi characters, Windows device names refused; illegal characters replaced | `transfer::hostile_names…` |
| Symlink/reparse escape from the inbox | Receiving uses capability-based directory handles (`cap-std`); exclusive creation; never overwrites | `transfer::existing_files_and_symlinks…` |
| Corrupted or truncated files | SHA-256 verified before the temporary file is renamed; partials removed | `transfer::wrong_hash…` |
| Stuck keys after a failure | Destination tracks injected presses and releases all on leave/loss/pause/panic/revocation; source filter never swallows the release of a key the local OS saw go down | `control` tests, `engine::emergency_shortcut…` |
| Input flood / slow peer | Bounded queues; motion coalesced without losing deltas; any non-motion overflow ends the session | `platform::sink…` |
| Clipboard ping-pong | Change-token + single short-lived hash + (origin,id) set; no history | `clipboard`, `engine_data::clipboard_text_syncs_once…` |
| Secrets in the clipboard | Opt-in per device; pause switch; password-manager markers respected; clearly documented as incomplete | `engine_data::clipboard_respects…` |
| Sensitive data in logs/errors | Logs and notices carry no clipboard content, file contents or full paths | code review; `docs/LEDGER.md` |

## Residual risks (accepted, documented)

* Malware on either computer; physical access; users who skip fingerprint comparison.
* On X11, the emergency shortcut's last key is also seen by the focused application while sharing is idle (raw events cannot be swallowed).
* The identity key falls back to a `0600` file when no credential store exists.
* No certificate expiry or rotation: identity is a long-lived key pinned by fingerprint; replacing it means re-pairing.
* Unsigned builds trigger OS warnings; signing is documented, not performed.
