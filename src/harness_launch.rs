//! The harness launch line itself — building it, and delivering the branch-context note
//! (see [`branch_context_note`]) with it. Which delivery a given harness gets is
//! `crate::harness`'s call; this module only knows how to build the line for each.

use std::path::Path;

use anyhow::{Context, Result};

/// How the branch-context note (see [`branch_context_note`]) reaches a harness's launch
/// command — resolved once, in [`crate::harness::resolve_note_delivery`], from `KANSTACK_HARNESS_SYSTEM_FLAG`
/// and the configured harness itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoteDelivery {
    /// `<flag> <shell-quoted note>`, ahead of the initial message — e.g. `claude
    /// --append-system-prompt "..."`. Additive to the harness's own default system
    /// prompt, not a replacement, and doesn't consume a "turn" the way the initial
    /// message does — so it goes out even when there's no initial message at all.
    Flag(String),
    /// Codex's `-c developer_instructions="<note>"` config override: a generic
    /// instructions-role message, not literally an appended system prompt, but the
    /// closest thing Codex CLI has to one today (no `--append-system-prompt` exists for
    /// it — see `crate::harness`). Same "goes out regardless of the initial
    /// message" property as `Flag`.
    CodexConfig,
    /// No known out-of-band mechanism for this harness (e.g. OpenCode, which as of
    /// writing has no CLI flag for this at all — a feature request for exactly this was
    /// filed and closed as not planned). Falls back to prepending the note directly onto
    /// the initial message text instead — the one thing every harness's launch command
    /// already has room for.
    FoldIntoMessage,
    /// Explicitly turned off via `KANSTACK_HARNESS_SYSTEM_FLAG=""`.
    Disabled,
}

/// The longest launch line typed straight into a fresh terminal. A new pane's shell is
/// still starting up when the line arrives, and until it switches the tty to raw mode for
/// its line editor the kernel's line discipline is in charge, which caps a line at about a
/// kilobyte and silently drops the rest — leaving a command cut off mid-quote that never
/// runs. Anything longer goes through files instead, see [`launch_line`]. Well under the
/// limit, since the limit counts bytes and the line has multibyte text in it.
const MAX_TYPED_LINE: usize = 700;

/// Builds the shell command line a split backend types into the fresh terminal — split out
/// so it can be unit tested without shelling out to a real `cmux`/`tmux`.
///
/// Always the whole text inline; see [`launch_line`] for the one that copes with a prompt
/// too long to type.
pub fn build_launch_command(
    cwd: &Path,
    harness: &str,
    note_delivery: &NoteDelivery,
    name: &str,
    initial_message: Option<&str>,
) -> String {
    let args = launch_args(note_delivery, name, initial_message, &mut |prefix, text| {
        shell_quote(&format!("{prefix}{text}"))
    });
    format!("cd {} && {}{}\n", shell_quote(&cwd.to_string_lossy()), harness, args)
}

/// The arguments after the harness, each preceded by a space. `word` turns a value into a
/// shell word: given the literal `prefix` that must stay in front of it (Codex's
/// `developer_instructions=`, empty otherwise) and the value's own `text`.
fn launch_args(
    note_delivery: &NoteDelivery,
    name: &str,
    initial_message: Option<&str>,
    word: &mut dyn FnMut(&str, &str) -> String,
) -> String {
    let mut args = String::new();
    let mut message = initial_message.map(str::to_string);

    match note_delivery {
        NoteDelivery::Flag(flag) => {
            args.push(' ');
            args.push_str(flag);
            args.push(' ');
            args.push_str(&word("", &branch_context_note(name)));
        }
        NoteDelivery::CodexConfig => {
            args.push_str(" -c ");
            args.push_str(&word("developer_instructions=", &toml_quote(&branch_context_note(name))));
        }
        NoteDelivery::FoldIntoMessage => {
            message = Some(match message {
                Some(m) => format!("{}\n\n{m}", branch_context_note(name)),
                None => branch_context_note(name),
            });
        }
        NoteDelivery::Disabled => {}
    }

    if let Some(message) = message {
        args.push(' ');
        args.push_str(&word("", &message));
    }
    args
}

/// [`build_launch_command`], unless that would be too long to type into a fresh terminal
/// (see `MAX_TYPED_LINE`): then each long value is written to a private temp file, and the
/// line typed is short — it reads the files into shell variables, deletes them, and
/// launches the harness with `"$variable"` arguments. The values reach the harness
/// byte-for-byte the same either way.
pub fn launch_line(
    cwd: &Path,
    harness: &str,
    note_delivery: &NoteDelivery,
    name: &str,
    initial_message: Option<&str>,
) -> Result<String> {
    launch_line_in(&std::env::temp_dir(), cwd, harness, note_delivery, name, initial_message)
}

fn launch_line_in(
    tmp: &Path,
    cwd: &Path,
    harness: &str,
    note_delivery: &NoteDelivery,
    name: &str,
    initial_message: Option<&str>,
) -> Result<String> {
    let direct = build_launch_command(cwd, harness, note_delivery, name, initial_message);
    if direct.len() <= MAX_TYPED_LINE {
        return Ok(direct);
    }

    use std::os::unix::fs::DirBuilderExt;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let dir = tmp.join(format!("kanstack-launch-{}-{nanos}", std::process::id()));
    // Owner-only: what's in here is the user's prompt.
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&dir)
        .with_context(|| format!("creating {}", dir.display()))?;

    let mut reads = Vec::new();
    let mut failure = None;
    let args = launch_args(note_delivery, name, initial_message, &mut |prefix, text| {
        let var = format!("k{}", reads.len());
        let path = dir.join(&var);
        if let Err(e) = std::fs::write(&path, text) {
            failure.get_or_insert_with(|| anyhow::Error::new(e).context(format!("writing {}", path.display())));
        }
        reads.push(format!("{var}=$(cat {})", shell_quote(&path.to_string_lossy())));
        format!("\"{prefix}${var}\"")
    });
    if let Some(e) = failure {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(e);
    }

    Ok(format!(
        "cd {} && {} && rm -rf {} && {}{}\n",
        shell_quote(&cwd.to_string_lossy()),
        reads.join(" "),
        shell_quote(&dir.to_string_lossy()),
        harness,
        args,
    ))
}

/// The context note delivered to a harness, telling it what `git status`/`git branch`
/// won't: it's sitting in `branch`, a GitButler *virtual* branch, not a real one — every
/// lane in the workspace shares the same checked-out ref, so those commands report that
/// shared state rather than anything specific to `branch`.
pub fn branch_context_note(branch: &str) -> String {
    format!(
        "You're running inside a GitButler virtual branch called \"{branch}\". `git status`/\
         `git branch` reflect the shared workspace checkout, not this branch specifically \
         — use `but status` or `but diff` (GitButler's CLI) to see the real state if you \
         need it."
    )
}

/// Single-quotes a path for the target shell, escaping any embedded `'`.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Double-quotes `s` as a TOML basic string value, escaping `\` and `"` — what Codex's
/// `-c key=value` needs for a string value, on top of (inside) the shell-level quoting
/// `shell_quote` already applies to the whole `-c` argument.
pub fn toml_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Disabled` (`KANSTACK_HARNESS_SYSTEM_FLAG=""`) is the one delivery that sends no
    /// note at all — the launch line is exactly what it would've been before this
    /// feature existed.
    #[test]
    fn build_launch_command_sends_nothing_when_disabled() {
        let launch = build_launch_command(Path::new("/repo"), "claude", &NoteDelivery::Disabled, "feat-x", Some("fix it"));
        assert_eq!(launch, "cd '/repo' && claude 'fix it'\n");
    }

    /// `Flag` puts the note through the configured flag, ahead of the initial message,
    /// which stays a separate, ordinary positional argument.
    #[test]
    fn build_launch_command_passes_the_note_through_a_flag() {
        let delivery = NoteDelivery::Flag("--append-system-prompt".to_string());
        let launch = build_launch_command(Path::new("/repo"), "claude", &delivery, "feat-x", Some("fix it"));
        assert_eq!(
            launch,
            format!(
                "cd '/repo' && claude --append-system-prompt {} 'fix it'\n",
                shell_quote(&branch_context_note("feat-x"))
            )
        );
    }

    /// A `Flag` delivery's note goes out even with no initial message at all — appending
    /// to the system prompt doesn't put words in anyone's mouth the way a positional
    /// prompt argument would, so an otherwise-blank REPL still gets to know where it's
    /// running.
    #[test]
    fn build_launch_command_sends_a_flag_note_even_without_an_initial_message() {
        let delivery = NoteDelivery::Flag("--append-system-prompt".to_string());
        let launch = build_launch_command(Path::new("/repo"), "claude", &delivery, "feat-x", None);
        assert!(
            launch.contains("--append-system-prompt"),
            "the note must go out regardless of whether there's a message: {launch:?}"
        );
        assert!(
            !launch.trim_end().ends_with("--append-system-prompt"),
            "the note's own text must follow the flag: {launch:?}"
        );
    }

    /// `CodexConfig` wraps the note in TOML double-quotes *inside* the shell-quoted
    /// `-c` argument — two independent layers of quoting, not one.
    #[test]
    fn build_launch_command_toml_quotes_the_note_for_codex_config() {
        let launch = build_launch_command(Path::new("/repo"), "codex", &NoteDelivery::CodexConfig, "feat-x", Some("fix it"));
        let expected_arg = shell_quote(&format!(
            "developer_instructions={}",
            toml_quote(&branch_context_note("feat-x"))
        ));
        assert_eq!(launch, format!("cd '/repo' && codex -c {expected_arg} 'fix it'\n"));
    }

    /// `FoldIntoMessage` has no separate channel — the note becomes the start of the
    /// initial message itself, with the original message kept intact after it.
    #[test]
    fn build_launch_command_folds_the_note_onto_an_existing_message() {
        let launch = build_launch_command(Path::new("/repo"), "opencode", &NoteDelivery::FoldIntoMessage, "feat-x", Some("fix it"));
        let expected_message = format!("{}\n\nfix it", branch_context_note("feat-x"));
        assert_eq!(launch, format!("cd '/repo' && opencode {}\n", shell_quote(&expected_message)));
    }

    /// With no initial message to fold onto, the note becomes the whole message — unlike
    /// `Flag`/`CodexConfig`, this is the only delivery that has to spend the "turn" a
    /// positional prompt argument costs, since folding is all it has.
    #[test]
    fn build_launch_command_sends_the_note_alone_when_folding_with_no_message() {
        let launch = build_launch_command(Path::new("/repo"), "opencode", &NoteDelivery::FoldIntoMessage, "feat-x", None);
        assert_eq!(
            launch,
            format!("cd '/repo' && opencode {}\n", shell_quote(&branch_context_note("feat-x")))
        );
    }

    #[test]
    fn branch_context_note_names_the_branch_and_points_at_but() {
        let note = branch_context_note("feat-x");
        assert!(note.contains("\"feat-x\""), "missing the branch name: {note:?}");
        assert!(note.contains("but status"), "missing the but-CLI pointer: {note:?}");
    }

    #[test]
    fn toml_quote_escapes_backslashes_and_quotes() {
        assert_eq!(toml_quote(r#"say "hi" \ bye"#), r#""say \"hi\" \\ bye""#);
    }

    /// Runs `line` under `sh` with `printf` standing in for the harness, so what comes back
    /// is exactly the argument vector a real harness would have received.
    fn args_seen_by_the_harness(line: &str) -> Vec<String> {
        // `sh` and the `cat` in a spilled line are both found through `PATH`, which the
        // backends' discovery tests swap out while holding this lock.
        let _guard = crate::SPLIT_BACKEND_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let out =std::process::Command::new("sh").arg("-c").arg(line).output().unwrap();
        assert!(out.status.success(), "{line:?} failed: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout)
            .unwrap()
            .split('\0')
            .map(str::to_string)
            .collect::<Vec<_>>()
    }

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("kanstack-launch-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A message that would blow the line-length limit, with everything a shell might
    /// mangle in it: quotes of both kinds, `$`, backticks, a backslash, a newline, unicode.
    fn awkward_long_message() -> String {
        format!("it's \"quoted\" $HOME `id` \\ done\nsecond line — ünï {}", "x".repeat(3000))
    }

    #[test]
    fn a_short_launch_line_is_typed_inline_exactly_as_before() {
        let tmp = scratch("short");
        let d = NoteDelivery::Flag("--append-system-prompt".to_string());
        let line = launch_line_in(&tmp, Path::new("/repo"), "claude", &d, "feat-x", Some("fix it")).unwrap();
        assert_eq!(line, build_launch_command(Path::new("/repo"), "claude", &d, "feat-x", Some("fix it")));
        assert_eq!(std::fs::read_dir(&tmp).unwrap().count(), 0, "nothing needs a file");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// The bug this exists for: a long prompt used to be typed whole, and the terminal
    /// dropped everything past about a kilobyte.
    #[test]
    fn a_long_launch_line_stays_short_and_hands_the_harness_the_same_arguments() {
        let tmp = scratch("long");
        let message = awkward_long_message();
        let d = NoteDelivery::Flag("--append-system-prompt".to_string());

        let inline = build_launch_command(Path::new("/tmp"), "printf '%s\\0'", &d, "feat-x", Some(&message));
        let spilled = launch_line_in(&tmp, Path::new("/tmp"), "printf '%s\\0'", &d, "feat-x", Some(&message)).unwrap();

        assert!(inline.len() > MAX_TYPED_LINE, "the fixture must be long enough to matter");
        assert!(spilled.len() < MAX_TYPED_LINE, "typed line is {} bytes: {spilled:?}", spilled.len());
        assert!(!spilled.contains("second line"), "the prompt itself must not be typed");

        let mut expected = args_seen_by_the_harness(&inline);
        let mut got = args_seen_by_the_harness(&spilled);
        assert_eq!(got.pop(), expected.pop()); // trailing empty field after the last NUL
        assert_eq!(got, expected);
        assert_eq!(got.last().unwrap(), &message);

        assert_eq!(std::fs::read_dir(&tmp).unwrap().count(), 0, "the line deletes what it wrote");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// Codex's note is a TOML string behind a fixed `developer_instructions=` prefix, and
    /// that prefix has to stay outside the variable it's joined to.
    #[test]
    fn a_long_launch_line_keeps_codexs_config_prefix_intact() {
        let tmp = scratch("codex");
        let message = awkward_long_message();
        let inline = build_launch_command(Path::new("/tmp"), "printf '%s\\0'", &NoteDelivery::CodexConfig, "b", Some(&message));
        let spilled =
            launch_line_in(&tmp, Path::new("/tmp"), "printf '%s\\0'", &NoteDelivery::CodexConfig, "b", Some(&message)).unwrap();
        assert!(spilled.len() < MAX_TYPED_LINE);
        assert_eq!(args_seen_by_the_harness(&spilled), args_seen_by_the_harness(&inline));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// With nowhere to put the files, say so rather than typing a truncated line.
    #[test]
    fn a_long_launch_line_fails_loudly_if_it_cannot_write_its_files() {
        let missing = std::env::temp_dir().join("kanstack-launch-test-no-such-dir/nested");
        let d = NoteDelivery::Disabled;
        assert!(launch_line_in(&missing, Path::new("/tmp"), "claude", &d, "b", Some(&awkward_long_message())).is_err());
    }
}
