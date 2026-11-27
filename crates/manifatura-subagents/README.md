# manifatura-subagents

[![crates.io](https://img.shields.io/crates/v/manifatura-subagents.svg)](https://crates.io/crates/manifatura-subagents)
[![docs.rs](https://img.shields.io/docsrs/manifatura-subagents)](https://docs.rs/manifatura-subagents)

Subagents for [manifatura](https://crates.io/crates/manifatura): a `task` tool that hands a self-contained task to another agent, or to a copy of the calling one, in a child session of its own, and returns its report.

Most programs depend on [`manifatura`](https://crates.io/crates/manifatura) instead, which re-exports this crate as `manifatura::subagents` (`subagents` feature, off by default).

- [API reference](https://docs.rs/manifatura-subagents)
- Guide: [Subagents](https://github.com/onatm/manifatura/blob/main/docs/subagents.md)
- [manifatura's README](https://github.com/onatm/manifatura#readme)

## Licence

Licensed under the [MIT licence](LICENSE).
