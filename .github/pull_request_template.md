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
cargo test
cargo clippy --all-targets
cargo fmt -- --check
cargo doc --no-deps
```

- [ ] All 333+ tests pass (`cargo test`)
- [ ] Zero clippy warnings (`cargo clippy --all-targets -- -D warnings`)
- [ ] Zero formatting violations (`cargo fmt -- --check`)
- [ ] Documentation builds without errors (`cargo doc --no-deps`)
- [ ] New functionality is covered by unit tests
- [ ] Integration tests added/updated if HTTP behaviour changed

## Security checklist (if applicable)

- [ ] No SQL injection vectors introduced (parameterized queries used)
- [ ] No path traversal vectors introduced (`is_safe_path_component` called)
- [ ] No PII stored (IPs only for hashing, never persisted)
- [ ] No new `unwrap()` / `expect()` that could panic in production
- [ ] `MALLARD_ADMIN_PASSWORD` and other secrets not logged

## Documentation

- [ ] CLAUDE.md updated with changes and verified test counts
- [ ] LESSONS.md updated if a new lesson was learned
- [ ] Public-facing docs updated (README, docs/src/) if behaviour changed
- [ ] CHANGELOG.md entry added

## Before/after evidence

<!-- If fixing a bug or adding a feature, show before/after output or test results. -->

```
# Before

# After
```
