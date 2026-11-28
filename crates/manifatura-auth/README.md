# manifatura-auth

[![crates.io](https://img.shields.io/crates/v/manifatura-auth.svg)](https://crates.io/crates/manifatura-auth)
[![docs.rs](https://img.shields.io/docsrs/manifatura-auth)](https://docs.rs/manifatura-auth)

Signing in to model vendors for [manifatura](https://crates.io/crates/manifatura), so an agent's model calls run on the user's own account or plan. `openai::SignIn` is OpenAI's Sign in with ChatGPT: the user signs in through the browser, and requests go to the Responses API on their ChatGPT plan.

Most programs depend on [`manifatura`](https://crates.io/crates/manifatura) instead, which re-exports this crate as `manifatura::auth` (`auth` feature, off by default).

- [API reference](https://docs.rs/manifatura-auth)
- Guide: [Models](https://github.com/onatm/manifatura/blob/main/docs/models.md)
- [manifatura's README](https://github.com/onatm/manifatura#readme)

## Licence

Licensed under the [MIT licence](LICENSE).
