//! `actmaton loop`: autonomous iteration with progress reporting and
//! mechanical stop conditions.
//!
//! Selection rule: the oldest open issue that is not labeled `hold` and has
//! not already been attempted in this loop invocation. If none exists,
//! `propose` files a new issue, which becomes that iteration's work.
//! After a non-failing run the branch is pushed and a *draft* PR is opened here,
//! in the orchestrator process (never inside the agent session). Merging is
//! left to the user, except in dev mode (`--base dev`): PRs target `dev` and a
//! PASS iteration is merged into dev (merge commit) once dev stays green; a red
//! dev stops the loop. The standing `dev` → `main` PR is only ever created or
//! updated here, never merged (main is always the user's).
//!
//! Verdict: FAIL if `cargo test` fails; PASS if tests pass and every done/
//! acceptance criterion in the issue is reported `VERIFIED` by the session
//! (or the issue states none); otherwise PARTIAL-HUMAN-ONLY (only `HUMAN-CHECK`
//! criteria remain; also merged into dev, items listed under 要確認 in the
//! standing PR) or PARTIAL-BLOCKED (some criterion `NOT-VERIFIED`; not merged),
//! with the unverified criteria named in the loop report and the PR body.

use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::run_loop::{decide_verdict, git, repo_root, run_number, Verdict};

const REPO: &str = "piriwata/actmaton";
const HOLD_LABEL: &str = "hold";

#[derive(Debug, Clone, PartialEq)]
pub struct Limits {
    pub max_iterations: u32,
    pub max_failures: u32,
    pub budget: Duration,
    /// Branch the loop's PRs and worktrees target; `None` = main, no auto-merge.
    pub base: Option<String>,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { max_iterations: 5, max_failures: 2, budget: Duration::from_secs(4 * 3600), base: None }
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

const BRANCH_PREFIX: &str = "actmaton/issue-";

#[derive(Debug, Clone, PartialEq)]
pub struct OpenPr {
    pub number: u64,
    pub branch: String,
    pub base: String,
    pub mergeable: String,
    pub draft: bool,
    pub labels: Vec<String>,
}

const NEEDS_HUMAN_LABEL: &str = "needs-human";

/// Human PRs that joined the dev flow by targeting dev: not actmaton's own
/// branch, not a draft, and not already escalated.
pub fn human_candidates(prs: &[OpenPr]) -> Vec<&OpenPr> {
    prs.iter()
        .filter(|p| p.base == crate::devflow::DEV && !p.branch.starts_with(BRANCH_PREFIX) && !p.draft)
        .filter(|p| !p.labels.iter().any(|l| l == NEEDS_HUMAN_LABEL))
        .collect()
}

/// Open PRs eligible for conflict repair: actmaton's own branches only,
/// conflicting only, and not already attempted in this invocation.
pub fn repair_candidates<'a>(prs: &'a [OpenPr], repaired: &[u64]) -> Vec<&'a OpenPr> {
    prs.iter()
        .filter(|p| p.branch.starts_with(BRANCH_PREFIX))
        .filter(|p| p.mergeable == "CONFLICTING")
        .filter(|p| !repaired.contains(&p.number))
        .collect()
}

fn fetch_open_prs() -> Result<Vec<OpenPr>, Box<dyn std::error::Error>> {
    let out = Command::new("gh")
        .args([
            "pr", "list", "--repo", REPO, "--state", "open", "--limit", "100", "--json",
            "number,headRefName,baseRefName,mergeable,isDraft,labels",
        ])
        .output()?;
    if !out.status.success() {
        return Err(format!("gh pr list failed: {}", String::from_utf8_lossy(&out.stderr).trim()).into());
    }
    let v: Value = serde_json::from_slice(&out.stdout)?;
    Ok(v.as_array()
        .map(|a| {
            a.iter()
                .map(|p| OpenPr {
                    number: p["number"].as_u64().unwrap_or(0),
                    branch: p["headRefName"].as_str().unwrap_or_default().to_string(),
                    base: p["baseRefName"].as_str().unwrap_or("main").to_string(),
                    mergeable: p["mergeable"].as_str().unwrap_or_default().to_string(),
                    draft: p["isDraft"].as_bool().unwrap_or(false),
                    labels: p["labels"]
                        .as_array()
                        .map(|a| a.iter().filter_map(|l| l["name"].as_str().map(String::from)).collect())
                        .unwrap_or_default(),
                })
                .collect()
        })
        .unwrap_or_default())
}

/// Repair each eligible conflicting PR once, comment on it, and return report lines.
async fn maintain(report_path: &Path, repaired: &mut Vec<u64>) {
    let prs = match fetch_open_prs() {
        Ok(p) => p,
        Err(e) => {
            report(report_path, &format!("- maintenance: could not list PRs: {}\n", one_line(&e.to_string())));
            return;
        }
    };
    let todo: Vec<OpenPr> = repair_candidates(&prs, repaired).into_iter().cloned().collect();
    for pr in todo {
        repaired.push(pr.number);
        let (line, comment) = match crate::run_loop::repair_conflict(pr.number, &pr.branch, &pr.base).await {
            Ok(o) => {
                let files = if o.files.is_empty() { "(none)".to_string() } else { o.files.join(", ") };
                let verdict = match (o.tests_ok, o.pushed) {
                    (true, true) => "REPAIRED (tests PASS, pushed)",
                    (true, false) => "no change needed",
                    _ => "!!! REPAIR FAILED — needs attention (nothing pushed)",
                };
                let comment = format!(
                    "actmaton conflict repair against `{}`\n\nConflict files: {files}\nTests: {}\nPushed: {}\n\nResolution:\n{}",
                    pr.base,
                    if o.tests_ok { "PASS" } else { "FAIL" },
                    o.pushed,
                    o.summary
                );
                (
                    format!("- maintenance: PR #{} | conflicts: {files} | {verdict} | {}\n", pr.number, one_line(&o.summary)),
                    comment,
                )
            }
            Err(e) => (
                format!("- maintenance: PR #{} | !!! REPAIR ERROR — needs attention | {}\n", pr.number, one_line(&e.to_string())),
                format!("actmaton conflict repair could not run: {e}"),
            ),
        };
        report(report_path, &line);
        let _ = Command::new("gh")
            .args(["pr", "comment", &pr.number.to_string(), "--repo", REPO, "--body", &comment])
            .output();
    }
}

/// Test the exact result GitHub would merge (`refs/pull/N/merge`) in a sibling worktree.
fn human_pr_green(root: &Path, pr: u64) -> Result<bool, Box<dyn std::error::Error>> {
    git(root, &["fetch", "origin", &format!("refs/pull/{pr}/merge")])?;
    let wt = root.parent().ok_or("repo root has no parent directory")?.join("actmaton-human-pr");
    if wt.exists() {
        git(&wt, &["checkout", "--detach", "FETCH_HEAD"])?;
    } else {
        git(root, &["worktree", "add", "--detach", wt.to_str().ok_or("path is not UTF-8")?, "FETCH_HEAD"])?;
    }
    Ok(crate::run_loop::cargo_test(&wt).0)
}

/// Mark a human PR as needing the human and record it for the standing PR.
fn escalate_human_pr(pr: u64, why: &str, dev: &mut DevState) {
    let n = pr.to_string();
    let _ = gh(&["label", "create", NEEDS_HUMAN_LABEL, "--repo", REPO, "--color", "d93f0b", "--description", "actmaton could not resolve this by itself"]);
    let _ = gh(&["pr", "edit", &n, "--repo", REPO, "--add-label", NEEDS_HUMAN_LABEL]);
    let _ = gh(&["pr", "comment", &n, "--repo", REPO, "--body", &format!("actmaton needs a human here: {why}\n\nNothing was pushed or resolved automatically.")]);
    dev.human_notes.push((pr, format!("#{pr}: 競合を自動で直せなかったため、人の確認待ちにしました（`{NEEDS_HUMAN_LABEL}`）。{}", one_line(why))));
}

/// Human PRs targeting dev: repair conflicts by merging dev in (escalating the
/// non-mechanical ones), and auto-merge green ones. Never edits their commits,
/// closes them, or touches main.
async fn maintain_human(root: &Path, report_path: &Path, repaired: &mut Vec<u64>, dev: &mut DevState) {
    let Ok(prs) = fetch_open_prs() else { return };
    let todo: Vec<OpenPr> = human_candidates(&prs).into_iter().cloned().collect();
    for pr in todo {
        match pr.mergeable.as_str() {
            "CONFLICTING" if !repaired.contains(&pr.number) => {
                repaired.push(pr.number);
                match crate::run_loop::repair_conflict(pr.number, &pr.branch, &pr.base).await {
                    Ok(o) if o.tests_ok && o.pushed => {
                        let files = if o.files.is_empty() { "(none)".to_string() } else { o.files.join(", ") };
                        let _ = gh(&["pr", "comment", &pr.number.to_string(), "--repo", REPO, "--body", &format!("actmaton merged `{}` into this branch (no rebase, no force-push).\n\nConflict files: {files}\nTests: PASS\n\nResolution:\n{}", pr.base, o.summary)]);
                        dev.human_notes.push((pr.number, format!("#{}: `{}` をブランチにマージして競合を直しました（対象: {files}）。", pr.number, pr.base)));
                        report(report_path, &format!("- maintenance: human PR #{} | conflicts: {files} | REPAIRED (dev merged in, pushed)\n", pr.number));
                    }
                    Ok(o) if o.tests_ok => {}
                    Ok(o) => {
                        report(report_path, &format!("- maintenance: human PR #{} | !!! ESCALATED needs-human | {}\n", pr.number, one_line(&o.summary)));
                        escalate_human_pr(pr.number, &o.summary, dev);
                    }
                    Err(e) => {
                        report(report_path, &format!("- maintenance: human PR #{} | !!! ESCALATED needs-human | {}\n", pr.number, one_line(&e.to_string())));
                        escalate_human_pr(pr.number, &e.to_string(), dev);
                    }
                }
            }
            "MERGEABLE" if dev.dev_green && !dev.human_tried.contains(&pr.number) => {
                dev.human_tried.push(pr.number);
                let n = pr.number.to_string();
                let line = match human_pr_green(root, pr.number) {
                    Ok(true) => match merge_into_dev(&n, &Verdict::Pass, root, dev) {
                        Ok(s) => s,
                        Err(e) => format!("!!! merge failed: {}", one_line(&e.to_string())),
                    },
                    Ok(false) => "not merged: tests FAIL on the merge result".to_string(),
                    Err(e) => format!("not merged: could not test: {}", one_line(&e.to_string())),
                };
                report(report_path, &format!("- maintenance: human PR #{n} | {line}\n"));
            }
            _ => {}
        }
        if dev.halt.is_some() {
            break;
        }
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
            "--base" => l.base = Some(it.next().ok_or("--base needs a branch name")?.clone()),
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
    base: Option<&str>,
) -> Result<String, Box<dyn std::error::Error>> {
    if !git(worktree, &["status", "--porcelain"])?.trim().is_empty() {
        git(worktree, &["add", "-A"])?;
        git(worktree, &["commit", "-m", &format!("{title}\n\nCloses #{number}")])?;
    }
    git(worktree, &["push", "-u", "origin", branch])?;
    let body = format!("Closes #{number}\n\n{summary}");
    let mut cmd = Command::new("gh");
    cmd.args(["pr", "create", "--draft", "--repo", REPO, "--head", branch, "--title", title, "--body", &body]);
    if let Some(b) = base {
        cmd.args(["--base", b]);
    }
    let out = cmd
        .current_dir(worktree)
        .output()?;
    if !out.status.success() {
        return Err(format!("gh pr create failed: {}", String::from_utf8_lossy(&out.stderr).trim()).into());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn one_line(s: &str) -> String {
    let line = s.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("(no summary)");
    truncate_at_word(line, 200)
}

/// Cut `s` to at most `max` chars at a word boundary, adding an ellipsis.
fn truncate_at_word(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    let boundary_ok = s.chars().nth(max).is_some_and(char::is_whitespace);
    let cut = if boundary_ok { cut.as_str() } else { cut.rsplit_once(char::is_whitespace).map_or(cut.as_str(), |(h, _)| h) };
    format!("{}…", cut.trim_end_matches(|c: char| c.is_whitespace() || ",;:-".contains(c)))
}

/// Drop session-local status lines (uncommitted / will push / next steps)
/// that orchestration makes false once the loop commits and opens the PR.
fn sanitize_pr_body(s: &str) -> String {
    const STALE: [&str; 7] = [
        "uncommitted",
        "not committed",
        "nothing is committed",
        "not been committed",
        "will push",
        "next step",
        "left for you",
    ];
    let kept: Vec<&str> = s
        .lines()
        .filter(|l| {
            let l = l.to_lowercase();
            !STALE.iter().any(|p| l.contains(p))
        })
        .collect();
    kept.join("\n").trim().to_string()
}

fn entry(iter: u32, issue: &str, model: &str, verdict: &str, pr: &str, summary: &str) -> String {
    format!(
        "- iteration {iter}: issue {issue} | model: {model} | verdict: {verdict} | PR: {pr} | {summary}\n"
    )
}

/// Post-iteration self-review: deterministic checks, ledger, and escalation of
/// recurring classes. Returns report lines to append under the iteration entry.
fn self_review(dir: &Path, line: &str, verdict: &str, pr: &str, r: &Reviewed, issue: Option<u64>) -> String {
    use crate::review::{self, LedgerEntry};
    let n = issue.unwrap_or(0);
    let root = crate::run_loop::repo_root().ok();
    let on_main = |p: &str| match &root {
        Some(r) => crate::run_loop::git(r, &["cat-file", "-e", &format!("origin/main:{p}")]).is_ok(),
        None => true,
    };
    let findings = review::check(&review::Artifacts {
        issue_body: &r.issue_body,
        final_message: &r.final_message,
        pr_body: &r.pr_body,
        commit_message: &r.commit_message,
        run_log: &r.run_log,
        report_line: line,
        verdict,
        pushed: !pr.is_empty() && pr != "-",
        issue_number: n,
        on_main: &on_main,
    });
    if findings.is_empty() {
        return String::new();
    }
    let mut out = review::render(&findings);
    let path = dir.join("review-ledger.jsonl");
    let new: Vec<LedgerEntry> = findings
        .iter()
        .map(|f| LedgerEntry { class: f.class.clone(), issue: n, evidence: f.evidence.clone(), escalated: false })
        .collect();
    let mut marks = Vec::new();
    for (class, entries) in review::to_escalate(&review::read_ledger(&path), &new) {
        let (title, body) = review::escalation_issue(&class, &entries);
        match crate::propose::create_issue(&title, &body) {
            Ok(url) => {
                out.push_str(&format!("  - escalated recurring class `{class}`: {url}\n"));
                marks.push(LedgerEntry { class, issue: 0, evidence: url, escalated: true });
            }
            Err(e) => out.push_str(&format!("  - escalation of `{class}` failed: {}\n", one_line(&e.to_string()))),
        }
    }
    let _ = review::append_ledger(&path, &[new, marks].concat());
    out
}

fn report(path: &Path, text: &str) {
    print!("{text}");
    if let Ok(mut f) = std::fs::OpenOptions::new().append(true).create(true).open(path) {
        let _ = f.write_all(text.as_bytes());
    }
}

/// Dev-mode bookkeeping for the standing dev → main PR.
#[derive(Default)]
struct DevState {
    items: Vec<crate::devflow::Item>,
    remaining: Vec<String>,
    dev_green: bool,
    dev_head: String,
    halt: Option<String>,
    /// (PR, note) for repairs/escalations on human PRs.
    human_notes: Vec<(u64, String)>,
    /// Human PRs already tested for auto-merge in this invocation.
    human_tried: Vec<u64>,
}

fn gh(args: &[&str]) -> Result<String, Box<dyn std::error::Error>> {
    let out = Command::new("gh").args(args).output()?;
    if !out.status.success() {
        return Err(format!("gh {} failed: {}", args[..2.min(args.len())].join(" "), String::from_utf8_lossy(&out.stderr).trim()).into());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Run cargo test on a fresh checkout of origin/dev in a sibling worktree.
fn dev_gate(root: &Path) -> Result<(bool, String), Box<dyn std::error::Error>> {
    use crate::devflow::DEV;
    git(root, &["fetch", "origin", DEV])?;
    let wt = root.parent().ok_or("repo root has no parent directory")?.join("actmaton-dev");
    let origin = format!("origin/{DEV}");
    if wt.exists() {
        git(&wt, &["checkout", "--detach", &origin])?;
    } else {
        git(root, &["worktree", "add", "--detach", wt.to_str().ok_or("path is not UTF-8")?, &origin])?;
    }
    let head = git(&wt, &["rev-parse", "--short", "HEAD"])?;
    Ok((crate::run_loop::cargo_test(&wt).0, head))
}

/// Ready the PR, merge it into dev (merge commit) if the decision allows, then
/// re-test dev. A red dev sets `dev.halt`. Returns a short status for the report.
fn merge_into_dev(pr: &str, verdict: &Verdict, root: &Path, dev: &mut DevState) -> Result<String, Box<dyn std::error::Error>> {
    let _ = gh(&["pr", "ready", pr, "--repo", REPO]);
    let (mut base, mut mergeable) = (String::new(), String::new());
    for _ in 0..6 {
        let v: Value = serde_json::from_str(&gh(&["pr", "view", pr, "--repo", REPO, "--json", "mergeable,baseRefName"])?)?;
        base = v["baseRefName"].as_str().unwrap_or_default().to_string();
        mergeable = v["mergeable"].as_str().unwrap_or_default().to_string();
        if mergeable != "UNKNOWN" {
            break;
        }
        std::thread::sleep(Duration::from_secs(5));
    }
    if !crate::devflow::should_merge(verdict, &base, &mergeable, dev.dev_green) {
        return Ok(format!("not merged (base={base}, mergeable={mergeable}, dev green={})", dev.dev_green));
    }
    gh(&["pr", "merge", pr, "--repo", REPO, "--merge"])?;
    let (ok, head) = dev_gate(root)?;
    dev.dev_green = ok;
    dev.dev_head = head;
    if ok {
        Ok("merged into dev; dev tests PASS".into())
    } else {
        dev.halt = Some(format!("!!! dev tests FAIL after merging {pr}; nothing further is merged"));
        Ok("merged into dev; !!! dev tests FAIL".into())
    }
}

fn item_json(i: &crate::devflow::Item) -> Value {
    serde_json::json!({"pr": i.pr, "title": i.title, "url": i.url, "human": i.human,
        "implemented": i.implemented, "decisions": i.decisions, "assumptions": i.assumptions,
        "files": i.files, "repair_attempts": i.repair_attempts, "human_checks": i.human_checks, "repair_note": i.repair_note, "closes": i.closes})
}

fn item_from_json(v: &Value) -> crate::devflow::Item {
    let s = |k: &str| v[k].as_str().unwrap_or_default().to_string();
    crate::devflow::Item {
        pr: v["pr"].as_u64().unwrap_or(0),
        title: s("title"),
        url: s("url"),
        human: v["human"].as_bool().unwrap_or(false),
        implemented: s("implemented"),
        decisions: s("decisions"),
        assumptions: s("assumptions"),
        files: v["files"].as_array().map(|a| a.iter().filter_map(|f| f.as_str().map(String::from)).collect()).unwrap_or_default(),
        repair_attempts: v["repair_attempts"].as_u64().unwrap_or(0) as u32,
        human_checks: v["human_checks"].as_array().map(|a| a.iter().filter_map(|f| f.as_str().map(String::from)).collect()).unwrap_or_default(),
        repair_note: v["repair_note"].as_str().map(String::from),
        closes: v["closes"].as_array().map(|a| a.iter().filter_map(|f| f.as_u64()).collect()).unwrap_or_default(),
    }
}

/// SKILL.md (and ALWAYS.md when present) of the nihongo skill folder; `None` if SKILL.md is absent.
fn load_nihongo(dir: &Path) -> Option<String> {
    let skill = std::fs::read_to_string(dir.join("SKILL.md")).ok()?;
    let always = std::fs::read_to_string(dir.join("ALWAYS.md")).unwrap_or_default();
    Some(format!("{skill}\n\n{always}"))
}

/// Create or update the standing dev → main PR. Items persist across loop runs
/// (in `dev-items.json`) and are kept while their merge is still ahead of main.
async fn update_standing_pr(dir: &Path, root: &Path, dev: &DevState, report_path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    use crate::devflow::{build_body, Morning};
    let store = dir.join("dev-items.json");
    let mut items: Vec<crate::devflow::Item> = std::fs::read_to_string(&store)
        .ok()
        .and_then(|t| serde_json::from_str::<Vec<Value>>(&t).ok())
        .map(|v| v.iter().map(item_from_json).collect())
        .unwrap_or_default();
    items.retain(|i| !dev.items.iter().any(|n| n.pr == i.pr));
    items.extend(dev.items.iter().cloned());
    git(root, &["fetch", "origin", "main", "dev"])?;
    let iterations = items.len() as u32;
    let human_touches = items.iter().filter(|i| i.human).count() as u32;
    let ahead = git(root, &["log", "origin/main..origin/dev", "--format=%H %s"])?;
    let mut failed_lookups: Vec<u64> = Vec::new();
    let shipped: Vec<crate::devflow::Item> = ahead
        .lines()
        .filter_map(|l| {
            let (sha, subject) = l.split_once(' ')?;
            let (pr, title) = crate::devflow::pr_from_subject(subject)?;
            let files = git(root, &["diff", "--name-only", &format!("{sha}^1"), sha])
                .map(|o| o.lines().map(String::from).collect())
                .unwrap_or_default();
            let closes = crate::devflow::closes_with_retry(|| gh(&["pr", "view", &pr.to_string(), "--repo", REPO, "--json", "body", "--jq", ".body"]))
                .unwrap_or_else(|e| {
                    report(report_path, &format!("- !!! closes lookup failed for PR #{pr}: {}\n", one_line(&e)));
                    failed_lookups.push(pr);
                    Vec::new()
                });
            Some(crate::devflow::Item { pr, title, url: format!("https://github.com/{REPO}/pull/{pr}"), files, closes, ..Default::default() })
        })
        .collect();
    let items = crate::devflow::merge_shipped(items, shipped);
    let notes_store = dir.join("human-pr-notes.json");
    let mut notes: Vec<(u64, String)> = std::fs::read_to_string(&notes_store)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    for n in &dev.human_notes {
        if !notes.contains(n) {
            notes.push(n.clone());
        }
    }
    // Keep notes only for PRs still open or already in dev ahead of main.
    if let Ok(open) = fetch_open_prs() {
        notes.retain(|(pr, _)| items.iter().any(|i| i.pr == *pr) || open.iter().any(|p| p.number == *pr));
    }
    std::fs::write(&notes_store, serde_json::to_string(&notes)?)?;    std::fs::write(&store, serde_json::to_string(&items.iter().map(item_json).collect::<Vec<_>>())?)?;

    let nihongo = std::env::var("ACTMATON_NIHONGO_DIR").ok().and_then(|d| load_nihongo(Path::new(&d)));
    let mut m = Morning {
        remaining: dev.remaining.clone(),
        dev_head: git(root, &["rev-parse", "--short", "origin/dev"]).unwrap_or_else(|_| dev.dev_head.clone()),
        dev_green: dev.dev_green,
        iterations,
        human_touches,
        items,
        yomiyasu_note: None,
        human_notes: notes.into_iter().map(|(_, n)| n).collect(),
    };
    if !failed_lookups.is_empty() {
        m.remaining.push(crate::devflow::lookup_failure_note(&failed_lookups));
    }
    if let Some(h) = &dev.halt {
        m.remaining.insert(0, h.clone());
    }
    let mut body = build_body(&m);
    match nihongo {
        Some(skill) => {
            let prompt = crate::devflow::nihongo_prompt(&skill, &body);
            match crate::propose::run_session(root, &prompt).await {
                Ok((Some(reply), _)) if crate::devflow::facts_preserved(&body, crate::devflow::unfence(&reply)) => {
                    body = crate::devflow::unfence(&reply).to_string();
                    report(report_path, "- nihongo: PR body Japanese refined by a session with the nihongo skill loaded\n");
                }
                other => {
                    let why = match other {
                        Ok((Some(_), _)) => "refined text dropped facts".to_string(),
                        Ok(_) => "session returned no final message".to_string(),
                        Err(e) => one_line(&e.to_string()),
                    };
                    report(report_path, &format!("- !!! nihongo: refinement not used ({why}); body is the deterministic text\n"));
                    m.yomiyasu_note = Some(crate::devflow::NIHONGO_NOTE.to_string());
                    body = build_body(&m);
                }
            }
        }
        None => {
            report(report_path, "- !!! nihongo skill not found (set ACTMATON_NIHONGO_DIR to the folder containing SKILL.md); standing PR body carries the degradation note\n");
            m.yomiyasu_note = Some(crate::devflow::NIHONGO_NOTE.to_string());
            body = build_body(&m);
        }
    }
    let title = "dev → main: 朝のまとめ";
    let existing = gh(&["pr", "list", "--repo", REPO, "--base", "main", "--head", crate::devflow::DEV, "--state", "open", "--json", "number", "--jq", ".[0].number"])?;
    let url = if existing.is_empty() {
        gh(&["pr", "create", "--repo", REPO, "--base", "main", "--head", crate::devflow::DEV, "--title", title, "--body", &body])?
    } else {
        gh(&["pr", "edit", &existing, "--repo", REPO, "--title", title, "--body", &body])?
    };
    report(report_path, &format!("- standing dev → main PR: {url}\n"));
    Ok(())
}

/// Maintenance: regenerate the standing PR body when dev has moved past the
/// head it names (e.g. after a manual merge), in any loop mode.
async fn refresh_stale_standing_pr(dir: &Path, root: &Path, report_path: &Path) {
    let check = || -> Result<Option<String>, Box<dyn std::error::Error>> {
        git(root, &["fetch", "origin", crate::devflow::DEV])?;
        let head = git(root, &["rev-parse", "--short", "origin/dev"])?;
        let body = gh(&["pr", "list", "--repo", REPO, "--base", "main", "--head", crate::devflow::DEV, "--state", "open", "--json", "body", "--jq", ".[0].body // empty"])?;
        Ok(if body.is_empty() || crate::devflow::body_names_head(&body, &head) { None } else { Some(head) })
    };
    let head = match check() {
        Ok(Some(x)) => x,
        Ok(None) => return,
        Err(e) => {
            report(report_path, &format!("- maintenance: standing PR staleness check failed: {}\n", one_line(&e.to_string())));
            return;
        }
    };
    report(report_path, &format!("- maintenance: standing PR body is stale (dev is `{head}`, body names a different head); regenerating\n"));
    let mut dev = DevState::default();
    match dev_gate(root) {
        Ok((ok, h)) => {
            dev.dev_green = ok;
            dev.dev_head = h;
        }
        Err(e) => {
            report(report_path, &format!("- maintenance: !!! could not test dev; standing PR not refreshed: {}\n", one_line(&e.to_string())));
            return;
        }
    }
    match update_standing_pr(dir, root, &dev, report_path).await {
        Ok(()) => report(report_path, &format!("- maintenance: standing PR body refreshed to dev `{}`\n", dev.dev_head)),
        Err(e) => report(report_path, &format!("- maintenance: !!! standing PR refresh failed: {}\n", one_line(&e.to_string()))),
    }
}

/// What the self-review needs from one passing iteration.
struct Reviewed {
    model: String,
    issue_body: String,
    final_message: String,
    pr_body: String,
    commit_message: String,
    run_log: String,
}

/// One iteration's work; returns (issue label, model, verdict, pr, summary, review input).
async fn do_iteration(
    action: &Action,
    attempted: &mut Vec<u64>,
    base: Option<&str>,
    dev: &mut DevState,
) -> Result<(String, String, String, String, String, Option<Reviewed>), (String, Box<dyn std::error::Error>)> {
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
    let o = run_number(number, base).await.map_err(|e| (label.clone(), e))?;
    let summary = one_line(o.final_message.as_deref().unwrap_or(""));
    let summary = match &o.recovery {
        Some(r) => format!("{r} | {summary}"),
        None => summary,
    };
    let model = o.model.clone().unwrap_or_else(|| "(not reported)".into());
    let verdict = decide_verdict(o.tests_ok, &o.criteria, o.final_message.as_deref());
    if verdict == Verdict::Fail {
        return Ok((format!("#{number} {}", o.title), model, "FAIL (tests failing)".into(), "-".into(), summary, None));
    }
    let unverified = match &verdict {
        Verdict::Partial(_) => format!("\nVerdict: {}. Unverified criteria:\n{}", verdict.label(), verdict.unverified_text()),
        _ => String::new(),
    };
    let final_message = o.final_message.clone().unwrap_or_default();
    let body = format!(
        "{}\n\nIssue: {}\nRun log: `{}`\nTests: PASS{unverified}",
        sanitize_pr_body(o.final_message.as_deref().unwrap_or("(no final message)")),
        o.url,
        o.log_path.display()
    );
    let pr = push_and_open_draft_pr(&o.worktree, &o.branch, number, &o.title, &body, base)
        .map_err(|e| (label, e))?;
    let mut summary = match verdict {
        Verdict::Partial(_) => format!(
            "{summary} | {}: {}",
            if verdict.human_only() { "human-check items" } else { "unverified" },
            verdict.unverified_text().replace('\n', " ")
        ),
        _ => summary,
    };
    if base == Some(crate::devflow::DEV) {
        let status = repo_root()
            .and_then(|root| merge_into_dev(&pr, &verdict, &root, dev))
            .unwrap_or_else(|e| format!("merge error: {}", one_line(&e.to_string())));
        summary = format!("{summary} | {status}");
        let files = git(&o.worktree, &["diff", "--name-only", &format!("origin/{}...HEAD", crate::devflow::DEV)])
            .map(|s| s.lines().map(String::from).collect())
            .unwrap_or_default();
        let fm = o.final_message.clone().unwrap_or_default();
        if status.starts_with("merged") {
            dev.items.push(crate::devflow::Item {
                pr: pr.rsplit('/').next().and_then(|n| n.parse().ok()).unwrap_or(0),
                title: o.title.clone(),
                url: pr.clone(),
                human: false,
                implemented: one_line(&fm),
                decisions: crate::devflow::extract_section(&fm, &["decision"]),
                assumptions: crate::devflow::extract_section(&fm, &["assumption"]),
                files,
                repair_attempts: o.repair_attempts,
                human_checks: verdict.human_checks(),
                repair_note: None,
                closes: vec![number],
            });
        } else {
            dev.remaining.push(format!("{pr}（#{number}）はマージされていません: {status}"));
        }
    }
    let run_log = std::fs::read_to_string(&o.log_path).unwrap_or_default();
    let reviewed = Reviewed {
        model: crate::review::logged_model(&run_log).unwrap_or_else(|| "default".into()),
        issue_body: o.issue_body.clone(),
        final_message,
        pr_body: format!("Closes #{number}\n\n{body}"),
        commit_message: format!("{}\n\nCloses #{number}", o.title),
        run_log,
    };
    Ok((format!("#{number} {}", o.title), model, verdict.label().into(), pr, summary, Some(reviewed)))
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
    let mut repaired: Vec<u64> = Vec::new();
    let base = limits.base.clone();
    let dev_mode = base.as_deref() == Some(crate::devflow::DEV);
    let mut dev = DevState::default();
    if dev_mode {
        match dev_gate(&root) {
            Ok((ok, head)) => {
                dev.dev_green = ok;
                dev.dev_head = head;
                if !ok {
                    dev.halt = Some("!!! dev tests FAIL at loop start; nothing is merged".into());
                }
            }
            Err(e) => dev.halt = Some(format!("!!! could not test dev at loop start: {}", one_line(&e.to_string()))),
        }
    }
    if dev.halt.is_none() {
        refresh_stale_standing_pr(&dir, &root, &path).await;
    }
    loop {
        if let Some(h) = &dev.halt {
            report(&path, &format!("\nstopped: {h}\n"));
            break;
        }
        maintain(&path, &mut repaired).await;
        if dev_mode {
            maintain_human(&root, &path, &mut repaired, &mut dev).await;
            if dev.halt.is_some() {
                continue;
            }
        }        let board = fetch_board()?;
        let action = plan(&board, &history, &limits, start.elapsed());
        if let Action::Stop(reason) = &action {
            report(&path, &format!("\nstopped: {reason}\n"));
            break;
        }
        history.iterations += 1;
        let line = match do_iteration(&action, &mut history.attempted, base.as_deref(), &mut dev).await {
            Ok((issue, model, verdict, pr, summary, reviewed)) => {
                if !verdict.starts_with("FAIL") {
                    history.consecutive_failures = 0;
                } else {
                    history.consecutive_failures += 1;
                }
                let model = reviewed.as_ref().map_or(model.as_str(), |r| r.model.as_str());
                let mut line = entry(history.iterations, &issue, model, &verdict, &pr, &summary);
                if let Some(r) = &reviewed {
                    line.push_str(&self_review(&dir, &line, &verdict, &pr, r, history.attempted.last().copied()));
                }
                line
            }
            Err((issue, e)) => {
                history.consecutive_failures += 1;
                entry(history.iterations, &issue, "(not run)", "ERROR", "-", &one_line(&e.to_string()))
            }
        };
        report(&path, &line);
    }
    if dev_mode {
        if let Err(e) = update_standing_pr(&dir, &root, &dev, &path).await {
            report(&path, &format!("- standing dev → main PR: !!! could not update: {}\n", one_line(&e.to_string())));
        }
    }
    println!("loop report: {}", path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nihongo_skill_present_and_absent() {
        let d = std::env::temp_dir().join(format!("actmaton-nihongo-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        assert!(load_nihongo(&d).is_none());
        std::fs::write(d.join("SKILL.md"), "skill-text").unwrap();
        std::fs::write(d.join("ALWAYS.md"), "always-text").unwrap();
        let s = load_nihongo(&d).unwrap();
        assert!(s.contains("skill-text") && s.contains("always-text"));
        std::fs::remove_dir_all(&d).ok();
    }

    fn issue(n: u64, ts: &str, labels: &[&str]) -> BoardIssue {
        BoardIssue {
            number: n,
            created_at: ts.into(),
            labels: labels.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn lim() -> Limits {
        Limits { max_iterations: 3, max_failures: 2, budget: Duration::from_secs(100), base: None }
    }

    #[test]
    fn repair_candidates_filter() {
        let pr = |n, b: &str, m: &str| OpenPr {
            number: n,
            branch: b.into(),
            base: "main".into(),
            mergeable: m.into(),
            draft: false,
            labels: Vec::new(),
        };
        let prs = vec![
            pr(1, "actmaton/issue-1", "CONFLICTING"),
            pr(2, "actmaton/issue-2", "MERGEABLE"),
            pr(3, "someone/feature", "CONFLICTING"),
            pr(4, "actmaton/issue-4", "CONFLICTING"),
            pr(5, "actmaton/issue-5", "UNKNOWN"),
        ];
        let nums = |r: Vec<&OpenPr>| r.iter().map(|p| p.number).collect::<Vec<_>>();
        assert_eq!(nums(repair_candidates(&prs, &[])), vec![1, 4]);
        assert_eq!(nums(repair_candidates(&prs, &[1])), vec![4]);
    }

    #[test]
    fn human_candidates_are_open_non_draft_dev_prs_of_others() {
        let pr = |n, b: &str, base: &str, draft, labels: &[&str]| OpenPr {
            number: n,
            branch: b.into(),
            base: base.into(),
            mergeable: "MERGEABLE".into(),
            draft,
            labels: labels.iter().map(|s| s.to_string()).collect(),
        };
        let prs = vec![
            pr(1, "alice/feat", "dev", false, &[]),
            pr(2, "actmaton/issue-2", "dev", false, &[]),
            pr(3, "bob/feat", "main", false, &[]),
            pr(4, "carol/wip", "dev", true, &[]),
            pr(5, "dan/feat", "dev", false, &["needs-human"]),
        ];
        let nums: Vec<u64> = human_candidates(&prs).iter().map(|p| p.number).collect();
        assert_eq!(nums, vec![1]);
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
    fn sanitizes_pr_body_and_truncates_cleanly() {
        let body = "Done.\nNothing is committed.\nChanged x.\nLeft uncommitted in worktree.";
        assert_eq!(sanitize_pr_body(body), "Done.\nChanged x.");
        assert_eq!(truncate_at_word("short one", 200), "short one");
        let long = "word ".repeat(100);
        let t = truncate_at_word(&long, 22);
        assert_eq!(t, "word word word word…");
        assert!(one_line(&long).ends_with("word…"));
    }

    #[test]
    fn parses_args_and_url() {
        let a: Vec<String> = ["--max-iterations", "2", "--budget-minutes", "10"].map(String::from).to_vec();
        let l = parse_args(&a).unwrap();
        assert_eq!((l.max_iterations, l.max_failures, l.budget.as_secs()), (2, 2, 600));
        assert!(parse_args(&["--bogus".to_string()]).is_err());
        let b: Vec<String> = ["--base", "dev"].map(String::from).to_vec();
        assert_eq!(parse_args(&b).unwrap().base.as_deref(), Some("dev"));
        assert!(parse_args(&["--base".to_string()]).is_err());
        assert_eq!(issue_number_from_url("https://github.com/o/r/issues/42\n"), Some(42));
    }
}
