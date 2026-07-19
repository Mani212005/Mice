//! `mice file <path>` — smart filing into registered project roots (M10).
//!
//! `--add-root` registers a root and indexes its candidate destination
//! folders (cached in Application Support with an optional one-line local
//! model description). Filing a path ranks the top three destinations —
//! locally only — asks the user to pick and confirm, then moves the file and
//! records the move in the shared tidy undo log.

use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use mice_core::{
    FilingCandidate, UndoAction, UndoKind, UndoRun, filing_prompt, filing_rank_instruction,
    parse_filing_ranking, rank_candidates_by_name,
};
use serde::{Deserialize, Serialize};

use crate::tidy::{
    OLLAMA_ENDPOINT, TidyPaths, ensure_undo_log_ready, move_file_without_overwrite, persist_run,
    restore_file_without_overwrite,
};

const MAX_INDEX_DEPTH: usize = 2;
const MAX_CANDIDATES_PER_ROOT: usize = 40;
const MAX_TOTAL_CANDIDATES: usize = 120;
const MAX_DESCRIBED_PER_RUN: usize = 30;
const MAX_PROMPT_CANDIDATES: usize = 40;
const MAX_FILE_SNIPPET_BYTES: usize = 2_048;
const INDEX_LOCK_STALE_AFTER: Duration = Duration::from_secs(120);

/// Folder descriptions come from the folder's own README or entry listing;
/// like every filing model call this uses the local lane only.
const FILING_DESCRIBE_INSTRUCTION: &str = "In one line of at most twelve words, say what this project folder is about. Answer with only that line, no punctuation at the end and no commentary.";

#[derive(Debug, Default, Serialize, Deserialize)]
struct FilingIndex {
    roots: Vec<String>,
    candidates: Vec<FilingCandidate>,
    updated_ts: u64,
}

pub fn file_cmd() -> Result<(), Box<dyn std::error::Error>> {
    let arguments = std::env::args().skip(2).collect::<Vec<_>>();
    match arguments.first().map(String::as_str) {
        Some("--finder") => {
            if arguments.len() != 1 {
                return Err(
                    "`mice file --finder` takes no path; select one file in Finder first.".into(),
                );
            }
            let config = crate::config()?;
            let path = crate::capture_finder_file(&config.gesture)?;
            file_path(&path)
        }
        Some("--add-root") => {
            let root = arguments
                .get(1)
                .ok_or("Usage: mice file --add-root <folder>")?;
            if arguments.len() > 2 {
                return Err("`mice file --add-root` takes exactly one folder.".into());
            }
            add_root(Path::new(root))
        }
        Some("--roots") => list_roots(),
        Some(path) if !path.starts_with('-') => {
            if arguments.len() > 1 {
                return Err("`mice file` takes exactly one path.".into());
            }
            file_path(Path::new(path))
        }
        _ => Err(
            "Usage: mice file --add-root <folder> | mice file --roots | mice file <path>".into(),
        ),
    }
}

fn index_path() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let home = std::env::var_os("HOME").ok_or("HOME is not set")?;
    Ok(PathBuf::from(home).join("Library/Application Support/MICE/filing-index.json"))
}

fn load_index(path: &Path) -> Result<FilingIndex, Box<dyn std::error::Error>> {
    if !path.exists() {
        return Ok(FilingIndex::default());
    }
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}

fn save_index(path: &Path, index: &FilingIndex) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    fs::write(&temporary, serde_json::to_vec_pretty(index)?)?;
    fs::rename(&temporary, path)?;
    Ok(())
}

struct FilingIndexLock(PathBuf);

impl Drop for FilingIndexLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn lock_index(path: &Path) -> Result<FilingIndexLock, Box<dyn std::error::Error>> {
    let parent = path
        .parent()
        .ok_or("The filing index has no parent directory.")?;
    fs::create_dir_all(parent)?;
    let lock_path = path.with_extension("lock");
    for _ in 0..2_000 {
        match fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&lock_path)
        {
            Ok(_) => return Ok(FilingIndexLock(lock_path)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let stale = fs::metadata(&lock_path)
                    .and_then(|metadata| metadata.modified())
                    .ok()
                    .and_then(|modified| modified.elapsed().ok())
                    .is_some_and(|age| age > INDEX_LOCK_STALE_AFTER);
                if stale {
                    let _ = fs::remove_file(&lock_path);
                } else {
                    thread::sleep(Duration::from_millis(5));
                }
            }
            Err(error) => return Err(error.into()),
        }
    }
    Err("Timed out waiting for the MICE filing-index lock.".into())
}

fn add_root(root: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let root = root
        .canonicalize()
        .map_err(|error| format!("Could not open {}: {error}", root.display()))?;
    if !root.is_dir() {
        return Err(format!("{} is not a folder.", root.display()).into());
    }
    let path = index_path()?;
    let _lock = lock_index(&path)?;
    let mut index = load_index(&path)?;
    let root_text = root.to_string_lossy().into_owned();
    if !index.roots.contains(&root_text) {
        index.roots.push(root_text.clone());
    }
    // Re-index only this root; other roots keep their cached descriptions.
    index
        .candidates
        .retain(|candidate| !Path::new(&candidate.path).starts_with(&root));
    let folders = candidate_folders(&root);
    let describe = describe_with_local_model_if_available();
    let mut described = 0usize;
    for folder in folders {
        if index.candidates.len() >= MAX_TOTAL_CANDIDATES {
            println!(
                "(index is at its {MAX_TOTAL_CANDIDATES}-folder cap; remove a root to make room)"
            );
            break;
        }
        let mut description = String::new();
        if let Some((model, _)) = &describe
            && described < MAX_DESCRIBED_PER_RUN
        {
            described += 1;
            description = describe_folder(model, &folder).unwrap_or_default();
        }
        index.candidates.push(FilingCandidate {
            path: folder.to_string_lossy().into_owned(),
            description,
        });
    }
    index.updated_ts = now();
    save_index(&path, &index)?;
    println!(
        "Registered {} — the filing index now has {} destination folders across {} roots.",
        root.display(),
        index.candidates.len(),
        index.roots.len()
    );
    Ok(())
}

fn list_roots() -> Result<(), Box<dyn std::error::Error>> {
    let index = load_index(&index_path()?)?;
    if index.roots.is_empty() {
        println!("No filing roots yet. Register one with `mice file --add-root <folder>`.");
        return Ok(());
    }
    println!(
        "Filing roots ({} destination folders):",
        index.candidates.len()
    );
    for root in &index.roots {
        println!("  {root}");
    }
    Ok(())
}

/// Candidate destinations are the root's visible subfolders, two levels deep,
/// skipping hidden and build/system directories. Symlinks are never followed.
fn candidate_folders(root: &Path) -> Vec<PathBuf> {
    const SKIPPED: [&str; 4] = ["node_modules", "target", "Library", "Applications"];
    let mut folders = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((directory, depth)) = stack.pop() {
        if folders.len() >= MAX_CANDIDATES_PER_ROOT {
            break;
        }
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        let mut children = entries
            .flatten()
            .filter(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                !name.starts_with('.')
                    && !SKIPPED.contains(&name.as_str())
                    && fs::symlink_metadata(entry.path()).is_ok_and(|metadata| metadata.is_dir())
            })
            .map(|entry| entry.path())
            .collect::<Vec<_>>();
        children.sort();
        for child in children {
            if folders.len() >= MAX_CANDIDATES_PER_ROOT {
                break;
            }
            folders.push(child.clone());
            if depth + 1 < MAX_INDEX_DEPTH {
                stack.push((child, depth + 1));
            }
        }
    }
    folders.sort();
    folders
}

fn describe_with_local_model_if_available() -> Option<(String, ())> {
    let config = crate::config().ok()?;
    if mice_providers::ollama_model_ready(OLLAMA_ENDPOINT, &config.local_model).is_err() {
        println!(
            "(local model {} is not available; folders are indexed by name only)",
            config.local_model
        );
        return None;
    }
    println!(
        "Describing folders with {} (local only)…",
        config.local_model
    );
    Some((config.local_model, ()))
}

fn describe_folder(model: &str, folder: &Path) -> Option<String> {
    let source = folder_summary(folder)?;
    let mut response = String::new();
    crate::stream_ollama(model, FILING_DESCRIBE_INSTRUCTION, Some(&source), |chunk| {
        response.push_str(chunk);
        Ok(())
    })
    .ok()?;
    let line = response.trim().lines().next()?.trim();
    (!line.is_empty() && line.chars().count() <= 90).then(|| line.to_owned())
}

/// The first KB of a README when one exists; otherwise a listing of up to ten
/// entry names. Nothing else in the folder is read.
fn folder_summary(folder: &Path) -> Option<String> {
    for readme in ["README.md", "README", "readme.md"] {
        let path = folder.join(readme);
        if let Ok(mut file) = fs::File::open(&path) {
            let mut buffer = vec![0u8; 1_024];
            let read = file.read(&mut buffer).ok()?;
            buffer.truncate(read);
            if read > 0 && !buffer.contains(&0) {
                return Some(format!(
                    "Folder name: {}\nREADME excerpt:\n{}",
                    folder.file_name().unwrap_or_default().to_string_lossy(),
                    String::from_utf8_lossy(&buffer)
                ));
            }
        }
    }
    let entries = fs::read_dir(folder)
        .ok()?
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| !name.starts_with('.'))
        .take(10)
        .collect::<Vec<_>>();
    Some(format!(
        "Folder name: {}\nContains: {}",
        folder.file_name().unwrap_or_default().to_string_lossy(),
        entries.join(", ")
    ))
}

fn file_path(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("Could not inspect {}: {error}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!(
            "{} is not a regular file; MICE refuses to follow or move symlinks.",
            path.display()
        )
        .into());
    }
    let file = path
        .canonicalize()
        .map_err(|error| format!("Could not open {}: {error}", path.display()))?;
    if !file.is_file() {
        return Err(format!(
            "{} is not a file; `mice file` moves single files in v1.",
            file.display()
        )
        .into());
    }
    let index = load_index(&index_path()?)?;
    if index.candidates.is_empty() {
        return Err(
            "No filing destinations yet. Register a project root first: `mice file --add-root ~/github`."
                .into(),
        );
    }
    let file_name = file
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();

    let ranked = rank_destinations(&file_name, &file, &index.candidates);
    println!("Where should {file_name} go?");
    for (position, candidate_index) in ranked.iter().enumerate() {
        let candidate = &index.candidates[*candidate_index];
        let description = if candidate.description.is_empty() {
            String::new()
        } else {
            format!(" — {}", candidate.description)
        };
        println!("  {}) {}{description}", position + 1, candidate.path);
    }
    let Some(choice) = prompt_line("Choose 1-3, or q to cancel: ") else {
        println!("Cancelled — nothing was moved.");
        return Ok(());
    };
    let Ok(position) = choice.trim().parse::<usize>() else {
        println!("Cancelled — nothing was moved.");
        return Ok(());
    };
    let Some(candidate_index) = position
        .checked_sub(1)
        .and_then(|position| ranked.get(position))
    else {
        println!("Cancelled — nothing was moved.");
        return Ok(());
    };
    let destination_dir = checked_filing_destination(
        Path::new(&index.candidates[*candidate_index].path),
        &index.roots,
    )?;
    let confirm = prompt_line(&format!(
        "Move {file_name} → {}/? [y/N] ",
        destination_dir.display()
    ));
    if confirm.as_deref().map(str::trim) != Some("y") {
        println!("Cancelled — nothing was moved.");
        return Ok(());
    }
    let paths = TidyPaths::default_paths()?;
    let landed = perform_filing_move(&file, &destination_dir, &paths)?;
    println!(
        "Moved {file_name} → {}. Run `mice tidy --undo` to restore it.",
        landed.display()
    );
    Ok(())
}

/// Rank destinations for a file: deterministic name-token prescoring bounds
/// the list, then the local model (when available) orders the finalists. The
/// model only ever returns candidate numbers, which are validated against the
/// list — it can never introduce a path of its own.
fn rank_destinations(file_name: &str, file: &Path, candidates: &[FilingCandidate]) -> Vec<usize> {
    let mut prescored = rank_candidates_by_name(file_name, candidates);
    let mut shortlist_indices = prescored.clone();
    for index in 0..candidates.len() {
        if shortlist_indices.len() >= MAX_PROMPT_CANDIDATES {
            break;
        }
        if !shortlist_indices.contains(&index) {
            shortlist_indices.push(index);
        }
    }
    let shortlist = shortlist_indices
        .iter()
        .map(|index| candidates[*index].clone())
        .collect::<Vec<_>>();

    if let Ok(config) = crate::config()
        && mice_providers::ollama_model_ready(OLLAMA_ENDPOINT, &config.local_model).is_ok()
    {
        let summary = filing_file_summary(file_name, file);
        let mut response = String::new();
        let streamed = crate::stream_ollama(
            &config.local_model,
            filing_rank_instruction(),
            Some(&filing_prompt(&summary, &shortlist)),
            |chunk| {
                response.push_str(chunk);
                Ok(())
            },
        );
        if streamed.is_ok()
            && let Some(ranking) = parse_filing_ranking(&response, shortlist.len())
        {
            let mut ranking = ranking
                .into_iter()
                .map(|position| shortlist_indices[position])
                .collect::<Vec<_>>();
            for index in &prescored {
                if ranking.len() >= 3 {
                    break;
                }
                if !ranking.contains(index) {
                    ranking.push(*index);
                }
            }
            return ranking;
        }
        println!("(the local ranking was unusable; falling back to name matching)");
    }
    prescored.truncate(3);
    prescored
}

fn checked_filing_destination(
    destination: &Path,
    roots: &[String],
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let metadata = fs::symlink_metadata(destination).map_err(|error| {
        format!(
            "{} no longer exists; re-run `mice file --add-root` for its root. ({error})",
            destination.display()
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(format!(
            "Refusing unsafe filing destination {}.",
            destination.display()
        )
        .into());
    }
    let resolved = destination.canonicalize()?;
    let inside_registered_root = roots.iter().any(|root| {
        Path::new(root)
            .canonicalize()
            .ok()
            .is_some_and(|root| resolved.starts_with(root))
    });
    if !inside_registered_root {
        return Err(format!(
            "Refusing destination {} because it is outside the registered filing roots.",
            resolved.display()
        )
        .into());
    }
    Ok(resolved)
}

fn filing_file_summary(file_name: &str, file: &Path) -> String {
    let size = fs::metadata(file)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    let mut summary = format!(
        "Name: {file_name}\nSize: {}\n",
        mice_core::format_bytes(size)
    );
    if let Some(snippet) = filing_snippet(file) {
        summary.push_str("First lines:\n");
        summary.push_str(&snippet);
    }
    summary
}

fn filing_snippet(path: &Path) -> Option<String> {
    let mut file = fs::File::open(path).ok()?;
    let mut buffer = vec![0u8; MAX_FILE_SNIPPET_BYTES];
    let read = file.read(&mut buffer).ok()?;
    buffer.truncate(read);
    if read == 0 || buffer.contains(&0) {
        return None;
    }
    Some(String::from_utf8_lossy(&buffer).into_owned())
}

/// Move one file into a destination folder without ever overwriting, and
/// record the move in the shared undo manifest before reporting success.
pub fn perform_filing_move(
    file: &Path,
    destination_dir: &Path,
    paths: &TidyPaths,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    // The interactive path additionally checks that this is inside a
    // registered root. Keep this low-level helper safe for tests/callers too.
    let metadata = fs::symlink_metadata(destination_dir)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(format!(
            "Refusing unsafe filing destination {}.",
            destination_dir.display()
        )
        .into());
    }
    ensure_undo_log_ready(paths)?;
    let to = move_file_without_overwrite(file, destination_dir)?;
    let run = UndoRun {
        id: format!("{}-{}", now(), std::process::id()),
        ts: now(),
        tool: "file".into(),
        actions: vec![UndoAction {
            kind: UndoKind::Move,
            from: file.to_string_lossy().into_owned(),
            to: to.to_string_lossy().into_owned(),
        }],
    };
    if let Err(error) = persist_run(&run, paths) {
        return rollback_unlogged_rename(&to, file, error);
    }
    Ok(to)
}

fn rollback_unlogged_rename(
    current: &Path,
    original: &Path,
    persist_error: Box<dyn std::error::Error>,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    match restore_file_without_overwrite(current, original) {
        Ok(()) => Err(format!(
            "Could not record the move in MICE's undo log ({persist_error}); the move was rolled back."
        )
        .into()),
        Err(rollback_error) => Err(format!(
            "Could not record the move in MICE's undo log ({persist_error}) and could not roll it back ({rollback_error}). The file is at {}.",
            current.display()
        )
        .into()),
    }
}

fn prompt_line(prompt: &str) -> Option<String> {
    print!("{prompt}");
    std::io::stdout().flush().ok()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).ok()?;
    let line = line.trim();
    (!line.is_empty() && line != "q").then(|| line.to_owned())
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tidy::{load_undo_log, undo_last};

    fn temp_dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "mice-filing-{name}-{}-{}",
            now(),
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn candidate_folders_skip_hidden_and_system_directories() {
        let root = temp_dir("candidates");
        for name in ["mice", "website", ".git", "node_modules", "mice/crates"] {
            fs::create_dir_all(root.join(name)).unwrap();
        }
        fs::write(root.join("loose-file.txt"), b"not a folder").unwrap();
        let folders = candidate_folders(&root);
        assert!(folders.contains(&root.join("mice")));
        assert!(folders.contains(&root.join("website")));
        assert!(folders.contains(&root.join("mice/crates")));
        assert!(
            !folders
                .iter()
                .any(|folder| { folder.ends_with(".git") || folder.ends_with("node_modules") })
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn filing_moves_record_in_the_shared_undo_log_and_revert() {
        let source = temp_dir("move-src");
        let destination = temp_dir("move-dest");
        let workspace = temp_dir("move-support");
        let paths = TidyPaths {
            trash_dir: workspace.join("trash"),
            log_path: workspace.join("tidy-log.json"),
        };
        let file = source.join("tax-return.pdf");
        fs::write(&file, b"pdf").unwrap();
        let landed = perform_filing_move(&file, &destination, &paths).unwrap();
        assert_eq!(landed, destination.join("tax-return.pdf"));
        assert!(!file.exists());
        let log = load_undo_log(&paths.log_path).unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].tool, "file");

        undo_last(&paths).unwrap();
        assert!(file.exists());
        assert!(!landed.exists());
        for directory in [&source, &destination, &workspace] {
            let _ = fs::remove_dir_all(directory);
        }
    }

    #[test]
    fn filing_never_overwrites_an_existing_destination_file() {
        let source = temp_dir("collide-src");
        let destination = temp_dir("collide-dest");
        let workspace = temp_dir("collide-support");
        let paths = TidyPaths {
            trash_dir: workspace.join("trash"),
            log_path: workspace.join("tidy-log.json"),
        };
        fs::write(destination.join("notes.txt"), b"existing").unwrap();
        let file = source.join("notes.txt");
        fs::write(&file, b"incoming").unwrap();
        let landed = perform_filing_move(&file, &destination, &paths).unwrap();
        assert_eq!(landed, destination.join("notes (mice-1).txt"));
        assert_eq!(
            fs::read(destination.join("notes.txt")).unwrap(),
            b"existing"
        );
        for directory in [&source, &destination, &workspace] {
            let _ = fs::remove_dir_all(directory);
        }
    }

    #[test]
    fn filing_refuses_an_unwritable_undo_log_before_moving() {
        let source = temp_dir("preflight-src");
        let destination = temp_dir("preflight-dest");
        let workspace = temp_dir("preflight-support");
        let blocked_parent = workspace.join("not-a-directory");
        fs::write(&blocked_parent, b"file").unwrap();
        let paths = TidyPaths {
            trash_dir: workspace.join("trash"),
            log_path: blocked_parent.join("tidy-log.json"),
        };
        let file = source.join("important.pdf");
        fs::write(&file, b"important").unwrap();
        assert!(perform_filing_move(&file, &destination, &paths).is_err());
        assert!(file.exists());
        assert!(!destination.join("important.pdf").exists());
        for directory in [&source, &destination, &workspace] {
            let _ = fs::remove_dir_all(directory);
        }
    }

    #[test]
    fn filing_rollback_never_replaces_a_recreated_source() {
        let source = temp_dir("rollback-source");
        let destination = temp_dir("rollback-destination");
        let current = destination.join("report.pdf");
        let original = source.join("report.pdf");
        fs::write(&current, b"moved-original").unwrap();
        fs::write(&original, b"new-source").unwrap();
        assert!(rollback_unlogged_rename(&current, &original, "log failure".into()).is_err());
        assert_eq!(fs::read(&original).unwrap(), b"new-source");
        assert_eq!(fs::read(&current).unwrap(), b"moved-original");
        for directory in [&source, &destination] {
            let _ = fs::remove_dir_all(directory);
        }
    }

    #[test]
    fn filing_destination_must_remain_inside_a_registered_root() {
        let root = temp_dir("root-check");
        let candidate = root.join("project");
        let outside = temp_dir("outside-check");
        fs::create_dir_all(&candidate).unwrap();
        let roots = vec![root.to_string_lossy().into_owned()];
        assert_eq!(
            checked_filing_destination(&candidate, &roots).unwrap(),
            candidate.canonicalize().unwrap()
        );
        assert!(checked_filing_destination(&outside, &roots).is_err());
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&outside);
    }
}
