# Contributing

## Spec words stay in the spec

The planning documents number things: requirements, user stories, tasks.
Those numbers mean nothing outside those documents, and they go stale
the moment a document is rewritten.

So they never appear in code, comments, commit messages, or this
README. Write the complete sentence instead.

```
Bad:   // Settling, per FR-018
Good:  // Long enough that what was just heard is not heard again.
```

## Comments

Few, and short. Three lines is the limit. A comment says why, not what
the line above already says.

## Commit messages

English, wrapped, ten lines at most. Documentation and test changes go
in their own commits, apart from the implementation they belong to.

## Models

Nothing is added to `core/assets/` without checking what it is licensed
under and writing that down in `THIRD-PARTY-LICENSES`, with a checksum
and where it came from.

Models fail quietly when fed the wrong shape. Both bugs found that way
here were caught by printing a model's real inputs and outputs before
trusting them:

```bash
cargo run --example probe_model -- path/to/model.onnx
```

Pin what you learn in a test.

## Before opening a pull request

```bash
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all --check
cargo test --workspace
```

Tests needing real hardware or a wake word model are marked ignored and
do not run by default.
