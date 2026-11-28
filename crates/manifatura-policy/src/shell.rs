//! Shell command lines, split into the simple commands they run, with tree-sitter-bash.

use tree_sitter::{Node, Parser};

/// What a command line runs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Parsed {
    /// Every simple command, in order, including those inside pipelines, lists, subshells,
    /// conditionals and loops; wrappers (`timeout`, `nice`, `nohup`, `time`, leading
    /// `VAR=value`) removed.
    pub commands: Vec<Command>,
    /// Part of the line cannot be known before it runs: a command substitution (`$(…)`,
    /// backticks), a process substitution, a function definition, a command name that is a
    /// variable, or a parse error. Such a line is never allowed by rules or as read-only.
    pub opaque: bool,
}

/// One simple command.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Command {
    /// The program and its arguments, unquoted. A variable stays as written (`$HOME`).
    pub words: Vec<String>,
    /// Its output is redirected into a file (`>`, `>>`), other than `/dev/null`.
    pub writes: bool,
}

/// Splits `line` into the simple commands it runs.
pub fn parse(line: &str) -> Parsed {
    let mut parser = Parser::new();
    let mut out = Parsed::default();
    if parser
        .set_language(&tree_sitter_bash::LANGUAGE.into())
        .is_err()
    {
        out.opaque = true;
        return out;
    }
    let Some(tree) = parser.parse(line, None) else {
        out.opaque = true;
        return out;
    };
    let root = tree.root_node();
    if root.has_error() {
        out.opaque = true;
    }
    walk(root, line, &mut out, false);
    out
}

fn text<'a>(node: Node, src: &'a str) -> &'a str {
    node.utf8_text(src.as_bytes()).unwrap_or_default()
}

fn walk(node: Node, src: &str, out: &mut Parsed, writes: bool) {
    match node.kind() {
        "command" => command(node, src, out, writes),
        "redirected_statement" => {
            let mut w = writes;
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if child.kind() == "file_redirect" && redirect_writes(child, src) {
                    w = true;
                }
            }
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                walk(child, src, out, w);
            }
        }
        "command_substitution" | "process_substitution" | "function_definition" | "ERROR" => {
            out.opaque = true;
            children(node, src, out, writes);
        }
        "declaration_command" | "unset_command" => {
            // `export A=b`, `local x`, `unset y`: shell state only.
            let words = text(node, src)
                .split_whitespace()
                .map(str::to_string)
                .collect();
            out.commands.push(Command { words, writes });
            children(node, src, out, writes);
        }
        "comment" => {}
        _ => children(node, src, out, writes),
    }
}

fn children(node: Node, src: &str, out: &mut Parsed, writes: bool) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk(child, src, out, writes);
    }
}

/// `>`, `>>`, `&>`, `>|` into a file other than `/dev/null`; not `2>&1`.
fn redirect_writes(node: Node, src: &str) -> bool {
    let t = text(node, src);
    if !t.contains('>') || t.contains(">&") {
        return false;
    }
    let dest = node
        .child_by_field_name("destination")
        .map(|d| text(d, src).trim_matches(|c| c == '"' || c == '\''))
        .unwrap_or_default();
    dest != "/dev/null"
}

fn command(node: Node, src: &str, out: &mut Parsed, writes: bool) {
    let mut words = Vec::new();
    let mut writes = writes;
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "variable_assignment" => children(child, src, out, writes),
            "file_redirect" => {
                if redirect_writes(child, src) {
                    writes = true;
                }
                children(child, src, out, writes);
            }
            "herestring_redirect" | "heredoc_redirect" => children(child, src, out, writes),
            "command_name" => {
                let name = child.named_child(0).unwrap_or(child);
                if matches!(name.kind(), "simple_expansion" | "expansion") {
                    out.opaque = true;
                }
                match literal(name, src) {
                    Some(w) => words.push(w),
                    None => out.opaque = true,
                }
                children(child, src, out, writes);
            }
            _ => {
                match literal(child, src) {
                    Some(w) => words.push(w),
                    None => out.opaque = true,
                }
                children(child, src, out, writes);
            }
        }
    }
    let words = unwrap(words);
    if !words.is_empty() {
        out.commands.push(Command { words, writes });
    }
}

fn has_substitution(node: Node) -> bool {
    if matches!(node.kind(), "command_substitution" | "process_substitution") {
        return true;
    }
    let mut cursor = node.walk();
    node.named_children(&mut cursor).any(has_substitution)
}

/// A word's value with quotes removed, or `None` when it holds a substitution.
fn literal(node: Node, src: &str) -> Option<String> {
    if has_substitution(node) {
        return None;
    }
    let t = text(node, src);
    Some(match node.kind() {
        "raw_string" => t.trim_matches('\'').to_string(),
        "string" => t.trim_matches('"').to_string(),
        "word" => unescape(t),
        "concatenation" => t
            .chars()
            .filter(|c| *c != '"' && *c != '\'')
            .collect::<String>(),
        _ => t.to_string(),
    })
}

fn unescape(word: &str) -> String {
    let mut out = String::with_capacity(word.len());
    let mut chars = word.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(n) = chars.next() {
                out.push(n);
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// The command without the wrappers that only change how it runs, and with `git`'s global
/// options (`-C dir`, `-c key=value`, `--no-pager`) removed, so rules see the real command.
fn unwrap(mut w: Vec<String>) -> Vec<String> {
    loop {
        match w.first().map(String::as_str) {
            Some("nohup" | "time") => {
                w.remove(0);
            }
            Some("command" | "builtin") if w.get(1).is_some_and(|a| !a.starts_with('-')) => {
                w.remove(0);
            }
            Some("timeout") => {
                w.remove(0);
                while w.first().is_some_and(|a| a.starts_with('-')) {
                    let opt = w.remove(0);
                    if matches!(opt.as_str(), "-s" | "-k" | "--signal" | "--kill-after")
                        && !w.is_empty()
                    {
                        w.remove(0);
                    }
                }
                if !w.is_empty() {
                    w.remove(0); // the duration
                }
            }
            Some("nice") => {
                w.remove(0);
                if w.first().is_some_and(|a| a == "-n") {
                    w.drain(..2.min(w.len()));
                } else if w.first().is_some_and(|a| a.starts_with('-')) {
                    w.remove(0);
                }
            }
            _ => break,
        }
    }
    if w.first().is_some_and(|h| h == "git") {
        let i = 1;
        while i < w.len() {
            match w[i].as_str() {
                "-C" | "-c" => {
                    w.drain(i..(i + 2).min(w.len()));
                }
                "--no-pager" | "--no-optional-locks" => {
                    w.remove(i);
                }
                _ => break,
            }
        }
    }
    w
}

/// Programs that change nothing whatever their arguments (except as [`read_only`] checks).
const READ_ONLY: &[&str] = &[
    "ls", "cat", "head", "tail", "wc", "pwd", "echo", "printf", "which", "whoami", "id", "file",
    "stat", "du", "df", "tree", "true", "false", "basename", "dirname", "realpath", "readlink",
    "date", "uname", "hostname", "sort", "uniq", "cut", "tr", "diff", "cmp", "nl", "column", "rg",
    "grep", "egrep", "fgrep", "jq", "test", "[", "find", "sed", "type", "export", "local", "unset",
    "cd",
];

/// Git subcommands that only read.
const GIT_READ_ONLY: &[&str] = &[
    "status",
    "log",
    "diff",
    "show",
    "rev-parse",
    "ls-files",
    "ls-tree",
    "blame",
    "grep",
    "describe",
    "shortlog",
    "cat-file",
    "merge-base",
    "rev-list",
    "branch",
    "tag",
    "remote",
    "config",
    "stash",
];

/// Whether `cmd` only reads: a program from a fixed list, without the arguments that make it
/// write or run something else, or `<program> --version`.
pub fn read_only(cmd: &Command) -> bool {
    if cmd.writes {
        return false;
    }
    let w = &cmd.words;
    let Some(head) = w.first().map(String::as_str) else {
        return true;
    };
    let args = &w[1..];
    let has = |opts: &[&str]| {
        args.iter().any(|a| {
            opts.iter()
                .any(|o| a == o || a.starts_with(&format!("{o}=")))
        })
    };
    if args.len() == 1 && matches!(args[0].as_str(), "--version" | "-V") {
        return true;
    }
    if head == "git" {
        let Some(sub) = args.first().map(String::as_str) else {
            return true;
        };
        let rest = &args[1..];
        let only_flags = |ok: &[&str]| rest.iter().all(|a| ok.contains(&a.as_str()));
        return GIT_READ_ONLY.contains(&sub)
            && !has(&["--output", "--ext-diff", "-o"])
            && match sub {
                "branch" => {
                    only_flags(&["--list", "-a", "-r", "-v", "-vv", "--show-current", "--all"])
                }
                "tag" => rest.is_empty() || only_flags(&["-l", "--list"]),
                "remote" => rest.is_empty() || only_flags(&["-v"]),
                "config" => rest
                    .first()
                    .is_some_and(|a| a == "--get" || a == "--list" || a == "-l"),
                "stash" => rest.first().is_some_and(|a| a == "list" || a == "show"),
                _ => true,
            };
    }
    if !READ_ONLY.contains(&head) {
        return false;
    }
    match head {
        "find" => !has(&[
            "-exec", "-execdir", "-ok", "-okdir", "-delete", "-fprint", "-fprint0", "-fprintf",
            "-fls",
        ]),
        "sort" => !has(&["-o", "--output"]),
        "tree" => !has(&["-o"]),
        "rg" => !has(&["--pre", "--pre-glob"]),
        "sed" => {
            // Only printing: `sed -n '<lines>p'`.
            !has(&["-i", "--in-place", "-e", "-f"])
                && args.iter().any(|a| a == "-n")
                && args.iter().any(|a| {
                    let s = a.trim_end_matches('p');
                    a.ends_with('p')
                        && !s.is_empty()
                        && s.split(',')
                            .all(|n| n.chars().all(|c| c.is_ascii_digit() || c == '$'))
                })
        }
        _ => true,
    }
}

/// Programs whose prefix is never offered for "always": allowing them allows anything.
const NEVER_SUGGEST: &[&str] = &[
    "rm",
    "rmdir",
    "sudo",
    "su",
    "doas",
    "bash",
    "sh",
    "zsh",
    "fish",
    "dash",
    "python",
    "python3",
    "node",
    "ruby",
    "perl",
    "php",
    "deno",
    "eval",
    "exec",
    "xargs",
    "env",
    "chmod",
    "chown",
    "chgrp",
    "dd",
    "mkfs",
    "kill",
    "killall",
    "pkill",
    "osascript",
    "curl",
    "wget",
    "ssh",
    "scp",
    "nc",
    "open",
];

/// Programs whose first argument is a subcommand, with how many words name a command
/// (`npm run build`: three).
fn arity(words: &[String]) -> usize {
    let head = words[0].as_str();
    let second = words.get(1).map(String::as_str).unwrap_or_default();
    match head {
        "npm" | "pnpm" | "yarn" | "bun" if second == "run" => 3,
        "docker" if second == "compose" => 3,
        "gh" => 3,
        "git" | "npm" | "pnpm" | "yarn" | "bun" | "cargo" | "go" | "docker" | "kubectl" | "uv"
        | "pip" | "pip3" | "brew" | "terraform" | "make" | "npx" | "bunx" | "dotnet" | "swift"
        | "mvn" | "gradle" | "./gradlew" | "rustup" | "poetry" | "mise" | "just" => 2,
        _ => 1,
    }
}

/// The prefix to offer for "always allow" for `cmd`: the program, with its subcommand when it
/// has them (`git commit`, `npm run build`). `None` for programs that would allow anything.
pub fn suggest(cmd: &Command) -> Option<Vec<String>> {
    let head = cmd.words.first()?;
    let base = head.rsplit('/').next().unwrap_or(head);
    if NEVER_SUGGEST.contains(&base) {
        return None;
    }
    let n = arity(&cmd.words);
    let prefix: Vec<String> = cmd
        .words
        .iter()
        .take(n)
        .take_while(|w| !w.starts_with('-'))
        .cloned()
        .collect();
    // A program with subcommands is offered only with its subcommand: `git` alone would allow
    // every git command.
    let wanted = n.min(cmd.words.len());
    (prefix.len() == wanted && (n == 1 || wanted > 1)).then_some(prefix)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(line: &str) -> Vec<Vec<String>> {
        let p = parse(line);
        assert!(!p.opaque, "{line} is opaque");
        p.commands.into_iter().map(|c| c.words).collect()
    }

    fn w(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn splits_lists_pipelines_and_nested_commands() {
        assert_eq!(
            words("cd src && cargo test -q | tail -5; echo 'done here'"),
            vec![
                w(&["cd", "src"]),
                w(&["cargo", "test", "-q"]),
                w(&["tail", "-5"]),
                w(&["echo", "done here"])
            ]
        );
        assert_eq!(
            words("for f in *.rs; do rm \"$f\"; done"),
            vec![w(&["rm", "$f"])]
        );
        assert_eq!(
            words("if test -f a; then (git add a || true); fi"),
            vec![
                w(&["test", "-f", "a"]),
                w(&["git", "add", "a"]),
                w(&["true"])
            ]
        );
    }

    #[test]
    fn strips_wrappers_and_git_globals() {
        assert_eq!(
            words("FOO=1 timeout -s KILL 10 nice -n 5 npm test"),
            vec![w(&["npm", "test"])]
        );
        assert_eq!(
            words("git -C ../x --no-pager log -1"),
            vec![w(&["git", "log", "-1"])]
        );
        assert_eq!(words("command -v rg"), vec![w(&["command", "-v", "rg"])]);
    }

    #[test]
    fn substitutions_and_variable_programs_are_opaque() {
        for line in [
            "echo $(rm -rf x)",
            "echo `id`",
            "diff <(ls a) <(ls b)",
            "$CMD arg",
            "f() { rm x; }; f",
            "echo 'unterminated",
        ] {
            assert!(parse(line).opaque, "{line}");
        }
        assert!(
            !parse("echo \"$HOME\" ${PATH}").opaque,
            "variables in arguments are fine"
        );
        // Commands inside a substitution are still found, so deny rules see them.
        assert!(
            parse("echo $(rm -rf x)")
                .commands
                .iter()
                .any(|c| c.words[0] == "rm")
        );
    }

    #[test]
    fn redirects_into_files_write() {
        let p = parse("cat a > out.txt; ls 2>/dev/null; make 2>&1 | tee log");
        let writes: Vec<bool> = p.commands.iter().map(|c| c.writes).collect();
        assert_eq!(writes, [true, false, false, false]);
    }

    #[test]
    fn knows_read_only_commands() {
        let ro = |line: &str| parse(line).commands.iter().all(read_only);
        for line in [
            "ls -la",
            "rg -n foo src",
            "git status && git diff --stat",
            "git log --oneline -5",
            "sed -n '10,20p' a.rs",
            "find . -name '*.rs'",
            "cargo --version",
            "git branch -a",
        ] {
            assert!(ro(line), "{line}");
        }
        for line in [
            "rm a",
            "git commit -m x",
            "git branch -D x",
            "sed -i s/a/b/ f",
            "find . -delete",
            "cat a > b",
            "sort -o x y",
            "rg --pre sh x",
            "npm install",
        ] {
            assert!(!ro(line), "{line}");
        }
    }

    #[test]
    fn suggests_prefixes_but_never_for_anything_goes_programs() {
        let s = |line: &str| suggest(&parse(line).commands[0]);
        assert_eq!(s("git commit -m 'x'"), Some(w(&["git", "commit"])));
        assert_eq!(
            s("npm run build -- --watch"),
            Some(w(&["npm", "run", "build"]))
        );
        assert_eq!(s("cargo build --release"), Some(w(&["cargo", "build"])));
        assert_eq!(s("pytest -q"), Some(w(&["pytest"])));
        assert_eq!(s("git --version"), None, "not `git` alone");
        assert_eq!(s("make"), None);
        assert_eq!(s("rm -rf target"), None);
        assert_eq!(s("python3 script.py"), None);
        assert_eq!(s("/usr/bin/sudo ls"), None);
    }
}
