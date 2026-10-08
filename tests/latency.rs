//! Engine-path latency: a pointer event captured on computer A until it is applied on computer B,
//! through the control state machine, the session queue, TLS 1.3 over loopback TCP, and B's injector
//! thread. The two ends of the OS (real capture, real injection) are fakes, so this is **not**
//! end-to-end cursor latency and says nothing about OS or network behaviour. Run it explicitly:
//!
//!   cargo test --release --no-default-features --test latency -- --ignored --nocapture
mod common;

use std::time::{Duration, Instant};

use common::*;
use synkflow::control::Raw;
use synkflow::view::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "benchmark; run explicitly with --ignored --nocapture"]
async fn pointer_event_engine_path_latency() {
    let a = Node::new("Alpha", 1000, 800).await;
    let b = Node::new("Beta", 1000, 800).await;
    pair(&a, &b, full_perms(), full_perms()).await;
    a.engine.send(Command::ApplyLayout(layout_side_by_side(&a, &b, 1000)));
    b.wait("layout", |s| s.config.layout.revision > 0).await;
    settle().await;
    a.physical(Raw::Pointer { x: 999.0, y: 400.0, dx: 6.0, dy: 0.0 });
    a.wait("controlling", |s| s.sharing.state == StateLabel::Controlling).await;
    b.wait("controlled", |s| s.sharing.state == StateLabel::BeingControlled).await;
    settle().await;
    b.input.clear_injected();

    // 2000 one-point moves paced at ~1 kHz, bouncing so the pointer stays inside B's screen.
    const N: usize = 2000;
    let mut sent = Vec::with_capacity(N);
    for i in 0..N {
        let dy = if (i / 300) % 2 == 0 { 1.0 } else { -1.0 };
        sent.push(Instant::now());
        a.physical(Raw::Pointer { x: 999.0, y: 400.0, dx: 0.0, dy });
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let applied = b.input.injected_times();
    assert!(applied.len() >= N * 95 / 100, "only {} of {N} events arrived", applied.len());
    let mut lat: Vec<f64> = applied.iter().zip(&sent).map(|(t, s)| t.duration_since(*s).as_secs_f64() * 1000.0).collect();
    lat.sort_by(|x, y| x.total_cmp(y));
    let pct = |p: f64| lat[((lat.len() as f64 * p) as usize).min(lat.len() - 1)];
    eprintln!(
        "engine-path latency over {} events (loopback TLS, fake OS ends): median {:.3} ms, p95 {:.3} ms, p99 {:.3} ms, max {:.3} ms",
        lat.len(),
        pct(0.5),
        pct(0.95),
        pct(0.99),
        lat[lat.len() - 1]
    );
}
