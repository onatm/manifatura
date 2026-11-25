# Changelog

All notable changes are recorded here. The project follows [Semantic Versioning](https://semver.org); before 1.0, a minor version may change the API.

## 0.1.0

The first release.

- `manifatura-agent`: the loop, fifteen typed hook points, sessions as an append-only tree with branching and compaction, steering and follow-ups, and the `Provider`, `Env` and `Store` traits.
- `manifatura-kit`: approvals with scoped "always" answers, compaction hooks and a summariser, a deferred tool catalog, input repair, `AGENTS.md` discovery, a system prompt built from tool guidelines, and the system delta for warm prompt caches.
- `manifatura-models`: llama.cpp and any other OpenAI-compatible server behind a `provider/model-id` registry.
- `manifatura-store`: sessions as JSON lines, repaired after a crash mid-write.
- `manifatura-tools`: `read`, `edit` (which also creates files), `grep` and `glob` (respecting `.gitignore`), and `bash` with background jobs; `Local`; script tools.
- `manifatura-mcp`: MCP servers' tools, over stdio or streamable HTTP.
- `manifatura`: the facade, with each block behind a feature.
