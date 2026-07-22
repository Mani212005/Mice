//! File-backed shared memory and artifact cache.
//!
//! Events are authoritative and append-only. Facts/digests are derived on
//! demand, keeping the store inspectable and recoverable without a database.

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEvent {
    pub event_ts: u64,
    pub recorded_ts: u64,
    pub session: String,
    pub agent: String,
    pub branch: String,
    pub kind: String,
    pub text: String,
    #[serde(default)]
    pub files: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedArtifact {
    pub key: String,
    pub tool: String,
    pub args: String,
    pub fingerprint: String,
    pub distilled: String,
    pub raw_output_tokens_est: usize,
    pub truncated: bool,
    pub created_ts: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WorkflowMacro {
    key: String,
    calls: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerRecord {
    pub task: String,
    pub lane: String,
    pub wall_ms: u128,
    pub raw_output_tokens_est: usize,
    pub returned_tokens_est: usize,
    pub frontier_tokens_avoided_est: usize,
    pub outcome: String,
}

#[derive(Debug, Default, Clone)]
pub struct SavingsReport {
    pub delegations: usize,
    pub frontier_tokens_avoided: usize,
    pub cache_hits: usize,
    pub macro_replays: usize,
    pub by_lane: BTreeMap<String, usize>,
}

#[derive(Debug, Clone)]
pub struct SharedMemory {
    root: PathBuf,
}

/// Local-only personal history. This intentionally has a separate root from
/// execution-manager memory: it contains no captures, clipboard bodies, or
/// tool transcripts—only questions and short answer digests chosen by MICE.
#[derive(Debug, Clone)]
pub struct UserHistory {
    root: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HistoryKind {
    Ask,
    See,
    Summarize,
    Palette,
    /// A goal the person explicitly asked MICE to plan, along with the
    /// bounded, local plan digest. Unlike captures and selections, a typed
    /// goal is intentional personal memory and can be reviewed later.
    GoalPlan,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HistoryEvent {
    pub ts: u64,
    pub kind: HistoryKind,
    pub question: String,
    pub answer_digest: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_context: Option<String>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct UserPreferences {
    #[serde(default)]
    notes: Vec<String>,
}

impl UserHistory {
    pub fn at(root: PathBuf) -> Result<Self, std::io::Error> {
        fs::create_dir_all(&root)?;
        restrict_directory_to_user(&root)?;
        Ok(Self { root })
    }

    pub fn default_path() -> Option<PathBuf> {
        std::env::var_os("HOME")
            .map(|home| PathBuf::from(home).join("Library/Application Support/MICE/history"))
    }

    pub fn record(&self, mut event: HistoryEvent) -> Result<(), std::io::Error> {
        event.question = bounded_chars(&event.question, 2_000);
        event.answer_digest = bounded_chars(&event.answer_digest, 500);
        event.app_context = event
            .app_context
            .map(|context| bounded_chars(&context, 120))
            .filter(|context| !context.trim().is_empty());
        let _lock = self.lock()?;
        let mut events: Vec<HistoryEvent> = read_jsonl(&self.root.join("history.jsonl"))?;
        events.push(event);
        if events.len() > 500 {
            events.drain(..events.len() - 500);
        }
        let body = events
            .iter()
            .map(serde_json::to_string)
            .collect::<Result<Vec<_>, _>>()
            .map_err(std::io::Error::other)?
            .join("\n");
        atomic_write(
            &self.root.join("history.jsonl"),
            format!("{body}\n").as_bytes(),
        )
    }

    pub fn search(&self, query: Option<&str>) -> Result<Vec<HistoryEvent>, std::io::Error> {
        let query = query.unwrap_or_default().trim().to_ascii_lowercase();
        let mut events: Vec<HistoryEvent> = read_jsonl(&self.root.join("history.jsonl"))?;
        events.retain(|event| {
            query.is_empty()
                || format!(
                    "{} {} {}",
                    event.question,
                    event.answer_digest,
                    event.app_context.as_deref().unwrap_or_default()
                )
                .to_ascii_lowercase()
                .contains(&query)
        });
        events.sort_by_key(|event| std::cmp::Reverse(event.ts));
        Ok(events)
    }

    pub fn remember(&self, note: &str) -> Result<(), std::io::Error> {
        let note = bounded_chars(note.trim(), 200);
        if note.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Preference note is empty.",
            ));
        }
        let _lock = self.lock()?;
        let path = self.root.join("preferences.json");
        let mut preferences: UserPreferences = fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        preferences.notes.push(note);
        if preferences.notes.len() > 10 {
            preferences.notes.drain(..preferences.notes.len() - 10);
        }
        atomic_write(
            &path,
            &serde_json::to_vec_pretty(&preferences).map_err(std::io::Error::other)?,
        )
    }

    pub fn preferences_preamble(&self) -> Result<Option<String>, std::io::Error> {
        let path = self.root.join("preferences.json");
        let preferences: UserPreferences = match fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        Ok((!preferences.notes.is_empty())
            .then(|| format!("The user prefers: {}.", preferences.notes.join("; "))))
    }

    pub fn clear(&self) -> Result<(), std::io::Error> {
        let _lock = self.lock()?;
        for file in ["history.jsonl", "preferences.json"] {
            match fs::remove_file(self.root.join(file)) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    fn lock(&self) -> Result<MemoryLock, std::io::Error> {
        lock_at(&self.root, "MICE history")
    }
}

impl SharedMemory {
    pub fn at(root: PathBuf) -> Result<Self, std::io::Error> {
        for directory in ["events", "facts", "digests", "artifacts", "macros"] {
            fs::create_dir_all(root.join(directory))?;
        }
        Ok(Self { root })
    }

    pub fn default_path() -> Option<PathBuf> {
        std::env::var_os("HOME")
            .map(|home| PathBuf::from(home).join("Library/Application Support/MICE/memory"))
    }

    pub fn append(&self, event: &MemoryEvent) -> Result<(), std::io::Error> {
        let _lock = self.lock()?;
        self.append_to("shared.jsonl", event)?;
        self.append_to(
            &format!("agent-{}.jsonl", digest_name(&event.session)),
            event,
        )?;
        self.rebuild_derived()?;
        Ok(())
    }

    fn append_to(&self, file: &str, event: &MemoryEvent) -> Result<(), std::io::Error> {
        let mut writer = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.root.join("events").join(file))?;
        let mut line = serde_json::to_vec(event).map_err(std::io::Error::other)?;
        line.push(b'\n');
        writer.write_all(&line)
    }

    pub fn events(&self) -> Result<Vec<MemoryEvent>, std::io::Error> {
        read_jsonl(&self.root.join("events/shared.jsonl"))
    }

    pub fn put_artifact(&self, key: &str, artifact: &CachedArtifact) -> Result<(), std::io::Error> {
        validate_storage_key(key)?;
        let _lock = self.lock()?;
        atomic_write(
            &self
                .root
                .join("artifacts")
                .join(format!("{}.json", digest_name(key))),
            &serde_json::to_vec_pretty(artifact).map_err(std::io::Error::other)?,
        )
    }

    pub fn artifact(&self, key: &str) -> Result<Option<CachedArtifact>, std::io::Error> {
        validate_storage_key(key)?;
        let path = self
            .root
            .join("artifacts")
            .join(format!("{}.json", digest_name(key)));
        if !path.exists() {
            return Ok(None);
        }
        let artifact: CachedArtifact =
            serde_json::from_slice(&fs::read(path)?).map_err(std::io::Error::other)?;
        if artifact.key != key {
            return Ok(None);
        }
        Ok(Some(artifact))
    }

    pub fn put_macro(&self, key: &str, calls: &serde_json::Value) -> Result<(), std::io::Error> {
        validate_storage_key(key)?;
        let _lock = self.lock()?;
        let stored = WorkflowMacro {
            key: key.into(),
            calls: calls.clone(),
        };
        atomic_write(
            &self
                .root
                .join("macros")
                .join(format!("{}.json", digest_name(key))),
            &serde_json::to_vec_pretty(&stored).map_err(std::io::Error::other)?,
        )
    }

    pub fn macro_for(&self, key: &str) -> Result<Option<serde_json::Value>, std::io::Error> {
        validate_storage_key(key)?;
        let path = self
            .root
            .join("macros")
            .join(format!("{}.json", digest_name(key)));
        if !path.exists() {
            return Ok(None);
        }
        let stored: WorkflowMacro = match serde_json::from_slice(&fs::read(path)?) {
            Ok(stored) => stored,
            // Old, unverified macro format is intentionally not replayed.
            Err(_) => return Ok(None),
        };
        Ok((stored.key == key).then_some(stored.calls))
    }

    pub fn query(&self, question: &str) -> Result<String, std::io::Error> {
        let terms = question
            .to_ascii_lowercase()
            .split_whitespace()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let mut matches = self
            .events()?
            .into_iter()
            .filter(|event| {
                let haystack = format!(
                    "{} {} {} {}",
                    event.kind,
                    event.text,
                    event.branch,
                    event.files.join(" ")
                )
                .to_ascii_lowercase();
                terms.iter().any(|term| haystack.contains(term))
            })
            .collect::<Vec<_>>();
        matches.sort_by_key(|event| std::cmp::Reverse(event.event_ts));
        if matches.is_empty() {
            return Ok("No matching shared-memory events.".into());
        }
        Ok(matches
            .into_iter()
            .take(12)
            .map(|event| format!("- [{}:{}] {}", event.session, event.branch, event.text))
            .collect::<Vec<_>>()
            .join("\n"))
    }

    pub fn team_status(&self) -> Result<String, std::io::Error> {
        let events = self.events()?;
        let mut latest = BTreeMap::<String, MemoryEvent>::new();
        let mut touched = BTreeMap::<String, BTreeSet<String>>::new();
        for event in events {
            for file in &event.files {
                touched
                    .entry(file.clone())
                    .or_default()
                    .insert(event.session.clone());
            }
            let replace = latest
                .get(&event.session)
                .is_none_or(|existing| existing.event_ts <= event.event_ts);
            if replace {
                latest.insert(event.session.clone(), event);
            }
        }
        let mut lines = latest
            .into_values()
            .map(|event| format!("- {} on {}: {}", event.agent, event.branch, event.text))
            .collect::<Vec<_>>();
        for (file, sessions) in touched {
            if sessions.len() > 1 {
                lines.push(format!(
                    "! overlap: {} touched by {}",
                    file,
                    sessions.into_iter().collect::<Vec<_>>().join(", ")
                ));
            }
        }
        Ok(if lines.is_empty() {
            "No active MICE sessions recorded.".into()
        } else {
            lines.join("\n")
        })
    }

    pub fn record_ledger(
        &self,
        session: &str,
        record: &LedgerRecord,
    ) -> Result<(), std::io::Error> {
        let event = MemoryEvent {
            event_ts: now(),
            recorded_ts: now(),
            session: session.into(),
            agent: session.into(),
            branch: "unknown".into(),
            kind: "ledger".into(),
            text: serde_json::to_string(record).map_err(std::io::Error::other)?,
            files: Vec::new(),
        };
        self.append(&event)
    }

    pub fn savings(&self) -> Result<SavingsReport, std::io::Error> {
        let mut report = SavingsReport::default();
        for event in self
            .events()?
            .into_iter()
            .filter(|event| event.kind == "ledger")
        {
            if let Ok(record) = serde_json::from_str::<LedgerRecord>(&event.text) {
                report.delegations += 1;
                report.frontier_tokens_avoided += record.frontier_tokens_avoided_est;
                *report.by_lane.entry(record.lane.clone()).or_default() += 1;
                if record.outcome == "cache_hit" {
                    report.cache_hits += 1;
                }
                if record.outcome == "macro_replay" {
                    report.macro_replays += 1;
                }
            }
        }
        Ok(report)
    }

    fn rebuild_derived(&self) -> Result<(), std::io::Error> {
        let events = self.events()?;
        let mut touched = BTreeMap::<String, Vec<String>>::new();
        let mut agents = BTreeMap::<String, String>::new();
        let mut decisions = Vec::new();
        for event in &events {
            agents.insert(
                event.session.clone(),
                format!("{} ({})", event.agent, event.branch),
            );
            for file in &event.files {
                touched
                    .entry(file.clone())
                    .or_default()
                    .push(event.session.clone());
            }
            if event.kind == "memory_note" {
                decisions.push(format!("- {} — {}", event.session, event.text));
            }
        }
        atomic_write(
            &self.root.join("facts/agents.json"),
            &serde_json::to_vec_pretty(&agents).map_err(std::io::Error::other)?,
        )?;
        atomic_write(
            &self.root.join("facts/touched.json"),
            &serde_json::to_vec_pretty(&touched).map_err(std::io::Error::other)?,
        )?;
        atomic_write(
            &self.root.join("facts/decisions.md"),
            decisions.join("\n").as_bytes(),
        )?;
        atomic_write(
            &self.root.join("digests/team.md"),
            self.team_status_from(&events).as_bytes(),
        )
    }

    fn team_status_from(&self, events: &[MemoryEvent]) -> String {
        events
            .iter()
            .rev()
            .take(20)
            .map(|event| format!("- [{}] {}", event.session, event.text))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

struct MemoryLock(PathBuf);

impl Drop for MemoryLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

impl SharedMemory {
    fn lock(&self) -> Result<MemoryLock, std::io::Error> {
        lock_at(&self.root, "MICE shared-memory")
    }
}

fn lock_at(root: &Path, name: &str) -> Result<MemoryLock, std::io::Error> {
    let path = root.join(".write.lock");
    for _ in 0..2_000 {
        match OpenOptions::new().create_new(true).write(true).open(&path) {
            Ok(_) => return Ok(MemoryLock(path)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                // A lock is held only across a small append/rebuild. If a
                // process crashed, reclaim a clearly stale lock instead
                // of permanently wedging every future writer.
                let stale = fs::metadata(&path)
                    .and_then(|metadata| metadata.modified())
                    .ok()
                    .and_then(|modified| modified.elapsed().ok())
                    .is_some_and(|age| age > Duration::from_secs(120));
                if stale {
                    let _ = fs::remove_file(&path);
                    continue;
                }
                thread::sleep(Duration::from_millis(5))
            }
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        format!("Timed out waiting for {name} writer lock."),
    ))
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn read_jsonl<T: for<'a> Deserialize<'a>>(path: &Path) -> Result<Vec<T>, std::io::Error> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let mut events = Vec::new();
    for line in BufReader::new(OpenOptions::new().read(true).open(path)?).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        // JSONL's final line can be torn by a crash. Keep valid history
        // readable and let the next append/rebuild repair derived views.
        if let Ok(event) = serde_json::from_str(&line) {
            events.push(event);
        }
    }
    Ok(events)
}

fn digest_name(value: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(value.as_bytes());
    format!("{:x}", digest.finalize())
}

fn validate_storage_key(key: &str) -> Result<(), std::io::Error> {
    if key.is_empty() || key.len() > 4_096 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "MICE storage key is empty or too large.",
        ));
    }
    Ok(())
}

fn atomic_write(path: &Path, contents: &[u8]) -> Result<(), std::io::Error> {
    let temporary = path.with_extension(format!("{}.tmp", now()));
    fs::write(&temporary, contents)?;
    restrict_to_user(&temporary)?;
    fs::rename(temporary, path)
}

fn restrict_to_user(path: &Path) -> Result<(), std::io::Error> {
    #[cfg(unix)]
    {
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

fn restrict_directory_to_user(path: &Path) -> Result<(), std::io::Error> {
    #[cfg(unix)]
    {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

fn bounded_chars(value: &str, max: usize) -> String {
    value.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_history_bounds_searches_and_clears_local_data() {
        let root = std::env::temp_dir().join(format!("mice-history-test-{}", now()));
        let history = UserHistory::at(root.clone()).unwrap();
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(&root).unwrap().permissions().mode() & 0o777,
            0o700
        );
        for index in 0..502 {
            history
                .record(HistoryEvent {
                    ts: index,
                    kind: HistoryKind::Ask,
                    question: format!("question {index}"),
                    answer_digest: "x".repeat(600),
                    app_context: None,
                })
                .unwrap();
        }
        let events = history.search(Some("question")).unwrap();
        assert_eq!(events.len(), 500);
        assert_eq!(events[0].question, "question 501");
        assert_eq!(events[0].answer_digest.chars().count(), 500);
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(root.join("history.jsonl"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        for index in 0..12 {
            history.remember(&format!("note {index}")).unwrap();
        }
        let preamble = history.preferences_preamble().unwrap().unwrap();
        assert!(!preamble.contains("note 0"));
        assert!(preamble.contains("note 11"));
        history.clear().unwrap();
        assert!(history.search(None).unwrap().is_empty());
        assert!(history.preferences_preamble().unwrap().is_none());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn shared_events_build_facts_and_flag_overlaps() {
        let root = std::env::temp_dir().join(format!("mice-memory-test-{}", now()));
        let store = SharedMemory::at(root.clone()).unwrap();
        for session in ["a", "b"] {
            store
                .append(&MemoryEvent {
                    event_ts: now(),
                    recorded_ts: now(),
                    session: session.into(),
                    agent: session.into(),
                    branch: session.into(),
                    kind: "tool".into(),
                    text: "edited module".into(),
                    files: vec!["src/lib.rs".into()],
                })
                .unwrap();
        }
        assert!(store.team_status().unwrap().contains("overlap: src/lib.rs"));
        assert!(root.join("facts/touched.json").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn artifact_cache_round_trips_a_bounded_result() {
        let root = std::env::temp_dir().join(format!("mice-artifact-test-{}", now()));
        let store = SharedMemory::at(root.clone()).unwrap();
        store
            .put_artifact(
                "git.status:key",
                &CachedArtifact {
                    key: "git.status:key".into(),
                    tool: "git.status".into(),
                    args: "{}".into(),
                    fingerprint: "head".into(),
                    distilled: "M main.rs".into(),
                    raw_output_tokens_est: 3,
                    truncated: false,
                    created_ts: now(),
                },
            )
            .unwrap();
        assert_eq!(
            store.artifact("git.status:key").unwrap().unwrap().distilled,
            "M main.rs"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn cryptographic_names_prevent_collisions_and_bound_long_keys() {
        assert_ne!(digest_name("a/b"), digest_name("a?b"));
        assert_eq!(digest_name(&"dirty-file/".repeat(10_000)).len(), 64);
    }

    #[test]
    fn concurrent_writers_leave_valid_complete_events() {
        let root =
            std::env::temp_dir().join(format!("mice-lock-test-{}-{}", now(), std::process::id()));
        let store = SharedMemory::at(root.clone()).unwrap();
        let workers = (0..8)
            .map(|index| {
                let store = store.clone();
                std::thread::spawn(move || {
                    store
                        .append(&MemoryEvent {
                            event_ts: now(),
                            recorded_ts: now(),
                            session: format!("s{index}"),
                            agent: "agent".into(),
                            branch: "main".into(),
                            kind: "tool".into(),
                            text: "completed".into(),
                            files: Vec::new(),
                        })
                        .unwrap()
                })
            })
            .collect::<Vec<_>>();
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(store.events().unwrap().len(), 8);
        let _ = fs::remove_dir_all(root);
    }
}
