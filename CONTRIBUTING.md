# Contributing

Thanks for helping. A few rules keep this project trustworthy.

1. **Security first.** No plaintext fallbacks, no "accept any certificate" path to a session, no automatic trust, no custom
   cryptography. If a change touches `tls.rs`, `proto.rs`, `engine/pairing.rs`, `transfer.rs` or `platform/*`, say how it was tested.
2. **Don't fake it.** If a capability can't be implemented correctly, expose the limitation in the UI and in
   `docs/CAPABILITIES.md`; never simulate success. Fake backends (`--features dev-backend`) are for tests only.
3. **Unsafe code** lives only in the small FFI blocks of `src/platform/{macos,windows}.rs`, each with a `SAFETY:` comment.
4. **Keep it small.** Prefer an existing maintained crate or the standard library; no abstraction for a single implementation.
5. **Tests with changes.** Logic gets a test that fails if it breaks. Don't weaken a test to pass. Report skipped tests as skipped.
6. **Accessibility and honesty in the UI**: keyboard reachable, visible focus, status never by colour alone, errors say what
   happened / what stayed safe / what to do.

```bash
cargo fmt && cargo clippy --all-targets -- -D warnings
cargo test                      # includes the loopback integration suites
cargo test --no-default-features
python3 tools/gen_notices.py    # after changing dependencies
```

Before a release run the manual checklist in `docs/TESTING.md` on real hardware for each OS you ship.
By contributing you agree your work is licensed GPL-3.0-only.
