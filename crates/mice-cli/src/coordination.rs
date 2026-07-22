//! Read-only repository/worktree discovery for MICE Coordination Mesh P0.
//!
//! This module intentionally records only Git metadata: repository identity,
//! worktree locations, branch names, commit identifiers, and Git's worktree
//! state flags. It never reads or persists source text, diffs, commands,
//! agent transcripts, captures, clipboard contents, credentials, or config.

use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeRecord {
    pub path: String,
    pub head: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default)]
    pub bare: bool,
    #[serde(default)]
    pub locked: bool,
    #[serde(default)]
    pub prunable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoSnapshot {
    /// SHA-256 of Git's canonical common-directory path. The path itself is
    /// deliberately not persisted as the repository identity.
    pub repo_id: String,
    pub captured_at: u64,
    pub worktrees: Vec<WorktreeRecord>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RiskLevel {
    Yellow,
    Red,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeRisk {
    pub level: RiskLevel,
    pub left_branch: String,
    pub right_branch: String,
    pub path: String,
    left_ranges: Vec<LineRange>,
    right_ranges: Vec<LineRange>,
    overlaps: Vec<(LineRange, LineRange)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnassessedPair {
    pub left_branch: String,
    pub right_branch: String,
    pub reason: String,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RiskReport {
    pub risks: Vec<MergeRisk>,
    pub unassessed: Vec<UnassessedPair>,
    pub suppressed_paths: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LineRange {
    start: u32,
    count: u32,
}

/// Owner-only local snapshot storage. This directory is outside the
/// repository, in keeping with MICE's no-user-state-in-git policy.
#[derive(Debug, Clone)]
pub struct SnapshotStore {
    root: PathBuf,
}

impl SnapshotStore {
    pub fn at(root: PathBuf) -> Result<Self, std::io::Error> {
        fs::create_dir_all(&root)?;
        restrict_directory_to_user(&root)?;
        Ok(Self { root })
    }

    pub fn default_path() -> Option<PathBuf> {
        std::env::var_os("HOME")
            .map(|home| PathBuf::from(home).join("Library/Application Support/MICE/coordination"))
    }

    /// Atomically replace the most recent metadata-only snapshot for this
    /// repository. Snapshots are intentionally a single bounded current view,
    /// not an ever-growing activity log.
    pub fn record(&self, snapshot: &RepoSnapshot) -> Result<PathBuf, std::io::Error> {
        let file = self.root.join(format!("{}.json", snapshot.repo_id));
        let temporary = self
            .root
            .join(format!(".{}.{}.tmp", snapshot.repo_id, std::process::id()));
        let bytes = serde_json::to_vec_pretty(snapshot).map_err(std::io::Error::other)?;
        let mut writer = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        writer.write_all(&bytes)?;
        writer.write_all(b"\n")?;
        writer.sync_all()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))?;
        }
        fs::rename(&temporary, &file)?;
        Ok(file)
    }
}

pub fn discover(cwd: &Path) -> Result<RepoSnapshot, std::io::Error> {
    let common_dir = git_output(cwd, &["rev-parse", "--git-common-dir"])?;
    let common_dir = resolve_git_path(cwd, &common_dir)?;
    let repo_id = digest_path(&common_dir);
    let output = git_output(cwd, &["worktree", "list", "--porcelain"])?;
    let worktrees = parse_worktree_porcelain(&output)?;
    if worktrees.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Git reported no worktrees for this repository.",
        ));
    }
    Ok(RepoSnapshot {
        repo_id,
        captured_at: now(),
        worktrees,
    })
}

pub fn render_status(snapshot: &RepoSnapshot, storage: &Path) -> String {
    let mut lines = vec![
        "MICE Coordination Mesh — read-only P0 snapshot".into(),
        format!("Repository: {}", short_id(&snapshot.repo_id)),
        format!("Worktrees: {}", snapshot.worktrees.len()),
        format!("Snapshot: {}", storage.display()),
        String::new(),
    ];
    for worktree in &snapshot.worktrees {
        let branch = worktree.branch.as_deref().unwrap_or("detached HEAD");
        let mut state = Vec::new();
        if worktree.bare {
            state.push("bare");
        }
        if worktree.locked {
            state.push("locked");
        }
        if worktree.prunable {
            state.push("prunable");
        }
        let suffix = (!state.is_empty()).then(|| format!(" [{}]", state.join(", ")));
        lines.push(format!(
            "- {branch} at {} ({}){}",
            worktree.path,
            short_id(&worktree.head),
            suffix.unwrap_or_default()
        ));
    }
    lines.push(String::new());
    lines.push(
        "P0 is observation only: no agents were launched, messaged, stopped, or reassigned; no Git branch or worktree was changed.".into(),
    );
    lines.join("\n")
}

/// Compare each active worktree against every other one. Git supplies the
/// merge base and the current worktree diff supplies ranges; source text is
/// retained only briefly in the subprocess pipe and is never persisted.
///
/// A yellow result means the worktrees both change a file but currently touch
/// disjoint base ranges. Red means at least one base range intersects. Neither
/// result promises a semantic merge outcome; a pair whose diff is too large or
/// unavailable is explicitly reported as unassessed instead of green.
pub fn merge_risks(snapshot: &RepoSnapshot) -> RiskReport {
    let mut report = RiskReport::default();
    if snapshot.worktrees.len() > MAX_WORKTREES {
        report.unassessed.push(UnassessedPair {
            left_branch: "repository worktrees".into(),
            right_branch: "risk scan".into(),
            reason: format!(
                "{} worktrees exceed the {MAX_WORKTREES}-worktree read-only analysis limit",
                snapshot.worktrees.len()
            ),
        });
        return report;
    }
    for left_index in 0..snapshot.worktrees.len() {
        for right_index in (left_index + 1)..snapshot.worktrees.len() {
            let left = &snapshot.worktrees[left_index];
            let right = &snapshot.worktrees[right_index];
            let left_branch = display_branch(left);
            let right_branch = display_branch(right);
            if left.bare || right.bare {
                report.unassessed.push(UnassessedPair {
                    left_branch,
                    right_branch,
                    reason: "a bare worktree has no editable checkout".into(),
                });
                continue;
            }
            let assessed = (|| -> Result<Vec<MergeRisk>, String> {
                if left.head == "unknown" || right.head == "unknown" {
                    return Err("Git did not report both worktree commits".into());
                }
                let base = git_output(
                    Path::new(&left.path),
                    &["merge-base", &left.head, &right.head],
                )
                .map_err(|error| error.to_string())?;
                let left_diff =
                    diff_hunks(Path::new(&left.path), &base).map_err(|error| error.to_string())?;
                let right_diff =
                    diff_hunks(Path::new(&right.path), &base).map_err(|error| error.to_string())?;
                if left_diff.truncated || right_diff.truncated {
                    return Err(format!(
                        "a diff exceeded the {} KiB read-only analysis limit",
                        MAX_DIFF_ANALYSIS_BYTES / 1024
                    ));
                }
                if left_diff.malformed_hunk || right_diff.malformed_hunk {
                    return Err(
                        "Git returned an unparseable hunk header; this pair was not assessed"
                            .into(),
                    );
                }
                Ok(classify_pair(
                    &left_branch,
                    &right_branch,
                    &left_diff.ranges,
                    &right_diff.ranges,
                ))
            })();
            match assessed {
                Ok(risks) => {
                    for risk in risks {
                        if should_suppress_risk(&risk) {
                            report.suppressed_paths.push(risk.path);
                        } else {
                            report.risks.push(risk);
                        }
                    }
                }
                Err(reason) => report.unassessed.push(UnassessedPair {
                    left_branch,
                    right_branch,
                    reason,
                }),
            }
        }
    }
    report.suppressed_paths.sort();
    report.suppressed_paths.dedup();
    report
}

pub fn render_risks(report: &RiskReport) -> String {
    let mut lines = vec!["MICE Coordination Mesh — read-only hunk-risk report".into()];
    if report.risks.is_empty() {
        lines.push(
            "No shared changed-file surfaces were detected among assessed worktree pairs.".into(),
        );
    } else {
        for risk in &report.risks {
            let level = match risk.level {
                RiskLevel::Yellow => "YELLOW",
                RiskLevel::Red => "RED",
            };
            let evidence = match risk.level {
                RiskLevel::Yellow => format!(
                    "same file, currently disjoint base ranges ({} vs {})",
                    risk.left_ranges.len(),
                    risk.right_ranges.len()
                ),
                RiskLevel::Red => format!(
                    "{} intersecting base edit range{} ({})",
                    risk.overlaps.len(),
                    if risk.overlaps.len() == 1 { "" } else { "s" },
                    render_overlap_samples(&risk.overlaps)
                ),
            };
            lines.push(format!(
                "- {level}: {} — {} ↔ {}; {evidence}",
                risk.path, risk.left_branch, risk.right_branch
            ));
        }
    }
    for pair in &report.unassessed {
        lines.push(format!(
            "- UNASSESSED: {} ↔ {}; {}",
            pair.left_branch, pair.right_branch, pair.reason
        ));
    }
    if !report.suppressed_paths.is_empty() {
        lines.push(format!(
            "- SUPPRESSED: {} low-signal generated lockfile path{} ({})",
            report.suppressed_paths.len(),
            if report.suppressed_paths.len() == 1 {
                ""
            } else {
                "s"
            },
            report.suppressed_paths.join(", ")
        ));
    }
    lines.push(
        "This report predicts textual edit-surface risk only. It does not perform a merge, read source into persistent storage, or determine semantic correctness.".into(),
    );
    lines.join("\n")
}

const MAX_GIT_METADATA_BYTES: usize = 256 * 1024;
const MAX_DIFF_ANALYSIS_BYTES: usize = 2 * 1024 * 1024;
const MAX_WORKTREES: usize = 16;
const GIT_COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

fn git_output(cwd: &Path, arguments: &[&str]) -> Result<String, std::io::Error> {
    let output = git_output_bounded(cwd, arguments, MAX_GIT_METADATA_BYTES)?;
    if output.truncated {
        return Err(std::io::Error::other(
            "Git metadata exceeded MICE's read-only analysis limit.",
        ));
    }
    Ok(output.text.trim().to_owned())
}

#[derive(Debug, Default)]
struct DiffHunks {
    ranges: BTreeMap<String, Vec<LineRange>>,
    truncated: bool,
    malformed_hunk: bool,
}

fn diff_hunks(cwd: &Path, base: &str) -> Result<DiffHunks, std::io::Error> {
    let output = git_output_bounded(
        cwd,
        &[
            "-c",
            "core.quotepath=false",
            "diff",
            "--no-ext-diff",
            "--no-color",
            "--unified=0",
            "--find-renames",
            base,
            "--",
        ],
        MAX_DIFF_ANALYSIS_BYTES,
    )?;
    let parsed = parse_diff_hunks(&output.text);
    Ok(DiffHunks {
        ranges: parsed.ranges,
        truncated: output.truncated,
        malformed_hunk: parsed.malformed_hunk,
    })
}

struct BoundedGitOutput {
    text: String,
    truncated: bool,
}

fn git_output_bounded(
    cwd: &Path,
    arguments: &[&str],
    maximum: usize,
) -> Result<BoundedGitOutput, std::io::Error> {
    let mut child = Command::new("git")
        .args(arguments)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("Git did not expose its output."))?;
    let reader = thread::spawn(move || read_bounded(stdout, maximum));
    let deadline = Instant::now() + GIT_COMMAND_TIMEOUT;
    loop {
        if let Some(status) = child.try_wait()? {
            let (bytes, truncated) = reader
                .join()
                .map_err(|_| std::io::Error::other("Git output reader stopped unexpectedly."))??;
            if !status.success() {
                return Err(std::io::Error::other(
                    "Git could not inspect worktree changes.",
                ));
            }
            return Ok(BoundedGitOutput {
                text: String::from_utf8_lossy(&bytes).into_owned(),
                truncated,
            });
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "Git inspection exceeded MICE's 5 second read-only timeout.",
            ));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn read_bounded(mut reader: impl Read, maximum: usize) -> Result<(Vec<u8>, bool), std::io::Error> {
    let mut bytes = Vec::with_capacity(maximum.min(16 * 1024));
    let mut buffer = [0_u8; 16 * 1024];
    let mut truncated = false;
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            return Ok((bytes, truncated));
        }
        let remaining = maximum.saturating_sub(bytes.len());
        let kept = read.min(remaining);
        bytes.extend_from_slice(&buffer[..kept]);
        truncated |= kept < read;
    }
}

#[derive(Debug, Default)]
struct ParsedDiffHunks {
    ranges: BTreeMap<String, Vec<LineRange>>,
    malformed_hunk: bool,
}

fn parse_diff_hunks(output: &str) -> ParsedDiffHunks {
    let mut parsed = ParsedDiffHunks::default();
    let mut old_path: Option<String> = None;
    let mut current_path: Option<String> = None;
    for line in output.lines() {
        if line.starts_with("diff --git ") {
            old_path = None;
            current_path = None;
        } else if let Some(path) = line.strip_prefix("--- ") {
            old_path = normalized_diff_path(path);
        } else if let Some(path) = line.strip_prefix("+++ ") {
            current_path = normalized_diff_path(path).or_else(|| old_path.clone());
        } else if line.starts_with("@@ ") {
            let Some(range) = parse_old_hunk_range(line) else {
                parsed.malformed_hunk = true;
                continue;
            };
            let Some(path) = current_path.as_ref() else {
                parsed.malformed_hunk = true;
                continue;
            };
            // `--find-renames` reports a rename's old and new path in the
            // ---/+++ headers. Index this base-relative hunk under both
            // aliases, so edits in another worktree using either name are
            // treated as one merge surface.
            parsed.ranges.entry(path.clone()).or_default().push(range);
            if let Some(old_path) = old_path.as_ref().filter(|old| *old != path) {
                parsed
                    .ranges
                    .entry(old_path.clone())
                    .or_default()
                    .push(range);
            }
        }
    }
    parsed
}

fn normalized_diff_path(value: &str) -> Option<String> {
    let value = value.trim_matches('"');
    if value == "/dev/null" {
        return None;
    }
    Some(
        value
            .strip_prefix("a/")
            .or_else(|| value.strip_prefix("b/"))
            .unwrap_or(value)
            .into(),
    )
}

fn parse_old_hunk_range(line: &str) -> Option<LineRange> {
    let body = line.strip_prefix("@@ ")?.strip_suffix(" @@").or_else(|| {
        line.strip_prefix("@@ ")
            .and_then(|body| body.split(" @@").next())
    })?;
    let old = body.split_whitespace().next()?.strip_prefix('-')?;
    let (start, count) = match old.split_once(',') {
        Some((start, count)) => (start.parse().ok()?, count.parse().ok()?),
        None => (old.parse().ok()?, 1),
    };
    Some(LineRange { start, count })
}

fn classify_pair(
    left_branch: &str,
    right_branch: &str,
    left: &BTreeMap<String, Vec<LineRange>>,
    right: &BTreeMap<String, Vec<LineRange>>,
) -> Vec<MergeRisk> {
    let mut risks = Vec::new();
    for (path, left_ranges) in left {
        let Some(right_ranges) = right.get(path) else {
            continue;
        };
        let overlaps = left_ranges
            .iter()
            .flat_map(|left| {
                right_ranges.iter().filter_map(move |right| {
                    ranges_overlap(*left, *right).then_some((*left, *right))
                })
            })
            .collect::<Vec<_>>();
        let level = if overlaps.is_empty() {
            RiskLevel::Yellow
        } else {
            RiskLevel::Red
        };
        risks.push(MergeRisk {
            level,
            left_branch: left_branch.into(),
            right_branch: right_branch.into(),
            path: path.into(),
            left_ranges: left_ranges.clone(),
            right_ranges: right_ranges.clone(),
            overlaps,
        });
    }
    risks
}

fn ranges_overlap(left: LineRange, right: LineRange) -> bool {
    match (left.count, right.count) {
        (0, 0) => left.start == right.start,
        (0, _) => insertion_touches(left.start, right),
        (_, 0) => insertion_touches(right.start, left),
        _ => {
            let left_end = left.start.saturating_add(left.count);
            let right_end = right.start.saturating_add(right.count);
            left.start < right_end && right.start < left_end
        }
    }
}

fn insertion_touches(position: u32, range: LineRange) -> bool {
    position >= range.start && position <= range.start.saturating_add(range.count)
}

fn render_overlap_samples(overlaps: &[(LineRange, LineRange)]) -> String {
    let examples = overlaps
        .iter()
        .take(3)
        .map(|(left, right)| format!("{} ↔ {}", render_range(*left), render_range(*right)))
        .collect::<Vec<_>>()
        .join(", ");
    if overlaps.len() > 3 {
        format!("{examples}; +{} more", overlaps.len() - 3)
    } else {
        examples
    }
}

fn render_range(range: LineRange) -> String {
    if range.count == 0 {
        format!("insert@{}", range.start)
    } else {
        format!("{}+{}", range.start, range.count)
    }
}

fn display_branch(worktree: &WorktreeRecord) -> String {
    worktree
        .branch
        .clone()
        .unwrap_or_else(|| format!("detached:{}", short_id(&worktree.head)))
}

fn is_low_signal_generated_path(path: &str) -> bool {
    matches!(
        path.rsplit('/').next(),
        Some("Cargo.lock" | "package-lock.json" | "pnpm-lock.yaml" | "yarn.lock")
    )
}

fn should_suppress_risk(risk: &MergeRisk) -> bool {
    risk.level == RiskLevel::Yellow && is_low_signal_generated_path(&risk.path)
}

fn resolve_git_path(cwd: &Path, value: &str) -> Result<PathBuf, std::io::Error> {
    let path = PathBuf::from(value);
    let path = if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    };
    path.canonicalize()
}

fn digest_path(path: &Path) -> String {
    let mut digest = Sha256::new();
    digest.update(path.to_string_lossy().as_bytes());
    format!("{:x}", digest.finalize())
}

fn short_id(value: &str) -> &str {
    value.get(..12).unwrap_or(value)
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn restrict_directory_to_user(path: &Path) -> Result<(), std::io::Error> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Parse `git worktree list --porcelain` without interpreting source files or
/// executing any Git operation that mutates state.
fn parse_worktree_porcelain(output: &str) -> Result<Vec<WorktreeRecord>, std::io::Error> {
    let mut records = Vec::new();
    let mut current: Option<WorktreeRecord> = None;
    for line in output.lines() {
        if line.is_empty() {
            if let Some(record) = current.take() {
                records.push(record);
            }
            continue;
        }
        if let Some(path) = line.strip_prefix("worktree ") {
            if let Some(record) = current.take() {
                records.push(record);
            }
            current = Some(WorktreeRecord {
                path: path.into(),
                head: "unknown".into(),
                branch: None,
                bare: false,
                locked: false,
                prunable: false,
            });
            continue;
        }
        let record = current.as_mut().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Git worktree data started without a worktree path.",
            )
        })?;
        if let Some(head) = line.strip_prefix("HEAD ") {
            record.head = head.into();
        } else if let Some(branch) = line.strip_prefix("branch ") {
            record.branch = Some(branch.strip_prefix("refs/heads/").unwrap_or(branch).into());
        } else if line == "bare" {
            record.bare = true;
        } else if line == "locked" || line.starts_with("locked ") {
            record.locked = true;
        } else if line == "prunable" || line.starts_with("prunable ") {
            record.prunable = true;
        }
    }
    if let Some(record) = current {
        records.push(record);
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_multiple_worktrees_and_normalizes_branch_refs() {
        let records = parse_worktree_porcelain(
            "worktree /repo\nHEAD abcdef0123456789\nbranch refs/heads/main\n\nworktree /feature\nHEAD fedcba9876543210\ndetached\nlocked reason\nprunable stale\n",
        )
        .unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].branch.as_deref(), Some("main"));
        assert_eq!(records[1].branch, None);
        assert!(records[1].locked);
        assert!(records[1].prunable);
    }

    #[test]
    fn rejects_fields_before_a_worktree_path() {
        let error = parse_worktree_porcelain("HEAD abcdef\n").unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn snapshot_store_is_owner_only_and_replaces_current_snapshot() {
        let root = std::env::temp_dir().join(format!(
            "mice-coordination-test-{}-{}",
            now(),
            std::process::id()
        ));
        let store = SnapshotStore::at(root.clone()).unwrap();
        let mut snapshot = RepoSnapshot {
            repo_id: "a".repeat(64),
            captured_at: 1,
            worktrees: vec![WorktreeRecord {
                path: "/repo".into(),
                head: "abcdef".into(),
                branch: Some("main".into()),
                bare: false,
                locked: false,
                prunable: false,
            }],
        };
        let path = store.record(&snapshot).unwrap();
        snapshot.captured_at = 2;
        assert_eq!(path, store.record(&snapshot).unwrap());
        let stored: RepoSnapshot = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(stored.captured_at, 2);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&root).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn status_marks_the_read_only_boundary() {
        let rendered = render_status(
            &RepoSnapshot {
                repo_id: "a".repeat(64),
                captured_at: 1,
                worktrees: vec![WorktreeRecord {
                    path: "/repo".into(),
                    head: "abcdef012345".into(),
                    branch: Some("main".into()),
                    bare: false,
                    locked: true,
                    prunable: false,
                }],
            },
            Path::new("/state/a.json"),
        );
        assert!(rendered.contains("main at /repo"));
        assert!(rendered.contains("[locked]"));
        assert!(rendered.contains("no agents were launched"));
    }

    #[test]
    fn parses_old_hunk_ranges_without_retaining_source_text() {
        let hunks = parse_diff_hunks(
            "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -10,2 +10,3 @@\n+source text is not retained\ndiff --git a/new.rs b/new.rs\n--- /dev/null\n+++ b/new.rs\n@@ -0,0 +1 @@\n+new\n",
        );
        assert_eq!(
            hunks.ranges["src/lib.rs"],
            vec![LineRange {
                start: 10,
                count: 2
            }]
        );
        assert_eq!(
            hunks.ranges["new.rs"],
            vec![LineRange { start: 0, count: 0 }]
        );
        assert!(!hunks.malformed_hunk);
    }

    #[test]
    fn indexes_renamed_hunks_under_both_paths() {
        let hunks = parse_diff_hunks(
            "diff --git a/file.txt b/renamed.txt\nsimilarity index 90%\nrename from file.txt\nrename to renamed.txt\n--- a/file.txt\n+++ b/renamed.txt\n@@ -3 +3 @@\n-before\n+after\n",
        );
        let expected = vec![LineRange { start: 3, count: 1 }];
        assert_eq!(hunks.ranges["file.txt"], expected);
        assert_eq!(
            hunks.ranges["renamed.txt"],
            vec![LineRange { start: 3, count: 1 }]
        );
    }

    #[test]
    fn flags_unparseable_hunk_headers_instead_of_silently_dropping_them() {
        let hunks = parse_diff_hunks(
            "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ malformed @@\n",
        );
        assert!(hunks.malformed_hunk);
        assert!(hunks.ranges.is_empty());
    }

    #[test]
    fn classifies_disjoint_hunks_yellow_and_intersecting_hunks_red() {
        let left = BTreeMap::from([(
            "src/lib.rs".into(),
            vec![LineRange {
                start: 10,
                count: 2,
            }],
        )]);
        let right = BTreeMap::from([(
            "src/lib.rs".into(),
            vec![LineRange {
                start: 30,
                count: 1,
            }],
        )]);
        assert_eq!(
            classify_pair("left", "right", &left, &right)[0].level,
            RiskLevel::Yellow
        );
        let overlapping = BTreeMap::from([(
            "src/lib.rs".into(),
            vec![LineRange {
                start: 11,
                count: 1,
            }],
        )]);
        assert_eq!(
            classify_pair("left", "right", &left, &overlapping)[0].level,
            RiskLevel::Red
        );
    }

    #[test]
    fn suppresses_common_generated_lockfiles_but_not_source_files() {
        assert!(is_low_signal_generated_path("Cargo.lock"));
        assert!(is_low_signal_generated_path("web/pnpm-lock.yaml"));
        assert!(!is_low_signal_generated_path("crates/mice-cli/src/main.rs"));
    }

    #[test]
    fn oversized_worktree_sets_are_explicitly_unassessed() {
        let worktree = WorktreeRecord {
            path: "/repo".into(),
            head: "abcdef".into(),
            branch: Some("main".into()),
            bare: false,
            locked: false,
            prunable: false,
        };
        let report = merge_risks(&RepoSnapshot {
            repo_id: "a".repeat(64),
            captured_at: 1,
            worktrees: vec![worktree; MAX_WORKTREES + 1],
        });
        assert!(report.risks.is_empty());
        assert!(report.unassessed[0].reason.contains("analysis limit"));
    }

    #[test]
    fn only_disjoint_generated_churn_is_suppressed() {
        let mut risk = MergeRisk {
            level: RiskLevel::Yellow,
            left_branch: "left".into(),
            right_branch: "right".into(),
            path: "Cargo.lock".into(),
            left_ranges: vec![],
            right_ranges: vec![],
            overlaps: vec![],
        };
        assert!(should_suppress_risk(&risk));
        risk.level = RiskLevel::Red;
        assert!(!should_suppress_risk(&risk));
    }

    #[test]
    fn simultaneous_insertions_at_the_same_base_position_are_red() {
        assert!(ranges_overlap(
            LineRange { start: 8, count: 0 },
            LineRange { start: 8, count: 0 }
        ));
        assert!(!ranges_overlap(
            LineRange { start: 8, count: 0 },
            LineRange { start: 9, count: 0 }
        ));
    }

    #[test]
    fn live_git_worktrees_report_an_uncommitted_same_hunk_risk() {
        let root = std::env::temp_dir().join(format!(
            "mice-coordination-git-test-{}-{}",
            now(),
            std::process::id()
        ));
        let main = root.join("main");
        let feature = root.join("feature");
        fs::create_dir_all(&main).unwrap();
        git(&main, &["init", "-q"]);
        git(
            &main,
            &["config", "user.email", "mice-test@example.invalid"],
        );
        git(&main, &["config", "user.name", "MICE test"]);
        fs::write(main.join("module.txt"), "before\n").unwrap();
        git(&main, &["add", "module.txt"]);
        git(&main, &["commit", "-qm", "baseline"]);
        git(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature",
                feature.to_str().unwrap(),
            ],
        );
        fs::write(main.join("module.txt"), "main change\n").unwrap();
        fs::write(feature.join("module.txt"), "feature change\n").unwrap();

        let snapshot = discover(&main).unwrap();
        let report = merge_risks(&snapshot);
        assert!(report.unassessed.is_empty());
        assert!(report.risks.iter().any(|risk| {
            risk.path == "module.txt" && risk.level == RiskLevel::Red && !risk.overlaps.is_empty()
        }));

        git(
            &main,
            &["worktree", "remove", "--force", feature.to_str().unwrap()],
        );
        let _ = fs::remove_dir_all(root);
    }

    fn git(cwd: &Path, arguments: &[&str]) {
        let output = Command::new("git")
            .args(arguments)
            .current_dir(cwd)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            arguments,
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
