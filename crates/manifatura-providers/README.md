# manifatura-providers

[![crates.io](https://img.shields.io/crates/v/manifatura-providers.svg)](https://crates.io/crates/manifatura-providers)
[![docs.rs](https://img.shields.io/docsrs/manifatura-providers)](https://docs.rs/manifatura-providers)

Model providers for [manifatura](https://crates.io/crates/manifatura): llama.cpp, built in, and any other server speaking OpenAI-compatible Chat Completions, and OpenAI's Responses API, behind one `provider/model-id` registry and streamed over SSE.

Most programs depend on [`manifatura`](https://crates.io/crates/manifatura) instead, which re-exports this crate as `manifatura::providers` (`providers-openai` feature, on by default; `providers-responses` for the Responses API).

- [API reference](https://docs.rs/manifatura-providers)
- Guide: [Models](https://github.com/onatm/manifatura/blob/main/docs/models.md)
- [manifatura's README](https://github.com/onatm/manifatura#readme)

## Licence

Licensed under the [MIT licence](LICENSE).
