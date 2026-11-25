# manifatura-store

[![crates.io](https://img.shields.io/crates/v/manifatura-store.svg)](https://crates.io/crates/manifatura-store)
[![docs.rs](https://img.shields.io/docsrs/manifatura-store)](https://docs.rs/manifatura-store)

Session stores for [manifatura](https://crates.io/crates/manifatura): each session as a file of JSON lines, appended to as the session runs and repaired after a crash mid-write.

Most programs depend on [`manifatura`](https://crates.io/crates/manifatura) instead, which re-exports this crate as `manifatura::store` (`store-jsonl` feature, on by default).

- [API reference](https://docs.rs/manifatura-store)
- Guide: [Agents and sessions](https://github.com/onatm/manifatura/blob/main/docs/sessions.md)
- [manifatura's README](https://github.com/onatm/manifatura#readme)

## Licence

Licensed under the [MIT licence](LICENSE).
