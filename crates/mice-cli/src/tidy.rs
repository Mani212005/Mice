//! `mice tidy` — privacy-first, propose-then-confirm folder organizer (M9).
//!
//! Three passes: a metadata scan with no model, an optional bounded local
//! labeling pass, and a review screen whose applied actions are all recorded
//! in an undo manifest. Deletes only ever go to the Trash and only when the
//! user set that row to trash individually; dry-run is the default.

use std::{
    collections::BTreeMap,
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use mice_core::{
    TIDY_STALE_SECONDS, TidyAction, TidyCategory, TidyFile, TidyProposal, UndoAction, UndoKind,
    UndoRun, parse_spotlight_date, propose_tidy_actions, tidy_label_instruction, tidy_report,
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
};
use sha2::{Digest, Sha256};

const MAX_SCAN_FILES: usize = 2_000;
const MAX_SCAN_DEPTH: usize = 6;
/// Hashing is only for duplicate confirmation; a file this large is treated
/// as unique rather than stalling the scan.
const MAX_HASH_BYTES: u64 = 256 * 1024 * 1024;
/// A dry run must not turn duplicate detection into an unbounded disk scan.
const MAX_TOTAL_HASH_BYTES: u64 = 512 * 1024 * 1024;
const MAX_SPOTLIGHT_CALLS: usize = 400;
const MAX_LABEL_FILES: usize = 25;
const MAX_LABEL_SNIPPET_BYTES: usize = 2_048;
const UNDO_LOCK_STALE_AFTER: Duration = Duration::from_secs(120);
/// Directory names that are effectively system/build output even when not
/// hidden. Hidden (dot) entries are always skipped.
const SKIPPED_DIRECTORY_NAMES: [&str; 4] = ["node_modules", "target", "Library", "Applications"];

pub const OLLAMA_ENDPOINT: &str = "http://127.0.0.1:11434";

/// Where applied actions land and where the undo manifest lives. Injectable
/// so tests operate entirely inside temporary directories.
pub struct TidyPaths {
    pub trash_dir: PathBuf,
    pub log_path: PathBuf,
}

impl TidyPaths {
    pub fn default_paths() -> Result<Self, Box<dyn std::error::Error>> {
        let home = std::env::var_os("HOME").ok_or("HOME is not set")?;
        let home = PathBuf::from(home);
        Ok(Self {
            trash_dir: home.join(".Trash"),
            log_path: home.join("Library/Application Support/MICE/tidy-log.json"),
        })
    }
}

pub fn tidy() -> Result<(), Box<dyn std::error::Error>> {
    let mut apply = false;
    let mut undo = false;
    let mut label = true;
    let mut folder: Option<PathBuf> = None;
    for argument in std::env::args().skip(2) {
        match argument.as_str() {
            "--apply" => apply = true,
            "--undo" => undo = true,
            "--no-label" => label = false,
            _ if argument.starts_with('-') => {
                return Err(format!("Unknown `mice tidy` option `{argument}`.").into());
            }
            _ if folder.is_some() => {
                return Err("`mice tidy` accepts one folder at most.".into());
            }
            _ => folder = Some(PathBuf::from(argument)),
        }
    }
    let paths = TidyPaths::default_paths()?;
    if undo {
        if apply || !label || folder.is_some() {
            return Err(
                "`mice tidy --undo` cannot be combined with a folder, --apply, or --no-label."
                    .into(),
            );
        }
        return undo_last(&paths);
    }
    let home = PathBuf::from(std::env::var_os("HOME").ok_or("HOME is not set")?);
    let root = folder.unwrap_or_else(|| home.join("Downloads"));
    let root = root
        .canonicalize()
        .map_err(|error| format!("Could not open {}: {error}", root.display()))?;
    if !root.is_dir() {
        return Err(format!("{} is not a folder.", root.display()).into());
    }
    if root == home || root.parent().is_none() {
        return Err("Refusing to tidy your entire home directory or the filesystem root; pick a specific folder such as ~/Downloads.".into());
    }

    println!("Scanning {}…", root.display());
    let outcome = scan_folder(&root)?;
    let mut files = outcome.files;
    fill_duplicate_keys(&root, &mut files);
    let stale_cutoff = now().saturating_sub(TIDY_STALE_SECONDS);
    fill_last_used(&root, &mut files, stale_cutoff);
    let report = tidy_report(&files, stale_cutoff);
    println!("{}", report.headline());
    if outcome.skipped_entries > 0 || outcome.capped {
        println!(
            "(skipped {} hidden/symlink/system entries{})",
            outcome.skipped_entries,
            if outcome.capped {
                format!("; stopped at the first {MAX_SCAN_FILES} files")
            } else {
                String::new()
            }
        );
    }
    let proposals = propose_tidy_actions(&files, stale_cutoff);

    if !apply {
        print_dry_run(&files, &proposals);
        println!(
            "\nDry run only — nothing was changed. Run `mice tidy --apply {}` to review and apply.",
            root.display()
        );
        return Ok(());
    }

    let labels = if label {
        label_files(&root, &files, &proposals)
    } else {
        BTreeMap::new()
    };
    let mut rows = review_rows(&files, &proposals, &labels);
    if rows.is_empty() {
        println!("Nothing to review — the folder is already tidy.");
        return Ok(());
    }
    if !run_review_tui(&mut rows)? {
        println!("Cancelled — nothing was changed.");
        return Ok(());
    }
    apply_actions(&root, &files, &rows, &paths)
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// --- Pass 1: metadata scan (no model) ---------------------------------------

struct ScanOutcome {
    files: Vec<TidyFile>,
    skipped_entries: usize,
    capped: bool,
}

/// Bounded recursive walk. Symlinks are never followed (so the scan cannot
/// leave the chosen root), hidden and system directories are skipped, and
/// depth/file caps keep a pathological folder from stalling the run.
fn scan_folder(root: &Path) -> Result<ScanOutcome, Box<dyn std::error::Error>> {
    let mut files = Vec::new();
    let mut skipped_entries = 0usize;
    let mut capped = false;
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    'walk: while let Some((directory, depth)) = stack.pop() {
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(_) => {
                skipped_entries += 1;
                continue;
            }
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                skipped_entries += 1;
                continue;
            }
            let path = entry.path();
            let Ok(metadata) = fs::symlink_metadata(&path) else {
                skipped_entries += 1;
                continue;
            };
            if metadata.file_type().is_symlink() {
                skipped_entries += 1;
                continue;
            }
            if metadata.is_dir() {
                if depth + 1 > MAX_SCAN_DEPTH || SKIPPED_DIRECTORY_NAMES.contains(&name.as_str()) {
                    skipped_entries += 1;
                } else {
                    stack.push((path, depth + 1));
                }
                continue;
            }
            if !metadata.is_file() {
                skipped_entries += 1;
                continue;
            }
            if files.len() >= MAX_SCAN_FILES {
                capped = true;
                break 'walk;
            }
            let relative_path = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();
            files.push(TidyFile {
                relative_path,
                size: metadata.len(),
                modified_ts: system_time_seconds(metadata.modified().ok()),
                created_ts: system_time_seconds(metadata.created().ok()),
                last_used_ts: None,
                content_key: None,
            });
        }
    }
    files.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    Ok(ScanOutcome {
        files,
        skipped_entries,
        capped,
    })
}

fn system_time_seconds(time: Option<SystemTime>) -> Option<u64> {
    time.and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
}

/// Size-then-hash duplicate detection: only files sharing a size are hashed,
/// so most files never need to be read at all.
fn fill_duplicate_keys(root: &Path, files: &mut [TidyFile]) {
    fill_duplicate_keys_with_budget(root, files, MAX_TOTAL_HASH_BYTES);
}

fn fill_duplicate_keys_with_budget(root: &Path, files: &mut [TidyFile], budget: u64) {
    let mut by_size = BTreeMap::<u64, Vec<usize>>::new();
    for (index, file) in files.iter().enumerate() {
        if file.size > 0 && file.size <= MAX_HASH_BYTES {
            by_size.entry(file.size).or_default().push(index);
        }
    }
    let mut remaining_budget = budget;
    for (size, indices) in by_size {
        if indices.len() < 2 {
            continue;
        }
        for index in indices {
            if size > remaining_budget {
                continue;
            }
            if let Ok(hash) = hash_file(&root.join(&files[index].relative_path)) {
                files[index].content_key = Some(format!("{size}:{hash}"));
                remaining_budget = remaining_budget.saturating_sub(size);
            }
        }
    }
}

fn hash_file(path: &Path) -> Result<String, std::io::Error> {
    let mut file = fs::File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

/// Ask Spotlight for last-used dates, but only for files whose filesystem
/// timestamps already look stale — a recent modification can never become
/// stale, so most files need no subprocess at all.
fn fill_last_used(root: &Path, files: &mut [TidyFile], stale_cutoff: u64) {
    let mut calls = 0usize;
    for file in files.iter_mut() {
        if calls >= MAX_SPOTLIGHT_CALLS {
            break;
        }
        if file
            .last_activity_ts()
            .is_some_and(|activity| activity >= stale_cutoff)
        {
            continue;
        }
        calls += 1;
        file.last_used_ts = spotlight_last_used(&root.join(&file.relative_path));
    }
}

fn spotlight_last_used(path: &Path) -> Option<u64> {
    let output = Command::new("/usr/bin/mdls")
        .args(["-name", "kMDItemLastUsedDate", "-raw"])
        .arg(path)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_spotlight_date(&String::from_utf8_lossy(&output.stdout))
}

// --- Pass 2: bounded local labeling (never cloud) ---------------------------

/// Label a bounded number of files with the configured local model to make
/// the review list readable. Hard rule enforced here in code, not in config:
/// this calls the local Ollama lane directly and never a routing function, so
/// file contents cannot reach a cloud provider in any privacy mode.
fn label_files(
    root: &Path,
    files: &[TidyFile],
    proposals: &[TidyProposal],
) -> BTreeMap<usize, String> {
    let mut labels = BTreeMap::new();
    let Ok(config) = crate::config() else {
        return labels;
    };
    if mice_providers::ollama_model_ready(OLLAMA_ENDPOINT, &config.local_model).is_err() {
        println!(
            "(local model {} is not available; review will show metadata only)",
            config.local_model
        );
        return labels;
    }
    println!(
        "Labeling up to {MAX_LABEL_FILES} files with {} (local only)…",
        config.local_model
    );
    let mut labelled = 0usize;
    for proposal in proposals {
        if proposal.action == TidyAction::Keep {
            continue;
        }
        if labelled >= MAX_LABEL_FILES {
            break;
        }
        let file = &files[proposal.file_index];
        let summary = label_summary(&root.join(&file.relative_path), file);
        labelled += 1;
        let mut response = String::new();
        let streamed = crate::stream_ollama(
            &config.local_model,
            tidy_label_instruction(),
            Some(&summary),
            |chunk| {
                response.push_str(chunk);
                Ok(())
            },
        );
        if streamed.is_ok() {
            let label = response.trim().lines().next().unwrap_or("").trim();
            if !label.is_empty() && label.chars().count() <= 80 {
                labels.insert(proposal.file_index, label.to_owned());
            }
        }
    }
    labels
}

fn label_summary(path: &Path, file: &TidyFile) -> String {
    let mut summary = format!(
        "Name: {}\nSize: {}\n",
        file.file_name(),
        mice_core::format_bytes(file.size)
    );
    if let Some(snippet) = text_snippet(path) {
        summary.push_str("First lines:\n");
        summary.push_str(&snippet);
    }
    summary
}

/// The first ~2 KB of a file, only when it looks like text. Binary content is
/// labeled from name and metadata alone.
fn text_snippet(path: &Path) -> Option<String> {
    let mut file = fs::File::open(path).ok()?;
    let mut buffer = vec![0u8; MAX_LABEL_SNIPPET_BYTES];
    let read = file.read(&mut buffer).ok()?;
    buffer.truncate(read);
    if read == 0 || buffer.contains(&0) {
        return None;
    }
    Some(String::from_utf8_lossy(&buffer).into_owned())
}

// --- Pass 3: propose → confirm → apply --------------------------------------

fn print_dry_run(files: &[TidyFile], proposals: &[TidyProposal]) {
    let mut lines = Vec::new();
    for proposal in proposals {
        let file = &files[proposal.file_index];
        match proposal.action {
            TidyAction::Keep => {}
            TidyAction::Move => lines.push(format!(
                "  move    {} → {}/",
                file.relative_path,
                proposal.category.folder_name()
            )),
            TidyAction::TrashCandidate => lines.push(format!(
                "  trash?  {} — {}",
                file.relative_path, proposal.reason
            )),
        }
    }
    if lines.is_empty() {
        println!("\nNo changes to propose — the folder is already tidy.");
    } else {
        println!("\nProposed actions:");
        for line in lines {
            println!("{line}");
        }
    }
}

pub struct ReviewRow {
    pub file_index: usize,
    pub name: String,
    pub action: TidyAction,
    pub category: TidyCategory,
    pub note: String,
}

/// Build the interactive rows. A proposed trash candidate deliberately starts
/// as Keep: trashing happens only for rows the user individually switches to
/// trash, which is the per-item confirmation the product rules require.
pub fn review_rows(
    files: &[TidyFile],
    proposals: &[TidyProposal],
    labels: &BTreeMap<usize, String>,
) -> Vec<ReviewRow> {
    proposals
        .iter()
        .filter(|proposal| proposal.action != TidyAction::Keep)
        .map(|proposal| {
            let mut note = match proposal.action {
                TidyAction::TrashCandidate => format!("suggested: trash — {}", proposal.reason),
                _ => proposal.reason.clone(),
            };
            if let Some(label) = labels.get(&proposal.file_index) {
                note = format!("{label} · {note}");
            }
            ReviewRow {
                file_index: proposal.file_index,
                name: files[proposal.file_index].relative_path.clone(),
                action: match proposal.action {
                    TidyAction::TrashCandidate => TidyAction::Keep,
                    action => action,
                },
                category: proposal.category,
                note,
            }
        })
        .collect()
}

fn action_name(action: TidyAction) -> &'static str {
    match action {
        TidyAction::Keep => "keep ",
        TidyAction::Move => "move ",
        TidyAction::TrashCandidate => "trash",
    }
}

fn cycle_action(action: TidyAction, forward: bool) -> TidyAction {
    match (action, forward) {
        (TidyAction::Keep, true) | (TidyAction::TrashCandidate, false) => TidyAction::Move,
        (TidyAction::Move, true) | (TidyAction::Keep, false) => TidyAction::TrashCandidate,
        (TidyAction::TrashCandidate, true) | (TidyAction::Move, false) => TidyAction::Keep,
    }
}

fn run_review_tui(rows: &mut [ReviewRow]) -> Result<bool, Box<dyn std::error::Error>> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    let result = review_event_loop(&mut terminal, rows);
    let _ = disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let _ = terminal.show_cursor();
    result
}

fn review_event_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    rows: &mut [ReviewRow],
) -> Result<bool, Box<dyn std::error::Error>> {
    let mut selected = 0usize;
    let mut confirming = false;
    loop {
        terminal.draw(|frame| draw_review(frame, rows, selected, confirming))?;
        if !event::poll(Duration::from_millis(250))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        if confirming {
            match key.code {
                KeyCode::Char('y') => return Ok(true),
                KeyCode::Char('n') | KeyCode::Esc => confirming = false,
                _ => {}
            }
            continue;
        }
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                selected = selected.checked_sub(1).unwrap_or(rows.len() - 1)
            }
            KeyCode::Down | KeyCode::Char('j') => selected = (selected + 1) % rows.len(),
            KeyCode::Left | KeyCode::Char('h') => {
                rows[selected].action = cycle_action(rows[selected].action, false)
            }
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Enter => {
                rows[selected].action = cycle_action(rows[selected].action, true)
            }
            KeyCode::Char('a') => confirming = true,
            KeyCode::Esc | KeyCode::Char('q') => return Ok(false),
            _ => {}
        }
    }
}

fn draw_review(frame: &mut ratatui::Frame, rows: &[ReviewRow], selected: usize, confirming: bool) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(6), Constraint::Length(3)])
        .split(area);
    let visible = chunks[0].height.saturating_sub(2) as usize;
    let offset = selected.saturating_sub(visible.saturating_sub(1));
    let lines = rows
        .iter()
        .enumerate()
        .skip(offset)
        .take(visible.max(1))
        .map(|(index, row)| {
            let marker = if index == selected { "› " } else { "  " };
            let style = if index == selected {
                Style::default().fg(Color::Black).bg(Color::Cyan)
            } else {
                Style::default()
            };
            let target = match row.action {
                TidyAction::Move => format!(" → {}/", row.category.folder_name()),
                _ => String::new(),
            };
            Line::from(Span::styled(
                format!(
                    "{marker}[{}] {}{target}  {}",
                    action_name(row.action),
                    row.name,
                    row.note
                ),
                style,
            ))
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" MICE tidy review — nothing changes until you confirm "),
        ),
        chunks[0],
    );
    let footer = if confirming {
        let moves = rows
            .iter()
            .filter(|row| row.action == TidyAction::Move)
            .count();
        let trash = rows
            .iter()
            .filter(|row| row.action == TidyAction::TrashCandidate)
            .count();
        format!("{moves} moves, {trash} to Trash (individually chosen).  y apply  n back")
    } else {
        "↑/↓ select  ←/→ change action  a apply  q cancel".into()
    };
    frame.render_widget(
        Paragraph::new(footer).block(Block::default().borders(Borders::ALL)),
        chunks[1],
    );
}

/// Apply the confirmed rows. Every successful rename is persisted to the undo
/// manifest immediately, so even a crash mid-run leaves a fully reversible
/// log. Failures skip the file and report; nothing is ever overwritten and
/// deletes only ever move files into the Trash directory.
pub fn apply_actions(
    root: &Path,
    files: &[TidyFile],
    rows: &[ReviewRow],
    paths: &TidyPaths,
) -> Result<(), Box<dyn std::error::Error>> {
    ensure_undo_log_ready(paths)?;
    let mut run = UndoRun {
        id: format!("{}-{}", now(), std::process::id()),
        ts: now(),
        tool: "tidy".into(),
        actions: Vec::new(),
    };
    let mut moves = 0usize;
    let mut trashed = 0usize;
    let mut skipped = Vec::new();
    for row in rows {
        let file = &files[row.file_index];
        let from = root.join(&file.relative_path);
        let (kind, destination_dir) = match row.action {
            TidyAction::Keep => continue,
            TidyAction::Move => (UndoKind::Move, root.join(row.category.folder_name())),
            TidyAction::TrashCandidate => (UndoKind::Trash, paths.trash_dir.clone()),
        };
        if row.action == TidyAction::Move && from.parent() == Some(destination_dir.as_path()) {
            continue;
        }
        if let Err(error) = fs::create_dir_all(&destination_dir) {
            skipped.push(format!("{}: {error}", file.relative_path));
            continue;
        }
        let destination_dir = match checked_destination_directory(
            &destination_dir,
            (row.action == TidyAction::Move).then_some(root),
        ) {
            Ok(path) => path,
            Err(error) => {
                skipped.push(format!("{}: {error}", file.relative_path));
                continue;
            }
        };
        match move_file_without_overwrite(&from, &destination_dir) {
            Ok(to) => {
                let action = UndoAction {
                    kind: kind.clone(),
                    from: from.to_string_lossy().into_owned(),
                    to: to.to_string_lossy().into_owned(),
                };
                run.actions.push(action);
                if let Err(error) = persist_run(&run, paths) {
                    run.actions.pop();
                    return rollback_unlogged_rename(&to, &from, error);
                }
                match kind {
                    UndoKind::Move => moves += 1,
                    UndoKind::Trash => trashed += 1,
                }
            }
            Err(error) => skipped.push(format!("{}: {error}", file.relative_path)),
        }
    }
    println!("Applied: {moves} moved, {trashed} sent to Trash.");
    for line in &skipped {
        println!("  skipped {line}");
    }
    if !run.actions.is_empty() {
        println!("Run `mice tidy --undo` to restore this run.");
    }
    Ok(())
}

fn checked_destination_directory(
    destination: &Path,
    allowed_root: Option<&Path>,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let metadata = fs::symlink_metadata(destination)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(format!(
            "Refusing unsafe destination directory {}.",
            destination.display()
        )
        .into());
    }
    let resolved = destination.canonicalize()?;
    if let Some(root) = allowed_root {
        let root = root.canonicalize()?;
        if !resolved.starts_with(&root) {
            return Err(format!(
                "Destination {} escapes the tidy folder.",
                resolved.display()
            )
            .into());
        }
    }
    Ok(resolved)
}

fn rollback_unlogged_rename(
    current: &Path,
    original: &Path,
    persist_error: Box<dyn std::error::Error>,
) -> Result<(), Box<dyn std::error::Error>> {
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

/// Reserve the destination with a hard link before removing the source.
/// POSIX `rename` replaces an existing target, so it cannot satisfy the
/// no-clobber contract. M9/M10 operate on regular files only; cross-volume
/// moves therefore fail safely instead of copy-deleting data.
pub fn move_file_without_overwrite(
    source: &Path,
    destination_dir: &Path,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let metadata = fs::symlink_metadata(source)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!("Refusing to move unsafe source {}.", source.display()).into());
    }
    let name = source
        .file_name()
        .ok_or("The source path has no file name.")?;
    let file_name = name.to_string_lossy();
    let (stem, extension) = match file_name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => (stem.to_owned(), format!(".{extension}")),
        _ => (file_name.into_owned(), String::new()),
    };
    for attempt in 0..10_000_usize {
        let destination = if attempt == 0 {
            destination_dir.join(name)
        } else {
            destination_dir.join(format!("{stem} (mice-{attempt}){extension}"))
        };
        match fs::hard_link(source, &destination) {
            Ok(()) => match fs::remove_file(source) {
                Ok(()) => return Ok(destination),
                Err(error) => {
                    let _ = fs::remove_file(&destination);
                    return Err(
                        format!("Could not finish moving {}: {error}", source.display()).into(),
                    );
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(format!(
                    "Could not move {} without overwriting: {error}",
                    source.display()
                )
                .into());
            }
        }
    }
    Err(format!(
        "Could not find a collision-free destination for {}.",
        source.display()
    )
    .into())
}

pub fn restore_file_without_overwrite(
    current: &Path,
    original: &Path,
) -> Result<(), std::io::Error> {
    fs::hard_link(current, original)?;
    fs::remove_file(current)
}

pub fn ensure_undo_log_ready(paths: &TidyPaths) -> Result<(), Box<dyn std::error::Error>> {
    let _lock = lock_undo_log(&paths.log_path)?;
    let log = load_undo_log(&paths.log_path)?;
    save_undo_log(&paths.log_path, &log)
}

// --- Undo manifest ----------------------------------------------------------

struct UndoLogLock(PathBuf);

impl Drop for UndoLogLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn lock_undo_log(path: &Path) -> Result<UndoLogLock, Box<dyn std::error::Error>> {
    let parent = path
        .parent()
        .ok_or("The MICE undo log has no parent directory.")?;
    fs::create_dir_all(parent)?;
    let lock_path = path.with_extension("lock");
    for _ in 0..2_000 {
        match fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&lock_path)
        {
            Ok(_) => return Ok(UndoLogLock(lock_path)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let stale = fs::metadata(&lock_path)
                    .and_then(|metadata| metadata.modified())
                    .ok()
                    .and_then(|modified| modified.elapsed().ok())
                    .is_some_and(|age| age > UNDO_LOCK_STALE_AFTER);
                if stale {
                    let _ = fs::remove_file(&lock_path);
                } else {
                    thread::sleep(Duration::from_millis(5));
                }
            }
            Err(error) => return Err(error.into()),
        }
    }
    Err("Timed out waiting for the MICE undo-log lock.".into())
}

pub fn load_undo_log(path: &Path) -> Result<Vec<UndoRun>, Box<dyn std::error::Error>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    serde_json::from_slice(&fs::read(path)?).map_err(|error| {
        format!(
            "The MICE undo log at {} is unreadable ({error}); refusing to guess about past moves.",
            path.display()
        )
        .into()
    })
}

pub fn save_undo_log(path: &Path, runs: &[UndoRun]) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    fs::write(&temporary, serde_json::to_vec_pretty(runs)?)?;
    fs::rename(&temporary, path)?;
    Ok(())
}

/// Upsert this run in the log. Called after every applied action so the
/// manifest always matches the filesystem.
pub fn persist_run(run: &UndoRun, paths: &TidyPaths) -> Result<(), Box<dyn std::error::Error>> {
    let _lock = lock_undo_log(&paths.log_path)?;
    let mut log = load_undo_log(&paths.log_path)?;
    log.retain(|entry| entry.id != run.id);
    log.push(run.clone());
    save_undo_log(&paths.log_path, &log)
}

/// Reverse the most recent run in strict LIFO order. An entry that cannot be
/// reverted (missing file, occupied original path) is reported and kept in
/// the log so a later `--undo` can retry it after the user intervenes.
pub fn undo_last(paths: &TidyPaths) -> Result<(), Box<dyn std::error::Error>> {
    let _lock = lock_undo_log(&paths.log_path)?;
    let mut log = load_undo_log(&paths.log_path)?;
    let Some(run) = log.pop() else {
        println!("Nothing to undo.");
        return Ok(());
    };
    let mut restored = 0usize;
    let mut remaining = Vec::new();
    for action in run.actions.iter().rev() {
        let current = PathBuf::from(&action.to);
        let original = PathBuf::from(&action.from);
        if !current.exists() {
            println!(
                "  cannot restore {}: it is no longer at {}",
                action.from, action.to
            );
            remaining.push(action.clone());
            continue;
        }
        if original.exists() {
            println!(
                "  cannot restore {}: something else now occupies that path",
                action.from
            );
            remaining.push(action.clone());
            continue;
        }
        if let Some(parent) = original.parent() {
            let _ = fs::create_dir_all(parent);
        }
        match restore_file_without_overwrite(&current, &original) {
            Ok(()) => restored += 1,
            Err(error) => {
                println!("  cannot restore {}: {error}", action.from);
                remaining.push(action.clone());
            }
        }
    }
    if !remaining.is_empty() {
        remaining.reverse();
        log.push(UndoRun {
            actions: remaining,
            ..run.clone()
        });
    }
    save_undo_log(&paths.log_path, &log)?;
    println!(
        "Restored {restored} of {} actions from the last run.",
        run.actions.len()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("mice-tidy-{name}-{}-{}", now(), std::process::id()));
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn scan_skips_hidden_entries_and_never_follows_symlinks() {
        let root = temp_root("scan");
        fs::write(root.join("visible.txt"), b"data").unwrap();
        fs::write(root.join(".hidden.txt"), b"data").unwrap();
        fs::create_dir_all(root.join("nested")).unwrap();
        fs::write(root.join("nested/inner.rs"), b"fn main() {}").unwrap();
        let outside = temp_root("scan-outside");
        fs::write(outside.join("secret.txt"), b"secret").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();
        let outcome = scan_folder(&root).unwrap();
        let names = outcome
            .files
            .iter()
            .map(|file| file.relative_path.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["nested/inner.rs", "visible.txt"]);
        assert!(outcome.skipped_entries >= 2);
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&outside);
    }

    #[test]
    fn scan_stops_traversing_once_the_file_cap_is_reached() {
        let root = temp_root("cap");
        for index in 0..=MAX_SCAN_FILES {
            fs::write(root.join(format!("{index}.txt")), b"x").unwrap();
        }
        let outcome = scan_folder(&root).unwrap();
        assert_eq!(outcome.files.len(), MAX_SCAN_FILES);
        assert!(outcome.capped);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn tidy_refuses_a_symlinked_category_destination() {
        let root = temp_root("symlink-destination");
        let outside = temp_root("symlink-destination-outside");
        std::os::unix::fs::symlink(&outside, root.join("Documents")).unwrap();
        assert!(checked_destination_directory(&root.join("Documents"), Some(&root)).is_err());
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&outside);
    }

    #[test]
    fn identical_content_gets_matching_duplicate_keys() {
        let root = temp_root("dup");
        fs::write(root.join("a.bin"), b"same-bytes").unwrap();
        fs::write(root.join("b.bin"), b"same-bytes").unwrap();
        fs::write(root.join("c.bin"), b"other-size!").unwrap();
        let mut files = scan_folder(&root).unwrap().files;
        fill_duplicate_keys(&root, &mut files);
        let key = |name: &str| {
            files
                .iter()
                .find(|file| file.relative_path == name)
                .unwrap()
                .content_key
                .clone()
        };
        assert!(key("a.bin").is_some());
        assert_eq!(key("a.bin"), key("b.bin"));
        assert_eq!(key("c.bin"), None);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn duplicate_hashing_respects_the_aggregate_io_budget() {
        let root = temp_root("dup-budget");
        fs::write(root.join("a.bin"), b"same").unwrap();
        fs::write(root.join("b.bin"), b"same").unwrap();
        let mut files = scan_folder(&root).unwrap().files;
        // Each matching file is four bytes; the second hash must not start
        // once the five-byte run budget is exhausted.
        fill_duplicate_keys_with_budget(&root, &mut files, 5);
        assert_eq!(
            files
                .iter()
                .filter(|file| file.content_key.is_some())
                .count(),
            1
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn apply_then_undo_restores_every_file_and_empties_the_log() {
        let root = temp_root("apply");
        fs::write(root.join("report.pdf"), b"pdf").unwrap();
        fs::write(root.join("old.zip"), b"zip").unwrap();
        let workspace = temp_root("apply-support");
        let paths = TidyPaths {
            trash_dir: workspace.join("trash"),
            log_path: workspace.join("tidy-log.json"),
        };
        let files = scan_folder(&root).unwrap().files;
        let index_of = |name: &str| {
            files
                .iter()
                .position(|file| file.relative_path == name)
                .unwrap()
        };
        let rows = vec![
            ReviewRow {
                file_index: index_of("report.pdf"),
                name: "report.pdf".into(),
                action: TidyAction::Move,
                category: TidyCategory::Documents,
                note: String::new(),
            },
            ReviewRow {
                file_index: index_of("old.zip"),
                name: "old.zip".into(),
                action: TidyAction::TrashCandidate,
                category: TidyCategory::Archives,
                note: String::new(),
            },
        ];
        apply_actions(&root, &files, &rows, &paths).unwrap();
        assert!(root.join("Documents/report.pdf").exists());
        assert!(!root.join("report.pdf").exists());
        assert!(paths.trash_dir.join("old.zip").exists());
        let log = load_undo_log(&paths.log_path).unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].actions.len(), 2);

        undo_last(&paths).unwrap();
        assert!(root.join("report.pdf").exists());
        assert!(root.join("old.zip").exists());
        assert!(!paths.trash_dir.join("old.zip").exists());
        assert!(load_undo_log(&paths.log_path).unwrap().is_empty());
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn undo_keeps_unrevertable_actions_for_a_later_retry() {
        let root = temp_root("undo-retry");
        fs::write(root.join("a.txt"), b"a").unwrap();
        let workspace = temp_root("undo-retry-support");
        let paths = TidyPaths {
            trash_dir: workspace.join("trash"),
            log_path: workspace.join("tidy-log.json"),
        };
        let files = scan_folder(&root).unwrap().files;
        let rows = vec![ReviewRow {
            file_index: 0,
            name: "a.txt".into(),
            action: TidyAction::TrashCandidate,
            category: TidyCategory::Other,
            note: String::new(),
        }];
        apply_actions(&root, &files, &rows, &paths).unwrap();
        // Occupy the original path so the revert cannot proceed safely.
        fs::write(root.join("a.txt"), b"new occupant").unwrap();
        undo_last(&paths).unwrap();
        assert_eq!(fs::read(root.join("a.txt")).unwrap(), b"new occupant");
        assert!(paths.trash_dir.join("a.txt").exists());
        let log = load_undo_log(&paths.log_path).unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].actions.len(), 1);
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn atomic_move_keeps_an_existing_destination_intact() {
        let root = temp_root("atomic-move");
        let destination = root.join("destination");
        fs::create_dir(&destination).unwrap();
        fs::write(root.join("report.txt"), b"source").unwrap();
        fs::write(destination.join("report.txt"), b"existing").unwrap();
        let moved = move_file_without_overwrite(&root.join("report.txt"), &destination).unwrap();
        assert_eq!(moved, destination.join("report (mice-1).txt"));
        assert_eq!(
            fs::read(destination.join("report.txt")).unwrap(),
            b"existing"
        );
        assert_eq!(fs::read(moved).unwrap(), b"source");
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn atomic_move_refuses_a_symlink_source() {
        let root = temp_root("symlink-source");
        let outside = root.join("outside.txt");
        let link = root.join("link.txt");
        let destination = root.join("destination");
        fs::write(&outside, b"outside").unwrap();
        fs::create_dir(&destination).unwrap();
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        assert!(move_file_without_overwrite(&link, &destination).is_err());
        assert_eq!(fs::read(&outside).unwrap(), b"outside");
        assert!(link.is_symlink());
        let _ = fs::remove_dir_all(&root);
    }
}
