//! The basic tools: `read`, `edit`, `grep`, `glob` and `bash`, and `apply_patch`, which edits
//! with a patch instead of `edit`'s replacements. Each acts through the session's [`Env`], so
//! the same tools run wherever the env points: locally, in a sandbox, or on a host that
//! implements it. [`Local`] (`local` feature, on by default) is the env on this machine.
//!
//! Tools of an app's own are built with [`Tool::new`]; [`ScriptTool`] makes one from a
//! program.
//!
//! ```no_run
//! let agent = manifatura_agent::Agent::new("You are a coding agent.")
//!     .tools(manifatura_tools::all());
//! ```

#![warn(missing_debug_implementations, missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]

mod bash;
mod files;
#[cfg(feature = "local")]
mod local;
mod patch;
mod script;
mod search;
pub mod truncate;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use manifatura_agent::{Env, Tool};

pub use bash::bash;
pub use files::{FileTracker, edit, read};
#[cfg(feature = "local")]
pub use local::Local;
pub use patch::{GRAMMAR as PATCH_GRAMMAR, apply_patch};
pub use script::ScriptTool;
pub use search::{glob, grep};

/// Every built-in tool name: those [`all`] gives, in its order, then `apply_patch`.
pub const NAMES: &[&str] = &["read", "edit", "grep", "glob", "bash", "apply_patch"];

/// `read`, `edit`, `grep`, `glob` and `bash`, with `read` and `edit` sharing one
/// [`FileTracker`].
pub fn all() -> Vec<Tool> {
    let t = Arc::new(FileTracker::default());
    vec![read(t.clone()), edit(t), grep(), glob(), bash()]
}

/// The named built-in tools, sharing one [`FileTracker`]. Unknown names are an error.
pub fn by_name<S: AsRef<str>>(names: &[S]) -> Result<Vec<Tool>, String> {
    let t = Arc::new(FileTracker::default());
    names
        .iter()
        .map(|n| match n.as_ref() {
            "read" => Ok(read(t.clone())),
            "edit" => Ok(edit(t.clone())),
            "grep" => Ok(grep()),
            "glob" => Ok(glob()),
            "bash" => Ok(bash()),
            "apply_patch" => Ok(apply_patch(t.clone())),
            other => Err(format!(
                "unknown tool `{other}` (built-in tools: {})",
                NAMES.join(", ")
            )),
        })
        .collect()
}

/// Resolves a tool path against the workspace. Small models often drop the leading `/` of an
/// absolute workspace path (`home/pi/app/src/x.rs` for `/home/pi/app/src/x.rs`); such a path
/// is taken as absolute when it lands inside the workspace.
pub fn resolve(env: &dyn Env, path: &str) -> PathBuf {
    let p = Path::new(path);
    if p.is_relative() {
        let rooted = Path::new("/").join(p);
        if rooted.starts_with(env.cwd()) {
            return rooted;
        }
    }
    env.resolve(p)
}
