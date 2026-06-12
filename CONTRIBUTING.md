# Contributing to Nexus

Thanks for your interest in improving Nexus!

## Quick start

```bash
git clone <this-repo>
cd nexus
cargo build --release
./target/release/nexus --version
```

## Development loop

```bash
cargo fmt --all && cargo clippy --all-targets && cargo test --all
```

All three must pass before sending a PR.

## Adding a new command filter

Filters live in `src/cmds/<ecosystem>/`. Each filter follows the same shape:

1. Add a variant to the `Commands` enum in `src/main.rs`.
2. Create `src/cmds/<eco>/<cmd>_cmd.rs` with a `pub fn run(args, verbose) -> Result<()>`.
3. Implement the filter: execute the underlying command, transform stdout, fall back to raw on error.
4. Add a fixture in `tests/fixtures/` and a snapshot test using `insta`.
5. Verify ≥60% token savings on the fixture.

See existing modules like `src/cmds/git/git.rs` for reference patterns.

## Code style

- `anyhow::Result` with `.context("…")?` for all error paths.
- All `regex::Regex` must be in `lazy_static!`.
- No `async`. Single-threaded by design.
- No `unwrap()` in production code paths — use `.context()` or `.expect("invariant")`.

## License

By contributing, you agree your contributions are licensed under Apache 2.0.
