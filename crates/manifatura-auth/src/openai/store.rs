//! The credential file: one per app, owner-only, written atomically, with a lock file that
//! serialises refreshes across processes.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Everything kept between runs.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct Saved {
    /// This installation's host id (`urn:uuid:…`), made once and never changed.
    pub host_id: String,
    /// The client id issued at the first registration, reused for every later sign-in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// The signed-in session, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<Session>,
}

/// A signed-in account's tokens.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Session {
    pub issuer: String,
    pub subject: String,
    #[serde(default)]
    pub email: Option<String>,
    pub id_token: String,
    pub access_token: String,
    pub refresh_token: String,
    pub token_type: String,
    /// Unix seconds.
    pub expires_at: u64,
    pub scopes: Vec<String>,
    /// Unix seconds.
    pub saved_at: u64,
}

impl Session {
    pub fn plan_usage(&self) -> bool {
        self.scopes.iter().any(|s| s == super::PLAN_SCOPE)
    }
}

#[derive(Debug)]
pub(crate) struct Store {
    path: PathBuf,
}

impl Store {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn read(&self) -> io::Result<Option<Saved>> {
        match std::fs::read(&self.path) {
            Ok(b) => serde_json::from_slice(&b)
                .map(Some)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// The saved state, made (with a new host id) when there is none.
    pub fn load(&self) -> io::Result<Saved> {
        if let Some(s) = self.read()? {
            return Ok(s);
        }
        let s = Saved {
            host_id: super::oauth::new_host_id(),
            ..Default::default()
        };
        self.write(&s)?;
        Ok(s)
    }

    pub fn write(&self, s: &Saved) -> io::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(s).map_err(io::Error::other)?;
        {
            let mut o = std::fs::OpenOptions::new();
            o.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                o.mode(0o600);
            }
            use std::io::Write;
            let mut f = o.open(&tmp)?;
            f.write_all(&bytes)?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, &self.path)
    }

    /// Takes the cross-process lock; it is released when the returned file is dropped.
    pub async fn lock(&self) -> io::Result<std::fs::File> {
        let path = self.path.with_extension("lock");
        tokio::task::spawn_blocking(move || {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let f = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(&path)?;
            f.lock()?;
            Ok(f)
        })
        .await
        .map_err(io::Error::other)?
    }
}
