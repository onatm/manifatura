# Skills

A skill is a folder of instructions for one kind of task, which an agent loads only when a task calls for it. manifatura follows the [Agent Skills](https://agentskills.io) format, so skills written for other agents work unchanged. The `manifatura-skills` crate, `manifatura::skills` behind the `skills` feature (on by default), finds them, lists them for the system prompt and gives the model a `skill` tool to load one.

## The format

A skill is a directory with a `SKILL.md`: YAML frontmatter, then Markdown.

```markdown
---
name: release-notes
description: Write release notes from the changes since the last tag. Use when the user asks for release notes or a changelog entry.
---

# Release notes

1. Find the last tag and list the commits since it.
2. Group them as in `references/style.md`.
```

`name` (lowercase letters, digits and hyphens, up to 64 characters, the same as the directory's name) and `description` (what the skill does and when to use it) are required. Other fields, such as `license` and `metadata`, are allowed and ignored. The directory may hold any other files the instructions point to, by convention in `scripts/`, `references/` and `assets/`.

## Finding skills

```rust
use manifatura::skills::{Skill, Skills};

let skills = Skills::new()
    .builtin(Skill::parse(include_str!("skills/tour/SKILL.md"))?) // built into the app
    .dir(home.join(".agents/skills"))                              // a directory of skills
    .project(".agents/skills")                                     // in each project directory
    .home(&home)
    .load(&env)
    .await;
```

`Skills::agents(&home)` is the convention's default: `.agents/skills` in the project, then `~/.agents/skills`.

Each subdirectory with a `SKILL.md` is a skill. Project directories are searched like [`AGENTS.md` files](system-prompts.md), from the repository root down to the working directory. When two skills share a name, the later source wins: built-in skills, then the `dir`s in order, then project directories, deeper ones last. `skills.warnings()` reports every skill hidden this way, and skills that were skipped: a missing description, frontmatter that cannot be read. A name that breaks the rules, or does not match its directory, loads with a warning. An unquoted colon in a value (`description: Use when: …`) is forgiven.

A project's skills come with the repository, which may not be trusted. Leave out `project` unless the user trusts it.

## Using them

Skills load in three steps, so that only what a task needs reaches the context.

1. **The catalog.** `SystemPrompt::skills(&skills)` adds each skill's name and description, and the `skill` tool's guideline says when to load one. Nothing is added when there are none.

   ```text
   # Skills

   Load these with the skill tool:

   - release-notes: Write release notes from the changes since the last tag. Use when …
   ```

2. **The instructions.** `skills.tool()` is the `skill` tool (`None` when there are no skills). Its one argument, `name`, is an enum of the skills' names. It returns the body of `SKILL.md`, the skill's directory and a list of its other files:

   ```text
   <skill_content name="release-notes">
   # Release notes
   …

   Skill directory: /home/me/.agents/skills/release-notes
   Relative paths in this skill are relative to the skill directory.
   <skill_resources>
   <file>references/style.md</file>
   </skill_resources>
   </skill_content>
   ```

3. **The files.** The model reads the files the instructions name with its own tools, such as `read`. A skill's files may be outside the working directory, so an app that asks before reads may want to allow reads under `skill.dir` without asking.

```rust
use manifatura::prelude::*; // brings in SystemPromptExt, which adds `skills` to SystemPrompt

tools.extend(skills.tool()); // with the other tools, so the prompt carries its guideline
let instructions = SystemPrompt::new(BASE).tools(&tools).skills(&skills).build();
let agent = Agent::new(instructions)
    .tools(tools)
    .hooks(manifatura::kit::compaction::trim_results(8_000, 2_000));
```

Without the prelude, `use manifatura::skills::SystemPromptExt;` does the same. `skills.section()` is the catalog as text, for a prompt built some other way.

The tool reads nothing outside the skills it was given, so it needs no approval.

## When the user asks for a skill

Apps often let the user start a skill themselves, with `/release-notes` or similar. `skills.activation(name, &env)` returns the same text as the tool; send it as the start of the user's message, followed by what they typed after the name. The model then has the instructions without spending a step on the tool.

## Skills and the context

The catalog is part of the system prompt, so it is fixed when the session starts and the prompt cache stays warm; skills added later appear in the next session. A skill's instructions apply for the rest of the task, so the `skill` tool [keeps its results](tools.md#flags): `kit::compaction::drop_old_results` and `trim_results` leave them whole when they drop or shorten old tool output.
