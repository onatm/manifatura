# manifatura-sandbox

[![crates.io](https://img.shields.io/crates/v/manifatura-sandbox.svg)](https://crates.io/crates/manifatura-sandbox)
[![docs.rs](https://img.shields.io/docsrs/manifatura-sandbox)](https://docs.rs/manifatura-sandbox)

A sandboxed env for [manifatura](https://crates.io/crates/manifatura): `Sandbox` wraps another env and runs its commands under macOS Seatbelt. They read anything, but write only to the workspace and scratch directories, never to `.git`, and have no network beyond `localhost`. The env's own writes follow the same rules. macOS only for now.

Most programs depend on [`manifatura`](https://crates.io/crates/manifatura) instead, which re-exports this crate as `manifatura::sandbox` (`sandbox` feature, off by default).

- [API reference](https://docs.rs/manifatura-sandbox)
- Guide: [Tools](https://github.com/onatm/manifatura/blob/main/docs/tools.md#a-sandbox)
- [manifatura's README](https://github.com/onatm/manifatura#readme)

## Licence

Licensed under the [MIT licence](LICENSE).
