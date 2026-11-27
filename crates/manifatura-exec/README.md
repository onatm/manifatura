# manifatura-exec

[![crates.io](https://img.shields.io/crates/v/manifatura-exec.svg)](https://crates.io/crates/manifatura-exec)
[![docs.rs](https://img.shields.io/docsrs/manifatura-exec)](https://docs.rs/manifatura-exec)

Commands that keep running, for [manifatura](https://crates.io/crates/manifatura): `exec_command` starts a command and returns when it exits or after a short wait, leaving it running in the background under a session ID; `write_stdin` polls it, types into its terminal or interrupts it. The tools take the shapes of OpenAI's Codex, which OpenAI's models are trained on, and work with any model.

Most programs depend on [`manifatura`](https://crates.io/crates/manifatura) instead, which re-exports this crate as `manifatura::exec` (`exec` feature, off by default).

- [API reference](https://docs.rs/manifatura-exec)
- Guide: [Tools](https://github.com/onatm/manifatura/blob/main/docs/tools.md#commands-that-keep-running)
- [manifatura's README](https://github.com/onatm/manifatura#readme)

## Licence

Licensed under the [MIT licence](LICENSE).
