//! Typed bindings for the `but status --format json` wire format.
//!
//! These mirror the *wire* format, not the upstream Rust structs. The two differ in
//! places that matter — `createdAt` is RFC3339 despite a doc comment claiming
//! `"YYYY-MM-DD HH:MM:SS +ZZZZ"`. Everything here was derived from observed output.
//!
//! Verified against 0.21.2. 0.19.3's wire format named the field below `unassignedChanges`
//! instead of `uncommittedChanges`, among other differences (`-j`/`--status-after` instead
//! of `--format json`, a `Branch ID map not found` bug in `branch show`, `but merge`
//! instead of `but land`) — this client no longer supports that release; see `but.rs`.
//!
//! Compatibility policy: unknown *new* fields are ignored (forward compatible), but a
//! renamed or removed field we depend on surfaces as a hard deserialization error rather
//! than a silently half-rendered board. Enums get an `Unknown` fallback so a new variant
//! degrades one card instead of failing the whole refresh.

// These types bind the whole documented payload, including fields the UI does not read
// yet. Keeping them present means a rename upstream fails a test here rather than going
// unnoticed until someone needs the field, so they are contract coverage, not dead code.
#![allow(dead_code)]

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceStatus {
    /// Working-tree changes not assigned to any stack.
    #[serde(default)]
    pub uncommitted_changes: Vec<FileChange>,
    #[serde(default)]
    pub conflicted_files: Vec<String>,
    #[serde(default)]
    pub stacks: Vec<Stack>,
    pub merge_base: Commit,
    pub upstream_state: UpstreamState,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpstreamState {
    pub behind: usize,
    pub latest_commit: Commit,
    pub last_fetched: Option<String>,
    #[serde(default)]
    pub upstream_commits: Option<Vec<Commit>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Stack {
    pub cli_id: String,
    #[serde(default)]
    pub assigned_changes: Vec<FileChange>,
    #[serde(default)]
    pub branches: Vec<Branch>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Branch {
    pub cli_id: String,
    pub name: String,
    #[serde(default)]
    pub commits: Vec<Commit>,
    #[serde(default)]
    pub upstream_commits: Vec<Commit>,
    pub branch_status: BranchStatus,
    pub review_id: Option<String>,
    #[serde(default)]
    pub ci: Option<Ci>,
    /// Omitted entirely when upstream has nothing to report.
    #[serde(default)]
    pub merge_status: Option<MergeStatus>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Commit {
    pub cli_id: String,
    #[serde(default)]
    pub change_id: Option<String>,
    pub commit_id: String,
    /// RFC3339 in practice, e.g. `2026-07-25T02:29:06+00:00`.
    pub created_at: String,
    pub message: String,
    pub author_name: String,
    pub author_email: String,
    #[serde(default)]
    pub conflicted: Option<bool>,
    #[serde(default)]
    pub review_id: Option<String>,
    /// Populated only when status is invoked with `-f`.
    #[serde(default)]
    pub changes: Option<Vec<FileChange>>,
}

impl Commit {
    /// First line of the commit message, which is what a card shows as its title.
    pub fn subject(&self) -> &str {
        self.message.lines().next().unwrap_or("").trim()
    }

    pub fn short_id(&self) -> &str {
        let n = self.commit_id.len().min(7);
        &self.commit_id[..n]
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileChange {
    pub cli_id: String,
    pub file_path: String,
    pub change_type: ChangeType,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ChangeType {
    Added,
    Removed,
    Modified,
    Renamed,
    #[serde(other)]
    Unknown,
}

impl ChangeType {
    pub fn label(self) -> &'static str {
        match self {
            ChangeType::Added => "added",
            ChangeType::Removed => "removed",
            ChangeType::Modified => "modified",
            ChangeType::Renamed => "renamed",
            ChangeType::Unknown => "changed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BranchStatus {
    NothingToPush,
    UnpushedCommits,
    UnpushedCommitsRequiringForce,
    CompletelyUnpushed,
    Integrated,
    #[serde(other)]
    Unknown,
}

impl BranchStatus {
    pub fn label(self) -> &'static str {
        match self {
            // "in sync" was too coy: after pushing, people look for the word "pushed".
            BranchStatus::NothingToPush => "pushed",
            BranchStatus::UnpushedCommits => "unpushed",
            BranchStatus::UnpushedCommitsRequiringForce => "needs force",
            BranchStatus::CompletelyUnpushed => "local only",
            BranchStatus::Integrated => "integrated",
            BranchStatus::Unknown => "unknown",
        }
    }
}

/// Note the asymmetric wire shape: unit variants are bare strings (`"clean"`), but
/// `Conflicted` carries a field and so arrives as `{"conflicted": {"rebasable": true}}`.
/// Serde's default external tagging handles both, which is why this is not `rename_all`-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MergeStatus {
    Clean,
    Integrated,
    Conflicted { rebasable: bool },
    Empty,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Ci {
    #[serde(default)]
    pub pending_check_titles: Vec<String>,
    #[serde(default)]
    pub passing_check_titles: Vec<String>,
    #[serde(default)]
    pub failing_check_titles: Vec<String>,
    pub status: CiStatus,
    pub conclusion: CiConclusion,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CiStatus {
    Complete,
    InProgress,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CiConclusion {
    Failure,
    Success,
    Unknown,
}

/// Output of `but push <branch> --format json`, with or without `--dry-run`.
///
/// The dry-run form is the useful one: it reports exactly what a push would do, including
/// whether it needs a force, without doing it.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PushPreview {
    #[serde(default)]
    pub branches: Vec<PushBranch>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PushBranch {
    pub branch_name: String,
    #[serde(default)]
    pub stack_name: Option<String>,
    #[serde(default)]
    pub unpushed_commits: usize,
    pub remote: String,
    /// Absent until the branch exists on the remote. Untyped rather than `Option<String>`:
    /// verified against 0.21.2, once present it serializes as a JSON array of byte values
    /// instead of a string (an upstream bug) — since only presence is ever checked here,
    /// there is no need to parse its content either way.
    #[serde(default)]
    pub remote_ref: Option<serde_json::Value>,
    #[serde(default)]
    pub commits: Vec<PushCommit>,
    /// Whether the push rewrites remote history. `but push` force-pushes by default, so
    /// this is the flag worth putting in front of someone before they confirm.
    #[serde(default)]
    pub requires_force: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PushCommit {
    pub sha_short: String,
    pub sha: String,
    pub message: String,
}

/// Output of `but diff --format json`, with or without a target.
///
/// The two forms differ in a way that matters: for *uncommitted* changes `but` emits one
/// entry per hunk, each carrying its own `id` that `rub` accepts — which is what makes
/// hunk-level staging possible. For a commit's diff the entries have no ids and may hold
/// several hunks each, because a hunk already in history is not something to stage.
#[derive(Debug, Clone, Deserialize)]
pub struct DiffOutput {
    #[serde(default)]
    pub changes: Vec<DiffChange>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffChange {
    /// Present only for uncommitted changes; this is the hunk's rub handle.
    #[serde(default)]
    pub id: Option<String>,
    pub path: String,
    #[serde(default)]
    pub status: Option<ChangeType>,
    #[serde(default)]
    pub diff: Option<DiffBody>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffBody {
    /// `"patch"` for text. Binary files report something else and carry no hunks.
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub hunks: Vec<Hunk>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Hunk {
    pub old_start: u32,
    pub old_lines: u32,
    pub new_start: u32,
    pub new_lines: u32,
    /// The unified patch text, `@@` header included.
    pub diff: String,
}

/// Output of `but pull --check --format json`: what rebasing onto the updated target would do,
/// without doing it.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullPreview {
    pub base_branch: BaseBranch,
    pub upstream_commits: UpstreamCommits,
    #[serde(default)]
    pub branch_statuses: Vec<PullBranchStatus>,
    pub up_to_date: bool,
    #[serde(default)]
    pub has_worktree_conflicts: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BaseBranch {
    pub name: String,
    pub remote_name: String,
    pub base_sha: String,
    pub current_sha: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpstreamCommits {
    pub count: usize,
    #[serde(default)]
    pub commits: Vec<UpstreamCommit>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpstreamCommit {
    pub id: String,
    pub description: String,
    #[serde(default)]
    pub author_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullBranchStatus {
    pub name: String,
    pub status: PullStatus,
    /// `Some(true)` on a conflicted branch that can still be rebased.
    #[serde(default)]
    pub rebasable: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PullStatus {
    /// Rebases cleanly.
    Updatable,
    /// Already merged into the target; the lane can be deleted afterwards.
    Integrated,
    /// Rebasing produces conflicts.
    Conflicted,
    #[serde(other)]
    Unknown,
}

/// Output of `but branch show <branch> --check --json`: whether landing the branch onto
/// the target would be clean, without doing it. The only preview `but land` itself
/// offers — unlike push and pull, it takes no `--dry-run`. Also the source for the
/// drawer's on-demand branch preview (see `App::open_branch_preview`), since it's the same
/// call either way — just aimed at an unapplied branch instead of an applied one.
///
/// Mixed wire casing, verified against 0.21.2: the top level and `mergeCheck` are
/// camelCase, but each entry in `commits` is snake_case.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MergeCheck {
    pub commits_ahead: usize,
    #[serde(default)]
    pub commits: Vec<MergeCheckCommit>,
    pub merge_check: MergeCheckResult,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MergeCheckCommit {
    pub short_sha: String,
    pub message: String,
    /// Present on the top-level `commits` entries; absent on the ones nested inside a
    /// `ConflictingFile`'s `branch_commits`/`upstream_commits` — `but` doesn't compute a
    /// diffstat for those, so both are `None` there rather than `Some(0)`.
    #[serde(default)]
    pub insertions: Option<usize>,
    #[serde(default)]
    pub deletions: Option<usize>,
}

impl MergeCheckCommit {
    pub fn subject(&self) -> &str {
        self.message.lines().next().unwrap_or("").trim()
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MergeCheckResult {
    pub merges_cleanly: bool,
    #[serde(default)]
    pub conflicting_files: Vec<ConflictingFile>,
}

/// One file `but` found touched on both sides of a would-be merge — verified against
/// 0.22.0, where this is an object (path plus the colliding commits on each side), not the
/// bare path string earlier `but` versions returned. Reusing `MergeCheckCommit` for the two
/// commit lists is safe even though those objects carry more fields (`author_name`,
/// `timestamp`, …) than it reads: unknown fields are ignored by default.
#[derive(Debug, Clone, Deserialize)]
pub struct ConflictingFile {
    pub path: String,
    #[serde(default)]
    pub branch_commits: Vec<MergeCheckCommit>,
    #[serde(default)]
    pub upstream_commits: Vec<MergeCheckCommit>,
}

/// Output of `but resolve conflicts <target> --json`: one commit's conflicts, without
/// entering resolution mode. Casing is mixed on the wire like `MergeCheck` above — the top
/// level is snake_case, `files`/`hunks` entries are camelCase — verified live against
/// 0.22.3. Only the fields the picker actually reads are bound; `but`'s reply also carries
/// per-hunk base text, line numbers and a pre-merged conflict-marker rendering, none of
/// which a plain ours/theirs picker needs.
#[derive(Debug, Clone, Deserialize)]
pub struct ResolveConflicts {
    pub branch: String,
    #[serde(default)]
    pub files: Vec<ConflictFile>,
    /// Conflicts `resolve apply --ours/--theirs` (and `--ai`) both refuse outright —
    /// verified live: a delete/rename conflict fails with "Resolve this commit in edit mode
    /// instead" even under `--ai`. Shown so the picker can say why a file has no ours/theirs
    /// choice, rather than silently omitting it.
    #[serde(default)]
    pub manual: Vec<ManualConflict>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ManualConflict {
    pub path: String,
    pub reason: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ConflictFile {
    pub path: String,
    #[serde(default)]
    pub hunks: Vec<ConflictHunk>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ConflictHunk {
    #[serde(default)]
    pub ours: String,
    #[serde(default)]
    pub theirs: String,
}

/// Output of `but branch list --json`.
///
/// The one command that sees branches the board otherwise cannot: `but status` reports
/// only *applied* stacks, so everything a user has parked — or has never applied at all —
/// exists solely in this payload's `branches`. Kept as a separate call rather than folded
/// into the refresh path because it is materially more expensive: by default it runs a
/// merge check and an ahead-count per branch, so it is fetched on demand when the drawer
/// opens, not on every file save.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BranchList {
    #[serde(default)]
    pub applied_stacks: Vec<AppliedStack>,
    /// The unapplied branches. Named `branches` on the wire despite the applied ones
    /// living in a sibling field, so this is not the whole set of branches — only the
    /// half the board cannot already draw.
    #[serde(default)]
    pub branches: Vec<ListedBranch>,
    /// True when the listing was truncated. `but branch list` shows the active branch plus
    /// the 20 most recent by default; `--all` returns the rest. Surfaced rather than
    /// silently swallowed, so a drawer that is not showing everything says so.
    #[serde(default)]
    pub has_more_branches: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppliedStack {
    pub id: String,
    #[serde(default)]
    pub heads: Vec<ListedBranch>,
}

/// One row of `but branch list`. The same shape serves both applied heads and unapplied
/// branches, but applied heads leave most of it blank — `lastCommitAt` arrives as `0` and
/// `commitsAhead`/`lastAuthor` as null — which is why nearly every field here is optional.
/// Only the unapplied half is ever read.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListedBranch {
    pub name: String,
    #[serde(default)]
    pub reviews: Vec<serde_json::Value>,
    /// Whether a local ref exists, as opposed to remote-only.
    #[serde(default)]
    pub has_local: Option<bool>,
    /// Epoch *milliseconds*, not seconds — and `0` on an applied head, which reports none.
    #[serde(default)]
    pub last_commit_at: i64,
    #[serde(default)]
    pub commits_ahead: Option<usize>,
    #[serde(default)]
    pub last_author: Option<ListedAuthor>,
    /// Whether it would merge cleanly into the upstream base target — *not* into your
    /// workspace. Absent under `--no-check`. This is the field that makes applying a
    /// branch predictable instead of a coin flip, so the drawer leads with it.
    #[serde(default)]
    pub merges_cleanly: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListedAuthor {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
}

/// Structured error payload `but` emits on stdout in `--json` mode, e.g. when the
/// current directory is not a GitButler project.
#[derive(Debug, Clone, Deserialize)]
pub struct CliError {
    pub error: String,
    pub message: String,
    #[serde(default)]
    pub hint: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured verbatim from `but status -f --format json` on a real workspace (but 0.21.2).
    const SAMPLE: &str = include_str!("../tests/fixtures/status.json");

    #[test]
    fn parses_real_status_output() {
        let s: WorkspaceStatus = serde_json::from_str(SAMPLE).expect("sample parses");
        assert_eq!(s.stacks.len(), 3, "sample has three parallel stacks");
        assert_eq!(s.uncommitted_changes.len(), 2);
        let names: Vec<_> = s
            .stacks
            .iter()
            .flat_map(|st| st.branches.iter().map(|b| b.name.as_str()))
            .collect();
        // Wire order (verified against 0.21.2): newest-created stack first. `but::parse_status`
        // reverses this back to oldest-first for callers; this test is pinning the raw
        // contract, not that normalization — see `but::tests::parses_captured_status`.
        assert_eq!(names, ["fix-flaky-tests", "feat-ui", "feat-auth"]);
    }

    #[test]
    fn commit_subject_is_first_line_only() {
        let c: Commit = serde_json::from_value(serde_json::json!({
            "cliId": "ab",
            "commitId": "0123456789abcdef",
            "createdAt": "2026-07-25T02:29:06+00:00",
            "message": "Add auth middleware\n\nLonger body text.\n",
            "authorName": "Dani",
            "authorEmail": "d@e.com",
        }))
        .unwrap();
        assert_eq!(c.subject(), "Add auth middleware");
        assert_eq!(c.short_id(), "0123456");
    }

    #[test]
    fn merge_check_handles_mixed_wire_casing() {
        // Captured verbatim from `but branch show feat-theme --check --json` (but 0.21.2):
        // the envelope is camelCase but each commit entry is snake_case.
        let raw = r#"{
            "branch": "feat-theme",
            "commitsAhead": 1,
            "commits": [{
                "sha": "67d01c027475ec483ec84c6454047eff1c380eb9",
                "short_sha": "67d01c0",
                "message": "theme passthrough from parent terminal",
                "author_name": "dprovder"
            }],
            "unassignedFiles": [],
            "reviews": [],
            "mergeCheck": {"mergesCleanly": true, "conflictingFiles": []}
        }"#;
        let check: MergeCheck = serde_json::from_str(raw).unwrap();
        assert_eq!(check.commits_ahead, 1);
        assert_eq!(check.commits[0].subject(), "theme passthrough from parent terminal");
        assert!(check.merge_check.merges_cleanly);
        assert!(check.merge_check.conflicting_files.is_empty());
    }

    #[test]
    fn merge_check_reports_conflicting_files() {
        // `conflictingFiles` entries are objects (verified against 0.22.0), not bare path
        // strings — each names the commits on both sides that touched the file.
        let raw = r#"{
            "commitsAhead": 2,
            "commits": [],
            "mergeCheck": {"mergesCleanly": false, "conflictingFiles": [{
                "path": "src/app.rs",
                "branch_commits": [{"short_sha": "abc1234", "message": "wip"}],
                "upstream_commits": [
                    {"short_sha": "def5678", "message": "unrelated change"},
                    {"short_sha": "9990000", "message": "another one"}
                ]
            }]}
        }"#;
        let check: MergeCheck = serde_json::from_str(raw).unwrap();
        assert!(!check.merge_check.merges_cleanly);
        let file = &check.merge_check.conflicting_files[0];
        assert_eq!(file.path, "src/app.rs");
        assert_eq!(file.branch_commits.len(), 1);
        assert_eq!(file.upstream_commits.len(), 2);
    }

    /// Captured verbatim from `but branch list --json` on a real workspace (but 0.22.0).
    ///
    /// Both halves matter and they have different shapes: an *unapplied* branch fills in
    /// every field, while an *applied* head reports `lastCommitAt: 0` and nulls for
    /// `commitsAhead`/`lastAuthor` and omits `hasLocal`/`mergesCleanly` outright. That
    /// asymmetry is the reason nearly everything on `ListedBranch` is optional, so the
    /// fixture covers both rather than only the half the drawer reads.
    const BRANCH_LIST: &str = include_str!("../tests/fixtures/branch_list.json");

    #[test]
    fn parses_real_branch_list_output() {
        let l: BranchList = serde_json::from_str(BRANCH_LIST).expect("sample parses");
        assert_eq!(l.branches.len(), 2, "two unapplied branches");
        assert!(!l.has_more_branches);

        let clean = &l.branches[0];
        assert_eq!(clean.name, "feat-theme");
        assert_eq!(clean.commits_ahead, Some(1));
        assert_eq!(clean.merges_cleanly, Some(true));
        assert_eq!(clean.has_local, Some(false));
        assert_eq!(clean.last_commit_at, 1_784_992_235_000);
        assert_eq!(clean.last_author.as_ref().unwrap().name.as_deref(), Some("dprovder"));

        // The branch that would conflict. `but` reports this without applying anything,
        // which is the whole reason the drawer can warn before you commit to an apply.
        assert_eq!(l.branches[1].merges_cleanly, Some(false));
    }

    #[test]
    fn an_applied_head_parses_despite_reporting_almost_nothing() {
        let l: BranchList = serde_json::from_str(BRANCH_LIST).unwrap();
        let head = &l.applied_stacks[0].heads[0];
        assert_eq!(head.name, "new-landing-page");
        // The fields that arrive null or absent on an applied head — each one a hard
        // deserialization failure if its binding were non-optional.
        assert_eq!(head.commits_ahead, None);
        assert_eq!(head.merges_cleanly, None);
        assert_eq!(head.has_local, None);
        assert_eq!(head.last_commit_at, 0);
        assert_eq!(head.last_author.as_ref().unwrap().name, None);
    }

    #[test]
    fn unknown_enum_variants_degrade_instead_of_failing() {
        // A future `but` adding a branch status must not break the whole refresh.
        let b: BranchStatus = serde_json::from_str("\"someFutureStatus\"").unwrap();
        assert_eq!(b, BranchStatus::Unknown);
    }

    #[test]
    fn merge_status_accepts_both_wire_shapes() {
        let clean: MergeStatus = serde_json::from_str("\"clean\"").unwrap();
        assert_eq!(clean, MergeStatus::Clean);
        let conflicted: MergeStatus =
            serde_json::from_str(r#"{"conflicted":{"rebasable":true}}"#).unwrap();
        assert_eq!(conflicted, MergeStatus::Conflicted { rebasable: true });
    }

    #[test]
    fn unknown_new_fields_are_ignored() {
        let mut v: serde_json::Value = serde_json::from_str(SAMPLE).unwrap();
        v["someFieldFromTheFuture"] = serde_json::json!(42);
        serde_json::from_value::<WorkspaceStatus>(v).expect("forward compatible");
    }

    #[test]
    fn missing_required_field_is_a_hard_error() {
        let mut v: serde_json::Value = serde_json::from_str(SAMPLE).unwrap();
        v.as_object_mut().unwrap().remove("mergeBase");
        assert!(
            serde_json::from_value::<WorkspaceStatus>(v).is_err(),
            "a removed field we depend on must fail loudly, not render a half board"
        );
    }
}
