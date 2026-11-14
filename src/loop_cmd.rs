//! `actmaton loop`: autonomous iteration with progress reporting and
//! mechanical stop conditions.
//!
//! Selection rule: the oldest open issue that is not labeled `hold` and has
//! not already been attempted in this loop invocation. If none exists,
//! `propose` files a new issue, which becomes that iteration's work.
//! After a passing run the branch is pushed and a *draft* PR is opened here,
//! in the orchestrator process (never inside the agent session). Merging is
//! left to the user.

use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::run_loop::{git, repo_root, run_number};

const REPO: &str = "piriwata/actmaton";
const HOLD_LABEL: &str = "hold";

#[derive(Debug, Clone, PartialEq)]
pub struct Limits {
    pub max_iterations: u32,
    pub max_failures: u32,
    pub budget: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { max_iterations: 5, max_failures: 2, budget: Duration::from_secs(4 * 3600) }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct BoardIssue {
    pub number: u64,
    pub created_at: String,
    pub labels: Vec<String>,
}

#[derive(Debug, Default, Clone)]
pub struct History {
    pub iterations: u32,
    pub consecutive_failures: u32,
    pub attempted: Vec<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Run(u64),
    Propose,
    Stop(String),
}

/// Decide the next action from the board, history, and elapsed time.
/// Stop conditions are checked first, in a fixed order.
pub fn plan(board: &[BoardIssue], history: &History, limits: &Limits, elapsed: Duration) -> Action {
    if history.iterations >= limits.max_iterations {
        return Action::Stop(format!("max iterations reached ({})", limits.max_iterations));
    }
    if history.consecutive_failures >= limits.max_failures {
        return Action::Stop(format!(
            "max consecutive failures reached ({})",
            limits.max_failures
        ));
    }
    if elapsed >= limits.budget {
        return Action::Stop(format!("wall-clock budget exhausted ({}s)", limits.budget.as_secs()));
    }
    let next = board
        .iter()
        .filter(|i| !i.labels.iter().any(|l| l == HOLD_LABEL))
        .filter(|i| !history.attempted.contains(&i.number))
        .min_by(|a, b| (&a.created_at, a.number).cmp(&(&b.created_at, b.number)));
    match next {
        Some(i) => Action::Run(i.number),
        None => Action::Propose,
    }
}

fn parse_args(args: &[String]) -> Result<Limits, String> {
    let mut l = Limits::default();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = |name: &str| -> Result<u64, String> {
            it.next()
                .and_then(|v| v.parse().ok())
                .ok_or(format!("{name} needs a non-negative integer"))
        };
        match a.as_str() {
            "--max-iterations" => l.max_iterations = val(a)? as u32,
            "--max-failures" => l.max_failures = val(a)? as u32,
            "--budget-minutes" => l.budget = Duration::from_secs(val(a)? * 60),
            other => return Err(format!("unknown loop argument: {other}")),
        }
    }
    Ok(l)
}

fn fetch_board() -> Result<Vec<BoardIssue>, Box<dyn std::error::Error>> {
    let out = Command::new("gh")
        .args([
            "issue", "list", "--repo", REPO, "--state", "open", "--limit", "200", "--json",
            "number,createdAt,labels",
        ])
        .output()?;
    if !out.status.success() {
        return Err(format!("gh issue list failed: {}", String::from_utf8_lossy(&out.stderr).trim()).into());
    }
    let v: Value = serde_json::from_slice(&out.stdout)?;
    Ok(v.as_array()
        .map(|a| {
            a.iter()
                .map(|i| BoardIssue {
                    number: i["number"].as_u64().unwrap_or(0),
                    created_at: i["createdAt"].as_str().unwrap_or_default().to_string(),
                    labels: i["labels"]
                        .as_array()
                        .map(|l| l.iter().filter_map(|x| x["name"].as_str().map(String::from)).collect())
                        .unwrap_or_default(),
                })
                .collect()
        })
        .unwrap_or_default())
}

fn issue_number_from_url(url: &str) -> Option<u64> {
    url.trim().rsplit('/').next()?.parse().ok()
}

/// Commit the worktree's changes, push the branch, and open a draft PR.
fn push_and_open_draft_pr(
    worktree: &Path,
    branch: &str,
    number: u64,
    title: &str,
    summary: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    if !git(worktree, &["status", "--porcelain"])?.trim().is_empty() {
        git(worktree, &["add", "-A"])?;
        git(worktree, &["commit", "-m", &format!("{title}\n\nRefs #{number}")])?;
    }
    git(worktree, &["push", "-u", "origin", branch])?;
    let body = format!("Refs #{number}\n\n{summary}");
    let out = Command::new("gh")
        .args([
            "pr", "create", "--draft", "--repo", REPO, "--head", branch, "--title", title,
            "--body", &body,
        ])
        .current_dir(worktree)
        .output()?;
    if !out.status.success() {
        return Err(format!("gh pr create failed: {}", String::from_utf8_lossy(&out.stderr).trim()).into());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn one_line(s: &str) -> String {
    let line = s.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("(no summary)");
    line.chars().take(200).collect()
}

fn entry(iter: u32, issue: &str, model: &str, verdict: &str, pr: &str, summary: &str) -> String {
    format!(
        "- iteration {iter}: issue {issue} | model: {model} | verdict: {verdict} | PR: {pr} | {summary}\n"
    )
}

fn report(path: &Path, text: &str) {
    print!("{text}");
    if let Ok(mut f) = std::fs::OpenOptions::new().append(true).create(true).open(path) {
        let _ = f.write_all(text.as_bytes());
    }
}

/// One iteration's work; returns (issue label, verdict, pr, summary).
async fn do_iteration(
    action: &Action,
    attempted: &mut Vec<u64>,
) -> Result<(String, String, String, String), (String, Box<dyn std::error::Error>)> {
    let number = match action {
        Action::Run(n) => *n,
        _ => {
            let url = crate::propose::propose_issue().await.map_err(|e| ("(propose)".to_string(), e))?;
            issue_number_from_url(&url)
                .ok_or_else(|| (url.clone(), "could not parse proposed issue URL".into()))?
        }
    };
    attempted.push(number);
    let label = format!("#{number}");
    let o = run_number(number).await.map_err(|e| (label.clone(), e))?;
    let summary = one_line(o.final_message.as_deref().unwrap_or(""));
    if !o.tests_ok {
        return Ok((format!("#{number} {}", o.title), "FAIL (tests failing)".into(), "-".into(), summary));
    }
    let body = format!(
        "{}\n\nIssue: {}\nRun log: `{}`\nTests: PASS",
        o.final_message.as_deref().unwrap_or("(no final message)"),
        o.url,
        o.log_path.display()
    );
    let pr = push_and_open_draft_pr(&o.worktree, &o.branch, number, &o.title, &body)
        .map_err(|e| (label, e))?;
    Ok((format!("#{number} {}", o.title), "PASS".into(), pr, summary))
}

pub async fn run_loop(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let limits = parse_args(args)?;
    let root = repo_root()?;
    let dir = root.parent().ok_or("repo root has no parent directory")?.join("actmaton-runs");
    std::fs::create_dir_all(&dir)?;
    let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs();
    let path = dir.join(format!("loop-{ts}.md"));

    report(
        &path,
        &format!(
            "# actmaton loop report ({ts})\n\nlimits: max-iterations={} max-failures={} budget={}min\n\n",
            limits.max_iterations,
            limits.max_failures,
            limits.budget.as_secs() / 60
        ),
    );

    let start = Instant::now();
    let mut history = History::default();
    loop {
        let board = fetch_board()?;
        let action = plan(&board, &history, &limits, start.elapsed());
        if let Action::Stop(reason) = &action {
            report(&path, &format!("\nstopped: {reason}\n"));
            break;
        }
        history.iterations += 1;
        let model = "default";
        let line = match do_iteration(&action, &mut history.attempted).await {
            Ok((issue, verdict, pr, summary)) => {
                if verdict == "PASS" {
                    history.consecutive_failures = 0;
                } else {
                    history.consecutive_failures += 1;
                }
                entry(history.iterations, &issue, model, &verdict, &pr, &summary)
            }
            Err((issue, e)) => {
                history.consecutive_failures += 1;
                entry(history.iterations, &issue, model, "ERROR", "-", &one_line(&e.to_string()))
            }
        };
        report(&path, &line);
    }
    println!("loop report: {}", path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issue(n: u64, ts: &str, labels: &[&str]) -> BoardIssue {
        BoardIssue {
            number: n,
            created_at: ts.into(),
            labels: labels.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn lim() -> Limits {
        Limits { max_iterations: 3, max_failures: 2, budget: Duration::from_secs(100) }
    }

    #[test]
    fn runs_oldest_skipping_held_and_attempted() {
        let board = vec![
            issue(5, "2026-02-01", &[]),
            issue(2, "2026-01-01", &["hold"]),
            issue(3, "2026-01-02", &[]),
            issue(4, "2026-01-03", &[]),
        ];
        let mut h = History::default();
        assert_eq!(plan(&board, &h, &lim(), Duration::ZERO), Action::Run(3));
        h.attempted.push(3);
        assert_eq!(plan(&board, &h, &lim(), Duration::ZERO), Action::Run(4));
    }

    #[test]
    fn proposes_when_nothing_eligible() {
        let board = vec![issue(2, "2026-01-01", &["hold"])];
        assert_eq!(plan(&board, &History::default(), &lim(), Duration::ZERO), Action::Propose);
        assert_eq!(plan(&[], &History::default(), &lim(), Duration::ZERO), Action::Propose);
    }

    #[test]
    fn stop_conditions() {
        let board = vec![issue(1, "2026-01-01", &[])];
        let h = History { iterations: 3, ..Default::default() };
        assert!(matches!(plan(&board, &h, &lim(), Duration::ZERO), Action::Stop(r) if r.contains("iterations")));
        let h = History { iterations: 1, consecutive_failures: 2, ..Default::default() };
        assert!(matches!(plan(&board, &h, &lim(), Duration::ZERO), Action::Stop(r) if r.contains("failures")));
        let h = History::default();
        assert!(matches!(plan(&board, &h, &lim(), Duration::from_secs(100)), Action::Stop(r) if r.contains("budget")));
    }

    #[test]
    fn parses_args_and_url() {
        let a: Vec<String> = ["--max-iterations", "2", "--budget-minutes", "10"].map(String::from).to_vec();
        let l = parse_args(&a).unwrap();
        assert_eq!((l.max_iterations, l.max_failures, l.budget.as_secs()), (2, 2, 600));
        assert!(parse_args(&["--bogus".to_string()]).is_err());
        assert_eq!(issue_number_from_url("https://github.com/o/r/issues/42\n"), Some(42));
    }
}
