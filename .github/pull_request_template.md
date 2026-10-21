# Pull request

Thank you for the patch. WALLY has a few non-negotiables — this checklist
exists so reviews are short. See CONTRIBUTING.md for the details.

## What does this PR change?

<!-- one paragraph: the problem, the approach, the trade-off -->

## Checklist

- [ ] `cargo fmt --check` passes
- [ ] `cargo clippy --all-targets -- -D warnings` passes
- [ ] `cargo test` passes
- [ ] Builds in all four feature shapes:
      default, `--no-default-features`, `--features jxl`,
      `--no-default-features --features jxl`
- [ ] Manually checked on at least one layer-shell compositor
      (which one, and what you looked at):
- [ ] No new heavyweight dependency (toolkit, system library, C dep) —
      if a crate was added, say why std or an existing dep could not do it:
- [ ] The one-shot path (`wally image.png`) still behaves exactly as before
- [ ] Performance identity intact: zero CPU when occluded, identical-frame
      skip, sub-rect recompose, `madvise` handback (tick these only if your
      change touches those paths — and explain how you verified)
- [ ] New flags are additive; no existing flag changed meaning
- [ ] `CHANGELOG.md` updated (Under a "Unreleased" heading when applicable)
