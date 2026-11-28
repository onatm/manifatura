//! `manifatura-skills` against an in-memory env: where skills are found, which wins, what the model
//! is given, and that compaction keeps them.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use manifatura_agent::testing::ScriptedProvider;
use manifatura_agent::*;
use manifatura_kit::compaction;
use manifatura_kit::context::SystemPrompt;
use manifatura_skills::{SKILL_TOOL, Skill, SkillSet, Skills, Source, SystemPromptExt, Visibility};
use serde_json::json;

/// Files by absolute path; every ancestor of a file is a directory. A path ending in `/` is an
/// empty directory.
struct MemEnv {
    cwd: PathBuf,
    files: BTreeMap<PathBuf, String>,
    dirs: Vec<PathBuf>,
}

impl MemEnv {
    fn new(cwd: &str, entries: &[(&str, &str)]) -> Self {
        let (mut files, mut dirs) = (BTreeMap::new(), Vec::new());
        for (p, text) in entries {
            let path = PathBuf::from(p.trim_end_matches('/'));
            dirs.extend(path.ancestors().skip(1).map(Path::to_path_buf));
            match p.ends_with('/') {
                true => dirs.push(path),
                false => {
                    files.insert(path, text.to_string());
                }
            }
        }
        dirs.sort();
        dirs.dedup();
        Self {
            cwd: cwd.into(),
            files,
            dirs,
        }
    }
}

#[async_trait]
impl Env for MemEnv {
    fn cwd(&self) -> &Path {
        &self.cwd
    }
    fn temp_dir(&self) -> &Path {
        Path::new("/tmp")
    }
    fn describe(&self) -> String {
        "memory".into()
    }
    async fn exec(&self, _: ExecSpec) -> io::Result<Box<dyn Process>> {
        Err(io::Error::other("no processes"))
    }
    async fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        self.files
            .get(path)
            .map(|t| t.clone().into_bytes())
            .ok_or(io::ErrorKind::NotFound.into())
    }
    async fn write(&self, _: &Path, _: &[u8]) -> io::Result<()> {
        Err(io::Error::other("read-only"))
    }
    async fn stat(&self, path: &Path) -> io::Result<Option<Stat>> {
        let kind = match (
            self.files.contains_key(path),
            self.dirs.iter().any(|d| d == path),
        ) {
            (true, _) => FileKind::File,
            (_, true) => FileKind::Dir,
            _ => return Ok(None),
        };
        Ok(Some(Stat {
            kind,
            len: 0,
            modified: 0,
        }))
    }
    async fn list(&self, path: &Path) -> io::Result<Vec<DirEntry>> {
        if !self.dirs.iter().any(|d| d == path) {
            return Err(io::ErrorKind::NotFound.into());
        }
        let child = |p: &PathBuf| {
            (p.parent() == Some(path))
                .then(|| p.file_name().unwrap().to_string_lossy().into_owned())
        };
        let mut out: Vec<DirEntry> = self
            .files
            .keys()
            .filter_map(child)
            .map(|name| DirEntry {
                name,
                kind: FileKind::File,
            })
            .collect();
        out.extend(self.dirs.iter().filter_map(child).map(|name| DirEntry {
            name,
            kind: FileKind::Dir,
        }));
        Ok(out)
    }
    async fn remove(&self, _: &Path, _: bool) -> io::Result<()> {
        Err(io::Error::other("read-only"))
    }
}

fn skill_md(name: &str, description: &str, body: &str) -> String {
    format!("---\nname: {name}\ndescription: {description}\n---\n{body}\n")
}

fn builtin(name: &str, description: &str) -> Skill {
    Skill::parse(&skill_md(name, description, "Built in.")).unwrap()
}

#[tokio::test]
async fn project_skills_hide_user_and_builtin_ones_and_deeper_dirs_win() {
    let env = MemEnv::new(
        "/r/app",
        &[
            ("/r/.git/", ""),
            (
                "/home/.agents/skills/tour/SKILL.md",
                &skill_md("tour", "User tour.", "u"),
            ),
            (
                "/home/.agents/skills/notes/SKILL.md",
                &skill_md("notes", "User notes.", "n"),
            ),
            (
                "/r/.agents/skills/tour/SKILL.md",
                &skill_md("tour", "Root tour.", "r"),
            ),
            (
                "/r/app/.agents/skills/tour/SKILL.md",
                &skill_md("tour", "App tour.", "a"),
            ),
            ("/r/app/.agents/skills/empty/", ""),
            (
                "/elsewhere/.agents/skills/x/SKILL.md",
                &skill_md("x", "Not searched.", ""),
            ),
        ],
    );
    let set = Skills::agents("/home")
        .builtin(builtin("tour", "Built-in tour."))
        .builtin(builtin("plan", "Built-in plan."))
        .load(&env)
        .await;
    assert_eq!(set.names(), ["notes", "plan", "tour"]);
    let tour = set.get("tour").unwrap();
    assert_eq!(
        (tour.description.as_str(), tour.source),
        ("App tour.", Source::Project)
    );
    assert_eq!(
        tour.path().unwrap(),
        Path::new("/r/app/.agents/skills/tour/SKILL.md")
    );
    assert_eq!(set.get("plan").unwrap().source, Source::Builtin);
    assert_eq!(set.get("notes").unwrap().source, Source::User);
    // Each of the three hidden tours is reported.
    assert_eq!(
        set.warnings()
            .iter()
            .filter(|w| w.contains("hides"))
            .count(),
        3,
        "{:?}",
        set.warnings()
    );
}

#[tokio::test]
async fn broken_skills_are_skipped_and_odd_names_loaded_with_a_warning() {
    let env = MemEnv::new(
        "/p",
        &[
            ("/p/s/good/SKILL.md", &skill_md("good", "Fine.", "")),
            ("/p/s/nodesc/SKILL.md", "---\nname: nodesc\n---\nbody"),
            (
                "/p/s/bad-yaml/SKILL.md",
                "---\nname: [unclosed\ndescription: d\n---\n",
            ),
            (
                "/p/s/renamed/SKILL.md",
                &skill_md("other-name", "Mismatched.", ""),
            ),
            ("/p/s/.hidden/SKILL.md", &skill_md("hidden", "Hidden.", "")),
            ("/p/s/README.md", "not a skill"),
        ],
    );
    let set = Skills::new().dir("s").load(&env).await;
    assert_eq!(set.names(), ["good", "other-name"]);
    let warnings = set.warnings().join("\n");
    assert!(
        warnings.contains("nodesc/SKILL.md: the frontmatter has no description"),
        "{warnings}"
    );
    assert!(
        warnings.contains("bad-yaml/SKILL.md: could not read the frontmatter"),
        "{warnings}"
    );
    assert!(
        warnings.contains("does not match the directory \"renamed\""),
        "{warnings}"
    );
}

#[tokio::test]
async fn activation_gives_the_body_the_directory_and_the_other_files() {
    let env = MemEnv::new(
        "/p",
        &[
            (
                "/p/.rustling/skills/pdf/SKILL.md",
                &skill_md("pdf", "PDFs.", "# PDF\n\nSee references/forms.md."),
            ),
            ("/p/.rustling/skills/pdf/references/forms.md", "forms"),
            ("/p/.rustling/skills/pdf/scripts/fill.py", ""),
            ("/p/.rustling/skills/pdf/scripts/deep/x.py", ""),
        ],
    );
    let set = Skills::new()
        .project(".rustling/skills")
        .builtin(builtin("tour", "Tour."))
        .load(&env)
        .await;
    let text = set.activation("pdf", &env).await.unwrap();
    assert_eq!(
        text,
        "<skill_content name=\"pdf\">\n# PDF\n\nSee references/forms.md.\n\nSkill directory: \
         /p/.rustling/skills/pdf\nRelative paths in this skill are relative to the skill \
         directory.\n<skill_resources>\n<file>references/forms.md</file>\n\
         <file>scripts/fill.py</file>\n</skill_resources>\n</skill_content>"
    );
    assert_eq!(
        set.activation("tour", &env).await.unwrap(),
        "<skill_content name=\"tour\">\nBuilt in.\n</skill_content>"
    );
    assert_eq!(set.activation("nope", &env).await, None);
}

#[tokio::test]
async fn the_tool_offers_only_known_names_and_the_prompt_lists_them() {
    let set = SkillSet::of([
        builtin("tour", "Explain\n  a codebase."),
        builtin("plan", "Plan."),
    ]);
    let tool = set.tool().unwrap();
    assert_eq!(tool.name(), SKILL_TOOL);
    assert_eq!(
        tool.spec().input_schema["properties"]["name"]["enum"],
        json!(["plan", "tour"])
    );
    let cx = ToolCx::detached("1");
    let ok = tool.call(json!({"name": "tour"}), cx.clone()).await;
    assert!(!ok.is_error && ok.output.contains("Built in."), "{ok:?}");
    let missing = tool.call(json!({"name": "x"}), cx).await;
    assert!(
        missing.is_error && missing.output.contains("plan, tour"),
        "{missing:?}"
    );

    let prompt = SystemPrompt::new("Base.")
        .tools(&[tool])
        .skills(&set)
        .section("Extra.")
        .build();
    assert_eq!(
        prompt,
        "Base.\n\nGuidelines:\n- When a task matches a skill in the Skills section, load it \
         with `skill` before you start, then follow its instructions.\n\n# Skills\n\nLoad \
         these with the skill tool:\n\n- plan: Plan.\n- tour: Explain a codebase.\n\nExtra.\n"
    );
    assert_eq!(
        SystemPrompt::new("Base.")
            .skills(&SkillSet::default())
            .build(),
        "Base.\n"
    );
}

#[tokio::test]
async fn visibility_decides_what_the_model_and_the_user_see() {
    let set = SkillSet::of([
        builtin("a", "All of it."),
        builtin("b", "Name only."),
        builtin("c", "The user's."),
        builtin("d", "Hidden."),
    ])
    .with_visibility(BTreeMap::from([
        ("b".to_string(), Visibility::NameOnly),
        ("c".to_string(), Visibility::UserOnly),
        ("d".to_string(), Visibility::Off),
    ]));
    assert_eq!(
        set.section(),
        "# Skills\n\nLoad these with the skill tool:\n\n- a: All of it.\n- b\n"
    );
    let tool = set.tool().unwrap();
    assert_eq!(
        tool.spec().input_schema["properties"]["name"]["enum"],
        json!(["a", "b"])
    );
    let refused = tool.call(json!({"name": "c"}), ToolCx::detached("1")).await;
    assert!(
        refused.is_error && refused.output.contains("the skills are: a, b"),
        "{refused:?}"
    );
    let user: Vec<&str> = set.for_user().map(|s| s.name.as_str()).collect();
    assert_eq!(user, ["a", "b", "c"]);
    assert_eq!(Visibility::Off.next(), Visibility::On);
    assert!(SkillSet::catalog_tokens(set.get("a").unwrap()) > 0);
    // Nothing the model may load: no tool, no section.
    let hidden = SkillSet::of([builtin("a", "x")])
        .with_visibility(BTreeMap::from([("a".to_string(), Visibility::UserOnly)]));
    assert!(hidden.tool().is_none() && hidden.section().is_empty());
}

#[tokio::test]
async fn trimming_keeps_skill_instructions_whole() {
    let long = "z".repeat(3_000);
    let set = SkillSet::of([Skill::parse(&skill_md("big", "Big.", &long)).unwrap()]);
    let big = Tool::new("big", "Returns a lot.").run_json(json!({"type":"object"}), |_, _| async {
        Ok("y".repeat(3_000))
    });
    let p = ScriptedProvider::new()
        .call(SKILL_TOOL, json!({"name": "big"}))
        .call("big", json!({}))
        .call("big", json!({}))
        .text("done");
    let rt = Runtime::builder().provider(p.clone()).build();
    let agent = Agent::new("x")
        .tools(set.tool())
        .tool(big)
        .hooks(compaction::trim_results(500, 600));
    let s = rt.session(&agent, SessionId::new());
    s.prompt("go").await.unwrap().wait().await.unwrap();
    let outputs: Vec<String> = p
        .requests()
        .last()
        .unwrap()
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|c| match c {
            Content::ToolResult { output, .. } => Some(output.clone()),
            _ => None,
        })
        .collect();
    assert!(outputs[0].contains(&long), "the skill was trimmed");
    assert!(
        outputs[1].contains("[tool output trimmed"),
        "{}",
        &outputs[1][..100]
    );
}
