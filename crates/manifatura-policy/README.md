# manifatura-policy

[![crates.io](https://img.shields.io/crates/v/manifatura-policy.svg)](https://crates.io/crates/manifatura-policy)
[![docs.rs](https://img.shields.io/docsrs/manifatura-policy)](https://docs.rs/manifatura-policy)

Approval policy for [manifatura](https://crates.io/crates/manifatura): rules that allow, ask about or deny tool calls, by tool and, for shell commands, by command prefix (`exec_command(git commit)`). Command lines are split with tree-sitter-bash into the simple commands they run, and each is checked. Questions go through `manifatura_kit::Approvals`, so any client can answer them, and "allow and remember" turns an answer into a rule.

Most programs depend on [`manifatura`](https://crates.io/crates/manifatura) instead, which re-exports this crate as `manifatura::policy` (`policy` feature, off by default).

- [API reference](https://docs.rs/manifatura-policy)
- Guide: [Approvals](https://github.com/onatm/manifatura/blob/main/docs/approvals.md)
- [manifatura's README](https://github.com/onatm/manifatura#readme)

## Licence

Licensed under the [MIT licence](LICENSE).
