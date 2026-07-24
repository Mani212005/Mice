//! Mission Control: deterministic planning preflight and review client.
//!
//! The review client turns a repository plan into a visible, editable task
//! mapping before any launch is possible. It intentionally stops before
//! process launch or `git worktree add`: M2 owns that explicit mutation
//! boundary.

use std::{
    collections::BTreeSet,
    env,
    fs::{self, OpenOptions},
    io::{self, IsTerminal, Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[cfg(unix)]
use std::os::unix::net::UnixStream;

use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use mice_core::{
    MissionAgentCapability, MissionAgentKind, MissionIdentity, MissionRecord, MissionTask,
    MissionTaskAssignment, MissionTaskGraph, MissionTaskRuntime, MissionTaskState,
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::coordination::{self, RepoSnapshot, RiskLevel, RiskReport};

const MAX_PLAN_BYTES: u64 = 512 * 1024;
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_TASK_CANDIDATES: usize = 24;
const WORKTREE_TIMEOUT: Duration = Duration::from_secs(45);
const MAX_PATH_INDEX_PATHS: usize = 320;
const MAX_PATH_INDEX_BYTES: usize = 12 * 1024;
const MAX_PLANNER_PLAN_CHARS: usize = 18_000;
const MAX_TASK_NOTES: usize = 4;
const MAX_TASK_NOTE_CHARS: usize = 360;

type PreferredAssignments = std::collections::BTreeMap<String, MissionAgentKind>;
type ResolvedTaskGraph = (MissionTaskGraph, PreferredAssignments, String);
type ModelTaskGraph = (MissionTaskGraph, PreferredAssignments);

pub fn command(arguments: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    match arguments.first().map(String::as_str) {
        Some("plan") | Some("synthesize") => plan_command(arguments),
        Some("status") => mission_status(arguments.get(1).map(String::as_str)),
        Some("watch") => mission_watch(arguments.get(1).map(String::as_str)),
        Some("report") => mission_report(
            arguments.get(1).map(String::as_str),
            arguments.get(2).map(String::as_str),
        ),
        Some("note") => mission_note(
            arguments.get(1).map(String::as_str),
            arguments.get(2).map(String::as_str),
            arguments.get(3).map(String::as_str),
        ),
        Some("verify") => mission_verify(
            arguments.get(1).map(String::as_str),
            arguments.get(2).map(String::as_str),
            arguments.get(3).is_some_and(|argument| argument == "--yes"),
        ),
        _ => Err(usage().into()),
    }
}

fn plan_command(arguments: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let options = MissionOptions::parse(arguments)?;
    let cwd = env::current_dir()?;
    let snapshot = coordination::discover(&cwd)
        .map_err(|error| format!("Mission Control requires a Git worktree: {error}"))?;
    let worktree_root = current_worktree_root(&snapshot, &cwd)?;
    let plan = if let Some(goal) = &options.goal {
        synthesize_plan_file(goal, &worktree_root, &options)?
    } else {
        load_plan(&worktree_root, &options.plan_path)?
    };
    let agents = options
        .agents
        .iter()
        .copied()
        .map(probe_agent)
        .collect::<Vec<_>>();
    let (mut graph, mut preferred_assignments, mut planner_detail) =
        resolve_task_graph(&plan, &worktree_root, &options)?;
    let identity = mission_identity(&snapshot.repo_id, &plan);
    if let Some(record) = MissionLedger::existing(&identity)?.load()?
        && !record.graph.tasks.is_empty()
    {
        graph = record.graph;
        preferred_assignments = record
            .assignments
            .into_iter()
            .map(|assignment| (assignment.task_id, assignment.agent))
            .collect();
        planner_detail = "persisted validated graph from the active mission".into();
    }
    let suggested_assignments = suggested_assignments(&graph, &agents, &preferred_assignments);
    let cleanliness = tracked_worktree_cleanliness(&worktree_root);
    let risk_report = coordination::merge_risks(&snapshot);
    let mut preflight = MissionPreflight {
        snapshot,
        worktree_root,
        plan,
        graph,
        identity,
        agents,
        cleanliness,
        risk_report,
        planner_detail,
    };

    let initial_assignments = preflight
        .graph
        .tasks
        .iter()
        .map(|task| {
            suggested_assignments
                .iter()
                .find(|(id, _)| id == &task.id)
                .and_then(|(_, agent)| *agent)
        })
        .collect::<Vec<_>>();
    if options.launch {
        if !options.yes {
            return Err("Non-interactive launch requires `--launch --yes`; in a terminal, review the mapping and press Enter instead.".into());
        }
        return launch_approved(&preflight, &initial_assignments);
    }
    // Piped/automated callers get a stable textual report. A terminal opens
    // the review surface by default; `--dry-run` explicitly keeps the old
    // non-interactive behavior.
    if options.dry_run || !io::stdout().is_terminal() {
        return print_preflight(&preflight, &suggested_assignments);
    }
    let Some(assignments) = run_review_tui(&mut preflight, initial_assignments)? else {
        println!(
            "Mission review cancelled; no worktrees, branches, terminals, agents, or mission state changed."
        );
        return Ok(());
    };
    if options.review_only {
        println!("Mission mapping approved for review:");
        for (task, agent) in assignments.iter().enumerate() {
            let task = &suggested_assignments[task].0;
            let agent = agent
                .map(MissionAgentKind::display_name)
                .unwrap_or("unassigned");
            println!("- {task} → {agent}");
        }
        println!("No worktrees or agents were launched because `--review` was requested.");
        return Ok(());
    }
    launch_approved(&preflight, &assignments)
}

struct MissionPreflight {
    snapshot: RepoSnapshot,
    worktree_root: PathBuf,
    plan: LoadedPlan,
    graph: MissionTaskGraph,
    identity: MissionIdentity,
    agents: Vec<MissionAgentCapability>,
    cleanliness: String,
    risk_report: RiskReport,
    planner_detail: String,
}

/// Values resolved for a lifecycle command. Keeping this as a named type
/// avoids passing an easy-to-misorder collection of repository facts between
/// report/verification paths.
struct MissionLocation {
    worktree_root: PathBuf,
    plan: LoadedPlan,
    ledger: MissionLedger,
}

fn print_preflight(
    preflight: &MissionPreflight,
    suggested_assignments: &[(String, Option<MissionAgentKind>)],
) -> Result<(), Box<dyn std::error::Error>> {
    println!("MICE Mission Control — dry run");
    println!("Mission: {}", preflight.identity.mission_id);
    println!("Repository: {}", short_id(&preflight.identity.repo_id));
    println!("Plan: {}", preflight.plan.relative_path.display());
    println!(
        "Plan fingerprint: {}",
        short_id(&preflight.identity.plan_fingerprint)
    );
    println!("Base worktree: {}", preflight.worktree_root.display());
    println!("Tracked base state: {}", preflight.cleanliness);
    println!("Task planner: {}", preflight.planner_detail);
    println!();
    println!("Agent preflight:");
    for agent in &preflight.agents {
        let state = if agent.launch_ready {
            "ready to launch"
        } else if agent.installed {
            "installed, but not launch-ready"
        } else {
            "unavailable"
        };
        println!(
            "- {} ({state}; MCP command: {}): {}",
            agent.agent.display_name(),
            yes_no(agent.mcp_available),
            agent.detail
        );
    }
    println!();
    println!("Validated task candidates:");
    for task in &preflight.graph.tasks {
        let assignment = suggested_assignments
            .iter()
            .find(|(id, _)| id == &task.id)
            .and_then(|(_, agent)| *agent)
            .map(MissionAgentKind::display_name)
            .unwrap_or("unassigned");
        let scope = if task.predicted_paths.is_empty() {
            "scope: unclassified"
        } else {
            "scope: declared"
        };
        println!("- {} → {assignment} ({scope}) — {}", task.id, task.title);
    }
    print_risk_summary(&preflight.risk_report);
    println!();
    if preflight.agents.iter().all(|agent| !agent.launch_ready) {
        println!(
            "BLOCKED: no selected harness is launch-ready. Install/configure Codex, Claude Code, or the Antigravity `agy` CLI."
        );
    } else if preflight.cleanliness != "clean" {
        println!(
            "BLOCKED FOR FUTURE LAUNCH: commit, stash, or explicitly resolve the tracked base changes first."
        );
    } else {
        println!(
            "Preflight passed. Run this from a terminal without `--dry-run` to review the mapping. No worktrees, branches, terminals, agents, or mission state were created."
        );
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MissionOptions {
    plan_path: PathBuf,
    goal: Option<String>,
    agents: Vec<MissionAgentKind>,
    dry_run: bool,
    review_only: bool,
    launch: bool,
    yes: bool,
    planner: MissionPlannerMode,
    allow_cloud: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MissionPlannerMode {
    Auto,
    Markdown,
}

impl MissionOptions {
    fn parse(arguments: &[String]) -> Result<Self, Box<dyn std::error::Error>> {
        let Some(command) = arguments.first().map(String::as_str) else {
            return Err(usage().into());
        };
        if command != "plan" && command != "synthesize" {
            return Err(usage().into());
        }

        let mut plan_path = PathBuf::new();
        let mut goal = None;
        let mut index = 1;

        if command == "plan" {
            let path_opt = arguments.get(1).filter(|p| !p.starts_with("--"));
            if let Some(path_str) = path_opt {
                plan_path = PathBuf::from(path_str);
                index = 2;
            }
        }

        let mut agents = None;
        let mut dry_run = false;
        let mut review_only = false;
        let mut launch = false;
        let mut yes = false;
        let mut planner = MissionPlannerMode::Auto;
        let mut allow_cloud = false;

        while index < arguments.len() {
            match arguments[index].as_str() {
                "--goal" if index + 1 < arguments.len() => {
                    goal = Some(arguments[index + 1].clone());
                    index += 2;
                }
                "--agents" if index + 1 < arguments.len() => {
                    if agents.is_some() {
                        return Err("Specify `--agents` only once.".into());
                    }
                    agents = Some(parse_agents(&arguments[index + 1])?);
                    index += 2;
                }
                "--dry-run" => {
                    dry_run = true;
                    index += 1;
                }
                "--review" => {
                    review_only = true;
                    index += 1;
                }
                "--launch" => {
                    launch = true;
                    index += 1;
                }
                "--yes" => {
                    yes = true;
                    index += 1;
                }
                "--planner" if index + 1 < arguments.len() => {
                    planner = match arguments[index + 1].as_str() {
                        "auto" | "local" => MissionPlannerMode::Auto,
                        "markdown" => MissionPlannerMode::Markdown,
                        value => {
                            return Err(format!(
                                "Unknown mission planner `{value}`. Choose auto or markdown."
                            )
                            .into());
                        }
                    };
                    index += 2;
                }
                "--allow-cloud" => {
                    allow_cloud = true;
                    index += 1;
                }
                _ => return Err(usage().into()),
            }
        }

        if goal.is_none() && plan_path.as_os_str().is_empty() {
            return Err(
                "Mission Control requires either a plan file path or `--goal \"...\"`.".into(),
            );
        }

        let agents =
            agents.ok_or("Mission Control requires `--agents codex,claude,antigravity`.")?;
        Ok(Self {
            plan_path,
            goal,
            agents,
            dry_run,
            review_only,
            launch,
            yes,
            planner,
            allow_cloud,
        })
    }
}

fn usage() -> &'static str {
    "Usage:\n  mice mission plan <plan-file-under-plan/> --agents <codex,claude,antigravity> [--goal \"...\"] [--planner auto|markdown] [--allow-cloud] [--review|--dry-run|--launch --yes]\n  mice mission synthesize --goal \"...\" --agents <codex,claude,antigravity> [--allow-cloud] [--review|--dry-run|--launch --yes]\n  mice mission status <plan-file-under-plan/>\n  mice mission watch <plan-file-under-plan/>\n  mice mission report <plan-file-under-plan/> <task-id>\n  mice mission note <plan-file-under-plan/> <task-id> <decision-or-blocker>\n  mice mission verify <plan-file-under-plan/> <task-id> --yes"
}

fn parse_agents(value: &str) -> Result<Vec<MissionAgentKind>, Box<dyn std::error::Error>> {
    let mut seen = BTreeSet::new();
    let agents = value
        .split(',')
        .map(str::trim)
        .map(|value| {
            MissionAgentKind::parse(value).ok_or_else(|| {
                format!("Unknown mission agent `{value}`. Choose codex, claude, or antigravity.")
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    if agents.is_empty() {
        return Err("Choose at least one mission agent.".into());
    }
    if agents.iter().any(|agent| !seen.insert(*agent)) {
        return Err("Each mission agent may appear only once.".into());
    }
    Ok(agents)
}

#[derive(Debug, Clone)]
struct LoadedPlan {
    contents: String,
    relative_path: PathBuf,
    display_name: String,
    fingerprint: String,
}

fn load_plan(
    worktree_root: &Path,
    requested: &Path,
) -> Result<LoadedPlan, Box<dyn std::error::Error>> {
    if requested.is_absolute() {
        return Err("Mission plans must be repository-relative paths under `plan/`.".into());
    }
    let plan_directory = worktree_root
        .join("plan")
        .canonicalize()
        .map_err(|error| format!("Mission plans require a readable `plan/` directory: {error}"))?;
    let path = worktree_root
        .join(requested)
        .canonicalize()
        .map_err(|error| {
            format!(
                "Could not read mission plan `{}`: {error}",
                requested.display()
            )
        })?;
    if !path.starts_with(&plan_directory)
        || path.extension().and_then(|value| value.to_str()) != Some("md")
    {
        return Err(
            "Mission plans must be Markdown files inside this repository's `plan/` directory."
                .into(),
        );
    }
    let metadata = fs::metadata(&path)?;
    if !metadata.is_file() || metadata.len() > MAX_PLAN_BYTES {
        return Err(format!(
            "Mission plan must be a regular Markdown file no larger than {} KiB.",
            MAX_PLAN_BYTES / 1024
        )
        .into());
    }
    let contents = fs::read_to_string(&path)?;
    let relative_path = path
        .strip_prefix(worktree_root)
        .map_err(|_| "Mission plan escaped the active worktree.")?
        .to_path_buf();
    let display_name = path
        .file_stem()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .ok_or("Mission plan has no usable filename.")?
        .to_owned();
    Ok(LoadedPlan {
        fingerprint: digest(&contents),
        contents,
        relative_path,
        display_name,
    })
}

fn current_worktree_root(
    snapshot: &RepoSnapshot,
    cwd: &Path,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let cwd = cwd.canonicalize()?;
    snapshot
        .worktrees
        .iter()
        .filter_map(|worktree| PathBuf::from(&worktree.path).canonicalize().ok())
        .filter(|candidate| cwd.starts_with(candidate))
        .max_by_key(|candidate| candidate.components().count())
        .ok_or("Git did not report the current worktree root.".into())
}

fn mission_identity(repo_id: &str, plan: &LoadedPlan) -> MissionIdentity {
    let slug = slug(&plan.display_name);
    // Bind the public mission ID to the full repository identity as well as
    // the plan fingerprint. Future persistence must still key by the pair in
    // `MissionIdentity`, but a reused plan file cannot accidentally produce
    // the same mission ID in another repository.
    let mut hasher = Sha256::new();
    hasher.update(b"mice-mission-v1\0");
    hasher.update(repo_id.as_bytes());
    hasher.update([0]);
    hasher.update(plan.fingerprint.as_bytes());
    let scoped_fingerprint = format!("{:x}", hasher.finalize());
    let short_fingerprint = scoped_fingerprint.get(..12).unwrap_or(&scoped_fingerprint);
    MissionIdentity {
        repo_id: repo_id.into(),
        mission_id: format!("{slug}-{short_fingerprint}"),
        plan_fingerprint: plan.fingerprint.clone(),
    }
}

fn task_graph_from_markdown(
    contents: &str,
    plan_name: &str,
) -> Result<MissionTaskGraph, Box<dyn std::error::Error>> {
    let checklist = contents
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim_start();
            trimmed
                .strip_prefix("- [ ] ")
                .or_else(|| trimmed.strip_prefix("* [ ] "))
                .map(str::trim)
        })
        .filter(|title| !title.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let lines = contents.lines().collect::<Vec<_>>();
    let headings = lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| {
            markdown_task_heading(line).map(|(depth, title)| (index, depth, title))
        })
        .collect::<Vec<_>>();
    // Prefer the more specific heading depth when a plan has it: `## High`
    // is normally a grouping label, while `### H1` is the real work unit.
    let has_detailed_headings = headings.iter().any(|(_, depth, _)| *depth >= 3);
    let heading_candidates = headings
        .iter()
        .enumerate()
        .filter(|(_, (_, depth, title))| {
            !title.is_empty() && (!has_detailed_headings || *depth >= 3)
        })
        .map(|(position, (line_index, depth, title))| {
            let next_boundary = headings
                .iter()
                .skip(position + 1)
                .find_map(|(next_index, next_depth, _)| {
                    (*next_depth <= *depth).then_some(*next_index)
                })
                .unwrap_or(lines.len());
            let metadata = mice_task_metadata(&lines[line_index + 1..next_boundary])?;
            Ok(((*title).to_owned(), metadata))
        })
        .collect::<Result<Vec<_>, String>>()?;
    let candidates = if checklist.is_empty() {
        heading_candidates
    } else {
        checklist
            .into_iter()
            .map(|title| (title, MiceTaskMetadata::default()))
            .collect()
    };
    if candidates.is_empty() {
        return Err(format!(
            "`{plan_name}` has no task candidates. Add Markdown headings or unchecked `- [ ]` items."
        )
        .into());
    }
    if candidates.len() > MAX_TASK_CANDIDATES {
        return Err(format!(
            "`{plan_name}` has {} task candidates; M0 limits a mission dry run to {MAX_TASK_CANDIDATES}.",
            candidates.len()
        )
        .into());
    }
    let tasks = candidates
        .into_iter()
        .enumerate()
        .map(|(index, (title, metadata))| MissionTask {
            id: metadata
                .id
                .unwrap_or_else(|| format!("task-{:02}-{}", index + 1, slug(&title))),
            acceptance: vec![format!(
                "Complete and verify `{title}` according to {plan_name}."
            )],
            title,
            dependencies: metadata.dependencies,
            // Free-form plans deliberately leave scope unknown. A planner
            // can opt in to these deterministic constraints, which are then
            // validated by the portable core before launch.
            predicted_paths: metadata.predicted_paths,
        })
        .collect();
    Ok(MissionTaskGraph { tasks })
}

/// The model is allowed to propose a task graph, never to bypass it. This
/// private decoding shape deliberately maps into the portable core type only
/// after every model-owned field has passed deterministic validation.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelMissionProposal {
    tasks: Vec<ModelMissionTask>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelMissionTask {
    id: String,
    title: String,
    acceptance: Vec<String>,
    #[serde(default)]
    dependencies: Vec<String>,
    #[serde(default)]
    predicted_paths: Vec<String>,
    #[serde(default)]
    preferred_agent: Option<String>,
}

fn resolve_task_graph(
    plan: &LoadedPlan,
    worktree_root: &Path,
    options: &MissionOptions,
) -> Result<ResolvedTaskGraph, Box<dyn std::error::Error>> {
    let fallback: Result<MissionTaskGraph, Box<dyn std::error::Error>> =
        task_graph_from_markdown(&plan.contents, &plan.display_name).and_then(|graph| {
            graph
                .validate()
                .map_err(|error| -> Box<dyn std::error::Error> {
                    format!("Plan task graph: {error}").into()
                })?;
            Ok(graph)
        });
    if options.planner == MissionPlannerMode::Markdown {
        return Ok((
            fallback?,
            Default::default(),
            "deterministic Markdown parser (explicit)".into(),
        ));
    }
    let path_index = tracked_path_index(worktree_root).unwrap_or_else(|error| {
        format!("(unavailable; model must leave uncertain scope empty: {error})")
    });
    let prompt = mission_planner_prompt(plan, &path_index, &options.agents);
    match crate::mission_planner_response(&prompt, options.allow_cloud)
        .and_then(|response| model_mission_proposal(&response, &options.agents))
    {
        Ok((graph, assignments)) => {
            let lane = if options.allow_cloud {
                "local-first planner (cloud fallback allowed)"
            } else {
                "local planner"
            };
            Ok((graph, assignments, lane.into()))
        }
        Err(error) => match fallback {
            Ok(graph) => Ok((
                graph,
                Default::default(),
                format!(
                    "deterministic Markdown fallback; local planner was unavailable or unsafe ({})",
                    short_message(&error.to_string(), 180)
                ),
            )),
            Err(fallback_error) => Err(format!(
                "Mission planner failed ({}) and the deterministic Markdown fallback also failed ({fallback_error}).",
                short_message(&error.to_string(), 180)
            )
            .into()),
        },
    }
}

fn model_mission_proposal(
    response: &str,
    available_agents: &[MissionAgentKind],
) -> Result<ModelTaskGraph, Box<dyn std::error::Error>> {
    let proposal: ModelMissionProposal = serde_json::from_str(json_only(response).ok_or(
        "Mission planner did not return one JSON object; MICE will use the deterministic plan parser.",
    )?)?;
    let mut assignments = std::collections::BTreeMap::new();
    let tasks = proposal
        .tasks
        .into_iter()
        .map(|task| {
            if let Some(agent) = task.preferred_agent.as_deref() {
                let agent = MissionAgentKind::parse(agent).ok_or_else(|| {
                    format!(
                        "Mission planner chose unknown agent `{agent}` for `{}`.",
                        task.id
                    )
                })?;
                if !available_agents.contains(&agent) {
                    return Err(format!(
                        "Mission planner assigned `{}` to `{}`, which was not requested.",
                        task.id,
                        agent.id()
                    ));
                }
                if assignments.insert(task.id.clone(), agent).is_some() {
                    return Err(format!(
                        "Mission planner assigned task `{}` more than once.",
                        task.id
                    ));
                }
            }
            Ok(MissionTask {
                id: task.id,
                title: task.title,
                acceptance: task.acceptance,
                dependencies: task.dependencies,
                predicted_paths: task.predicted_paths,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let graph = MissionTaskGraph { tasks };
    graph
        .validate()
        .map_err(|error| format!("Mission planner proposed an unsafe graph: {error}"))?;
    Ok((graph, assignments))
}

/// Accept a bare object or a fenced JSON object, but never attempt to fish a
/// partial object out of arbitrary prose. A planner that cannot follow this
/// contract simply falls back to the conservative Markdown parser.
fn json_only(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    if trimmed.starts_with('{') && trimmed.ends_with('}') {
        return Some(trimmed);
    }
    let fenced = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))?
        .trim();
    fenced.strip_suffix("```").map(str::trim)
}

fn mission_planner_prompt(
    plan: &LoadedPlan,
    path_index: &str,
    agents: &[MissionAgentKind],
) -> String {
    let agents = agents
        .iter()
        .map(|agent| agent.id())
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "You are MICE Mission Control. Convert the repository plan below into one JSON object only. Do not include markdown or prose. Return {{\"tasks\":[...]}}. Each task must have exactly id, title, acceptance, dependencies, predicted_paths, preferred_agent. Use lowercase kebab-case IDs. acceptance must be one or more concrete checks. dependencies must contain task IDs only. predicted_paths must be relative repository files or directories without globs; use [] when uncertain. Parallel tasks may not share a file or directory scope. preferred_agent must be one of [{agents}] or null. Do not invent source contents, credentials, commands that bypass permissions, merges, rebases, or terminal actions. Keep the plan to at most {MAX_TASK_CANDIDATES} tasks.\n\nRepository plan ({})\n---\n{}\n---\n\nBounded tracked-path index (paths only; it may be incomplete)\n---\n{}\n---",
        plan.relative_path.display(),
        planner_plan_excerpt(&plan.contents),
        path_index
    )
}

fn planner_plan_excerpt(contents: &str) -> String {
    let excerpt = contents
        .chars()
        .take(MAX_PLANNER_PLAN_CHARS)
        .collect::<String>();
    if contents.chars().count() > MAX_PLANNER_PLAN_CHARS {
        format!(
            "{excerpt}\n\n[MICE truncated the plan excerpt; leave scope empty rather than guessing.]"
        )
    } else {
        excerpt
    }
}

fn synthesize_plan_file(
    goal: &str,
    worktree_root: &Path,
    options: &MissionOptions,
) -> Result<LoadedPlan, Box<dyn std::error::Error>> {
    let path_index = tracked_path_index(worktree_root).unwrap_or_default();
    let dummy_plan = LoadedPlan {
        contents: format!("# Goal: {goal}\n"),
        relative_path: PathBuf::from("plan/synthetic.md"),
        display_name: slug(goal),
        fingerprint: digest(goal),
    };
    let prompt = mission_planner_prompt(&dummy_plan, &path_index, &options.agents);
    let (graph, _assignments) = match crate::mission_planner_response(&prompt, options.allow_cloud)
        .and_then(|response| model_mission_proposal(&response, &options.agents))
    {
        Ok(result) => result,
        Err(_) => {
            let task = MissionTask {
                id: format!("task-01-{}", slug(goal)),
                title: goal.to_owned(),
                acceptance: vec![format!("Complete `{goal}`")],
                dependencies: vec![],
                predicted_paths: vec![],
            };
            let graph = MissionTaskGraph { tasks: vec![task] };
            graph
                .validate()
                .map_err(|e| format!("Synthetic graph validation: {e}"))?;
            (graph, std::collections::BTreeMap::new())
        }
    };

    let slug_name = slug(goal);
    let plan_dir = worktree_root.join("plan");
    fs::create_dir_all(&plan_dir)?;
    let file_name = format!("synthesized-{slug_name}.md");
    let target_path = plan_dir.join(&file_name);

    let mut md = format!("# Mission: {goal}\n\n## Tasks\n\n");
    for task in &graph.tasks {
        md.push_str(&format!("### {}\n\n", task.title));
        md.push_str(&format!("- mice:id: {}\n", task.id));
        if !task.dependencies.is_empty() {
            md.push_str(&format!(
                "- mice:depends: {}\n",
                task.dependencies.join(", ")
            ));
        }
        if !task.predicted_paths.is_empty() {
            md.push_str(&format!(
                "- mice:paths: {}\n",
                task.predicted_paths.join(", ")
            ));
        }
        md.push_str("\nAcceptance:\n");
        for check in &task.acceptance {
            md.push_str(&format!("- [ ] {check}\n"));
        }
        md.push('\n');
    }

    fs::write(&target_path, &md)?;
    let relative_path = PathBuf::from("plan").join(&file_name);

    Ok(LoadedPlan {
        fingerprint: digest(&md),
        contents: md,
        relative_path,
        display_name: format!("synthesized-{slug_name}"),
    })
}

fn tracked_path_index(worktree_root: &Path) -> Result<String, String> {
    let output = bounded_command_output(
        "git",
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ],
        worktree_root,
        PROBE_TIMEOUT,
        MAX_PATH_INDEX_BYTES,
    )?;
    let paths = output
        .split('\0')
        .map(str::trim)
        .filter(|path| !path.is_empty() && !path.contains('\n') && !path.contains('\r'))
        .take(MAX_PATH_INDEX_PATHS)
        .collect::<Vec<_>>();
    if paths.is_empty() {
        return Err("Git did not return any tracked paths".into());
    }
    Ok(paths.join("\n"))
}

fn bounded_command_output(
    binary: &str,
    arguments: &[&str],
    current_dir: &Path,
    timeout: Duration,
    max_bytes: usize,
) -> Result<String, String> {
    let mut child = Command::new(binary)
        .args(arguments)
        .current_dir(current_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("could not start `{binary}`: {error}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| format!("could not capture `{binary}` output"))?;
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let mut stdout = stdout;
        let mut output = Vec::with_capacity(max_bytes);
        let mut buffer = [0_u8; 4096];
        let result = loop {
            match stdout.read(&mut buffer) {
                Ok(0) => break Ok(output),
                Ok(count) => {
                    let remaining = max_bytes.saturating_sub(output.len());
                    output.extend_from_slice(&buffer[..count.min(remaining)]);
                }
                Err(error) => break Err(error.to_string()),
            }
        };
        let _ = sender.send(result);
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("could not inspect `{binary}`: {error}"))?
        {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "`{binary}` did not finish within {} seconds",
                timeout.as_secs()
            ));
        }
        thread::sleep(Duration::from_millis(20));
    };
    let output = receiver
        .recv_timeout(timeout)
        .map_err(|_| format!("`{binary}` output reader did not finish"))?
        .map_err(|error| format!("could not read `{binary}` output: {error}"))?;
    if !status.success() {
        return Err(format!("`{binary}` returned an unsuccessful status"));
    }
    String::from_utf8(output).map_err(|_| format!("`{binary}` returned non-UTF-8 output"))
}

fn short_message(value: &str, maximum: usize) -> String {
    let compact = value
        .chars()
        .filter(|character| !character.is_control() || *character == ' ')
        .take(maximum)
        .collect::<String>();
    if value.chars().count() > maximum {
        format!("{compact}…")
    } else {
        compact
    }
}

/// Optional, deterministic metadata emitted by a planning model. A plan may
/// give each heading a stable ID, file/directory scope, and predecessor IDs.
/// Paths are deliberately limited to actual relative files/directories rather
/// than globs, so the portable core can prove parallel work is disjoint.
///
/// ```markdown
/// ### Add the launcher
/// <!-- mice: id=launcher; paths=crates/mice-cli/src/mission.rs,agent-macos; depends=core -->
/// ```
#[derive(Default)]
struct MiceTaskMetadata {
    id: Option<String>,
    dependencies: Vec<String>,
    predicted_paths: Vec<String>,
}

fn mice_task_metadata(lines: &[&str]) -> Result<MiceTaskMetadata, String> {
    let mut metadata = MiceTaskMetadata::default();
    for line in lines {
        let Some(body) = line
            .trim()
            .strip_prefix("<!-- mice:")
            .and_then(|value| value.strip_suffix("-->"))
        else {
            continue;
        };
        for field in body
            .trim()
            .split(';')
            .map(str::trim)
            .filter(|field| !field.is_empty())
        {
            let Some((key, value)) = field.split_once('=') else {
                return Err(format!(
                    "Invalid MICE task metadata `{field}`; use key=value."
                ));
            };
            let values = value
                .split(',')
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>();
            match key.trim() {
                "id" => {
                    if metadata.id.is_some() || values.len() != 1 {
                        return Err("MICE task metadata permits exactly one `id`.".into());
                    }
                    metadata.id = values.into_iter().next();
                }
                "paths" => metadata.predicted_paths.extend(values),
                "depends" => metadata.dependencies.extend(values),
                unknown => {
                    return Err(format!(
                        "Unknown MICE task metadata key `{unknown}`; use id, paths, or depends."
                    ));
                }
            }
        }
    }
    Ok(metadata)
}

fn markdown_task_heading(line: &str) -> Option<(usize, &str)> {
    let trimmed = line.trim_start();
    let hashes = trimmed
        .chars()
        .take_while(|character| *character == '#')
        .count();
    (2..=4)
        .contains(&hashes)
        .then(|| trimmed.get(hashes..))
        .flatten()
        .and_then(|rest| rest.strip_prefix(' '))
        .map(str::trim)
        .map(|title| (hashes, title))
}

fn suggested_assignments(
    graph: &MissionTaskGraph,
    agents: &[MissionAgentCapability],
    preferred: &std::collections::BTreeMap<String, MissionAgentKind>,
) -> Vec<(String, Option<MissionAgentKind>)> {
    let ready = agents
        .iter()
        .filter(|agent| agent.launch_ready)
        .map(|agent| agent.agent)
        .collect::<Vec<_>>();
    graph
        .tasks
        .iter()
        .enumerate()
        .map(|(index, task)| {
            (
                task.id.clone(),
                preferred
                    .get(&task.id)
                    .copied()
                    .filter(|agent| ready.contains(agent))
                    .or_else(|| ready.get(index % ready.len().max(1)).copied()),
            )
        })
        .collect()
}

fn probe_agent(agent: MissionAgentKind) -> MissionAgentCapability {
    let (binary, mcp_probe, adapter_supported, adapter_name) = match agent {
        MissionAgentKind::Codex => ("codex", Some(vec!["mcp", "--help"]), true, "Codex"),
        // Claude Code's normal interactive command line is launched in a
        // visible terminal, keeping its built-in permission prompts intact.
        MissionAgentKind::Claude => ("claude", Some(vec!["mcp", "--help"]), true, "Claude Code"),
        // Antigravity CLI loads MCP servers from a user-managed JSON config;
        // it does not expose an `agy mcp` management command. Its unknown
        // subcommand help exits successfully, so treating that as a probe
        // would falsely advertise a connected MICE MCP server.
        MissionAgentKind::Antigravity => ("agy", None, true, "Antigravity"),
    };
    let version = bounded_status(binary, &["--version"]);
    let installed = version.as_ref().is_ok_and(|status| status.success());
    let mcp_available = installed
        && mcp_probe.as_ref().is_some_and(|arguments| {
            bounded_status(binary, arguments).is_ok_and(|status| status.success())
        });
    // A provider's MICE launcher does not require an MCP server to be
    // configured; MCP availability is still reported so the user can assess
    // richer cross-agent integration separately.
    let launch_ready = installed && adapter_supported;
    let detail = match (installed, adapter_supported, mcp_available, version) {
        (false, _, _, Err(error)) => error,
        (false, _, _, Ok(_)) => format!("`{binary} --version` exited unsuccessfully"),
        (true, false, _, _) => {
            format!("executable detected; MICE needs a verified {adapter_name} launch adapter")
        }
        (true, true, false, _) if mcp_probe.is_none() => {
            "executable passed the direct launch probe; shared mission context is prompt-based because this Antigravity CLI has no direct MCP configuration command".into()
        }
        (true, true, false, _) => {
            "executable passed the direct launch probe; its MCP command did not pass".into()
        }
        (true, true, true, _) => "executable and MCP command passed bounded probes".into(),
    };
    MissionAgentCapability {
        agent,
        installed,
        mcp_available,
        launch_ready,
        detail,
    }
}

fn bounded_status(binary: &str, arguments: &[&str]) -> Result<ExitStatus, String> {
    bounded_status_in(binary, arguments, None)
}

fn bounded_status_in(
    binary: &str,
    arguments: &[&str],
    current_dir: Option<&Path>,
) -> Result<ExitStatus, String> {
    let mut command = Command::new(binary);
    command
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(current_dir) = current_dir {
        command.current_dir(current_dir);
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("`{binary}` is not runnable: {error}"))?;
    let deadline = Instant::now() + PROBE_TIMEOUT;
    loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("could not inspect `{binary}`: {error}"))?
        {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "`{binary}` did not answer its capability probe within {} seconds",
                PROBE_TIMEOUT.as_secs()
            ));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn tracked_worktree_cleanliness(worktree_root: &Path) -> String {
    let unstaged = bounded_status_in("git", &["diff", "--quiet"], Some(worktree_root));
    let staged = bounded_status_in("git", &["diff", "--cached", "--quiet"], Some(worktree_root));
    match (unstaged, staged) {
        (Ok(left), Ok(right)) if left.success() && right.success() => "clean".into(),
        (Ok(left), Ok(right)) if left.code() == Some(1) || right.code() == Some(1) => {
            "tracked changes present".into()
        }
        (Err(error), _) | (_, Err(error)) => format!("unassessed ({error})"),
        _ => "unassessed (Git diff returned an unexpected status)".into(),
    }
}

fn digest(value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn slug(value: &str) -> String {
    let mut slug = String::new();
    let mut previous_dash = false;
    for character in value.chars() {
        if character.is_ascii_alphanumeric() {
            slug.push(character.to_ascii_lowercase());
            previous_dash = false;
        } else if !previous_dash {
            slug.push('-');
            previous_dash = true;
        }
    }
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        "plan".into()
    } else {
        slug.chars().take(40).collect()
    }
}

fn short_id(value: &str) -> &str {
    value.get(..12).unwrap_or(value)
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn print_risk_summary(report: &RiskReport) {
    println!();
    if report.risks.is_empty() && report.unassessed.is_empty() {
        println!("Current worktree risk: no shared changed-file surfaces detected.");
        return;
    }
    println!("Current worktree risk:");
    for risk in &report.risks {
        let level = match risk.level {
            RiskLevel::Yellow => "YELLOW",
            RiskLevel::Red => "RED",
        };
        println!(
            "- {level}: {} — {} ↔ {}",
            risk.path, risk.left_branch, risk.right_branch
        );
    }
    for pair in &report.unassessed {
        println!(
            "- UNASSESSED: {} ↔ {}; {}",
            pair.left_branch, pair.right_branch, pair.reason
        );
    }
}

/// Best-effort delivery to the resident `mice start` process. Mission
/// lifecycle data stays in the portable core; the daemon forwards this short,
/// sanitized operational notice over its existing JSON-RPC bridge to the
/// native macOS surface. If MICE is not running, the terminal status remains
/// authoritative and no socket or daemon is started implicitly.
fn notify_running_daemon(text: &str) {
    #[cfg(unix)]
    {
        let Some(home) = env::var_os("HOME") else {
            return;
        };
        let socket = PathBuf::from(home).join("Library/Application Support/MICE/bridge.sock");
        let Ok(mut stream) = UnixStream::connect(socket) else {
            return;
        };
        let text = text
            .chars()
            .filter(|character| !character.is_control() || *character == '\n')
            .take(600)
            .collect::<String>();
        if text.trim().is_empty() {
            return;
        }
        let _ = mice_ipc::write_frame(
            &mut stream,
            &serde_json::json!({"type":"mission.notification", "text":text}),
        );
    }
    #[cfg(not(unix))]
    {
        let _ = text;
    }
}

/// M1 is deliberately a review client rather than a second daemon. Closing
/// the terminal simply abandons this in-memory view; no mission state has
/// been persisted and no worktree has been created yet.
struct MissionReview {
    selected: usize,
    assignments: Vec<Option<MissionAgentKind>>,
    message: String,
}

fn run_review_tui(
    preflight: &mut MissionPreflight,
    assignments: Vec<Option<MissionAgentKind>>,
) -> Result<Option<Vec<Option<MissionAgentKind>>>, Box<dyn std::error::Error>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    let result = review_event_loop(
        &mut terminal,
        preflight,
        MissionReview {
            selected: 0,
            assignments,
            message: "Review mappings before MICE can create anything.".into(),
        },
    );
    let _ = disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let _ = terminal.show_cursor();
    result
}

fn review_event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    preflight: &mut MissionPreflight,
    mut review: MissionReview,
) -> Result<Option<Vec<Option<MissionAgentKind>>>, Box<dyn std::error::Error>> {
    loop {
        terminal.draw(|frame| draw_review(frame, preflight, &review))?;
        if !event::poll(Duration::from_millis(250))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                review.selected = review
                    .selected
                    .checked_sub(1)
                    .unwrap_or(preflight.graph.tasks.len() - 1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                review.selected = (review.selected + 1) % preflight.graph.tasks.len();
            }
            KeyCode::Left | KeyCode::Char('h') => {
                review.assignments[review.selected] = cycle_assignment(
                    review.assignments[review.selected],
                    &preflight.agents,
                    false,
                );
                review.message = "Assignment changed locally; nothing has launched.".into();
            }
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Char('e') => {
                review.assignments[review.selected] =
                    cycle_assignment(review.assignments[review.selected], &preflight.agents, true);
                review.message = "Assignment changed locally; nothing has launched.".into();
            }
            KeyCode::Char('r') => match refresh_preflight(preflight) {
                Ok(()) => {
                    review.message = "Refreshed Git worktrees and bounded overlap scan.".into()
                }
                Err(error) => review.message = format!("Refresh was unassessed: {error}"),
            },
            KeyCode::Enter => return Ok(Some(review.assignments)),
            KeyCode::Esc | KeyCode::Char('q') => return Ok(None),
            _ => {}
        }
    }
}

fn refresh_preflight(preflight: &mut MissionPreflight) -> Result<(), String> {
    let snapshot =
        coordination::discover(&preflight.worktree_root).map_err(|error| error.to_string())?;
    preflight.risk_report = coordination::merge_risks(&snapshot);
    preflight.snapshot = snapshot;
    preflight.cleanliness = tracked_worktree_cleanliness(&preflight.worktree_root);
    Ok(())
}

fn cycle_assignment(
    current: Option<MissionAgentKind>,
    agents: &[MissionAgentCapability],
    forward: bool,
) -> Option<MissionAgentKind> {
    let mut choices = vec![None];
    choices.extend(
        agents
            .iter()
            .filter(|agent| agent.launch_ready)
            .map(|agent| Some(agent.agent)),
    );
    choices.sort();
    let current_index = choices
        .iter()
        .position(|choice| *choice == current)
        .unwrap_or(0);
    let next = if forward {
        (current_index + 1) % choices.len()
    } else {
        current_index.checked_sub(1).unwrap_or(choices.len() - 1)
    };
    choices[next]
}

fn draw_review(frame: &mut ratatui::Frame, preflight: &MissionPreflight, review: &MissionReview) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(7),
            Constraint::Min(8),
            Constraint::Length(8),
            Constraint::Length(3),
        ])
        .split(area);
    let base_status = if preflight.cleanliness == "clean" {
        Span::styled("clean", Style::default().fg(Color::Green))
    } else {
        Span::styled(
            preflight.cleanliness.as_str(),
            Style::default().fg(Color::Red),
        )
    };
    let ready_agents = preflight
        .agents
        .iter()
        .filter(|agent| agent.launch_ready)
        .map(|agent| agent.agent.display_name())
        .collect::<Vec<_>>();
    let header = vec![
        Line::from(vec![
            Span::styled(
                " MICE Mission Control ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("review before launch"),
        ]),
        Line::from(format!(
            " mission {} · repo {} · plan {}",
            preflight.identity.mission_id,
            short_id(&preflight.identity.repo_id),
            preflight.plan.relative_path.display()
        )),
        Line::from(vec![Span::raw(" base: "), base_status]),
        Line::from(format!(
            " launch-capable: {} · active worktrees: {}",
            if ready_agents.is_empty() {
                "none".into()
            } else {
                ready_agents.join(", ")
            },
            preflight.snapshot.worktrees.len()
        )),
        Line::from(format!(
            " planner: {}",
            short_message(&preflight.planner_detail, 96)
        )),
    ];
    frame.render_widget(
        Paragraph::new(header).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Mission preflight "),
        ),
        chunks[0],
    );

    let visible = chunks[1].height.saturating_sub(2) as usize;
    let offset = review.selected.saturating_sub(visible.saturating_sub(1));
    let task_lines = preflight
        .graph
        .tasks
        .iter()
        .enumerate()
        .skip(offset)
        .take(visible.max(1))
        .map(|(index, task)| {
            let marker = if index == review.selected { "›" } else { " " };
            let agent = review.assignments[index]
                .map(MissionAgentKind::display_name)
                .unwrap_or("unassigned");
            let scope = if task.predicted_paths.is_empty() {
                "scope unknown"
            } else {
                "declared scope"
            };
            let schedule = if task.dependencies.is_empty() {
                "ready"
            } else {
                "waiting on dependencies"
            };
            let text = format!(
                "{marker} {:<20} → {:<12} [{schedule}; {scope}]  {}",
                task.id, agent, task.title
            );
            let style = if index == review.selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else if review.assignments[index].is_none() {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default()
            };
            Line::from(Span::styled(text, style))
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(task_lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Tasks · ←/→ assign only proven-capable harnesses "),
        ),
        chunks[1],
    );

    let mut risk_lines = Vec::new();
    if preflight.risk_report.risks.is_empty() && preflight.risk_report.unassessed.is_empty() {
        risk_lines.push(Line::from(Span::styled(
            "No shared changed-file surfaces detected among assessed worktrees.",
            Style::default().fg(Color::Green),
        )));
    }
    for risk in preflight.risk_report.risks.iter().take(3) {
        let (label, color) = match risk.level {
            RiskLevel::Yellow => ("YELLOW", Color::Yellow),
            RiskLevel::Red => ("RED", Color::Red),
        };
        risk_lines.push(Line::from(vec![
            Span::styled(format!("{label}: "), Style::default().fg(color)),
            Span::raw(format!(
                "{} — {} ↔ {}",
                risk.path, risk.left_branch, risk.right_branch
            )),
        ]));
    }
    for pair in preflight.risk_report.unassessed.iter().take(2) {
        risk_lines.push(Line::from(Span::styled(
            format!(
                "UNASSESSED: {} ↔ {}; {}",
                pair.left_branch, pair.right_branch, pair.reason
            ),
            Style::default().fg(Color::Yellow),
        )));
    }
    frame.render_widget(
        Paragraph::new(risk_lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Live Git overlap scan · r refresh "),
        ),
        chunks[2],
    );
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(review.message.as_str()),
            Line::from("↑/↓ select  ←/→/e change agent  r refresh  Enter approve review  q cancel"),
        ])
        .block(Block::default().borders(Borders::ALL)),
        chunks[3],
    );
}

/// Launch the first safe M2 slice. A plain Markdown heading has unknown file
/// scope, so MICE launches at most one independent task in that case instead
/// of pretending it can safely parallelize unscoped work. Structured planners
/// can populate declared paths later; the core validator then permits
/// disjoint root tasks to start together.
fn launch_approved(
    preflight: &MissionPreflight,
    assignments: &[Option<MissionAgentKind>],
) -> Result<(), Box<dyn std::error::Error>> {
    if preflight.cleanliness != "clean" {
        return Err("Mission launch refused: the base worktree has tracked changes. Commit, stash, or resolve them before launching isolated agents.".into());
    }
    if assignments.len() != preflight.graph.tasks.len() {
        return Err("Mission launch refused: the approved assignment set does not match the validated task graph.".into());
    }
    if preflight
        .risk_report
        .risks
        .iter()
        .any(|risk| risk.level == RiskLevel::Red)
    {
        return Err("Mission launch paused: the live worktree scan found a RED shared-file overlap. Resolve or explicitly coordinate that conflict before MICE starts more agents.".into());
    }
    let ledger = MissionLedger::for_identity(&preflight.identity)?;
    let mut record = ledger.load()?.unwrap_or(MissionRecord {
        identity: preflight.identity.clone(),
        updated_at: unix_timestamp(),
        graph: preflight.graph.clone(),
        assignments: preflight
            .graph
            .tasks
            .iter()
            .zip(assignments)
            .filter_map(|(task, agent)| {
                agent.map(|agent| MissionTaskAssignment {
                    task_id: task.id.clone(),
                    agent,
                })
            })
            .collect(),
        tasks: Vec::new(),
    });
    if record.identity != preflight.identity {
        return Err("Mission launch refused: persisted lifecycle state belongs to a different plan identity.".into());
    }
    if record
        .tasks
        .iter()
        .any(|task| task.state == MissionTaskState::Running)
    {
        return Err("Mission launch refused: this mission already has a running task. Use `mice mission status` before launching another task.".into());
    }
    if record.graph.tasks.is_empty() {
        record.graph = preflight.graph.clone();
        record.assignments = preflight
            .graph
            .tasks
            .iter()
            .zip(assignments)
            .filter_map(|(task, agent)| {
                agent.map(|agent| MissionTaskAssignment {
                    task_id: task.id.clone(),
                    agent,
                })
            })
            .collect();
    } else if same_task_ids(&record.graph, &preflight.graph) {
        // A repeated review of the same approved graph may deliberately move
        // an unstarted task to a different available harness. Never apply a
        // mapping from a re-planned graph with different task IDs.
        record.assignments = preflight
            .graph
            .tasks
            .iter()
            .zip(assignments)
            .filter_map(|(task, agent)| {
                agent.map(|agent| MissionTaskAssignment {
                    task_id: task.id.clone(),
                    agent,
                })
            })
            .collect();
    }
    let graph = record.graph.clone();
    let effective_assignments = assignments_for_graph(&graph, &record);
    let mut candidates = launch_candidates(&graph, &effective_assignments, &record);
    if candidates.is_empty() {
        return Err("Mission launch needs at least one dependency-ready task assigned to a launch-capable agent.".into());
    }
    let all_scopes_declared = candidates
        .iter()
        .all(|(_, task)| !task.predicted_paths.is_empty());
    if !all_scopes_declared {
        candidates.truncate(1);
        println!(
            "MICE is starting one task only because the remaining parallel tasks have unknown file scope. Add declared paths or use a structured planner before parallel launch."
        );
    }

    let mut failures = Vec::new();
    for (index, task) in candidates {
        let agent =
            effective_assignments[index].expect("launch candidates require an assigned agent");
        let branch = mission_branch_name(&preflight.identity, task);
        let worktree = owned_worktree_path(&preflight.identity, task)?;
        match provision_worktree(&preflight.worktree_root, &branch, &worktree) {
            Ok(()) => {
                // Persist before the child can begin. A fast headless agent
                // may call `mice mission report` immediately after startup;
                // it must never race an absent mission record.
                record.tasks.push(MissionTaskRuntime {
                    task_id: task.id.clone(),
                    agent,
                    state: MissionTaskState::Running,
                    branch,
                    worktree_path: worktree.to_string_lossy().into_owned(),
                    process_id: None,
                    exit_code: None,
                    observed_paths: Vec::new(),
                    coordination_notes: Vec::new(),
                    verified_at: None,
                    started_at: unix_timestamp(),
                });
                record.updated_at = unix_timestamp();
                ledger.record(&record)?;
                match spawn_agent_task(
                    &ledger,
                    &worktree,
                    task,
                    agent,
                    &preflight.plan.relative_path,
                    &mission_agent_context(&graph, &effective_assignments, &record),
                ) {
                    Ok(process_id) => {
                        let runtime = record
                            .tasks
                            .last_mut()
                            .ok_or("MICE lost the task record before starting its agent.")?;
                        runtime.process_id = process_id;
                    }
                    Err(error) => {
                        let runtime = record
                            .tasks
                            .last_mut()
                            .ok_or("MICE lost the task record after a launch failure.")?;
                        runtime.state = MissionTaskState::Blocked;
                        failures.push(format!("{}: {error}", task.id));
                    }
                }
            }
            Err(error) => failures.push(format!("{}: {error}", task.id)),
        }
        // Persist after each provision attempt. This intentionally retains
        // only operational facts, so a terminal close or daemon restart does
        // not erase knowledge of an owned worktree.
        record.updated_at = unix_timestamp();
        ledger.record(&record)?;
    }
    let running = record
        .tasks
        .iter()
        .filter(|task| task.state == MissionTaskState::Running)
        .count();
    println!(
        "Mission launched {running} task{} in owned external worktrees. They are Running, not finished; use `mice mission status` to inspect lifecycle truth.",
        if running == 1 { "" } else { "s" }
    );
    if !failures.is_empty() {
        return Err(format!("Some task launches failed: {}", failures.join("; ")).into());
    }
    Ok(())
}

fn assignments_for_graph(
    graph: &MissionTaskGraph,
    record: &MissionRecord,
) -> Vec<Option<MissionAgentKind>> {
    graph
        .tasks
        .iter()
        .map(|task| {
            record
                .assignments
                .iter()
                .find(|assignment| assignment.task_id == task.id)
                .map(|assignment| assignment.agent)
        })
        .collect()
}

fn same_task_ids(left: &MissionTaskGraph, right: &MissionTaskGraph) -> bool {
    left.tasks
        .iter()
        .map(|task| &task.id)
        .eq(right.tasks.iter().map(|task| &task.id))
}

fn mission_branch_name(identity: &MissionIdentity, task: &MissionTask) -> String {
    format!("mice/{}/{}", identity.mission_id, task.id)
}

fn provision_worktree(base_worktree: &Path, branch: &str, worktree: &Path) -> Result<(), String> {
    if worktree.exists() {
        return Err(format!(
            "owned worktree path already exists: {}",
            worktree.display()
        ));
    }
    let parent = worktree
        .parent()
        .ok_or("MICE could not determine the owned worktree parent.")?;
    fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    restrict_directory_to_user(parent).map_err(|error| error.to_string())?;
    let status = bounded_command_status(
        "git",
        &[
            "worktree",
            "add",
            "-b",
            branch,
            worktree.to_string_lossy().as_ref(),
            "HEAD",
        ],
        base_worktree,
        WORKTREE_TIMEOUT,
    )?;
    if status.success() {
        Ok(())
    } else {
        Err("`git worktree add` returned an unsuccessful status; no agent was started".into())
    }
}

/// Each provider gets a deliberately narrow, documented command line. On
/// macOS the default is a dedicated Terminal window; other platforms and
/// `MICE_MISSION_HEADLESS=1` use a direct child process. Both paths leave
/// only a PID/exit marker for lifecycle observation, never a transcript.
fn spawn_agent_task(
    ledger: &MissionLedger,
    worktree: &Path,
    task: &MissionTask,
    agent: MissionAgentKind,
    plan_path: &Path,
    shared_context: &str,
) -> Result<Option<u32>, String> {
    let prompt = agent_task_prompt(task, plan_path, shared_context)?;
    let (binary, arguments) = match agent {
        MissionAgentKind::Codex => ("codex", vec!["exec", "--sandbox", "workspace-write"]),
        // Claude's interactive mode preserves its own normal permission
        // boundary in the user-visible terminal. The headless escape hatch
        // uses its documented print mode and intentionally does not bypass
        // permissions.
        MissionAgentKind::Claude => ("claude", Vec::new()),
        // Antigravity's documented `agy -p` runs one prompt from the current
        // workspace. Its own permission policy remains in force; MICE never
        // supplies a bypass flag or writes Antigravity configuration.
        MissionAgentKind::Antigravity => ("agy", vec!["-p"]),
    };
    let executable = resolve_agent_executable(binary)?;
    #[cfg(target_os = "macos")]
    if env::var_os("MICE_MISSION_HEADLESS").is_none() {
        return spawn_terminal_agent_task(
            ledger,
            worktree,
            task,
            &executable,
            &arguments,
            &prompt,
            plan_path,
        )
        .map(|_| None);
    }
    let mut headless_arguments = arguments;
    if agent == MissionAgentKind::Claude {
        headless_arguments.push("--print");
    }
    spawn_headless_agent_task(
        ledger,
        worktree,
        task,
        &executable,
        &headless_arguments,
        &prompt,
        plan_path,
    )
    .map(|child| Some(child.id()))
}

/// Resolve a worker executable before opening Terminal. A GUI-launched shell
/// may have a different PATH from the controller; using this path preserves
/// the exact adapter that passed MICE's probe.
fn resolve_agent_executable(binary: &str) -> Result<PathBuf, String> {
    let binary_path = Path::new(binary);
    let candidate = if binary_path.is_absolute() || binary_path.components().count() > 1 {
        binary_path.to_path_buf()
    } else {
        env::var_os("PATH")
            .as_deref()
            .and_then(|path| {
                env::split_paths(path)
                    .map(|directory| directory.join(binary))
                    .find(|candidate| candidate.is_file())
            })
            .ok_or_else(|| {
                format!("could not resolve the `{binary}` executable from MICE's PATH")
            })?
    };
    candidate
        .canonicalize()
        .map_err(|error| format!("could not resolve `{binary}` for a worker launch: {error}"))
}

fn agent_task_prompt(
    task: &MissionTask,
    plan_path: &Path,
    shared_context: &str,
) -> Result<String, String> {
    let mut prompt = format!(
        "You are the assigned coding agent for MICE task `{}`.\n\nObjective: {}\n\nWork only in this isolated Git worktree. Read AGENTS.md and the repository plan before editing. Do not change unrelated features or attempt merges/rebases. Implement the task, run the relevant verification, and leave a concise final report.",
        task.id, task.title
    );
    if !task.acceptance.is_empty() {
        prompt.push_str("\n\nAcceptance:\n");
        for item in &task.acceptance {
            prompt.push_str("- ");
            prompt.push_str(item.trim());
            prompt.push('\n');
        }
    }
    if !task.predicted_paths.is_empty() {
        prompt.push_str("\nDeclared scope:\n");
        for path in &task.predicted_paths {
            prompt.push_str("- ");
            prompt.push_str(path);
            prompt.push('\n');
        }
    }
    if !shared_context.is_empty() {
        prompt.push_str("\nShared mission context (read-only):\n");
        prompt.push_str(shared_context);
        prompt.push_str(
            "\nKeep to your declared scope. If work would require touching another task's scope, stop and report the dependency rather than editing it.\n",
        );
    }
    // The worker must report to the exact executable which created the
    // mission ledger. A bare `mice` can resolve to an older installed build
    // which either lacks Mission Control or reads a different data root.
    let mice_binary = env::current_exe().map_err(|error| {
        format!("MICE could not resolve its executable for agent reporting: {error}")
    })?;
    let mice_binary = mice_binary.to_string_lossy();
    let report = format!(
        "{} mission report {} {}",
        shell_quote(&mice_binary),
        shell_quote(&plan_path.to_string_lossy()),
        shell_quote(&task.id),
    );
    let note = format!(
        "{} mission note {} {} \"your short note\"",
        shell_quote(&mice_binary),
        shell_quote(&plan_path.to_string_lossy()),
        shell_quote(&task.id),
    );
    prompt.push_str(&format!(
        "\nIf a short decision or blocker matters to another task, run `{note}` from this worktree; never put secrets or transcripts in that note. When you are ready for human verification, run `{report}` from this worktree. A process exit alone is never completion.\n",
    ));
    Ok(prompt)
}

/// The controller shares only bounded operational facts with each worker: the
/// plan's task map, current lifecycle state, and declared scopes. It never
/// copies an agent transcript, credentials, captures, or local configuration
/// into another agent's prompt.
fn mission_agent_context(
    graph: &MissionTaskGraph,
    assignments: &[Option<MissionAgentKind>],
    record: &MissionRecord,
) -> String {
    graph
        .tasks
        .iter()
        .enumerate()
        .map(|(index, task)| {
            let agent = assignments
                .get(index)
                .and_then(|agent| *agent)
                .map(MissionAgentKind::display_name)
                .unwrap_or("unassigned");
            let runtime = record
                .tasks
                .iter()
                .find(|runtime| runtime.task_id == task.id);
            let state = runtime
                .map(|runtime| mission_state_name(runtime.state))
                .unwrap_or("Scheduled");
            let scope = if task.predicted_paths.is_empty() {
                "scope unknown".to_owned()
            } else {
                task.predicted_paths.join(", ")
            };
            let dependencies = if task.dependencies.is_empty() {
                "none".to_owned()
            } else {
                task.dependencies.join(", ")
            };
            let observed = runtime
                .filter(|runtime| !runtime.observed_paths.is_empty())
                .map(|runtime| format!("; observed: {}", runtime.observed_paths.join(", ")))
                .unwrap_or_default();
            let notes = runtime
                .filter(|runtime| !runtime.coordination_notes.is_empty())
                .map(|runtime| format!("; notes: {}", runtime.coordination_notes.join(" | ")))
                .unwrap_or_default();
            format!(
                "- {} → {agent} [{state}; dependencies: {dependencies}; scope: {scope}{observed}{notes}]",
                task.id,
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn mission_state_name(state: MissionTaskState) -> &'static str {
    match state {
        MissionTaskState::Proposed => "Proposed",
        MissionTaskState::Running => "Running",
        MissionTaskState::ExitedUnreported => "ExitedUnreported",
        MissionTaskState::ReportedReady => "ReportedReady",
        MissionTaskState::VerifiedReady => "VerifiedReady",
        MissionTaskState::Blocked => "Blocked",
    }
}

fn spawn_headless_agent_task(
    ledger: &MissionLedger,
    worktree: &Path,
    task: &MissionTask,
    binary: &Path,
    arguments: &[&str],
    prompt: &str,
    plan_path: &Path,
) -> Result<Child, String> {
    let runner = write_agent_runner(ledger, worktree, task, binary, arguments, prompt, plan_path)?;
    Command::new("sh")
        .arg(&runner)
        .current_dir(worktree)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| {
            format!(
                "could not start headless agent runner for {} in {}: {error}",
                binary.display(),
                worktree.display()
            )
        })
}

#[cfg(target_os = "macos")]
fn spawn_terminal_agent_task(
    ledger: &MissionLedger,
    worktree: &Path,
    task: &MissionTask,
    binary: &Path,
    arguments: &[&str],
    prompt: &str,
    plan_path: &Path,
) -> Result<(), String> {
    let runner = write_agent_runner(ledger, worktree, task, binary, arguments, prompt, plan_path)?;
    Command::new("open")
        .args(["-a", "Terminal", runner.to_string_lossy().as_ref()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("could not open Terminal for the agent task: {error}"))?;
    Ok(())
}

fn write_agent_runner(
    ledger: &MissionLedger,
    worktree: &Path,
    task: &MissionTask,
    binary: &Path,
    arguments: &[&str],
    prompt: &str,
    plan_path: &Path,
) -> Result<PathBuf, String> {
    let worktree = worktree
        .to_str()
        .ok_or("MICE refuses to create a runner from a non-Unicode worktree path.")?;
    let runner = ledger.runner_file(&task.id);
    let pid_file = ledger.pid_file(&task.id);
    let done_file = ledger.done_file(&task.id);
    let _ = fs::remove_file(&pid_file);
    let _ = fs::remove_file(&done_file);
    let mut writer = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&runner)
        .map_err(|error| format!("could not create the MICE agent runner: {error}"))?;
    let agent_command = std::iter::once(shell_quote(&binary.to_string_lossy()))
        .chain(arguments.iter().map(|argument| shell_quote(argument)))
        .chain(std::iter::once(shell_quote(prompt)))
        .collect::<Vec<_>>()
        .join(" ");
    let mice_binary = env::current_exe().map_err(|error| {
        format!("MICE could not resolve its executable for lifecycle reporting: {error}")
    })?;
    let mice_binary = mice_binary
        .to_str()
        .ok_or("MICE refuses to create a runner with a non-Unicode executable path.")?;
    let plan_path = plan_path
        .to_str()
        .ok_or("MICE refuses to create a runner with a non-Unicode plan path.")?;
    let data_root_export = env::var_os("MICE_MISSION_DATA_DIR")
        .map(PathBuf::from)
        .map(|path| {
            if !path.is_absolute() {
                return Err("MICE_MISSION_DATA_DIR must be an absolute path.".to_owned());
            }
            Ok(format!(
                "export MICE_MISSION_DATA_DIR={}\n",
                shell_quote(&path.to_string_lossy())
            ))
        })
        .transpose()?
        .unwrap_or_default();
    let script = format!(
        "#!/bin/sh\nset -u\nrm -f -- \"$0\"\nPID_FILE={}\nDONE_FILE={}\nMICE_BIN={}\nPLAN_PATH={}\n{}cleanup() {{ status=$?; trap - 0; rm -f -- \"$PID_FILE\"; printf '%s\\n' \"$status\" > \"$DONE_FILE\"; \"$MICE_BIN\" mission status \"$PLAN_PATH\" >/dev/null 2>&1 || true; exit \"$status\"; }}\ntrap cleanup 0\ncd {}\n{} &\nagent_pid=$!\nprintf '%s\\n' \"$agent_pid\" > \"$PID_FILE\"\nwait \"$agent_pid\"\n",
        shell_quote(&pid_file.to_string_lossy()),
        shell_quote(&done_file.to_string_lossy()),
        shell_quote(mice_binary),
        shell_quote(plan_path),
        data_root_export,
        shell_quote(worktree),
        agent_command,
    );
    writer
        .write_all(script.as_bytes())
        .and_then(|_| writer.sync_all())
        .map_err(|error| format!("could not write the MICE agent runner: {error}"))?;
    restrict_runner_to_user(&runner).map_err(|error| error.to_string())?;
    Ok(runner)
}

fn restrict_runner_to_user(path: &Path) -> Result<(), std::io::Error> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn bounded_command_status(
    binary: &str,
    arguments: &[&str],
    current_dir: &Path,
    timeout: Duration,
) -> Result<ExitStatus, String> {
    let mut child = Command::new(binary)
        .args(arguments)
        .current_dir(current_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("could not start `{binary}`: {error}"))?;
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|error| format!("could not inspect `{binary}`: {error}"))?
        {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "`{binary}` did not finish within {} seconds",
                timeout.as_secs()
            ));
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// Mission state is deliberately outside the repository, avoiding both Git
/// churn and any chance of committing agent process metadata or configuration.
#[derive(Debug, Clone)]
struct MissionLedger {
    root: PathBuf,
    file: PathBuf,
}

impl MissionLedger {
    fn for_identity(identity: &MissionIdentity) -> Result<Self, Box<dyn std::error::Error>> {
        let root = mission_data_root()?
            .join("missions")
            .join(&identity.repo_id)
            .join(&identity.mission_id);
        fs::create_dir_all(&root)?;
        restrict_directory_to_user(&root)?;
        Ok(Self {
            file: root.join("state.json"),
            root,
        })
    }

    fn existing(identity: &MissionIdentity) -> Result<Self, Box<dyn std::error::Error>> {
        let root = mission_data_root_path()?
            .join("missions")
            .join(&identity.repo_id)
            .join(&identity.mission_id);
        Ok(Self {
            file: root.join("state.json"),
            root,
        })
    }

    fn load(&self) -> Result<Option<MissionRecord>, Box<dyn std::error::Error>> {
        if !self.file.exists() {
            return Ok(None);
        }
        let bytes = fs::read(&self.file)?;
        let record = serde_json::from_slice(&bytes)
            .map_err(|error| format!("MICE could not parse its mission metadata: {error}"))?;
        Ok(Some(record))
    }

    fn runner_file(&self, task_id: &str) -> PathBuf {
        self.root.join(format!("run-{task_id}.command"))
    }

    fn pid_file(&self, task_id: &str) -> PathBuf {
        self.root.join(format!("{task_id}.pid"))
    }

    fn done_file(&self, task_id: &str) -> PathBuf {
        self.root.join(format!("{task_id}.done"))
    }

    fn recorded_process_id(&self, task_id: &str) -> Option<u32> {
        let value = fs::read_to_string(self.pid_file(task_id)).ok()?;
        value
            .trim()
            .parse::<u32>()
            .ok()
            .filter(|process_id| *process_id > 1)
    }

    fn recorded_exit_code(&self, task_id: &str) -> Option<i32> {
        let value = fs::read_to_string(self.done_file(task_id)).ok()?;
        value
            .trim()
            .parse::<i32>()
            .ok()
            .filter(|exit_code| (0..=255).contains(exit_code))
    }

    fn record(&self, record: &MissionRecord) -> Result<(), Box<dyn std::error::Error>> {
        let bytes = serde_json::to_vec_pretty(record)?;
        let temporary = self.root.join(format!(".state.{}.tmp", std::process::id()));
        let mut writer = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        writer.write_all(&bytes)?;
        writer.write_all(b"\n")?;
        writer.sync_all()?;
        restrict_file_to_user(&temporary)?;
        fs::rename(&temporary, &self.file)?;
        Ok(())
    }
}

struct MissionStatusView {
    snapshot: RepoSnapshot,
    worktree_root: PathBuf,
    plan: LoadedPlan,
    record: Option<MissionRecord>,
    risk_report: RiskReport,
    next_safe_task: Option<String>,
}

struct MissionLifecycleRow {
    task_id: String,
    agent: Option<MissionAgentKind>,
    state: Option<MissionTaskState>,
    exit_code: Option<i32>,
    observed_paths: Vec<String>,
    coordination_notes: Vec<String>,
}

fn mission_lifecycle_rows(record: &MissionRecord) -> Vec<MissionLifecycleRow> {
    if record.graph.tasks.is_empty() {
        return record
            .tasks
            .iter()
            .map(|task| MissionLifecycleRow {
                task_id: task.task_id.clone(),
                agent: Some(task.agent),
                state: Some(task.state),
                exit_code: task.exit_code,
                observed_paths: task.observed_paths.clone(),
                coordination_notes: task.coordination_notes.clone(),
            })
            .collect();
    }
    record
        .graph
        .tasks
        .iter()
        .map(|task| {
            let runtime = record
                .tasks
                .iter()
                .find(|runtime| runtime.task_id == task.id);
            MissionLifecycleRow {
                task_id: task.id.clone(),
                agent: runtime.map(|runtime| runtime.agent).or_else(|| {
                    record
                        .assignments
                        .iter()
                        .find(|assignment| assignment.task_id == task.id)
                        .map(|assignment| assignment.agent)
                }),
                state: runtime.map(|runtime| runtime.state),
                exit_code: runtime.and_then(|runtime| runtime.exit_code),
                observed_paths: runtime
                    .map(|runtime| runtime.observed_paths.clone())
                    .unwrap_or_default(),
                coordination_notes: runtime
                    .map(|runtime| runtime.coordination_notes.clone())
                    .unwrap_or_default(),
            }
        })
        .collect()
}

fn load_mission_status(
    plan_argument: Option<&str>,
) -> Result<MissionStatusView, Box<dyn std::error::Error>> {
    let Some(plan_argument) = plan_argument else {
        return Err("Mission status needs the repository-relative plan path: `mice mission status plan/release.md`.".into());
    };
    let cwd = env::current_dir()?;
    let snapshot = coordination::discover(&cwd)
        .map_err(|error| format!("Mission status requires a Git worktree: {error}"))?;
    let worktree_root = current_worktree_root(&snapshot, &cwd)?;
    let plan = load_plan(&worktree_root, Path::new(plan_argument))?;
    let identity = mission_identity(&snapshot.repo_id, &plan);
    let ledger = MissionLedger::existing(&identity)?;
    let Some(mut record) = ledger.load()? else {
        return Ok(MissionStatusView {
            risk_report: coordination::merge_risks(&snapshot),
            snapshot,
            worktree_root,
            plan,
            record: None,
            next_safe_task: None,
        });
    };
    let mut changed = false;
    let mut exited_without_report = Vec::new();
    for task in &mut record.tasks {
        if let Ok(observed_paths) = observed_worktree_paths(Path::new(&task.worktree_path))
            && task.observed_paths != observed_paths
        {
            task.observed_paths = observed_paths;
            changed = true;
        }
        if task.state == MissionTaskState::ExitedUnreported {
            let exit_code = ledger.recorded_exit_code(&task.task_id);
            if task.exit_code != exit_code {
                task.exit_code = exit_code;
                changed = true;
            }
            continue;
        }
        if task.state != MissionTaskState::Running {
            continue;
        }
        if task.process_id.is_none()
            && let Some(process_id) = ledger.recorded_process_id(&task.task_id)
        {
            task.process_id = Some(process_id);
            changed = true;
        }
        let exit_code = ledger.recorded_exit_code(&task.task_id);
        let exited = ledger.done_file(&task.task_id).exists()
            || task
                .process_id
                .is_some_and(|process_id| !process_is_alive(process_id));
        if exited {
            task.state = MissionTaskState::ExitedUnreported;
            task.process_id = None;
            task.exit_code = exit_code;
            changed = true;
            exited_without_report.push(task.task_id.clone());
        }
    }
    if changed {
        record.updated_at = unix_timestamp();
        ledger.record(&record)?;
        for task_id in exited_without_report {
            notify_running_daemon(&format!(
                "MICE: `{task_id}` exited. It is not marked finished until the agent reports readiness and you verify its worktree."
            ));
        }
    }
    let graph = graph_for_record(&record, &plan)?;
    Ok(MissionStatusView {
        risk_report: coordination::merge_risks(&snapshot),
        next_safe_task: next_safe_task(&graph, &record).map(|task| task.id.clone()),
        snapshot,
        worktree_root,
        plan,
        record: Some(record),
    })
}

fn observed_worktree_paths(worktree: &Path) -> Result<Vec<String>, String> {
    let output = bounded_command_output(
        "git",
        &["diff", "--name-only", "HEAD"],
        worktree,
        PROBE_TIMEOUT,
        8 * 1024,
    )?;
    Ok(output
        .lines()
        .map(str::trim)
        .filter(|path| !path.is_empty() && !path.contains('\n') && !path.contains('\r'))
        .take(16)
        .map(str::to_owned)
        .collect())
}

fn graph_for_record(
    record: &MissionRecord,
    plan: &LoadedPlan,
) -> Result<MissionTaskGraph, Box<dyn std::error::Error>> {
    if !record.graph.tasks.is_empty() {
        record
            .graph
            .validate()
            .map_err(|error| format!("Persisted mission graph is invalid: {error}"))?;
        return Ok(record.graph.clone());
    }
    task_graph_from_markdown(&plan.contents, &plan.display_name)
}

fn mission_status(plan_argument: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    let view = load_mission_status(plan_argument)?;
    let Some(record) = view.record else {
        println!(
            "No persisted Mission Control launch exists for `{}` in this repository.",
            view.plan.relative_path.display()
        );
        return Ok(());
    };
    println!("MICE Mission Control — status");
    println!("Mission: {}", record.identity.mission_id);
    println!("Plan: {}", view.plan.relative_path.display());
    println!(
        "Tracked base state: {}",
        tracked_worktree_cleanliness(&view.worktree_root)
    );
    println!();
    for task in mission_lifecycle_rows(&record) {
        let state = task.state.map(mission_state_name).unwrap_or("Scheduled");
        let agent = task
            .agent
            .map(MissionAgentKind::display_name)
            .unwrap_or("unassigned");
        println!("- {state}: {} → {agent}", task.task_id);
        if let Some(runtime) = record
            .tasks
            .iter()
            .find(|runtime| runtime.task_id == task.task_id)
        {
            println!("  branch {} · {}", runtime.branch, runtime.worktree_path);
        } else {
            println!("  not launched; awaiting reviewed approval");
        }
        if let Some(exit_code) = task.exit_code {
            println!("  adapter exit: {exit_code}");
        }
        if !task.observed_paths.is_empty() {
            println!("  changed: {}", task.observed_paths.join(", "));
        }
        for note in &task.coordination_notes {
            println!("  note: {note}");
        }
    }
    println!();
    print_risk_summary(&view.risk_report);
    if record
        .tasks
        .iter()
        .any(|task| task.state == MissionTaskState::ExitedUnreported)
    {
        println!(
            "ACTION NEEDED: an agent process exited without an explicit completion report. MICE will not mark it finished."
        );
    }
    if let Some(task_id) = view.next_safe_task {
        println!(
            "NEXT SAFE TASK: `{}` is dependency-ready with declared scope. Re-open the Mission Control review to launch it.",
            task_id
        );
    }
    Ok(())
}

/// Read-only mission facts for agents connected through MICE MCP. This keeps
/// checkout paths, process IDs, and every agent's transcript private while
/// giving a worker the shared task ownership and live conflict picture it
/// needs to stay in scope.
pub fn mcp_status(plan_path: &str) -> Result<String, Box<dyn std::error::Error>> {
    let view = load_mission_status(Some(plan_path))?;
    let Some(record) = view.record else {
        return Ok(format!(
            "No persisted Mission Control launch exists for `{}`.",
            view.plan.relative_path.display()
        ));
    };
    Ok(render_mcp_status(
        &view.plan.relative_path,
        &record,
        &view.risk_report,
        view.next_safe_task.as_deref(),
    ))
}

fn render_mcp_status(
    plan_path: &Path,
    record: &MissionRecord,
    risk_report: &RiskReport,
    next_safe_task: Option<&str>,
) -> String {
    let mut output = format!(
        "MICE mission {} · plan {}\n",
        record.identity.mission_id,
        plan_path.display()
    );
    for task in mission_lifecycle_rows(record) {
        let state = task.state.map(mission_state_name).unwrap_or("Scheduled");
        let agent = task
            .agent
            .map(MissionAgentKind::display_name)
            .unwrap_or("unassigned");
        output.push_str(&format!("- {state}: {} → {agent}\n", task.task_id));
        if let Some(exit_code) = task.exit_code {
            output.push_str(&format!("  adapter exit: {exit_code}\n"));
        }
        if !task.observed_paths.is_empty() {
            output.push_str(&format!(
                "  observed paths: {}\n",
                task.observed_paths.join(", ")
            ));
        }
        for note in &task.coordination_notes {
            output.push_str(&format!("  coordination note: {note}\n"));
        }
    }
    for risk in risk_report.risks.iter().take(8) {
        let level = match risk.level {
            RiskLevel::Yellow => "YELLOW",
            RiskLevel::Red => "RED",
        };
        output.push_str(&format!(
            "- {level} overlap: {} — {} ↔ {}\n",
            risk.path, risk.left_branch, risk.right_branch
        ));
    }
    for pair in risk_report.unassessed.iter().take(4) {
        output.push_str(&format!(
            "- UNASSESSED: {} ↔ {}; {}\n",
            pair.left_branch, pair.right_branch, pair.reason
        ));
    }
    if let Some(task_id) = next_safe_task {
        output.push_str(&format!("- next safe task: {task_id}\n"));
    }
    output
}

fn mission_watch(plan_argument: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    if !io::stdout().is_terminal() {
        return Err("Mission watch needs an interactive terminal; use `mice mission status` for text output.".into());
    }
    let plan_argument = plan_argument
        .ok_or("Mission watch needs the repository-relative plan path.")?
        .to_owned();
    let mut view = load_mission_status(Some(&plan_argument))?;
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        loop {
            terminal.draw(|frame| draw_mission_watch(frame, &view))?;
            if event::poll(Duration::from_secs(1))? {
                let Event::Key(key) = event::read()? else {
                    continue;
                };
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => return Ok(()),
                    KeyCode::Char('r') => {}
                    _ => continue,
                }
            }
            view = load_mission_status(Some(&plan_argument))?;
        }
    })();
    let _ = disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let _ = terminal.show_cursor();
    result
}

fn draw_mission_watch(frame: &mut ratatui::Frame, view: &MissionStatusView) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(7),
            Constraint::Min(7),
            Constraint::Length(8),
            Constraint::Length(3),
        ])
        .split(area);
    let mission = view
        .record
        .as_ref()
        .map(|record| record.identity.mission_id.as_str())
        .unwrap_or("not launched");
    let base = tracked_worktree_cleanliness(&view.worktree_root);
    let base_style = if base == "clean" {
        Style::default().fg(Color::Green)
    } else {
        Style::default().fg(Color::Red)
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                " MICE Mission Control ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(format!(
                " mission {mission} · plan {}",
                view.plan.relative_path.display()
            )),
            Line::from(vec![Span::raw(" base: "), Span::styled(base, base_style)]),
            Line::from(format!(
                " active worktrees: {} · refreshes every second",
                view.snapshot.worktrees.len()
            )),
        ])
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Live mission status "),
        ),
        chunks[0],
    );

    let task_lines = view.record.as_ref().map_or_else(
        || {
            vec![Line::from(Span::styled(
                "No persisted mission launch exists for this plan.",
                Style::default().fg(Color::Yellow),
            ))]
        },
        |record| {
            mission_lifecycle_rows(record)
                .into_iter()
                .map(|task| {
                    let (state, color) = match task.state {
                        Some(MissionTaskState::Running) => ("Running", Color::Cyan),
                        Some(MissionTaskState::ReportedReady)
                        | Some(MissionTaskState::VerifiedReady) => (
                            task.state.map(mission_state_name).unwrap_or_default(),
                            Color::Green,
                        ),
                        Some(MissionTaskState::ExitedUnreported)
                        | Some(MissionTaskState::Blocked) => (
                            task.state.map(mission_state_name).unwrap_or_default(),
                            Color::Red,
                        ),
                        Some(MissionTaskState::Proposed) => ("Proposed", Color::Yellow),
                        None => ("Scheduled", Color::Yellow),
                    };
                    let agent = task
                        .agent
                        .map(MissionAgentKind::display_name)
                        .unwrap_or("unassigned");
                    let observed = if task.observed_paths.is_empty() {
                        "scope not yet observed".into()
                    } else {
                        format!("{} changed path(s)", task.observed_paths.len())
                    };
                    let outcome = task
                        .exit_code
                        .map(|exit_code| format!(" · exit {exit_code}"))
                        .unwrap_or_default();
                    Line::from(vec![
                        Span::styled(format!("{state:<18}"), Style::default().fg(color)),
                        Span::raw(format!("{} → {agent} · {observed}{outcome}", task.task_id)),
                    ])
                })
                .collect()
        },
    );
    frame.render_widget(
        Paragraph::new(task_lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Agent lifecycle "),
        ),
        chunks[1],
    );

    let mut risk_lines = Vec::new();
    if view.risk_report.risks.is_empty() && view.risk_report.unassessed.is_empty() {
        risk_lines.push(Line::from(Span::styled(
            "No shared changed-file surfaces among assessed worktrees.",
            Style::default().fg(Color::Green),
        )));
    }
    for risk in view.risk_report.risks.iter().take(3) {
        let (label, color) = match risk.level {
            RiskLevel::Yellow => ("YELLOW", Color::Yellow),
            RiskLevel::Red => ("RED", Color::Red),
        };
        risk_lines.push(Line::from(vec![
            Span::styled(format!("{label}: "), Style::default().fg(color)),
            Span::raw(format!(
                "{} — {} ↔ {}",
                risk.path, risk.left_branch, risk.right_branch
            )),
        ]));
    }
    for pair in view.risk_report.unassessed.iter().take(2) {
        risk_lines.push(Line::from(Span::styled(
            format!(
                "UNASSESSED: {} ↔ {}; {}",
                pair.left_branch, pair.right_branch, pair.reason
            ),
            Style::default().fg(Color::Yellow),
        )));
    }
    if let Some(task_id) = &view.next_safe_task {
        risk_lines.push(Line::from(Span::styled(
            format!("NEXT SAFE TASK: `{task_id}` is dependency-ready with declared scope."),
            Style::default().fg(Color::Green),
        )));
    }
    frame.render_widget(
        Paragraph::new(risk_lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Live Git overlap scan "),
        ),
        chunks[2],
    );
    frame.render_widget(
        Paragraph::new("q/Esc close · r refresh · native lifecycle notices require `mice start`")
            .block(Block::default().borders(Borders::ALL)),
        chunks[3],
    );
}

fn mission_report(
    plan_argument: Option<&str>,
    task_id: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    let location = mission_for_current_worktree(plan_argument)?;
    let mut record = location.ledger.load()?.ok_or(
        "MICE has no launch record for this mission; a task cannot report into an unknown mission.",
    )?;
    let task_id = task_id.ok_or("Mission report needs a task ID.")?;
    let task = record
        .tasks
        .iter_mut()
        .find(|task| task.task_id == task_id)
        .ok_or("This task is not part of the recorded mission launch.")?;
    let recorded_worktree = PathBuf::from(&task.worktree_path).canonicalize()?;
    if location.worktree_root.canonicalize()? != recorded_worktree {
        return Err("Mission report refused: it must run from the task's owned worktree.".into());
    }
    if task.state != MissionTaskState::Running {
        return Err("Mission report refused: only a running task may report readiness.".into());
    }
    verify_worktree_git_evidence(&location.worktree_root)?;
    task.state = MissionTaskState::ReportedReady;
    record.updated_at = unix_timestamp();
    location.ledger.record(&record)?;
    notify_running_daemon(&format!(
        "MICE: `{task_id}` is ready for your verification. Its worktree remains isolated; no merge was attempted."
    ));
    println!(
        "MICE recorded `{task_id}` as ReportedReady. It is not complete until `mice mission verify {} {task_id} --yes` checks its Git evidence.",
        location.plan.relative_path.display()
    );
    Ok(())
}

fn mission_note(
    plan_argument: Option<&str>,
    task_id: Option<&str>,
    note: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    let location = mission_for_current_worktree(plan_argument)?;
    let task_id = task_id.ok_or("Mission note needs a task ID.")?;
    let note =
        sanitize_coordination_note(note.ok_or("Mission note needs a short decision or blocker.")?)?;
    let mut record = location
        .ledger
        .load()?
        .ok_or("MICE has no launch record for this mission.")?;
    let task = record
        .tasks
        .iter_mut()
        .find(|task| task.task_id == task_id)
        .ok_or("This task is not part of the recorded mission launch.")?;
    let recorded_worktree = PathBuf::from(&task.worktree_path).canonicalize()?;
    if location.worktree_root.canonicalize()? != recorded_worktree {
        return Err("Mission notes must be recorded from the task's owned worktree.".into());
    }
    task.coordination_notes.push(note);
    if task.coordination_notes.len() > MAX_TASK_NOTES {
        let excess = task.coordination_notes.len() - MAX_TASK_NOTES;
        task.coordination_notes.drain(..excess);
    }
    record.updated_at = unix_timestamp();
    location.ledger.record(&record)?;
    println!("MICE recorded a bounded coordination note for `{task_id}`.");
    Ok(())
}

fn sanitize_coordination_note(value: &str) -> Result<String, Box<dyn std::error::Error>> {
    let compact = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.is_empty() {
        return Err("Mission note cannot be empty.".into());
    }
    if compact.chars().count() > MAX_TASK_NOTE_CHARS {
        return Err(format!(
            "Mission note is limited to {MAX_TASK_NOTE_CHARS} characters; use a short decision or blocker."
        )
        .into());
    }
    let lowered = compact.to_ascii_lowercase();
    if [
        "password",
        "secret",
        "api key",
        "api_key",
        "access token",
        "token",
        "credential",
        "authorization",
        "bearer ",
        "private key",
    ]
    .iter()
    .any(|needle| lowered.contains(needle))
    {
        return Err(
            "Mission note refused because it appears to contain a credential. Store no secrets, captures, or transcript text in mission context."
                .into(),
        );
    }
    Ok(compact)
}

fn mission_verify(
    plan_argument: Option<&str>,
    task_id: Option<&str>,
    yes: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if !yes {
        return Err("Mission verification changes lifecycle state; rerun with `--yes` after reviewing the task worktree.".into());
    }
    let location = mission_for_current_worktree(plan_argument)?;
    let task_id = task_id.ok_or("Mission verification needs a task ID.")?;
    let mut record = location
        .ledger
        .load()?
        .ok_or("MICE has no launch record for this mission.")?;
    let task = record
        .tasks
        .iter_mut()
        .find(|task| task.task_id == task_id)
        .ok_or("This task is not part of the recorded mission launch.")?;
    if task.state != MissionTaskState::ReportedReady {
        return Err("Mission verification requires the agent to report readiness first.".into());
    }
    let task_worktree = PathBuf::from(&task.worktree_path).canonicalize()?;
    verify_worktree_git_evidence(&task_worktree)?;
    task.state = MissionTaskState::VerifiedReady;
    task.process_id = None;
    task.verified_at = Some(unix_timestamp());
    record.updated_at = unix_timestamp();
    location.ledger.record(&record)?;
    println!(
        "MICE verified `{task_id}`. Its worktree remains isolated for your review; no merge or rebase was attempted."
    );
    let graph = graph_for_record(&record, &location.plan)?;
    let next_task = next_safe_task(&graph, &record).map(|task| task.id.as_str());
    notify_running_daemon(&completion_notice(&record, task_id, next_task));
    if let Some(next) = next_safe_task(&graph, &record) {
        println!(
            "Next safe task suggestion: `{}`. Re-open Mission Control to review and launch it.",
            next.id
        );
    }
    Ok(())
}

fn completion_notice(record: &MissionRecord, task_id: &str, next_task: Option<&str>) -> String {
    let mut counts = std::collections::BTreeMap::<&str, usize>::new();
    for task in mission_lifecycle_rows(record) {
        let state = task.state.map(mission_state_name).unwrap_or("Scheduled");
        *counts.entry(state).or_default() += 1;
    }
    let states = counts
        .into_iter()
        .map(|(state, count)| format!("{state}={count}"))
        .collect::<Vec<_>>()
        .join(", ");
    let next = next_task.map_or_else(
        || "No dependency-ready disjoint task is currently available.".into(),
        |next| format!("Suggested next task: `{next}` (requires your approval)."),
    );
    format!(
        "MICE: `{task_id}` verified. Agent status: {states}. {next} Changes remain isolated; no merge was attempted."
    )
}

fn mission_for_current_worktree(
    plan_argument: Option<&str>,
) -> Result<MissionLocation, Box<dyn std::error::Error>> {
    let plan_argument = plan_argument
        .ok_or("Mission lifecycle commands need the repository-relative plan path.")?;
    let cwd = env::current_dir()?;
    let snapshot = coordination::discover(&cwd)
        .map_err(|error| format!("Mission lifecycle commands require a Git worktree: {error}"))?;
    let worktree_root = current_worktree_root(&snapshot, &cwd)?;
    let plan = load_plan(&worktree_root, Path::new(plan_argument))?;
    let identity = mission_identity(&snapshot.repo_id, &plan);
    let ledger = MissionLedger::existing(&identity)?;
    Ok(MissionLocation {
        worktree_root,
        plan,
        ledger,
    })
}

fn verify_worktree_git_evidence(worktree: &Path) -> Result<(), Box<dyn std::error::Error>> {
    for arguments in [
        ["diff", "--check"].as_slice(),
        ["diff", "--cached", "--check"].as_slice(),
    ] {
        let status = bounded_command_status("git", arguments, worktree, PROBE_TIMEOUT)
            .map_err(|error| format!("MICE could not verify Git evidence: {error}"))?;
        if !status.success() {
            return Err(
                "MICE found Git whitespace errors, so this task cannot yet be verified.".into(),
            );
        }
    }
    Ok(())
}

fn next_safe_task<'a>(
    graph: &'a MissionTaskGraph,
    record: &MissionRecord,
) -> Option<&'a MissionTask> {
    graph.tasks.iter().find(|candidate| {
        !candidate.predicted_paths.is_empty()
            && !record.tasks.iter().any(|task| task.task_id == candidate.id)
            && candidate.dependencies.iter().all(|dependency| {
                record.tasks.iter().any(|task| {
                    task.task_id == *dependency && task.state == MissionTaskState::VerifiedReady
                })
            })
    })
}

fn launch_candidates<'a>(
    graph: &'a MissionTaskGraph,
    assignments: &[Option<MissionAgentKind>],
    record: &MissionRecord,
) -> Vec<(usize, &'a MissionTask)> {
    graph
        .tasks
        .iter()
        .enumerate()
        .filter(|(index, task)| {
            assignments[*index].is_some_and(mice_launch_supported)
                && !record
                    .tasks
                    .iter()
                    .any(|runtime| runtime.task_id == task.id)
                && task.dependencies.iter().all(|dependency| {
                    record.tasks.iter().any(|runtime| {
                        runtime.task_id == *dependency
                            && runtime.state == MissionTaskState::VerifiedReady
                    })
                })
        })
        .collect()
}

fn mice_launch_supported(agent: MissionAgentKind) -> bool {
    matches!(
        agent,
        MissionAgentKind::Codex | MissionAgentKind::Claude | MissionAgentKind::Antigravity
    )
}

fn process_is_alive(process_id: u32) -> bool {
    #[cfg(unix)]
    {
        let process_id = process_id.to_string();
        match bounded_status("kill", &["-0", &process_id]) {
            Ok(status) => status.success(),
            // Unknown is safer than falsely declaring an agent complete.
            Err(_) => true,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = process_id;
        true
    }
}

fn mission_data_root() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let root = mission_data_root_path()?;
    fs::create_dir_all(&root)?;
    restrict_directory_to_user(&root)?;
    Ok(root)
}

fn mission_data_root_path() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let root = env::var_os("MICE_MISSION_DATA_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            env::var_os("HOME")
                .map(PathBuf::from)
                .map(|home| home.join("Library/Application Support/MICE/mission-control"))
        })
        .ok_or("MICE needs HOME or MICE_MISSION_DATA_DIR to store mission metadata outside Git.")?;
    if !root.is_absolute() {
        return Err("MICE_MISSION_DATA_DIR must be an absolute path.".into());
    }
    Ok(root)
}

fn owned_worktree_path(
    identity: &MissionIdentity,
    task: &MissionTask,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let root = mission_data_root()?.canonicalize()?;
    let parent = root
        .join("worktrees")
        .join(&identity.repo_id)
        .join(&identity.mission_id);
    fs::create_dir_all(&parent)?;
    restrict_directory_to_user(&parent)?;
    let parent = parent.canonicalize()?;
    if !parent.starts_with(&root) {
        return Err(
            "MICE refused an owned worktree path outside its mission data directory.".into(),
        );
    }
    Ok(parent.join(&task.id))
}

fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// `pub(crate)`: reused outside this module by the M19b job-progress log
/// (`main.rs`), which needs the same atomic-write + owner-only-permission
/// discipline this module already established for `Ledger::record`.
pub(crate) fn restrict_file_to_user(path: &Path) -> Result<(), std::io::Error> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

pub(crate) fn restrict_directory_to_user(path: &Path) -> Result<(), std::io::Error> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_unique_explicit_agent_lists() {
        assert_eq!(
            parse_agents("codex, claude,antigravity").unwrap(),
            vec![
                MissionAgentKind::Codex,
                MissionAgentKind::Claude,
                MissionAgentKind::Antigravity,
            ]
        );
        assert!(parse_agents("codex,codex").is_err());
    }

    #[test]
    fn review_assignment_cycles_only_proven_capable_harnesses() {
        let agents = vec![
            MissionAgentCapability {
                agent: MissionAgentKind::Claude,
                installed: true,
                mcp_available: true,
                launch_ready: true,
                detail: String::new(),
            },
            MissionAgentCapability {
                agent: MissionAgentKind::Antigravity,
                installed: true,
                mcp_available: false,
                launch_ready: false,
                detail: String::new(),
            },
            MissionAgentCapability {
                agent: MissionAgentKind::Codex,
                installed: true,
                mcp_available: true,
                launch_ready: true,
                detail: String::new(),
            },
        ];
        assert_eq!(
            cycle_assignment(None, &agents, true),
            Some(MissionAgentKind::Codex)
        );
        assert_eq!(
            cycle_assignment(Some(MissionAgentKind::Codex), &agents, true),
            Some(MissionAgentKind::Claude)
        );
        assert_eq!(
            cycle_assignment(Some(MissionAgentKind::Claude), &agents, true),
            None
        );
        assert_eq!(
            cycle_assignment(None, &agents, false),
            Some(MissionAgentKind::Claude)
        );
    }

    #[test]
    fn dry_run_is_an_explicit_noninteractive_mode() {
        let options = MissionOptions::parse(&[
            "plan".into(),
            "plan/mission.md".into(),
            "--agents".into(),
            "codex".into(),
            "--dry-run".into(),
        ])
        .unwrap();
        assert!(options.dry_run);
    }

    #[test]
    fn launch_refuses_a_dirty_base_before_creating_state() {
        let task = MissionTask {
            id: "core".into(),
            title: "Build the core".into(),
            acceptance: vec!["cargo test -p mice-core".into()],
            dependencies: vec![],
            predicted_paths: vec![],
        };
        let preflight = MissionPreflight {
            snapshot: RepoSnapshot {
                repo_id: "repo".into(),
                captured_at: 0,
                worktrees: vec![],
            },
            worktree_root: PathBuf::from("/not-used"),
            plan: LoadedPlan {
                contents: String::new(),
                relative_path: PathBuf::from("plan/mission.md"),
                display_name: "mission".into(),
                fingerprint: "f".repeat(64),
            },
            graph: MissionTaskGraph { tasks: vec![task] },
            identity: MissionIdentity {
                repo_id: "repo".into(),
                mission_id: "mission".into(),
                plan_fingerprint: "f".repeat(64),
            },
            agents: vec![],
            cleanliness: "tracked changes present".into(),
            risk_report: RiskReport::default(),
            planner_detail: "test".into(),
        };
        assert!(
            launch_approved(&preflight, &[Some(MissionAgentKind::Codex)])
                .unwrap_err()
                .to_string()
                .contains("base worktree has tracked changes")
        );
    }

    #[test]
    fn derives_tasks_from_checklists_before_generic_headings() {
        let graph = task_graph_from_markdown(
            "# Plan\n\n## Background\n\n- [ ] Build the mission parser\n- [ ] Add dry-run tests\n",
            "mission",
        )
        .unwrap();
        assert_eq!(graph.tasks.len(), 2);
        assert_eq!(graph.tasks[0].title, "Build the mission parser");
        assert!(graph.validate().is_ok());
    }

    #[test]
    fn derives_headings_when_a_plan_has_no_checklist() {
        let graph =
            task_graph_from_markdown("# Plan\n\n## First task\n### Second task\n", "mission")
                .unwrap();
        assert_eq!(
            graph
                .tasks
                .iter()
                .map(|task| task.title.as_str())
                .collect::<Vec<_>>(),
            ["Second task"]
        );
    }

    #[test]
    fn heading_metadata_declares_safe_parallel_scope() {
        let graph = task_graph_from_markdown(
            "# Plan\n\n### Build CLI\n<!-- mice: id=cli; paths=crates/mice-cli/src/mission.rs -->\n\n### Build agent\n<!-- mice: id=agent; paths=agent-macos; depends=cli -->\n",
            "mission",
        )
        .unwrap();
        assert_eq!(graph.tasks[0].id, "cli");
        assert_eq!(
            graph.tasks[0].predicted_paths,
            vec!["crates/mice-cli/src/mission.rs"]
        );
        assert_eq!(graph.tasks[1].dependencies, vec!["cli"]);
        assert_eq!(graph.tasks[1].predicted_paths, vec!["agent-macos"]);
        assert!(graph.validate().is_ok());
    }

    #[test]
    fn heading_metadata_rejects_unknown_keys() {
        let error = task_graph_from_markdown(
            "# Plan\n\n### Build CLI\n<!-- mice: owner=codex -->\n",
            "mission",
        )
        .unwrap_err();
        assert!(error.to_string().contains("Unknown MICE task metadata key"));
    }

    #[test]
    fn verified_predecessor_unlocks_only_its_safe_dependent_task() {
        let graph = MissionTaskGraph {
            tasks: vec![
                MissionTask {
                    id: "core".into(),
                    title: "Core".into(),
                    acceptance: vec!["test".into()],
                    dependencies: vec![],
                    predicted_paths: vec!["crates/mice-core".into()],
                },
                MissionTask {
                    id: "cli".into(),
                    title: "CLI".into(),
                    acceptance: vec!["test".into()],
                    dependencies: vec!["core".into()],
                    predicted_paths: vec!["crates/mice-cli".into()],
                },
            ],
        };
        let record = MissionRecord {
            identity: MissionIdentity {
                repo_id: "repo".into(),
                mission_id: "mission".into(),
                plan_fingerprint: "f".repeat(64),
            },
            updated_at: 0,
            graph: graph.clone(),
            assignments: vec![MissionTaskAssignment {
                task_id: "core".into(),
                agent: MissionAgentKind::Codex,
            }],
            tasks: vec![MissionTaskRuntime {
                task_id: "core".into(),
                agent: MissionAgentKind::Codex,
                state: MissionTaskState::VerifiedReady,
                branch: "mice/mission/core".into(),
                worktree_path: "/tmp/core".into(),
                process_id: None,
                exit_code: None,
                observed_paths: Vec::new(),
                coordination_notes: Vec::new(),
                verified_at: None,
                started_at: 0,
            }],
        };
        let candidates = launch_candidates(
            &graph,
            &[Some(MissionAgentKind::Codex), Some(MissionAgentKind::Codex)],
            &record,
        );
        assert_eq!(
            candidates
                .iter()
                .map(|(_, task)| task.id.as_str())
                .collect::<Vec<_>>(),
            ["cli"]
        );
        assert_eq!(
            next_safe_task(&graph, &record).map(|task| task.id.as_str()),
            Some("cli")
        );
    }

    #[test]
    fn shared_agent_context_contains_only_task_map_and_lifecycle_facts() {
        let graph = MissionTaskGraph {
            tasks: vec![MissionTask {
                id: "core".into(),
                title: "Core".into(),
                acceptance: vec!["test".into()],
                dependencies: vec![],
                predicted_paths: vec!["crates/mice-core".into()],
            }],
        };
        let record = MissionRecord {
            identity: MissionIdentity {
                repo_id: "repo".into(),
                mission_id: "mission".into(),
                plan_fingerprint: "f".repeat(64),
            },
            updated_at: 0,
            graph: graph.clone(),
            assignments: vec![MissionTaskAssignment {
                task_id: "core".into(),
                agent: MissionAgentKind::Codex,
            }],
            tasks: vec![MissionTaskRuntime {
                task_id: "core".into(),
                agent: MissionAgentKind::Codex,
                state: MissionTaskState::Running,
                branch: "mice/mission/core".into(),
                worktree_path: "/private/worktree".into(),
                process_id: Some(12),
                exit_code: None,
                observed_paths: Vec::new(),
                coordination_notes: Vec::new(),
                verified_at: None,
                started_at: 0,
            }],
        };
        let context = mission_agent_context(&graph, &[Some(MissionAgentKind::Codex)], &record);
        assert!(context.contains("core → Codex [Running"));
        assert!(context.contains("scope: crates/mice-core"));
        assert!(!context.contains("/private/worktree"));
        assert!(!context.contains("pid"));
    }

    #[test]
    fn task_slug_is_stable_and_path_safe() {
        assert_eq!(slug("M0: Mission Control!"), "m0-mission-control");
        assert_eq!(slug("---"), "plan");
    }

    #[test]
    fn mission_identity_is_bound_to_the_repository() {
        let plan = LoadedPlan {
            contents: "# Plan".into(),
            relative_path: PathBuf::from("plan/mission.md"),
            display_name: "mission".into(),
            fingerprint: "a".repeat(64),
        };
        let first = mission_identity("repo-a", &plan);
        let second = mission_identity("repo-b", &plan);
        assert_ne!(first.mission_id, second.mission_id);
        assert_eq!(first.plan_fingerprint, second.plan_fingerprint);
    }

    #[test]
    fn model_proposal_is_validated_and_prefers_requested_agent() {
        let response = r#"```json
{"tasks":[
  {"id":"core","title":"Build core","acceptance":["cargo test -p mice-core"],"dependencies":[],"predicted_paths":["crates/mice-core"],"preferred_agent":"codex"},
  {"id":"cli","title":"Build CLI","acceptance":["cargo test -p mice-cli"],"dependencies":["core"],"predicted_paths":["crates/mice-cli"],"preferred_agent":"claude"}
]}
```"#;
        let (graph, assignments) = model_mission_proposal(
            response,
            &[MissionAgentKind::Codex, MissionAgentKind::Claude],
        )
        .unwrap();
        assert_eq!(graph.tasks.len(), 2);
        assert_eq!(assignments.get("core"), Some(&MissionAgentKind::Codex));
        assert_eq!(assignments.get("cli"), Some(&MissionAgentKind::Claude));
    }

    #[test]
    fn model_proposal_rejects_parallel_overlap_and_unrequested_agent() {
        let overlap = r#"{"tasks":[
          {"id":"left","title":"Left","acceptance":["test"],"dependencies":[],"predicted_paths":["src/lib.rs"],"preferred_agent":"codex"},
          {"id":"right","title":"Right","acceptance":["test"],"dependencies":[],"predicted_paths":["src/lib.rs"],"preferred_agent":"codex"}
        ]}"#;
        assert!(
            model_mission_proposal(overlap, &[MissionAgentKind::Codex])
                .unwrap_err()
                .to_string()
                .contains("overlapping")
        );
        let unrequested = r#"{"tasks":[
          {"id":"core","title":"Core","acceptance":["test"],"dependencies":[],"predicted_paths":[],"preferred_agent":"antigravity"}
        ]}"#;
        assert!(model_mission_proposal(unrequested, &[MissionAgentKind::Codex]).is_err());
    }

    #[test]
    fn planner_options_support_markdown_and_explicit_cloud_fallback() {
        let options = MissionOptions::parse(&[
            "plan".into(),
            "plan/mission.md".into(),
            "--agents".into(),
            "codex".into(),
            "--planner".into(),
            "markdown".into(),
            "--allow-cloud".into(),
        ])
        .unwrap();
        assert_eq!(options.planner, MissionPlannerMode::Markdown);
        assert!(options.allow_cloud);
    }

    #[test]
    fn coordination_notes_are_bounded_and_reject_credentials() {
        assert_eq!(
            sanitize_coordination_note("Waiting on API shape decision").unwrap(),
            "Waiting on API shape decision"
        );
        assert!(sanitize_coordination_note("api_key=not-for-storage").is_err());
        assert!(sanitize_coordination_note(&"x".repeat(MAX_TASK_NOTE_CHARS + 1)).is_err());
    }

    #[test]
    fn completion_notice_reports_states_and_manual_next_task() {
        let record = MissionRecord {
            identity: MissionIdentity {
                repo_id: "repo".into(),
                mission_id: "mission".into(),
                plan_fingerprint: "f".repeat(64),
            },
            updated_at: 1,
            graph: MissionTaskGraph::default(),
            assignments: vec![],
            tasks: vec![MissionTaskRuntime {
                task_id: "core".into(),
                agent: MissionAgentKind::Codex,
                state: MissionTaskState::VerifiedReady,
                branch: "mice/mission/core".into(),
                worktree_path: "/private/worktree".into(),
                process_id: None,
                exit_code: None,
                observed_paths: vec!["crates/mice-core/src/lib.rs".into()],
                coordination_notes: vec![],
                verified_at: Some(1),
                started_at: 0,
            }],
        };
        let notice = completion_notice(&record, "core", Some("cli"));
        assert!(notice.contains("VerifiedReady=1"));
        assert!(notice.contains("Suggested next task: `cli`"));
        assert!(!notice.contains("/private/worktree"));
    }

    #[test]
    fn mcp_context_exposes_bounded_facts_not_private_runtime_details() {
        let record = MissionRecord {
            identity: MissionIdentity {
                repo_id: "repo".into(),
                mission_id: "mission".into(),
                plan_fingerprint: "f".repeat(64),
            },
            updated_at: 1,
            graph: MissionTaskGraph {
                tasks: vec![
                    MissionTask {
                        id: "cli".into(),
                        title: "CLI".into(),
                        acceptance: vec!["test".into()],
                        dependencies: vec![],
                        predicted_paths: vec!["crates/mice-cli".into()],
                    },
                    MissionTask {
                        id: "package".into(),
                        title: "Package".into(),
                        acceptance: vec!["test".into()],
                        dependencies: vec!["cli".into()],
                        predicted_paths: vec!["scripts".into()],
                    },
                ],
            },
            assignments: vec![
                MissionTaskAssignment {
                    task_id: "cli".into(),
                    agent: MissionAgentKind::Claude,
                },
                MissionTaskAssignment {
                    task_id: "package".into(),
                    agent: MissionAgentKind::Codex,
                },
            ],
            tasks: vec![MissionTaskRuntime {
                task_id: "cli".into(),
                agent: MissionAgentKind::Claude,
                state: MissionTaskState::Running,
                branch: "mice/mission/cli".into(),
                worktree_path: "/private/owned-worktree".into(),
                process_id: Some(4242),
                exit_code: Some(1),
                observed_paths: vec!["crates/mice-cli/src/mission.rs".into()],
                coordination_notes: vec!["Waiting on CLI shape.".into()],
                verified_at: None,
                started_at: 0,
            }],
        };
        let output = render_mcp_status(
            Path::new("plan/mission.md"),
            &record,
            &RiskReport::default(),
            Some("package"),
        );
        assert!(output.contains("observed paths: crates/mice-cli/src/mission.rs"));
        assert!(output.contains("adapter exit: 1"));
        assert!(output.contains("coordination note: Waiting on CLI shape."));
        assert!(output.contains("next safe task: package"));
        assert!(output.contains("Scheduled: package → Codex"));
        assert!(!output.contains("/private/owned-worktree"));
        assert!(!output.contains("4242"));
    }
}
