use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use manifatura_agent::{DirEntry, Env, ExecSpec, Process, Stat, Tool, ToolCx, ToolResult};
use manifatura_tools::Local;
use manifatura_tools::*;
use serde_json::{Value, json};

struct Ws {
    dir: PathBuf,
    env: Arc<dyn Env>,
}

impl Ws {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("mf-tools-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let env: Arc<dyn Env> = Arc::new(Local::new(&dir).unwrap());
        Self {
            dir: std::fs::canonicalize(&dir).unwrap(),
            env,
        }
    }

    fn file(&self, rel: &str, text: &str) {
        let p = self.dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    fn text(&self, rel: &str) -> String {
        std::fs::read_to_string(self.dir.join(rel)).unwrap()
    }

    async fn call(&self, tool: &Tool, input: Value) -> ToolResult {
        tool.call(input, ToolCx::detached("c").with_env(self.env.clone()))
            .await
    }
}

impl Drop for Ws {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn tools() -> (Tool, Tool) {
    let t = Arc::new(FileTracker::default());
    (read(t.clone()), edit(t))
}

#[tokio::test]
async fn read_numbers_lines_and_pages() {
    let ws = Ws::new("read");
    let (read, _) = tools();
    let body: String = (1..=5).map(|i| format!("l{i}\n")).collect();
    ws.file("a.txt", &body);
    let r = ws
        .call(&read, json!({"path": "a.txt", "offset": 2, "limit": 2}))
        .await;
    assert!(!r.is_error);
    assert!(
        r.output.starts_with("     2\tl2\n     3\tl3\n"),
        "{}",
        r.output
    );
    assert!(r.output.contains("Use offset=4 to continue"));
    // A workspace path without its leading slash still resolves.
    let stripped = ws
        .dir
        .join("a.txt")
        .to_str()
        .unwrap()
        .trim_start_matches('/')
        .to_string();
    assert!(!ws.call(&read, json!({"path": stripped})).await.is_error);
    let r = ws.call(&read, json!({"path": "missing.txt"})).await;
    assert!(r.is_error && r.output.contains("not found"));
    let r = ws.call(&read, json!({"path": "."})).await;
    assert!(r.is_error && r.output.contains("directory"));
}

#[tokio::test]
async fn edit_creates_files_and_refuses_stale_ones() {
    let ws = Ws::new("create");
    let (read, edit) = tools();
    let create = |path: &str, text: &str| json!({"path": path, "edits": [{"old_text": "", "new_text": text}]});
    let r = ws.call(&edit, create("new/dir/f.txt", "one")).await;
    assert!(
        !r.is_error && r.output.starts_with("Created"),
        "{}",
        r.output
    );
    assert_eq!(ws.text("new/dir/f.txt"), "one");
    // Our own edit counts as seen, so the next one needs no read.
    let r = ws
        .call(
            &edit,
            json!({"path": "new/dir/f.txt", "edits": [{"old_text": "one", "new_text": "two"}]}),
        )
        .await;
    assert!(!r.is_error, "{}", r.output);
    let r = ws.call(&edit, create("new/dir/f.txt", "x")).await;
    assert!(
        r.is_error && r.output.contains("already exists"),
        "{}",
        r.output
    );
    let r = ws
        .call(
            &edit,
            json!({"path": "missing.txt", "edits": [{"old_text": "a", "new_text": "b"}]}),
        )
        .await;
    assert!(
        r.is_error && r.output.contains("empty old_text"),
        "{}",
        r.output
    );

    ws.file("old.txt", "original");
    ws.call(&read, json!({"path": "old.txt"})).await;
    std::thread::sleep(Duration::from_millis(20));
    ws.file("old.txt", "changed by someone else");
    let r = ws
        .call(
            &edit,
            json!({"path": "old.txt", "edits": [{"old_text": "changed", "new_text": "x"}]}),
        )
        .await;
    assert!(
        r.is_error && r.output.contains("changed since"),
        "{}",
        r.output
    );
}

#[tokio::test]
async fn edit_applies_unique_non_overlapping_replacements() {
    let ws = Ws::new("edit");
    let (_, edit) = tools();
    ws.file("m.rs", "fn a() {}\nfn b() {}\nfn a2() {}\n");
    let r = ws
        .call(
            &edit,
            json!({"path": "m.rs", "edits": [
                {"old_text": "fn b() {}", "new_text": "fn b() { 1 }"},
                {"old_text": "fn a2", "new_text": "fn c"}
            ]}),
        )
        .await;
    assert!(!r.is_error, "{}", r.output);
    assert_eq!(ws.text("m.rs"), "fn a() {}\nfn b() { 1 }\nfn c() {}\n");
    assert!(r.output.contains("@@ line 2"));

    let r = ws
        .call(
            &edit,
            json!({"path": "m.rs", "edits": [{"old_text": "fn ", "new_text": "pub fn "}]}),
        )
        .await;
    assert!(r.is_error && r.output.contains("matches 3 times"));
    let r = ws
        .call(
            &edit,
            json!({"path": "m.rs", "edits": [{"old_text": "nope", "new_text": ""}]}),
        )
        .await;
    assert!(r.is_error && r.output.contains("not found"));
    let r = ws
        .call(
            &edit,
            json!({"path": "m.rs", "edits": [
                {"old_text": "fn b() { 1 }", "new_text": ""},
                {"old_text": "{ 1 }\nfn c", "new_text": ""}
            ]}),
        )
        .await;
    assert!(r.is_error && r.output.contains("overlap"), "{}", r.output);
    assert_eq!(ws.text("m.rs"), "fn a() {}\nfn b() { 1 }\nfn c() {}\n");
}

#[tokio::test]
async fn bash_reports_exit_code_and_output() {
    let ws = Ws::new("bash");
    let r = ws
        .call(
            &bash(),
            json!({"command": "echo out; echo err >&2; exit 2"}),
        )
        .await;
    assert!(r.is_error);
    assert_eq!(r.output, "out\n[stderr]\nerr\n[exit code 2]");
    let r = ws.call(&bash(), json!({"command": "pwd"})).await;
    assert!(!r.is_error);
    assert!(
        r.output.starts_with(ws.dir.to_str().unwrap()),
        "{}",
        r.output
    );
}

#[tokio::test]
async fn bash_keeps_the_end_of_long_output() {
    let ws = Ws::new("bash-long");
    let r = ws.call(&bash(), json!({"command": "seq 1 200000"})).await;
    assert!(!r.is_error);
    let first = r.output.lines().next().unwrap();
    assert!(
        first.starts_with(
            "[stdout truncated: showing its last 2000 lines, of 1.2 MB; full output in "
        ),
        "{first}"
    );
    assert!(
        r.output.ends_with("199999\n200000\n[exit code 0]"),
        "{}",
        r.output
    );
    assert_eq!(r.output.lines().nth(1), Some("198001"));
}

#[tokio::test]
async fn bash_backgrounds_long_commands() {
    let ws = Ws::new("bg");
    let r = ws
        .call(
            &bash(),
            json!({"command": "echo started; sleep 2; echo done", "wait_seconds": 1}),
        )
        .await;
    assert!(!r.is_error);
    assert!(
        r.output.contains("Still running") && r.output.contains("started"),
        "{}",
        r.output
    );
    let job = r
        .output
        .lines()
        .find_map(|l| l.strip_prefix("job_dir: "))
        .unwrap();
    let exit = Path::new(job).join("exit");
    for _ in 0..50 {
        if exit.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(std::fs::read_to_string(exit).unwrap(), "0");
    assert_eq!(
        std::fs::read_to_string(Path::new(job).join("stdout")).unwrap(),
        "started\ndone\n"
    );
}

#[tokio::test]
async fn grep_and_glob_respect_gitignore() {
    let ws = Ws::new("search");
    ws.file(".gitignore", "target/\n");
    ws.file("src/main.rs", "fn main() {\n    println!(\"hello\");\n}\n");
    ws.file("src/lib.rs", "pub fn hello() {}\n");
    ws.file("target/out.rs", "hello from build output\n");
    let r = ws.call(&grep(), json!({"pattern": "hello"})).await;
    assert!(r.output.contains("src/main.rs:2:"), "{}", r.output);
    assert!(r.output.contains("src/lib.rs:1:"));
    assert!(!r.output.contains("target/"));
    let r = ws
        .call(
            &grep(),
            json!({"pattern": "HELLO", "ignore_case": true, "glob": "*.rs", "path": "src/lib.rs"}),
        )
        .await;
    assert_eq!(r.output.trim(), "src/lib.rs:1: pub fn hello() {}");
    let r = ws.call(&grep(), json!({"pattern": "("})).await;
    assert!(r.is_error);

    let r = ws.call(&glob(), json!({"pattern": "**/*.rs"})).await;
    let mut found: Vec<&str> = r.output.lines().collect();
    found.sort();
    assert_eq!(found, ["src/lib.rs", "src/main.rs"]);
}

#[tokio::test]
async fn script_tool_gets_json_on_stdin() {
    let ws = Ws::new("script");
    let t = ScriptTool::new(
        "shout",
        "Upper-case text.",
        json!({"type": "object", "properties": {"text": {"type": "string"}}}),
        vec!["sh".into(), "-c".into(), "tr a-z A-Z".into()],
    )
    .build();
    let r = ws.call(&t, json!({"text": "hi"})).await;
    assert_eq!(r.output, "{\"TEXT\":\"HI\"}");
    let fail = ScriptTool::new(
        "f",
        "fails",
        json!({"type": "object"}),
        vec!["sh".into(), "-c".into(), "echo bad >&2; exit 4".into()],
    )
    .build();
    let r = ws.call(&fail, json!({})).await;
    assert!(
        r.is_error && r.output.starts_with("exit code 4\nbad"),
        "{}",
        r.output
    );
}

#[test]
fn by_name_rejects_unknown_tools() {
    assert_eq!(by_name(&["read", "bash"]).unwrap().len(), 2);
    assert!(by_name(&["rm"]).unwrap_err().contains("unknown tool `rm`"));
}

/// `Local`, but reporting itself as remote, so tools take their `Env`-only paths.
struct Remote(Local);

#[async_trait::async_trait]
impl Env for Remote {
    fn cwd(&self) -> &Path {
        self.0.cwd()
    }
    fn temp_dir(&self) -> &Path {
        self.0.temp_dir()
    }
    fn describe(&self) -> String {
        "remote (test)".into()
    }
    fn is_local(&self) -> bool {
        false
    }
    async fn exec(&self, spec: ExecSpec) -> std::io::Result<Box<dyn Process>> {
        self.0.exec(spec).await
    }
    async fn read(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        self.0.read(path).await
    }
    async fn write(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        self.0.write(path, bytes).await
    }
    async fn stat(&self, path: &Path) -> std::io::Result<Option<Stat>> {
        self.0.stat(path).await
    }
    async fn list(&self, path: &Path) -> std::io::Result<Vec<DirEntry>> {
        self.0.list(path).await
    }
    async fn remove(&self, path: &Path, recursive: bool) -> std::io::Result<()> {
        self.0.remove(path, recursive).await
    }
}

fn remote(name: &str) -> Ws {
    let mut ws = Ws::new(name);
    ws.env = Arc::new(Remote(Local::new(&ws.dir).unwrap()));
    ws
}

#[tokio::test]
async fn remote_glob_counts_every_match() {
    let ws = remote("remote-glob-many");
    for i in 0..503 {
        ws.file(&format!("f{i:03}.txt"), "");
    }
    let r = ws.call(&glob(), json!({"pattern": "*.txt"})).await;
    let lines: Vec<&str> = r.output.lines().collect();
    assert_eq!(lines.len(), 501, "{}", r.output);
    assert_eq!(lines[0], "f000.txt");
    assert_eq!(
        lines[500],
        "[3 more not shown; in path order; use a narrower pattern]"
    );

    // A short list is newest first, as with a local env.
    let ws = remote("remote-glob-few");
    ws.file("old.txt", "");
    std::thread::sleep(Duration::from_millis(20));
    ws.file("new.txt", "");
    let r = ws.call(&glob(), json!({"pattern": "*.txt"})).await;
    assert_eq!(r.output, "new.txt\nold.txt");
}

#[tokio::test]
async fn apply_patch_adds_updates_moves_and_deletes_or_changes_nothing() {
    let ws = Ws::new("patch");
    let t = Arc::new(FileTracker::default());
    let (read, edit, patch) = (read(t.clone()), edit(t.clone()), apply_patch(t));
    assert!(patch.spec().format.is_text());
    ws.file("src/a.rs", "fn a() {\n    old();\n}\n");
    ws.file("gone.txt", "bye\n");
    ws.file("keep.txt", "keep\n");

    let r = ws
        .call(
            &patch,
            json!({ "input": "*** Begin Patch\n\
                *** Add File: notes/new.md\n+# New\n\
                *** Update File: src/a.rs\n*** Move to: src/b.rs\n@@ fn a() {\n-    old();\n+    new();\n\
                *** Delete File: gone.txt\n\
                *** End Patch" }),
        )
        .await;
    assert!(!r.is_error, "{}", r.output);
    assert_eq!(
        r.output,
        "Success. Updated the following files:\nA notes/new.md\nM src/b.rs\nD gone.txt"
    );
    assert_eq!(ws.text("notes/new.md"), "# New\n");
    assert_eq!(ws.text("src/b.rs"), "fn a() {\n    new();\n}\n");
    assert!(!ws.dir.join("src/a.rs").exists() && !ws.dir.join("gone.txt").exists());

    // The second update fails, so the first is not written either.
    let r = ws
        .call(
            &patch,
            json!({ "input": "*** Begin Patch\n\
                *** Update File: keep.txt\n@@\n-keep\n+changed\n\
                *** Update File: src/b.rs\n@@\n-missing();\n+x();\n\
                *** End Patch" }),
        )
        .await;
    assert!(
        r.is_error
            && r.output
                .contains("failed to find the expected lines in src/b.rs")
    );
    assert_eq!(ws.text("keep.txt"), "keep\n");

    let r = ws
        .call(
            &patch,
            json!({ "input": "*** Begin Patch\n*** Update File: nope.rs\n@@\n-a\n+b\n*** End Patch" }),
        )
        .await;
    assert!(r.is_error && r.output.contains("file not found: nope.rs"));

    // A file the patch changed counts as seen: `edit` may change it without reading again.
    ws.call(&read, json!({"path": "src/b.rs"})).await;
    let r = ws
        .call(
            &patch,
            json!({ "input": "*** Begin Patch\n*** Update File: src/b.rs\n@@\n-    new();\n+    newer();\n*** End Patch" }),
        )
        .await;
    assert!(!r.is_error, "{}", r.output);
    let r = ws
        .call(
            &edit,
            json!({"path": "src/b.rs", "edits": [{"old_text": "newer", "new_text": "newest"}]}),
        )
        .await;
    assert!(!r.is_error, "{}", r.output);
}
