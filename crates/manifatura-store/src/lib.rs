//! Session stores: append-only JSONL and in-memory.
//!
//! [`JsonlStore`] keeps one file per session, `<dir>/<session-id>.jsonl`, one [`Entry`] per
//! line, starting with the header. Every append is handed to the OS, so it survives a killed
//! process; nothing calls `fsync` unless [`JsonlStore::sync`] asks for it, so sessions do not
//! wear flash storage.
//!
//! ```sh
//! jq -c 'select(.type == "message") | .message' ~/.local/share/app/sessions/<id>.jsonl
//! ```

#![warn(missing_debug_implementations, missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
pub use manifatura_agent::{Entry, MemoryStore, Record, SessionId, SessionMeta, Store, StoreError};

/// Open file handles kept for appends. Beyond this, idle handles are closed, so an app with
/// many sessions does not run out of descriptors.
const MAX_OPEN: usize = 32;

/// Sessions as JSON lines, one file per session in a directory.
pub struct JsonlStore {
    dir: PathBuf,
    sync: bool,
    open: Arc<Mutex<HashMap<SessionId, BufWriter<File>>>>,
}

impl std::fmt::Debug for JsonlStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JsonlStore")
            .field("dir", &self.dir)
            .field("sync", &self.sync)
            .finish_non_exhaustive()
    }
}

impl JsonlStore {
    /// Uses (and creates) `dir`.
    pub fn new(dir: impl Into<PathBuf>) -> std::io::Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        Ok(Self {
            dir,
            sync: false,
            open: Arc::default(),
        })
    }

    /// Calls `fsync` after every append, so appends also survive a power cut. Off by default.
    pub fn sync(mut self, on: bool) -> Self {
        self.sync = on;
        self
    }

    /// The directory the session files are in.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The file holding `session`.
    pub fn path(&self, session: &SessionId) -> PathBuf {
        self.dir.join(format!("{session}.jsonl"))
    }

    fn checked_path(&self, session: &SessionId) -> Result<PathBuf, StoreError> {
        if !session.is_path_safe() {
            return Err(StoreError::new(format!(
                "session id {session:?} is not usable as a file name"
            )));
        }
        Ok(self.path(session))
    }

    async fn blocking<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut HashMap<SessionId, BufWriter<File>>) -> Result<T, StoreError>
        + Send
        + 'static,
    ) -> Result<T, StoreError> {
        let open = self.open.clone();
        tokio::task::spawn_blocking(move || f(&mut open.lock().unwrap()))
            .await
            .map_err(|e| StoreError::with_source("the store task failed", e))?
    }
}

#[async_trait]
impl Store for JsonlStore {
    async fn append(&self, session: &SessionId, entries: &[Entry]) -> Result<(), StoreError> {
        let path = self.checked_path(session)?;
        let mut buf = Vec::new();
        for e in entries {
            serde_json::to_writer(&mut buf, e)?;
            buf.push(b'\n');
        }
        let session = session.clone();
        let sync = self.sync;
        self.blocking(move |open| {
            if !open.contains_key(&session) {
                if open.len() >= MAX_OPEN {
                    open.clear();
                }
                let file = OpenOptions::new()
                    .create(true)
                    .read(true)
                    .append(true)
                    .open(&path)?;
                end_with_newline(&file)?;
                open.insert(session.clone(), BufWriter::with_capacity(16 * 1024, file));
            }
            let w = open.get_mut(&session).unwrap();
            w.write_all(&buf)?;
            w.flush()?;
            if sync {
                w.get_ref().sync_data()?;
            }
            Ok(())
        })
        .await
    }

    async fn load(&self, session: &SessionId) -> Result<Vec<Entry>, StoreError> {
        let path = self.checked_path(session)?;
        let session = session.clone();
        self.blocking(move |open| {
            if let Some(w) = open.get_mut(&session) {
                w.flush()?;
            }
            read_entries(&path)
        })
        .await
    }

    async fn list(&self) -> Result<Vec<SessionMeta>, StoreError> {
        let dir = self.dir.clone();
        self.blocking(move |open| {
            for w in open.values_mut() {
                w.flush()?;
            }
            let mut out = Vec::new();
            for item in std::fs::read_dir(&dir)? {
                let path = item?.path();
                let Some(id) = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .and_then(|n| n.strip_suffix(".jsonl"))
                else {
                    continue;
                };
                let mut meta =
                    manifatura_agent::session_meta(SessionId::from(id), &read_header(&path)?);
                meta.updated = modified_ms(&path).unwrap_or(0);
                out.push(meta);
            }
            out.sort_by(|a, b| a.id.cmp(&b.id));
            Ok(out)
        })
        .await
    }
}

/// Reads a session file. A torn final line (a crash mid-append) is ignored; corruption
/// anywhere else is an error.
fn read_entries(path: &Path) -> Result<Vec<Entry>, StoreError> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut lines = BufReader::new(file).lines().peekable();
    let mut out = Vec::new();
    let mut n = 0;
    while let Some(line) = lines.next() {
        let line = line?;
        n += 1;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str(&line) {
            Ok(e) => out.push(e),
            Err(_) if lines.peek().is_none() => break,
            Err(e) => {
                return Err(StoreError::new(format!("{}:{n}: {e}", path.display())));
            }
        }
    }
    Ok(out)
}

/// Makes a file left by a crash mid-append safe to append to. Every append ends in a newline,
/// so a file that does not is torn: its last line is dropped if it does not parse (as
/// [`read_entries`] drops it) and finished with a newline if it does. Without this, the next
/// entry would be written onto the torn line, and the file could no longer be read.
fn end_with_newline(file: &File) -> std::io::Result<()> {
    use std::io::{Read, Seek, SeekFrom};
    let len = file.metadata()?.len();
    let mut reader = file;
    if len == 0 {
        return Ok(());
    }
    let mut last = [0u8; 1];
    reader.seek(SeekFrom::Start(len - 1))?;
    reader.read_exact(&mut last)?;
    if last[0] == b'\n' {
        return Ok(());
    }
    // Find where the torn line starts, reading backwards in chunks.
    let mut start = len;
    let mut chunk = vec![0u8; 8192];
    let line_start = loop {
        if start == 0 {
            break 0;
        }
        let n = chunk.len().min(start as usize);
        start -= n as u64;
        reader.seek(SeekFrom::Start(start))?;
        reader.read_exact(&mut chunk[..n])?;
        if let Some(i) = chunk[..n].iter().rposition(|&b| b == b'\n') {
            break start + i as u64 + 1;
        }
    };
    let mut line = Vec::new();
    reader.seek(SeekFrom::Start(line_start))?;
    reader.read_to_end(&mut line)?;
    if serde_json::from_slice::<Entry>(&line).is_ok() {
        (&*file).write_all(b"\n")
    } else {
        file.set_len(line_start)
    }
}

fn read_header(path: &Path) -> Result<Vec<Entry>, StoreError> {
    let mut line = String::new();
    BufReader::new(File::open(path)?).read_line(&mut line)?;
    Ok(serde_json::from_str(&line).into_iter().collect())
}

fn modified_ms(path: &Path) -> Option<u64> {
    let t = std::fs::metadata(path).ok()?.modified().ok()?;
    Some(t.duration_since(std::time::UNIX_EPOCH).ok()?.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use manifatura_agent::Message;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mf-store-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn entries() -> Vec<Entry> {
        let h = Entry::new(
            None,
            Record::Header {
                v: 1,
                agent: "a".into(),
            },
        );
        let m = Entry::new(
            Some(h.id.clone()),
            Record::Message {
                message: Message::user("hi"),
            },
        );
        let l = Entry::new(
            None,
            Record::Label {
                name: "named".into(),
            },
        );
        vec![h, m, l]
    }

    #[tokio::test]
    async fn round_trip_with_and_without_sync() {
        let dir = tmp("rt");
        let store = JsonlStore::new(&dir).unwrap();
        let id = SessionId::from("one");
        let e = entries();
        store.append(&id, &e[..2]).await.unwrap();
        // Every append reaches the file, without waiting for a load or a sync.
        let on_disk = std::fs::read_to_string(store.path(&id)).unwrap();
        assert_eq!(on_disk.lines().count(), 2);
        JsonlStore::new(&dir)
            .unwrap()
            .sync(true)
            .append(&id, &e[2..])
            .await
            .unwrap();

        // A fresh store (new process) reads the same entries.
        let again = JsonlStore::new(&dir).unwrap();
        assert_eq!(again.load(&id).await.unwrap(), e);
        let list = again.list().await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].agent.as_deref(), Some("a"));

        let text = std::fs::read_to_string(store.path(&id)).unwrap();
        let first: serde_json::Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        assert_eq!(first["type"], "header");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn torn_last_line_is_ignored() {
        let dir = tmp("torn");
        let store = JsonlStore::new(&dir).unwrap();
        let id = SessionId::from("t");
        store.append(&id, &entries()).await.unwrap();
        let mut f = OpenOptions::new()
            .append(true)
            .open(store.path(&id))
            .unwrap();
        f.write_all(b"{\"id\":\"x\",\"ts\":1,\"type\":\"mess")
            .unwrap();
        drop(f);
        assert_eq!(
            JsonlStore::new(&dir)
                .unwrap()
                .load(&id)
                .await
                .unwrap()
                .len(),
            3
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn appending_after_a_torn_line_keeps_the_file_readable() {
        let dir = tmp("torn-append");
        let id = SessionId::from("t");
        let e = entries();
        let store = JsonlStore::new(&dir).unwrap();
        store.append(&id, &e[..2]).await.unwrap();
        drop(store);
        let mut f = OpenOptions::new()
            .append(true)
            .open(dir.join("t.jsonl"))
            .unwrap();
        f.write_all(b"{\"id\":\"x\",\"ts\":1,\"type\":\"mess")
            .unwrap();
        drop(f);

        // A new process appends: the torn line goes, and the file still loads.
        let store = JsonlStore::new(&dir).unwrap();
        store.append(&id, &e[2..]).await.unwrap();
        assert_eq!(JsonlStore::new(&dir).unwrap().load(&id).await.unwrap(), e);

        // A last entry that was written whole but lost its newline is kept.
        let text = std::fs::read_to_string(store.path(&id)).unwrap();
        std::fs::write(store.path(&id), text.trim_end()).unwrap();
        let store = JsonlStore::new(&dir).unwrap();
        store.append(&id, &e[..1]).await.unwrap();
        let loaded = store.load(&id).await.unwrap();
        assert_eq!(loaded.len(), 4);
        assert_eq!(loaded[2], e[2]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn rejects_unsafe_ids() {
        let store = JsonlStore::new(tmp("ids")).unwrap();
        for bad in ["../x", "a/b", "", ".hidden"] {
            assert!(store.load(&SessionId::from(bad)).await.is_err(), "{bad}");
        }
    }
}
