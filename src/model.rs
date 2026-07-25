//! Typed bindings for the `but status --json` wire format.
//!
//! These mirror the *wire* format, not the upstream Rust structs. The two differ in
//! places that matter — upstream's `uncommitted_changes` field is emitted as
//! `unassignedChanges`, and `createdAt` is RFC3339 despite a doc comment claiming
//! `"YYYY-MM-DD HH:MM:SS +ZZZZ"`. Everything here was derived from observed output.
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
    /// Working-tree changes not assigned to any stack. Upstream calls this
    /// `uncommitted_changes` internally; the wire name is `unassignedChanges`.
    #[serde(default)]
    pub unassigned_changes: Vec<FileChange>,
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
            BranchStatus::NothingToPush => "in sync",
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

/// Output of `but push <branch> -j`, with or without `--dry-run`.
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
    /// Absent until the branch exists on the remote.
    #[serde(default)]
    pub remote_ref: Option<String>,
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

/// Structured error payload `but` emits on stdout in `--json` mode, e.g. when the
/// current directory is not a GitButler project.
#[derive(Debug, Clone, Deserialize)]
pub struct CliError {
    pub error: String,
    pub message: String,
    #[serde(default)]
    pub hint: Option<String>,
}

/// Envelope produced by mutation commands run with `--status-after`.
#[derive(Debug, Clone, Deserialize)]
pub struct MutationEnvelope {
    #[serde(default)]
    pub result: Option<serde_json::Value>,
    #[serde(default)]
    pub status: Option<WorkspaceStatus>,
    /// Upstream documents this key as snake_case while the rest of the payload is
    /// camelCase, so accept either spelling.
    #[serde(default, alias = "statusError")]
    pub status_error: Option<serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured verbatim from `but status -f -j` on a real workspace (but 0.19.3).
    const SAMPLE: &str = include_str!("../tests/fixtures/status.json");

    #[test]
    fn parses_real_status_output() {
        let s: WorkspaceStatus = serde_json::from_str(SAMPLE).expect("sample parses");
        assert_eq!(s.stacks.len(), 3, "sample has three parallel stacks");
        assert_eq!(
            s.unassigned_changes.len(),
            2,
            "unassignedChanges is read despite upstream naming the field uncommitted_changes"
        );
        let names: Vec<_> = s
            .stacks
            .iter()
            .flat_map(|st| st.branches.iter().map(|b| b.name.as_str()))
            .collect();
        assert_eq!(names, ["feat-auth", "feat-ui", "fix-flaky-tests"]);
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
