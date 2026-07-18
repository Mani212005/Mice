//! File-backed shared memory and artifact cache.
//!
//! Events are authoritative and append-only. Facts/digests are derived on
//! demand, keeping the store inspectable and recoverable without a database.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

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
    pub tool: String,
    pub args: String,
    pub fingerprint: String,
    pub distilled: String,
    pub raw: String,
    pub truncated: bool,
    pub created_ts: u64,
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
        self.append_to("shared.jsonl", event)?;
        self.append_to(&format!("agent-{}.jsonl", safe_name(&event.session)), event)?;
        self.rebuild_derived()?;
        Ok(())
    }

    fn append_to(&self, file: &str, event: &MemoryEvent) -> Result<(), std::io::Error> {
        let mut writer = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.root.join("events").join(file))?;
        serde_json::to_writer(&mut writer, event).map_err(std::io::Error::other)?;
        writer.write_all(b"\n")
    }

    pub fn events(&self) -> Result<Vec<MemoryEvent>, std::io::Error> {
        read_jsonl(&self.root.join("events/shared.jsonl"))
    }

    pub fn put_artifact(&self, key: &str, artifact: &CachedArtifact) -> Result<(), std::io::Error> {
        fs::write(
            self.root
                .join("artifacts")
                .join(format!("{}.json", safe_name(key))),
            serde_json::to_vec_pretty(artifact).map_err(std::io::Error::other)?,
        )
    }

    pub fn artifact(&self, key: &str) -> Result<Option<CachedArtifact>, std::io::Error> {
        let path = self
            .root
            .join("artifacts")
            .join(format!("{}.json", safe_name(key)));
        if !path.exists() {
            return Ok(None);
        }
        serde_json::from_slice(&fs::read(path)?)
            .map(Some)
            .map_err(std::io::Error::other)
    }

    pub fn put_macro(&self, key: &str, calls: &serde_json::Value) -> Result<(), std::io::Error> {
        fs::write(
            self.root
                .join("macros")
                .join(format!("{}.json", safe_name(key))),
            serde_json::to_vec_pretty(calls).map_err(std::io::Error::other)?,
        )
    }

    pub fn macro_for(&self, key: &str) -> Result<Option<serde_json::Value>, std::io::Error> {
        let path = self
            .root
            .join("macros")
            .join(format!("{}.json", safe_name(key)));
        if !path.exists() {
            return Ok(None);
        }
        serde_json::from_slice(&fs::read(path)?)
            .map(Some)
            .map_err(std::io::Error::other)
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
        fs::write(
            self.root.join("facts/agents.json"),
            serde_json::to_vec_pretty(&agents).map_err(std::io::Error::other)?,
        )?;
        fs::write(
            self.root.join("facts/touched.json"),
            serde_json::to_vec_pretty(&touched).map_err(std::io::Error::other)?,
        )?;
        fs::write(self.root.join("facts/decisions.md"), decisions.join("\n"))?;
        fs::write(
            self.root.join("digests/team.md"),
            self.team_status_from(&events),
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
    BufReader::new(OpenOptions::new().read(true).open(path)?)
        .lines()
        .map_while(Result::ok)
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(&line).map_err(std::io::Error::other))
        .collect()
}

fn safe_name(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

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
                    tool: "git.status".into(),
                    args: "{}".into(),
                    fingerprint: "head".into(),
                    distilled: "M main.rs".into(),
                    raw: "M main.rs".into(),
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
}
