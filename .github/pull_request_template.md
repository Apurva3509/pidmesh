## Summary

Describe the coordination problem and the behavior this changes.

## Validation

- [ ] `cargo fmt --check`
- [ ] `cargo clippy --all-targets --all-features -- -D warnings`
- [ ] `cargo test --all-targets`
- [ ] Multiprocess behavior is covered when transaction or liveness semantics change

## Local-first review

- [ ] No required cloud service or account was introduced
- [ ] Workspace isolation and owner-only storage permissions are preserved
- [ ] Transaction, lease, or PID-liveness changes are explained
