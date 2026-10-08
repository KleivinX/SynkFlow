# Benchmarks

Everything below was **measured on one machine**: macOS 13.7.8, Intel x86_64, Rust 1.96.1. Nothing was measured on Windows or
Linux, on two physical computers, or over a real network. Numbers from a different machine will differ. A figure that is not in
this file was not measured.

## 1. Engine-path latency (pointer events)

`tests/latency.rs` (ignored by default). Two complete engines are paired over **loopback TLS 1.3**; a pointer-move event is
injected at the sending engine's capture end and timed until it arrives at the receiving engine's injection end. Both OS ends are
the in-memory fakes (`dev-backend`).

```bash
cargo test --no-default-features --test latency -- --ignored --nocapture
```

| Events | Median | p95 | p99 | Max |
|---|---|---|---|---|
| 2000 | 0.348 ms | 0.889 ms | 1.798 ms | 4.614 ms |

What this is: the cost of Synkflow's own path (state machine, framing, TLS record, scheduling) on one host, in an **unoptimised
debug build**. What it is not: end-to-end input latency. It excludes real OS capture and injection, the network, Wi-Fi jitter and
display latency. A real LAN adds its own round-trip time on top.

## 2. Large-file transfer: memory and speed

`transfer::tests::large_file` streams a file between two engines over loopback and verifies its SHA-256.

* 96 MiB transfer: peak resident set **≈ 9.8 MB** (`/usr/bin/time -l`), i.e. the file is streamed in bounded chunks, not buffered.
* Duration for 96 MiB: **1.5–1.75 s** (debug build, loopback, SHA-256 included).
* These two figures were taken earlier in the session. The final full test pass ran the same test at 32 MiB (`SYNKFLOW_LARGE_MIB=32`)
  because the build machine's disk was almost full; it passed, and the 96 MiB figures were not repeated.

Loopback speed says nothing about real network throughput.

## 3. Idle resource use of the packaged macOS app

The `Synkflow.app` binary from the final `.dmg`, mounted read-only, started with `--background` (tray item only), a throw-away
config folder and the file-based secret store, no paired devices, mDNS discovery on. Sampled with `ps` every 3 s for 24 s after a
6 s settle, plus `top`:

| Measure | Result |
|---|---|
| CPU | 0.0 % in all eight `ps` samples; 0.4 % in one `top` sample |
| Resident memory | 35.1 → 34.3 MB (`ps` RSS) |
| Threads | 12 |
| Listening sockets | 1 TCP |

This is an idle, tray-only process on one machine. CPU and memory **while sharing input, with several peers, with the main window
open or during a transfer** were not measured, and neither was battery impact.

## 4. Binary sizes (as built)

| File | Size |
|---|---|
| macOS `synkflow` binary (inside the `.app`) | 23.3 MB |
| `Synkflow-0.1.0.dmg` | 12.6 MB |
| Windows `synkflow.exe` (cross-built, never run) | 32.8 MB |
| `Synkflow-Setup-0.1.0.exe` (installer with embedded app) | 34.4 MB |

## Not measured

Real-network throughput and latency; behaviour under packet loss; Windows and Linux anything; multi-peer scaling; CPU under
sustained input; startup time; battery; memory after days of uptime. Reproduce and extend the numbers above on your own hardware
before relying on them.
