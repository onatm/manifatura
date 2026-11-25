# manifatura-mcp

[![crates.io](https://img.shields.io/crates/v/manifatura-mcp.svg)](https://crates.io/crates/manifatura-mcp)
[![docs.rs](https://img.shields.io/docsrs/manifatura-mcp)](https://docs.rs/manifatura-mcp)

MCP servers' tools for [manifatura](https://crates.io/crates/manifatura), over stdio or streamable HTTP, on [`rmcp`](https://crates.io/crates/rmcp). A server's tools become `<server>__<tool>` tools, deferred to the catalog unless the server is `eager`.

Most programs depend on [`manifatura`](https://crates.io/crates/manifatura) instead, which re-exports this crate as `manifatura::mcp` (`mcp` feature).

- [API reference](https://docs.rs/manifatura-mcp)
- Guide: [Tools](https://github.com/onatm/manifatura/blob/main/docs/tools.md#mcp-servers)
- [manifatura's README](https://github.com/onatm/manifatura#readme)

## Licence

Licensed under the [MIT licence](LICENSE).
