# Contributing

Thanks for wanting to help. Issues and pull requests are welcome; for anything larger than a fix, open an issue first so we can agree on the shape before you write it.

## Working on it

You need Rust 1.91 or newer.

```sh
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features
cargo fmt --all
```

The examples need a model: see the [README](README.md#quick-start).

## What fits

manifatura stays small on purpose. The loop holds mechanism; policy belongs in hooks, and most new behaviour fits as a bundle in `kit` or in your own crate rather than in the core. Before adding to `manifatura-agent`, ask whether a hook could do it.

- Keep the core's dependencies as they are: `serde`, `tokio`, `futures-util`, `async-trait` and `schemars`.
- Anything that touches files, processes or the clock in the core or `kit` goes through `Env`, `Store` or `Provider`, or is passed in by the app.
- Public items need docs; CI denies warnings.
- New behaviour needs a test. `manifatura::testing::ScriptedProvider` (`testing` feature) scripts a model's replies and records its requests.
- Changes users will notice go in [CHANGELOG.md](CHANGELOG.md).

## Licence

Unless you state otherwise, any contribution you submit for inclusion in this project is licensed under the [MIT licence](LICENSE), without any additional terms or conditions.
