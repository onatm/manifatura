# manifatura-models

[![crates.io](https://img.shields.io/crates/v/manifatura-models.svg)](https://crates.io/crates/manifatura-models)
[![docs.rs](https://img.shields.io/docsrs/manifatura-models)](https://docs.rs/manifatura-models)

Model providers for [manifatura](https://crates.io/crates/manifatura): llama.cpp, built in, and any other server speaking OpenAI-compatible Chat Completions, behind one `provider/model-id` registry and streamed over SSE.

Most programs depend on [`manifatura`](https://crates.io/crates/manifatura) instead, which re-exports this crate as `manifatura::models` (`models-openai` feature, on by default).

- [API reference](https://docs.rs/manifatura-models)
- Guide: [Models](https://github.com/onatm/manifatura/blob/main/docs/models.md)
- [manifatura's README](https://github.com/onatm/manifatura#readme)

## Licence

Licensed under the [MIT licence](LICENSE).
