## What this changes

## How it was checked

- [ ] `cargo fmt --check`
- [ ] `cargo clippy --all-targets -- -D warnings`
- [ ] `cargo test --no-default-features`
- [ ] Anything touching input capture, injection or a platform backend was tried on a real computer (say which OS)

## Notes for the reviewer
Security-relevant changes (pairing, trust, TLS, file handling, clipboard) should explain which rule in `docs/THREAT_MODEL.md` they keep.
