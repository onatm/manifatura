# manifatura-kit

[![crates.io](https://img.shields.io/crates/v/manifatura-kit.svg)](https://crates.io/crates/manifatura-kit)
[![docs.rs](https://img.shields.io/docsrs/manifatura-kit)](https://docs.rs/manifatura-kit)

Hook bundles for [manifatura](https://crates.io/crates/manifatura): approvals with scoped "always" answers, compaction, a deferred tool catalog (`find_tools`, `run_tool`), input repair for small models, `AGENTS.md` discovery and a system prompt built from tool guidelines.

Most programs depend on [`manifatura`](https://crates.io/crates/manifatura) instead, which re-exports this crate as `manifatura::kit` (`kit` feature, on by default).

- [API reference](https://docs.rs/manifatura-kit)
- Guide: [Hooks](https://github.com/onatm/manifatura/blob/main/docs/hooks.md)
- [manifatura's README](https://github.com/onatm/manifatura#readme)

## Licence

Licensed under the [MIT licence](LICENSE).
