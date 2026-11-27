# manifatura-snapshots

[![crates.io](https://img.shields.io/crates/v/manifatura-snapshots.svg)](https://crates.io/crates/manifatura-snapshots)
[![docs.rs](https://img.shields.io/docsrs/manifatura-snapshots)](https://docs.rs/manifatura-snapshots)

Workspace snapshots for [manifatura](https://crates.io/crates/manifatura): a hook snapshots the working directory as each turn starts, as a git tree in a repository of its own (never the project's `.git`), and records it in the session, so an app can put the files back: an undo for what an agent changed, through a file tool or a command.

Most programs depend on [`manifatura`](https://crates.io/crates/manifatura) instead, which re-exports this crate as `manifatura::snapshots` (`snapshots` feature, off by default).

- [API reference](https://docs.rs/manifatura-snapshots)
- Guide: [Agents and sessions](https://github.com/onatm/manifatura/blob/main/docs/sessions.md#going-back)
- [manifatura's README](https://github.com/onatm/manifatura#readme)

## Licence

Licensed under the [MIT licence](LICENSE).
