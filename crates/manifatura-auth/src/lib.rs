//! Signing in to model vendors for manifatura. A sign-in gives a provider its credentials, so
//! an agent's model calls run on the user's own account or plan.
//!
//! | Module | Sign-in |
//! | --- | --- |
//! | [`openai`] | OpenAI's Sign in with ChatGPT: requests to the Responses API on the user's ChatGPT plan |

#![warn(missing_debug_implementations, missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]

pub mod openai;
