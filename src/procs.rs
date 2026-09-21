//! What can be learned about a pane from the process table alone, for a multiplexer that
//! can't say for itself whether a pane's harness is busy, idle or still there — see
//! [`crate::mux::Multiplexer::tracks_pids`]. `crate::tmux` reads the same table for the same
//! CPU sum, so the helpers live here rather than in either.
//!
//! One `ps -A -o pid=,ppid=,pcpu=` call yields every process's parent and CPU%, and from
//! that the CPU of any process's whole subtree, or whether it is there at all.

use std::process::Command;

use anyhow::{bail, Context, Result};

/// One row of `ps -A -o pid=,ppid=,pcpu=` output: a process, its parent, and its CPU%.
pub type PsRow = (u32, u32, f64);

/// Parses `ps -A -o pid=,ppid=,pcpu=` output (the `=` suffixes suppress the header row,
/// portable across macOS/BSD and GNU `ps`) into `(pid, ppid, cpu_percent)` rows.
pub fn parse_ps_table(text: &str) -> Vec<PsRow> {
    text.lines()
        .filter_map(|line| {
            let mut cols = line.split_whitespace();
            let pid = cols.next()?.parse().ok()?;
            let ppid = cols.next()?.parse().ok()?;
            let cpu = cols.next()?.parse().ok()?;
            Some((pid, ppid, cpu))
        })
        .collect()
}

pub fn read_ps_table() -> Result<Vec<PsRow>> {
    let out = Command::new("ps")
        .args(["-A", "-o", "pid=,ppid=,pcpu="])
        .output()
        .context("failed to spawn `ps`")?;
    if !out.status.success() {
        bail!("`ps` failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(parse_ps_table(&String::from_utf8_lossy(&out.stdout)))
}

/// Sums CPU% over `root_pid` and every one of its descendants in `table` — a harness
/// process (e.g. `claude`) that spends its CPU in child processes rather than itself would
/// otherwise read as idle even mid-generation.
pub fn subtree_cpu(root_pid: u32, table: &[PsRow]) -> f64 {
    let mut total = 0.0;
    let mut frontier = vec![root_pid];
    while let Some(pid) = frontier.pop() {
        for &(row_pid, ppid, cpu) in table {
            if row_pid == pid {
                total += cpu;
            }
            if ppid == pid && row_pid != pid {
                frontier.push(row_pid);
            }
        }
    }
    total
}

/// Whether `pid` is in `table` at all — the process is still running, or at least still has
/// an entry (a zombie awaiting its parent's `wait` does, and so reads as present).
pub fn pid_present(pid: u32, table: &[PsRow]) -> bool {
    table.iter().any(|&(row_pid, _, _)| row_pid == pid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ps_table_reads_pid_ppid_cpu() {
        let table = parse_ps_table("  111   1   0.0\n  222 111  35.4\n");
        assert_eq!(table, vec![(111, 1, 0.0), (222, 111, 35.4)]);
    }

    /// The harness's own CPU can sit near zero while it's actually busy generating, if all
    /// the work happens in a child process — a real shape for e.g. a harness that shells
    /// out to a language server or a build. Summing over the whole subtree catches that;
    /// looking at `root_pid` alone would miss it.
    #[test]
    fn subtree_cpu_sums_a_harnesss_child_processes_too() {
        // shell(pid 100) -> claude(pid 200) -> ripgrep(pid 300), all descendants of 100.
        let table = vec![(100, 1, 0.1), (200, 100, 0.5), (300, 200, 40.0)];
        assert_eq!(subtree_cpu(100, &table), 40.6);
    }

    #[test]
    fn subtree_cpu_ignores_unrelated_processes() {
        let table = vec![(100, 1, 5.0), (999, 1, 90.0)];
        assert_eq!(subtree_cpu(100, &table), 5.0);
    }

    #[test]
    fn pid_present_is_whether_the_table_lists_it_as_a_process_not_a_parent() {
        let table = vec![(100, 1, 0.0), (200, 100, 0.0)];
        assert!(pid_present(100, &table));
        assert!(pid_present(200, &table));
        assert!(!pid_present(1, &table), "init is only named as a parent here, it has no row");
        assert!(!pid_present(300, &table));
        assert!(!pid_present(100, &[]));
    }
}
