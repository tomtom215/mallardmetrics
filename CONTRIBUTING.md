# Contributing to Mallard Metrics

## Development Setup

1. Install Rust 1.85.0+ (the `rust-toolchain.toml` will handle this automatically)
2. Clone the repository
3. Run `cargo test` to verify your setup

## Development Workflow

1. Read `CLAUDE.md` and `LESSONS.md` before starting
2. Run the validation suite to establish baseline:
   ```bash
   cargo test && cargo clippy --all-targets && cargo fmt -- --check
   ```
3. Make your changes
4. Run the validation suite again
5. Update documentation if needed

## Code Style

- **Zero clippy warnings**: Pedantic, nursery, and cargo lint groups are enabled
- **Standard formatting**: Run `cargo fmt` before committing
- **Parameterized queries**: Never interpolate user input into SQL
- **Test everything**: Every public function needs tests covering happy path, edge cases, and error cases

## Testing Protocol

```bash
# Unit tests
cargo test --lib

# Integration tests
cargo test --test ingest_test

# All tests
cargo test

# Benchmarks (compilation check)
cargo bench --no-run
```

## Benchmark Protocol

- Use Criterion.rs with 100 samples
- Run benchmarks 3+ times before comparing
- Report mean with 95% confidence intervals
- Document negative results with the same rigor as positive results
- Never batch multiple optimizations into one measurement

## Pull Request Checklist

- [ ] `cargo test` — all tests pass
- [ ] `cargo clippy --all-targets` — zero warnings
- [ ] `cargo fmt -- --check` — zero violations
- [ ] `cargo doc --no-deps` — builds without errors
- [ ] CHANGELOG.md updated
- [ ] Documentation updated if applicable
