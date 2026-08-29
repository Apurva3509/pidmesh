# Contributing

Issues and focused pull requests are welcome.

1. Fork the repository and create a branch.
2. Add tests for behavior changes, including multiprocess tests for transaction or liveness changes.
3. Run `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D warnings`, and
   `cargo test --all-targets`.
4. Run the release benchmark when changing storage or retrieval hot paths.
5. Explain changes to transaction boundaries, database compatibility, or PID semantics in the PR.

Keep the core local-first and usable without an account or hosted service.
