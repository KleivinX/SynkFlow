# Security

## Reporting a vulnerability

Please report security problems privately to the maintainers (open a private security advisory on the repository once it is
hosted, or contact the maintainer listed in the repository profile). Include the version (`synkflow --version`), your OS, and
steps to reproduce. Do not post exploit details publicly before a fix is available. There is no bug bounty.

## What Synkflow promises

* **Authenticated, encrypted transport only.** TLS 1.3, mutual authentication, pinned peer identities. No TLS 1.2, no
  plaintext fallback, no session resumption (so revocation cannot be bypassed by a cached session).
* **No automatic trust.** Discovery, a matching name, or a matching IP address creates an *untrusted candidate* and nothing
  more. Trust is created only by a verified, reciprocal approval of the **full SHA-256 fingerprint**.
* **Explicit, separate permissions** per device: receive keyboard/mouse control, send control, clipboard each way, file receive.
  All off by default except what you tick while pairing.
* **Safe failure.** Any disconnect, pause, lock, panic, revocation or permission loss releases every key and button that
  Synkflow pressed on the controlled computer and returns control locally.
* **Bounded everything.** Frames, queues, clipboard payloads, file chunks, image dimensions and message counts are limited
  before allocation. Malformed or oversized input ends the session.
* **100 % local.** No account, cloud backend, relay, analytics, telemetry, update check or external fonts/scripts/images.

## What it does *not* protect against

* Malware or a malicious administrator **already running on either computer**. It can read the clipboard, press keys, or
  read the identity key file if the OS credential store is unavailable.
* Physical access to an unlocked computer.
* A person who approves a pairing without comparing fingerprints.
* Compromised keyboard/mouse hardware.
* Traffic analysis: an observer on your LAN can see that two computers talk, and how much.

## The one deliberate unauthenticated door

The **pairing channel** (ALPN `synkflow-pair/1`) must accept a stranger's certificate, because that is how a stranger is
introduced. It still verifies that the peer holds the private key for the certificate it presents, but it grants *nothing*:
only `PairHello`/`PairDecision` (and ping/bye) are accepted, any other message ends it, it is rate-limited per address,
open only while *you* opened a pairing window (default 2 minutes) and only one at a time, and trust is recorded only after
both people approved. It is not an "accept every certificate" verifier for sessions; sessions use a strict verifier.

## Key storage

The identity key (Ed25519, PKCS#8) lives in the OS credential store (macOS Keychain, Windows Credential Manager, Linux Secret
Service) where available. Otherwise Synkflow falls back to a file readable only by your user (`0600` on Unix) and **tells you
so in the app**; that fallback is *not* equivalent to hardware-backed storage. Secret bytes are held in zeroizing buffers where
practical; Synkflow does not claim the whole process is scrubbed.

## Dependencies

Build-time dependency downloads are allowed; runtime makes no external connections. Run `cargo audit` / `cargo deny` in CI
(not run in the author's environment — see `docs/LEDGER.md`).
