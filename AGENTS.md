# Project Instructions

## README synchronization

When changing README content, update `README.md` and `README-ja.md` in the same
change set. Keep their heading structure, explanations, links, and code examples
in semantic parity.

## Source comments

Write source-code comments in Japanese. Document the contract owned by the code
and the reason for non-obvious constraints so that a reader can reconstruct the
design from the source file alone. Do not narrate behavior already expressed by
names, types, or straightforward control flow.

## Quality gate

After changing code, run all of the following commands and resolve every failure:

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
```
