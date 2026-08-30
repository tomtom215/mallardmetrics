## Summary

<!-- One paragraph explaining what this PR does and why. -->

## Changes

<!-- Bullet list of the specific changes made. -->

-
-

## Type of change

- [ ] Bug fix (non-breaking change that fixes an issue)
- [ ] New feature (non-breaking change that adds functionality)
- [ ] Breaking change (fix or feature that changes existing behaviour)
- [ ] Refactor (no behaviour change)
- [ ] Documentation update
- [ ] CI / infrastructure change

## Testing

<!-- Describe how you tested this. Include test names added, commands run, and any manual verification. -->

```
cargo test --all-targets
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt -- --check
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps

# Behavioral-extension tests skip when the extension cannot be downloaded,
# so a green run does not prove they ran. CI turns a skip into a failure.
MALLARD_REQUIRE_BEHAVIORAL=1 cargo test --all-targets

# The in-process tests cannot see a route that only breaks once the binary
# is assembled and listening.
cargo build && scripts/smoke-test.sh
```

- [ ] All tests pass (`cargo test --all-targets`), and the count in DEVELOPMENT.md matches
- [ ] Behavioral tests really ran (`MALLARD_REQUIRE_BEHAVIORAL=1`)
- [ ] Zero clippy warnings (`cargo clippy --all-targets --all-features -- -D warnings`)
- [ ] Zero formatting violations (`cargo fmt -- --check`)
- [ ] Documentation builds clean (`RUSTDOCFLAGS="-D warnings" cargo doc --no-deps`)
- [ ] `scripts/smoke-test.sh` passes against the real binary
- [ ] New functionality is covered by unit tests
- [ ] Integration tests added/updated if HTTP behaviour changed
- [ ] If the dashboard changed: `node scripts/check-dashboard-browser.mjs` passes

## Security checklist (if applicable)

- [ ] No SQL injection vectors introduced (parameterized queries used)
- [ ] No path traversal vectors introduced (`is_safe_path_component` called)
- [ ] No PII stored (IPs only for hashing, never persisted; auth logs keep only a truncated prefix)
- [ ] Filter and query values are bound, never interpolated — only fixed-enum column names are
- [ ] No new `unwrap()` / `expect()` that could panic in production
- [ ] `MALLARD_ADMIN_PASSWORD` and other secrets not logged

## Documentation

- [ ] DEVELOPMENT.md updated with changes and verified test counts
- [ ] LESSONS.md updated if a new lesson was learned
- [ ] Public-facing docs updated (README, docs/src/) if behaviour changed
- [ ] CHANGELOG.md entry added

## Before/after evidence

<!-- If fixing a bug or adding a feature, show before/after output or test results. -->

```
# Before

# After
```
