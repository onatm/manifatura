# manifatura-tools

[![crates.io](https://img.shields.io/crates/v/manifatura-tools.svg)](https://crates.io/crates/manifatura-tools)
[![docs.rs](https://img.shields.io/docsrs/manifatura-tools)](https://docs.rs/manifatura-tools)

The basic tools for [manifatura](https://crates.io/crates/manifatura) (`read`, `edit`, `grep`, `glob` and `bash`), script tools made from any program, and `Local`, the env on this machine. The tools act through an `Env`, so they run wherever the env points.

Most programs depend on [`manifatura`](https://crates.io/crates/manifatura) instead, which re-exports this crate as `manifatura::tools` (`tools` feature, on by default).

- [API reference](https://docs.rs/manifatura-tools)
- Guide: [Tools](https://github.com/onatm/manifatura/blob/main/docs/tools.md)
- [manifatura's README](https://github.com/onatm/manifatura#readme)

## Licence

Licensed under the [MIT licence](LICENSE).
