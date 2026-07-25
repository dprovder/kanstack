//! The diff view: a flat list of hunks, and how to colour them.
//!
//! Both shapes `but diff` produces are flattened into the same list of entries, so the
//! renderer does not care whether it is looking at uncommitted work or a commit. The one
//! difference that survives is [`Entry::rub_id`]: uncommitted hunks carry a handle that
//! `rub` accepts, which is what lets a single file be split across lanes. Committed hunks
//! do not, and are read-only.

use crate::model::DiffOutput;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Added,
    Removed,
    Context,
    /// The `@@ -a,b +c,d @@` header.
    Header,
}

#[derive(Debug, Clone)]
pub struct DiffLine {
    pub kind: LineKind,
    pub text: String,
    /// Line number in the new file, absent for removed lines and headers.
    pub new_no: Option<u32>,
}

/// One hunk, with its file and its lines.
#[derive(Debug, Clone)]
pub struct Entry {
    pub path: String,
    /// `Some` when this hunk can be staged; `None` for anything already committed.
    pub rub_id: Option<String>,
    pub lines: Vec<DiffLine>,
    pub added: usize,
    pub removed: usize,
}

#[derive(Debug, Clone)]
pub struct DiffView {
    pub title: String,
    pub entries: Vec<Entry>,
    /// Which hunk the cursor is on.
    pub cursor: usize,
}

impl DiffView {
    pub fn from_output(title: impl Into<String>, out: &DiffOutput) -> DiffView {
        let mut entries = Vec::new();
        for change in &out.changes {
            let Some(body) = &change.diff else { continue };
            for hunk in &body.hunks {
                let lines = parse_hunk(&hunk.diff, hunk.new_start);
                let added = lines.iter().filter(|l| l.kind == LineKind::Added).count();
                let removed = lines.iter().filter(|l| l.kind == LineKind::Removed).count();
                entries.push(Entry {
                    path: change.path.clone(),
                    rub_id: change.id.clone(),
                    lines,
                    added,
                    removed,
                });
            }
            // A binary file has no hunks; say so rather than showing nothing.
            if body.hunks.is_empty() {
                entries.push(Entry {
                    path: change.path.clone(),
                    rub_id: change.id.clone(),
                    lines: vec![DiffLine {
                        kind: LineKind::Context,
                        text: "  (no textual diff)".into(),
                        new_no: None,
                    }],
                    added: 0,
                    removed: 0,
                });
            }
        }
        DiffView {
            title: title.into(),
            entries,
            cursor: 0,
        }
    }

    pub fn selected(&self) -> Option<&Entry> {
        self.entries.get(self.cursor)
    }

    /// Moves between hunks, wrapping like everything else on the board.
    pub fn move_cursor(&mut self, delta: isize) {
        let n = self.entries.len();
        if n == 0 {
            return;
        }
        self.cursor = (self.cursor as isize + delta).rem_euclid(n as isize) as usize;
    }

    pub fn totals(&self) -> (usize, usize) {
        (
            self.entries.iter().map(|e| e.added).sum(),
            self.entries.iter().map(|e| e.removed).sum(),
        )
    }
}

/// Splits unified patch text into typed lines, tracking new-file line numbers.
fn parse_hunk(patch: &str, new_start: u32) -> Vec<DiffLine> {
    let mut out = Vec::new();
    let mut no = new_start;
    for raw in patch.lines() {
        let (kind, text) = match raw.as_bytes().first() {
            Some(b'@') => (LineKind::Header, raw.to_string()),
            Some(b'+') => (LineKind::Added, raw[1..].to_string()),
            Some(b'-') => (LineKind::Removed, raw[1..].to_string()),
            // A bare empty line inside a patch is an empty context line.
            None => (LineKind::Context, String::new()),
            _ => (LineKind::Context, raw[1..].to_string()),
        };
        let new_no = match kind {
            LineKind::Added | LineKind::Context => {
                let n = no;
                no += 1;
                Some(n)
            }
            _ => None,
        };
        out.push(DiffLine { kind, text, new_no });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> DiffOutput {
        serde_json::from_str(
            r#"{"changes":[
              {"id":"h0","path":"a.txt","status":"modified","diff":{"type":"patch","hunks":[
                {"oldStart":1,"oldLines":4,"newStart":1,"newLines":4,
                 "diff":"@@ -1,4 +1,4 @@\n-one\n+ONE\n two\n three\n"}]}},
              {"id":"i0","path":"a.txt","status":"modified","diff":{"type":"patch","hunks":[
                {"oldStart":7,"oldLines":2,"newStart":7,"newLines":2,
                 "diff":"@@ -7,2 +7,2 @@\n seven\n-ten\n+TEN\n"}]}}
            ]}"#,
        )
        .unwrap()
    }

    #[test]
    fn each_hunk_becomes_its_own_stageable_entry() {
        let v = DiffView::from_output("a.txt", &sample());
        assert_eq!(v.entries.len(), 2, "two hunks of the same file, listed apart");
        assert_eq!(v.entries[0].rub_id.as_deref(), Some("h0"));
        assert_eq!(v.entries[1].rub_id.as_deref(), Some("i0"));
        assert!(v.entries.iter().all(|e| e.path == "a.txt"));
    }

    #[test]
    fn lines_are_classified_and_counted() {
        let v = DiffView::from_output("a.txt", &sample());
        let e = &v.entries[0];
        assert_eq!(e.added, 1);
        assert_eq!(e.removed, 1);
        assert_eq!(e.lines[0].kind, LineKind::Header);
        assert_eq!(e.lines[1].kind, LineKind::Removed);
        assert_eq!(e.lines[1].text, "one");
        assert_eq!(e.lines[2].kind, LineKind::Added);
        assert_eq!(e.lines[2].text, "ONE");
        assert_eq!(e.lines[3].kind, LineKind::Context);
        assert_eq!(e.lines[3].text, "two");
        assert_eq!(v.totals(), (2, 2));
    }

    #[test]
    fn new_file_line_numbers_skip_removed_lines() {
        let v = DiffView::from_output("a.txt", &sample());
        let nos: Vec<Option<u32>> = v.entries[0].lines.iter().map(|l| l.new_no).collect();
        // header, -one, +ONE, two, three
        assert_eq!(nos, [None, None, Some(1), Some(2), Some(3)]);
    }

    #[test]
    fn a_committed_diff_has_no_stageable_hunks() {
        // Commit diffs omit `id`, so nothing in them can be rubbed.
        let out: DiffOutput = serde_json::from_str(
            r#"{"changes":[{"path":"a.txt","status":"modified","diff":{"type":"patch","hunks":[
                {"oldStart":1,"oldLines":1,"newStart":1,"newLines":1,"diff":"@@ -1 +1 @@\n-a\n+b\n"}]}}]}"#,
        )
        .unwrap();
        let v = DiffView::from_output("commit", &out);
        assert_eq!(v.entries.len(), 1);
        assert!(v.entries[0].rub_id.is_none(), "history is not stageable");
    }

    #[test]
    fn a_binary_change_still_lists_the_file() {
        let out: DiffOutput = serde_json::from_str(
            r#"{"changes":[{"id":"b0","path":"logo.png","status":"modified",
                "diff":{"type":"binary","hunks":[]}}]}"#,
        )
        .unwrap();
        let v = DiffView::from_output("x", &out);
        assert_eq!(v.entries.len(), 1, "a binary file is not silently dropped");
        assert_eq!(v.entries[0].path, "logo.png");
    }

    #[test]
    fn the_hunk_cursor_wraps() {
        let mut v = DiffView::from_output("a.txt", &sample());
        assert_eq!(v.cursor, 0);
        v.move_cursor(1);
        assert_eq!(v.cursor, 1);
        v.move_cursor(1);
        assert_eq!(v.cursor, 0, "wraps like lane and card movement");
        v.move_cursor(-1);
        assert_eq!(v.cursor, 1);
    }
}
