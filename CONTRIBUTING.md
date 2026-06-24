# Contributing to muxel

Thanks for your interest in muxel!

## License of contributions (please read)

muxel is **dual-licensed**: GPL-3.0 for open-source use, plus a commercial
license offered by **ProjectHax LLC** (see [LICENSING.md](LICENSING.md)). For
that to remain possible, ProjectHax LLC needs the right to ship every
contribution under **both** licenses.

**By submitting a contribution** — a pull request, patch, or any code, docs, or
other material — **to this project, you agree that:**

1. You are the author of the contribution (or have permission from the rights
   holder to submit it), and to your knowledge it does not violate any third
   party's rights.
2. You license your contribution under the **GPL-3.0**, and you additionally
   grant ProjectHax LLC a perpetual, worldwide, royalty-free, irrevocable right
   to **relicense** it under other terms, including ProjectHax LLC's commercial
   license.

This inbound grant is what lets muxel stay open under the GPL while ProjectHax
LLC can also offer a commercial license. If you can't agree to it, please don't
submit a contribution.

## Development

Build and run instructions are in the [README](README.md). Before opening a PR,
run the full gate and fix everything it reports:

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings   # warnings are errors
cargo test --workspace
cargo build -p muxel
```

`CLAUDE.md` documents the workspace layout and project conventions. Keep pure,
testable logic in `muxel-core`, and add a `FEATURES.md` entry when you add or
change a user-facing feature.
