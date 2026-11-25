# manifatura-agent

[![crates.io](https://img.shields.io/crates/v/manifatura-agent.svg)](https://crates.io/crates/manifatura-agent)
[![docs.rs](https://img.shields.io/docsrs/manifatura-agent)](https://docs.rs/manifatura-agent)

The core of [manifatura](https://crates.io/crates/manifatura): the agent loop, fifteen typed hook points, sessions kept as an append-only tree, and the `Provider`, `Env` and `Store` traits that the other crates implement.

Most programs depend on [`manifatura`](https://crates.io/crates/manifatura) instead, which re-exports this crate at its root.

- [API reference](https://docs.rs/manifatura-agent)
- Guide: [Agents and sessions](https://github.com/onatm/manifatura/blob/main/docs/sessions.md)
- [manifatura's README](https://github.com/onatm/manifatura#readme)

## Licence

Licensed under the [MIT licence](LICENSE).
