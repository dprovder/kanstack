//! `kanstack skill install` / `kanstack skill check` — put kanstack's agent skill (see
//! `crate::skill`) where a coding agent will load it, and keep it current. Modelled on
//! `but skill install` / `but skill check`, closely enough to feel like the same family:
//! `install` writes into the current repository unless `--global`; `check` reports every
//! copy, local and global, and `--update` refreshes the ones behind this binary in place.
//!
//! Which harnesses to write for comes from what's installed, never a prompt: `--agent` if
//! given, else every harness `crate::setup::detect_harnesses` finds on `PATH` plus the
//! configured one (`KANSTACK_HARNESS`, default `claude`). A harness with no
//! `crate::skill::SkillFormat` gets an explicit "no skill format known" entry, and when that's
//! every harness asked for, `unsupported_harness`.
//!
//! The one thing that does prompt is writing into a repository: that file shows up in
//! `git status` and will likely be committed and shared with a team, so an interactive
//! `install` (stdin a terminal, no `--json`) shows what it's about to write and asks first.
//! Overwriting a copy someone has edited since kanstack wrote it is asked about the same way —
//! and without a terminal to ask on, refused (`skill_modified`) unless `--force`. `check
//! --update` never overwrites an edited copy at all.

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use serde::Serialize;

use crate::skill::{self, Existing, Scope, Target, VERSION};

use super::exit::{tag, ErrorCode::*};

/// The `"schema"` of `skill install --json` and `skill check --json`, which, like `status`
/// and `prune`, print shapes of their own rather than the generic result envelope — there is
/// no workstream for its `"workstream"` to name.
pub const SKILL_SCHEMA: u32 = 1;

/// Everything `install`/`check` read from outside themselves, gathered once by
/// [`Context::from_process`] so tests can hand in their own instead of touching the real
/// `$HOME`, `PATH` or terminal.
pub(super) struct Context {
    pub repo: Option<PathBuf>,
    pub home: Option<PathBuf>,
    /// `crate::harness::Harness::id`s (or, for one kanstack doesn't recognize, the name as
    /// given) to write skills for, in order.
    pub harnesses: Vec<String>,
    pub interactive: bool,
}

impl Context {
    fn from_process(cwd: &Path, agent: Option<&str>, json: bool) -> Context {
        let harnesses = match agent {
            Some(agent) => vec![harness_id(agent)],
            None => {
                let configured = std::env::var("KANSTACK_HARNESS").unwrap_or_else(|_| "claude".to_string());
                let mut ids = vec![harness_id(&configured)];
                for id in crate::setup::detect_harnesses() {
                    if !ids.iter().any(|i| i == id) {
                        ids.push(id.to_string());
                    }
                }
                ids
            }
        };
        Context {
            repo: skill::repo_root(cwd),
            home: skill::home_dir(),
            harnesses,
            interactive: !json && std::io::stdin().is_terminal(),
        }
    }
}

/// The harness `command` names, as `crate::harness::for_command` resolves it — `kiro-cli`
/// is `kiro` — or its own program name when that's no harness kanstack knows.
fn harness_id(command: &str) -> String {
    match crate::harness::for_command(command).id() {
        "generic" => {
            let program = command.split_whitespace().next().unwrap_or(command);
            Path::new(program).file_name().and_then(|n| n.to_str()).unwrap_or(program).to_string()
        }
        id => id.to_string(),
    }
}

#[derive(Debug, Serialize)]
struct Unsupported {
    harness: String,
    message: String,
}

fn unsupported(ctx: &Context) -> Vec<Unsupported> {
    ctx.harnesses
        .iter()
        .filter(|h| skill::format_for(h).is_none())
        .map(|h| Unsupported { harness: h.clone(), message: skill::unsupported_message(h) })
        .collect()
}

#[derive(Debug, Serialize)]
struct InstallEntry {
    harness: &'static str,
    format_name: &'static str,
    scope: Scope,
    path: PathBuf,
    /// `created`, `updated` (replacing another version kanstack wrote), `replaced` (an edited
    /// copy, overwritten with consent), or `unchanged` (already this version, untouched).
    action: &'static str,
    /// What was there before, when it carried a stamp.
    previous_version: Option<String>,
}

#[derive(Debug, Serialize)]
struct InstallReport {
    schema: u32,
    version: &'static str,
    skills: Vec<InstallEntry>,
    unsupported: Vec<Unsupported>,
}

pub(super) fn run_install(global: bool, agent: Option<String>, force: bool, json: bool, cwd: &Path, out: &mut impl Write) -> Result<()> {
    let ctx = Context::from_process(cwd, agent.as_deref(), json);
    install(&ctx, global, force, json, out, &mut ask_on_terminal)
}

/// Asks `prompt` on stderr — stdout is what a caller may be capturing — and reads a yes/no
/// from stdin. Anything but `y`/`yes` is no.
fn ask_on_terminal(prompt: &str) -> Result<bool> {
    eprint!("{prompt}");
    std::io::stderr().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes"))
}

/// `install` proper. `confirm` is only called when `ctx.interactive`, with the whole question
/// to ask; `false` writes nothing and is not an error.
pub(super) fn install(
    ctx: &Context,
    global: bool,
    force: bool,
    json: bool,
    out: &mut impl Write,
    confirm: &mut dyn FnMut(&str) -> Result<bool>,
) -> Result<()> {
    let unsupported = unsupported(ctx);
    let formats: Vec<_> = ctx.harnesses.iter().filter_map(|h| skill::format_for(h)).collect();
    if formats.is_empty() {
        let names = unsupported.iter().map(|u| u.message.as_str()).collect::<Vec<_>>().join("; ");
        return Err(tag(
            UnsupportedHarness,
            anyhow!("{names} — Claude Code is the only harness `kanstack skill install` can write a skill for so far"),
        ));
    }
    let (scope, base) = if global {
        let home = ctx.home.clone().ok_or_else(|| anyhow!("$HOME is not set, so there's no global skill directory to install into"))?;
        (Scope::Global, home)
    } else {
        let repo = ctx.repo.clone().ok_or_else(|| {
            tag(
                InvalidArguments,
                anyhow!(
                    "not inside a git repository — `kanstack skill install` installs into the current \
                     repository unless given --global, which installs it for every repository on this machine"
                ),
            )
        })?;
        (Scope::Local, repo)
    };
    let targets = formats
        .into_iter()
        .map(|f| {
            let path = match scope {
                Scope::Local => f.local_path(&base),
                Scope::Global => f.global_path(&base),
            };
            Target::inspect(f, scope, path)
        })
        .collect::<Result<Vec<_>>>()?;

    let writes: Vec<&Target> = targets.iter().filter(|t| t.existing != Existing::Current).collect();
    let edited: Vec<&Target> = writes.iter().copied().filter(|t| matches!(t.existing, Existing::Modified(_))).collect();
    if ctx.interactive && !writes.is_empty() && (scope == Scope::Local || (!edited.is_empty() && !force)) {
        if !confirm(&install_prompt(&writes, scope))? {
            writeln!(out, "nothing written")?;
            return Ok(());
        }
    } else if !edited.is_empty() && !force {
        let paths = edited.iter().map(|t| t.path.display().to_string()).collect::<Vec<_>>().join(", ");
        return Err(tag(
            SkillModified,
            anyhow!(
                "{paths} has been edited since kanstack wrote it (or wasn't written by kanstack at all), so it \
                 was left alone — pass --force to replace it with kanstack {VERSION}'s skill"
            ),
        ));
    }

    let mut skills = Vec::new();
    for t in &targets {
        let action = match &t.existing {
            Existing::Current => "unchanged",
            Existing::Missing => "created",
            Existing::OtherVersion(_) => "updated",
            Existing::Modified(_) => "replaced",
        };
        if action != "unchanged" {
            t.write()?;
        }
        skills.push(InstallEntry {
            harness: t.format.harness(),
            format_name: t.format.format_name(),
            scope: t.scope,
            path: t.path.clone(),
            action,
            previous_version: match &t.existing {
                Existing::Current => None,
                _ => t.installed_version().map(str::to_string),
            },
        });
    }

    if json {
        writeln!(out, "{}", serde_json::to_string(&InstallReport { schema: SKILL_SCHEMA, version: VERSION, skills, unsupported })?)?;
        return Ok(());
    }
    for s in &skills {
        let what = match (s.action, &s.previous_version) {
            ("unchanged", _) => "already up to date".to_string(),
            ("updated", Some(v)) => format!("updated from {v} to {VERSION}"),
            ("replaced", _) => format!("replaced an edited copy with {VERSION}"),
            _ => format!("installed {VERSION}"),
        };
        writeln!(out, "{} ({}): {what} — {}", s.format_name, s.scope, s.path.display())?;
    }
    for u in &unsupported {
        writeln!(out, "{}: {}, skipped", u.harness, u.message)?;
    }
    if scope == Scope::Local && skills.iter().any(|s| s.action != "unchanged") {
        writeln!(
            out,
            "This is a file in your repository: commit it to share the skill with everyone working here, \
             or use `kanstack skill install --global` instead to keep it to yourself."
        )?;
    }
    Ok(())
}

fn install_prompt(writes: &[&Target], scope: Scope) -> String {
    let mut prompt = String::from("kanstack skill install will:\n");
    for t in writes {
        let what = match &t.existing {
            Existing::Missing => "create".to_string(),
            Existing::OtherVersion(v) => format!("replace kanstack {v}'s skill with {VERSION}'s:"),
            Existing::Modified(_) => "OVERWRITE a copy edited since kanstack wrote it (those edits will be lost):".to_string(),
            Existing::Current => unreachable!("not a write"),
        };
        prompt.push_str(&format!("  {what} {}\n", t.path.display()));
    }
    if scope == Scope::Local {
        prompt.push_str(
            "That's a file in this repository's working tree, so it will show up in `git status` and is likely \
             to be committed and shared with anyone else working here. `kanstack skill install --global` puts it \
             in your home directory instead.\n",
        );
    }
    prompt.push_str("Go ahead? [y/N] ");
    prompt
}

#[derive(Debug, Serialize)]
struct CheckEntry {
    harness: &'static str,
    format_name: &'static str,
    scope: Scope,
    path: PathBuf,
    /// `null` for a copy carrying no stamp — one kanstack didn't write.
    installed_version: Option<String>,
    up_to_date: bool,
    /// Edited since kanstack wrote it, or never written by kanstack. `--update` leaves these
    /// alone.
    modified: bool,
    /// Whether this run's `--update` refreshed it.
    updated: bool,
}

#[derive(Debug, Serialize)]
struct CheckReport {
    schema: u32,
    cli_version: &'static str,
    skills: Vec<CheckEntry>,
    /// How many copies are still behind this binary, after any `--update`.
    outdated_count: usize,
    unsupported: Vec<Unsupported>,
}

pub(super) fn run_check(update: bool, json: bool, cwd: &Path, out: &mut impl Write) -> Result<()> {
    check(&Context::from_process(cwd, None, json), update, json, out)
}

pub(super) fn check(ctx: &Context, update: bool, json: bool, out: &mut impl Write) -> Result<()> {
    let mut skills = Vec::new();
    let scopes = [(Scope::Local, ctx.repo.as_deref()), (Scope::Global, ctx.home.as_deref())];
    for format in skill::FORMATS.iter().copied() {
        for (scope, base) in scopes {
            let Some(base) = base else { continue };
            let path = match scope {
                Scope::Local => format.local_path(base),
                Scope::Global => format.global_path(base),
            };
            let mut target = Target::inspect(format, scope, path.clone())?;
            if target.existing == Existing::Missing {
                continue;
            }
            let mut updated = false;
            if update && target.is_behind() && matches!(target.existing, Existing::OtherVersion(_)) {
                target.write()?;
                target = Target::inspect(format, scope, path)?;
                updated = true;
            }
            skills.push((target, updated));
        }
    }
    let outdated_count = skills.iter().filter(|(t, _)| t.is_behind()).count();
    let unsupported = unsupported(ctx);

    if json {
        let skills = skills
            .into_iter()
            .map(|(t, updated)| CheckEntry {
                harness: t.format.harness(),
                format_name: t.format.format_name(),
                scope: t.scope,
                installed_version: t.installed_version().map(str::to_string),
                up_to_date: t.installed_version() == Some(VERSION),
                modified: matches!(t.existing, Existing::Modified(_)),
                path: t.path,
                updated,
            })
            .collect();
        let report = CheckReport { schema: SKILL_SCHEMA, cli_version: VERSION, skills, outdated_count, unsupported };
        writeln!(out, "{}", serde_json::to_string(&report)?)?;
        return Ok(());
    }

    writeln!(out, "kanstack {VERSION}")?;
    if skills.is_empty() {
        writeln!(
            out,
            "no kanstack skill installed — `kanstack skill install` adds one to this repository, \
             `kanstack skill install --global` one for every repository"
        )?;
    }
    for (t, updated) in &skills {
        let version = t.installed_version().unwrap_or("no version stamp");
        let (mark, note) = match &t.existing {
            _ if *updated => ("✓", "updated".to_string()),
            Existing::Current => ("✓", String::new()),
            Existing::Modified(None) => ("!", "not written by kanstack, left alone".to_string()),
            Existing::Modified(Some(_)) => {
                ("!", "edited since kanstack wrote it, left alone — `kanstack skill install --force` replaces it".to_string())
            }
            Existing::OtherVersion(_) if t.is_behind() => {
                ("✗", "behind — `kanstack skill check --update` refreshes it".to_string())
            }
            Existing::OtherVersion(_) => ("✓", "written by a newer kanstack".to_string()),
            Existing::Missing => unreachable!("skipped above"),
        };
        let note = if note.is_empty() { String::new() } else { format!(" — {note}") };
        writeln!(out, "  {mark} {} ({}) {} [{version}]{note}", t.format.format_name(), t.scope, t.path.display())?;
    }
    for u in &unsupported {
        writeln!(out, "  {}: {}", u.harness, u.message)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::cli::test_support::args;
    use crate::cli::{error_code, parse, Command, ErrorCode};
    use crate::skill::claude::ClaudeSkill;
    use crate::skill::SkillFormat;

    /// A temp directory standing in for both a repository and a home directory — nothing here
    /// ever touches the real `~/.claude`.
    struct Sandbox {
        root: PathBuf,
    }

    impl Sandbox {
        fn new(tag: &str) -> Sandbox {
            let root = std::env::temp_dir().join(format!("kanstack-skill-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join("repo/.git")).unwrap();
            std::fs::create_dir_all(root.join("home")).unwrap();
            Sandbox { root }
        }

        fn repo(&self) -> PathBuf {
            self.root.join("repo")
        }

        fn home(&self) -> PathBuf {
            self.root.join("home")
        }

        fn ctx(&self, harnesses: &[&str]) -> Context {
            Context {
                repo: Some(self.repo()),
                home: Some(self.home()),
                harnesses: harnesses.iter().map(|h| h.to_string()).collect(),
                interactive: false,
            }
        }

        fn local(&self) -> PathBuf {
            ClaudeSkill.local_path(&self.repo())
        }

        fn global(&self) -> PathBuf {
            ClaudeSkill.global_path(&self.home())
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn never_asked(_: &str) -> Result<bool> {
        panic!("non-interactive install must not prompt")
    }

    fn install_json(ctx: &Context, global: bool, force: bool) -> Result<serde_json::Value> {
        let mut out = Vec::new();
        install(ctx, global, force, true, &mut out, &mut never_asked)?;
        Ok(serde_json::from_slice(&out).unwrap())
    }

    fn check_json(ctx: &Context, update: bool) -> serde_json::Value {
        let mut out = Vec::new();
        check(ctx, update, true, &mut out).unwrap();
        serde_json::from_slice(&out).unwrap()
    }

    fn stamped_version(path: &Path) -> Option<String> {
        ClaudeSkill.read_stamp(&std::fs::read_to_string(path).unwrap()).version
    }

    #[test]
    fn parses_install_and_check() {
        assert_eq!(
            parse("skill", args(&["install"])).unwrap(),
            Some(Command::SkillInstall { global: false, agent: None, force: false, json: false })
        );
        assert_eq!(
            parse("skill", args(&["install", "--global", "--agent", "codex", "--force", "--json"])).unwrap(),
            Some(Command::SkillInstall { global: true, agent: Some("codex".into()), force: true, json: true })
        );
        assert_eq!(parse("skill", args(&["check", "--update"])).unwrap(), Some(Command::SkillCheck { update: true, json: false }));
        assert!(parse("skill", args(&[])).is_err());
        assert!(parse("skill", args(&["frobnicate"])).is_err());
        assert!(parse("skill", args(&["check", "--global"])).is_err(), "--global is install's");
        assert!(parse("skill", args(&["install", "--update"])).is_err(), "--update is check's");
        assert!(parse("skill", args(&["install", "extra"])).is_err());
    }

    #[test]
    fn install_defaults_to_the_repository_and_global_goes_home() {
        let sb = Sandbox::new("paths");
        let ctx = sb.ctx(&["claude"]);
        let local = install_json(&ctx, false, false).unwrap();
        assert_eq!(local["skills"][0]["scope"], "local");
        assert_eq!(local["skills"][0]["path"], sb.local().to_str().unwrap());
        assert!(sb.local().is_file());
        assert!(!sb.global().exists(), "a local install leaves $HOME alone");

        install_json(&ctx, true, false).unwrap();
        assert!(sb.global().is_file());
    }

    /// The repository is found from a subdirectory too, not just its root.
    #[test]
    fn install_finds_the_repository_root() {
        let sb = Sandbox::new("root");
        let nested = sb.repo().join("src/deep");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(skill::repo_root(&nested), Some(sb.repo()));
    }

    #[test]
    fn install_outside_a_repository_is_an_argument_error_pointing_at_global() {
        let sb = Sandbox::new("norepo");
        let mut ctx = sb.ctx(&["claude"]);
        ctx.repo = None;
        let e = install_json(&ctx, false, false).unwrap_err();
        assert_eq!(error_code(&e), ErrorCode::InvalidArguments);
        assert!(e.to_string().contains("--global"), "{e}");
    }

    #[test]
    fn the_written_file_is_stamped_with_this_binarys_version() {
        let sb = Sandbox::new("stamp");
        install_json(&sb.ctx(&["claude"]), false, false).unwrap();
        assert_eq!(stamped_version(&sb.local()).as_deref(), Some(env!("CARGO_PKG_VERSION")));
        let written = std::fs::read_to_string(sb.local()).unwrap();
        assert!(written.starts_with("---\nname: kanstack\n"), "{written}");
    }

    #[test]
    fn install_json_shape_and_idempotence() {
        let sb = Sandbox::new("json");
        let first = install_json(&sb.ctx(&["claude"]), false, false).unwrap();
        assert_eq!(
            first,
            serde_json::json!({
                "schema": 1,
                "version": VERSION,
                "skills": [{
                    "harness": "claude",
                    "format_name": "Claude Code",
                    "scope": "local",
                    "path": sb.local(),
                    "action": "created",
                    "previous_version": null,
                }],
                "unsupported": [],
            })
        );
        let again = install_json(&sb.ctx(&["claude"]), false, false).unwrap();
        assert_eq!(again["skills"][0]["action"], "unchanged");
    }

    #[test]
    fn an_unsupported_harness_alone_is_an_error_and_alongside_claude_is_reported() {
        let sb = Sandbox::new("unsupported");
        let e = install_json(&sb.ctx(&["codex"]), false, false).unwrap_err();
        assert_eq!(error_code(&e), ErrorCode::UnsupportedHarness);
        assert!(e.to_string().contains("no skill format known for codex yet"), "{e}");
        assert!(!sb.local().exists(), "nothing written for a harness with no format");

        let mixed = install_json(&sb.ctx(&["claude", "gemini"]), false, false).unwrap();
        assert_eq!(mixed["skills"].as_array().unwrap().len(), 1);
        assert_eq!(
            mixed["unsupported"],
            serde_json::json!([{"harness": "gemini", "message": "no skill format known for gemini yet"}])
        );

        let mut out = Vec::new();
        install(&sb.ctx(&["claude", "pi"]), false, false, false, &mut out, &mut never_asked).unwrap();
        assert!(String::from_utf8(out).unwrap().contains("pi: no skill format known for pi yet, skipped"));
    }

    #[test]
    fn harness_names_resolve_like_spawn_agent_does() {
        assert_eq!(harness_id("kiro-cli"), "kiro");
        assert_eq!(harness_id("/usr/local/bin/claude --dangerously-skip-permissions"), "claude");
        assert_eq!(harness_id("my-wrapper --flag"), "my-wrapper");
    }

    #[test]
    fn install_over_an_older_copy_updates_it_but_an_edited_one_needs_force() {
        let sb = Sandbox::new("overwrite");
        let ctx = sb.ctx(&["claude"]);
        std::fs::create_dir_all(sb.local().parent().unwrap()).unwrap();
        std::fs::write(sb.local(), ClaudeSkill.render("0.0.1")).unwrap();
        let updated = install_json(&ctx, false, false).unwrap();
        assert_eq!(updated["skills"][0]["action"], "updated");
        assert_eq!(updated["skills"][0]["previous_version"], "0.0.1");

        let edited = std::fs::read_to_string(sb.local()).unwrap() + "\nmy team's own note\n";
        std::fs::write(sb.local(), &edited).unwrap();
        let e = install_json(&ctx, false, false).unwrap_err();
        assert_eq!(error_code(&e), ErrorCode::SkillModified);
        assert_eq!(std::fs::read_to_string(sb.local()).unwrap(), edited, "refused means untouched");

        let forced = install_json(&ctx, false, true).unwrap();
        assert_eq!(forced["skills"][0]["action"], "replaced");
        assert_eq!(forced["skills"][0]["previous_version"], VERSION);
        assert!(ClaudeSkill.read_stamp(&std::fs::read_to_string(sb.local()).unwrap()).pristine);
    }

    #[test]
    fn interactive_install_asks_before_writing_into_the_repository() {
        let sb = Sandbox::new("prompt");
        let mut ctx = sb.ctx(&["claude"]);
        ctx.interactive = true;
        let mut asked = Vec::new();
        let mut out = Vec::new();
        install(&ctx, false, false, false, &mut out, &mut |p: &str| {
            asked.push(p.to_string());
            Ok(false)
        })
        .unwrap();
        assert!(!sb.local().exists(), "declined means nothing written");
        assert_eq!(String::from_utf8(out).unwrap(), "nothing written\n");
        assert!(asked[0].contains(&format!("create {}", sb.local().display())), "{}", asked[0]);
        assert!(asked[0].contains("git status"), "{}", asked[0]);

        install(&ctx, false, false, false, &mut Vec::new(), &mut |_: &str| Ok(true)).unwrap();
        assert!(sb.local().is_file());

        // Already current: nothing to write, nothing to ask.
        install(&ctx, false, false, false, &mut Vec::new(), &mut never_asked).unwrap();
        // Global, nothing edited: no repository involved, no prompt.
        install(&ctx, true, false, false, &mut Vec::new(), &mut never_asked).unwrap();
        assert!(sb.global().is_file());

        // An edited copy is named in the prompt as one about to be overwritten.
        std::fs::write(sb.global(), "---\nname: kanstack\n---\nmine\n").unwrap();
        let mut prompt = String::new();
        install(&ctx, true, false, false, &mut Vec::new(), &mut |p: &str| {
            prompt = p.to_string();
            Ok(false)
        })
        .unwrap();
        assert!(prompt.contains("OVERWRITE"), "{prompt}");
        assert_eq!(std::fs::read_to_string(sb.global()).unwrap(), "---\nname: kanstack\n---\nmine\n");
    }

    #[test]
    fn check_reports_nothing_installed() {
        let sb = Sandbox::new("check-none");
        let report = check_json(&sb.ctx(&["claude"]), false);
        assert_eq!(
            report,
            serde_json::json!({"schema": 1, "cli_version": VERSION, "skills": [], "outdated_count": 0, "unsupported": []})
        );
    }

    #[test]
    fn check_detects_a_stale_copy_and_update_refreshes_it() {
        let sb = Sandbox::new("check-stale");
        let ctx = sb.ctx(&["claude", "codex"]);
        install_json(&ctx, false, false).unwrap();
        std::fs::create_dir_all(sb.global().parent().unwrap()).unwrap();
        std::fs::write(sb.global(), ClaudeSkill.render("0.0.1")).unwrap();

        let before = check_json(&ctx, false);
        assert_eq!(before["outdated_count"], 1);
        assert_eq!(
            before["skills"],
            serde_json::json!([
                {"harness": "claude", "format_name": "Claude Code", "scope": "local", "path": sb.local(),
                 "installed_version": VERSION, "up_to_date": true, "modified": false, "updated": false},
                {"harness": "claude", "format_name": "Claude Code", "scope": "global", "path": sb.global(),
                 "installed_version": "0.0.1", "up_to_date": false, "modified": false, "updated": false},
            ])
        );
        assert_eq!(before["unsupported"][0]["harness"], "codex");
        assert_eq!(stamped_version(&sb.global()).as_deref(), Some("0.0.1"), "check alone writes nothing");

        let after = check_json(&ctx, true);
        assert_eq!(after["outdated_count"], 0);
        assert_eq!(after["skills"][1]["updated"], true);
        assert_eq!(after["skills"][1]["installed_version"], VERSION);
        assert_eq!(after["skills"][0]["updated"], false, "an up-to-date copy isn't rewritten");
        assert_eq!(stamped_version(&sb.global()).as_deref(), Some(VERSION));
    }

    #[test]
    fn update_never_touches_an_edited_or_unstamped_copy() {
        let sb = Sandbox::new("check-edited");
        std::fs::create_dir_all(sb.local().parent().unwrap()).unwrap();
        let edited = ClaudeSkill.render("0.0.1") + "\nlocal tweak\n";
        std::fs::write(sb.local(), &edited).unwrap();
        // What kanstack's own source repository looks like: the unstamped source itself.
        std::fs::create_dir_all(sb.global().parent().unwrap()).unwrap();
        std::fs::write(sb.global(), skill::claude::SOURCE).unwrap();

        let report = check_json(&sb.ctx(&["claude"]), true);
        assert_eq!(report["skills"][0]["modified"], true);
        assert_eq!(report["skills"][0]["updated"], false);
        assert_eq!(report["skills"][0]["installed_version"], "0.0.1");
        assert_eq!(report["skills"][1]["installed_version"], serde_json::Value::Null);
        assert_eq!(report["skills"][1]["modified"], true);
        assert_eq!(std::fs::read_to_string(sb.local()).unwrap(), edited);
        assert_eq!(std::fs::read_to_string(sb.global()).unwrap(), skill::claude::SOURCE);
    }

    #[test]
    fn check_human_output_says_what_to_run() {
        let sb = Sandbox::new("check-human");
        let ctx = sb.ctx(&["claude"]);
        let mut out = Vec::new();
        check(&ctx, false, false, &mut out).unwrap();
        assert!(String::from_utf8(out).unwrap().contains("`kanstack skill install`"));

        std::fs::create_dir_all(sb.local().parent().unwrap()).unwrap();
        std::fs::write(sb.local(), ClaudeSkill.render("0.0.1")).unwrap();
        let mut out = Vec::new();
        check(&ctx, false, false, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("[0.0.1] — behind — `kanstack skill check --update`"), "{text}");
    }
}
