# Contributing

Issues and focused pull requests are welcome.

1. Fork the repository and create a branch.
2. Add tests for behavior changes, including multiprocess tests for concurrency changes.
3. Run `uv sync --group dev`, `ruff check .`, `ruff format --check .`, and `pytest`.
4. Explain any change to transaction boundaries or process-liveness semantics in the pull request.

Please keep the core local-first and usable without an account or hosted service.
