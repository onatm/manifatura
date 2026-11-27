use std::path::{Path, PathBuf};
use std::sync::Arc;

use manifatura_agent::testing::ScriptedProvider;
use manifatura_agent::{Agent, Env, Record, Role, Runtime, SessionId, Tool};
use manifatura_snapshots::{Change, Snapshots, before};
use manifatura_tools::Local;
use serde_json::json;

struct Ws {
    root: PathBuf,
    dir: PathBuf,
    env: Arc<Local>,
    snapshots: Snapshots,
}

impl Ws {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("mf-snap-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.join("ws");
        std::fs::create_dir_all(&dir).unwrap();
        let env = Arc::new(Local::new(&dir).unwrap());
        let dir = std::fs::canonicalize(&dir).unwrap();
        Self {
            snapshots: Snapshots::new(root.join("snapshots.git")),
            root,
            dir,
            env,
        }
    }

    fn write(&self, path: &str, text: &str) {
        let p = self.dir.join(path);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    fn read(&self, path: &str) -> Option<String> {
        std::fs::read_to_string(self.dir.join(path)).ok()
    }
}

impl Drop for Ws {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_restore_undoes_changes_additions_and_deletions() {
    let ws = Ws::new("restore");
    ws.write(".gitignore", "target/\n");
    ws.write("src/a.rs", "fn a() {}\n");
    ws.write("gone.txt", "bye\n");
    ws.write("target/out", "built\n");
    ws.write(".git/HEAD", "ref: refs/heads/main\n");
    let env: &dyn Env = ws.env.as_ref();
    let tree = ws.snapshots.take(env).await.unwrap();

    ws.write("src/a.rs", "fn a() { changed(); }\n");
    ws.write("src/new.rs", "fn new() {}\n");
    std::fs::remove_file(ws.dir.join("gone.txt")).unwrap();
    ws.write("target/out", "rebuilt\n");
    ws.write(".git/HEAD", "ref: refs/heads/other\n");

    let mut changes = ws.snapshots.changes(env, &tree).await.unwrap();
    changes.sort_by(|a, b| a.1.cmp(&b.1));
    assert_eq!(
        changes,
        [
            (Change::Deleted, "gone.txt".to_string()),
            (Change::Modified, "src/a.rs".to_string()),
            (Change::Added, "src/new.rs".to_string()),
        ],
        "ignored files and the project's .git are not snapshotted"
    );

    let restored = ws.snapshots.restore(env, &tree).await.unwrap();
    assert_eq!(restored.removed, ["src/new.rs"]);
    assert_eq!(restored.restored.len(), 2);
    assert_eq!(ws.read("src/a.rs").as_deref(), Some("fn a() {}\n"));
    assert_eq!(ws.read("gone.txt").as_deref(), Some("bye\n"));
    assert_eq!(ws.read("src/new.rs"), None);
    assert_eq!(
        ws.read("target/out").as_deref(),
        Some("rebuilt\n"),
        "ignored: left alone"
    );
    assert_eq!(
        ws.read(".git/HEAD").as_deref(),
        Some("ref: refs/heads/other\n"),
        "the project's own .git"
    );
    assert!(ws.snapshots.changes(env, &tree).await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn each_turn_records_the_files_it_started_from() {
    let ws = Ws::new("hook");
    ws.write("notes.md", "first\n");
    let write = Tool::new("write", "Writes notes.md.").run_json(
        json!({"type": "object"}),
        |i, cx| async move {
            let text = i["text"].as_str().unwrap_or_default().to_string();
            cx.env()
                .write(Path::new("notes.md"), text.as_bytes())
                .await?;
            Ok("written")
        },
    );
    let p = ScriptedProvider::new()
        .call("write", json!({"text": "second\n"}))
        .text("done")
        .call("write", json!({"text": "third\n"}))
        .text("done");
    let rt = Runtime::builder()
        .provider(p)
        .env(ws.env.clone())
        .model("m")
        .build();
    let agent = Agent::new("t").tool(&write).hooks(ws.snapshots.hooks());
    let session = rt.session(&agent, SessionId::new());
    session.prompt("one").await.unwrap().wait().await.unwrap();
    session.prompt("two").await.unwrap().wait().await.unwrap();
    assert_eq!(ws.read("notes.md").as_deref(), Some("third\n"));

    let path = session.path().await.unwrap();
    let users: Vec<String> = path
        .iter()
        .filter(|e| matches!(&e.record, Record::Message { message } if message.role == Role::User))
        .map(|e| e.id.clone())
        .collect();
    assert_eq!(users.len(), 2);
    // Back to before "two": the second write is undone.
    let tree = before(&path, &users[1]).expect("a snapshot for the second turn");
    ws.snapshots.restore(ws.env.as_ref(), &tree).await.unwrap();
    assert_eq!(ws.read("notes.md").as_deref(), Some("second\n"));
    // And before "one".
    let tree = before(&path, &users[0]).unwrap();
    ws.snapshots.restore(ws.env.as_ref(), &tree).await.unwrap();
    assert_eq!(ws.read("notes.md").as_deref(), Some("first\n"));
    assert_eq!(before(&path, "no-such-entry"), None);
}
