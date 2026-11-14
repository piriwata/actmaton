//! `actmaton run <issue>` — the M0 minimal agent loop.
//!
//! Manual trigger, single issue, this repository only:
//! read a GitHub Issue with `gh`, create an isolated git worktree on a
//! task branch, let a Copilot SDK session (working directory = worktree)
//! implement it, then run `cargo test` in the worktree and print a
//! summary. Nothing is pushed and no PR is opened — that stays a
//! separate, human-approved step.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use github_copilot_sdk::handler::{PermissionHandler, PermissionResult};
use github_copilot_sdk::{
    Client, ClientOptions, MessageOptions, PermissionRequestData, PermissionRequestKind,
    RequestId, SessionConfig, SessionEvent, SessionId,
};
use serde_json::Value;

const REPO: &str = "piriwata/actmaton";
const SESSION_TIMEOUT: Duration = Duration::from_secs(1200);

struct Issue {
    number: u64,
    title: String,
    body: String,
    url: String,
    comments: Vec<Comment>,
}

struct Comment {
    author: String,
    body: String,
}

/// Follow-up sessions allowed after a failing post-run `cargo test`.
const MAX_REPAIR_ATTEMPTS: u32 = 2;
const MAX_TEST_OUTPUT_CHARS: usize = 6000;
const MAX_COMMENTS: usize = 20;
const MAX_COMMENT_CHARS: usize = 2000;

/// Chronological comments section (empty string when there are none),
/// bounded to the last `MAX_COMMENTS` comments of `MAX_COMMENT_CHARS` each.
fn comments_section(comments: &[Comment]) -> String {
    if comments.is_empty() {
        return String::new();
    }
    let skipped = comments.len().saturating_sub(MAX_COMMENTS);
    let mut s = String::from(
        "## Issue comments (chronological)\n\n\
         These comments are part of the task input. Where a comment conflicts with the \
         issue body above, the comment wins (it is later, human steering).\n\n",
    );
    if skipped > 0 {
        s.push_str(&format!(
            "[Note: {skipped} older comment(s) omitted; showing the last {MAX_COMMENTS}.]\n\n"
        ));
    }
    for c in &comments[skipped..] {
        let text = c.body.trim();
        let truncated = text.chars().count() > MAX_COMMENT_CHARS;
        let text: String = text.chars().take(MAX_COMMENT_CHARS).collect();
        s.push_str(&format!("### @{}\n{}", c.author, text));
        if truncated {
            s.push_str(&format!("\n[comment truncated to {MAX_COMMENT_CHARS} characters]"));
        }
        s.push_str("\n\n");
    }
    s
}

/// Entry point for `actmaton run <issue-number-or-url>`.
pub async fn run(issue_ref: &str, dry_run: bool) -> Result<(), Box<dyn std::error::Error>> {
    let number = parse_issue_ref(issue_ref)?;
    let repo_root = repo_root()?;
    println!("repo root: {}", repo_root.display());

    let issue = fetch_issue(number)?;
    println!("issue #{}: {}", issue.number, issue.title);
    println!("         {}", issue.url);

    if dry_run {
        println!("\n== dry run: prompt ==\n{}", build_prompt(&issue));
        println!("\n== dry run: permission policy ==\n{}", permission_policy_summary());
        println!("\ndry run: no worktree or Copilot session created, no message sent.");
        return Ok(());
    }

    let log: RunLog = Arc::new(Mutex::new(Vec::new()));
    log_line(&log, format!("issue #{}: {} ({})", issue.number, issue.title, issue.url));

    let (branch, worktree) = setup_worktree(&repo_root, number)?;
    println!("branch:   {branch}");
    println!("worktree: {}", worktree.display());
    log_line(&log, format!("worktree created: {} (branch {branch})", worktree.display()));

    let mut final_message = run_session(&issue, build_prompt(&issue), &worktree, &log).await?;

    // Post-session verification, run by us (not the agent).
    log_line(&log, "running cargo test in worktree".to_string());
    let (mut tests_ok, mut test_tail) = cargo_test(&worktree);
    let mut attempts = 0;
    while let RepairStep::Repair(attempt) = repair_decision(tests_ok, attempts) {
        attempts = attempt;
        println!("cargo test failed; repair attempt {attempt}/{MAX_REPAIR_ATTEMPTS}");
        log_line(&log, format!("repair attempt {attempt}/{MAX_REPAIR_ATTEMPTS}: starting follow-up session"));
        let prompt = build_repair_prompt(&issue, attempt, &test_tail);
        let reply = run_session(&issue, prompt, &worktree, &log).await?;
        let summary = one_line_summary(reply.as_deref());
        if reply.is_some() {
            final_message = reply;
        }
        (tests_ok, test_tail) = cargo_test(&worktree);
        log_line(
            &log,
            format!(
                "repair attempt {attempt}/{MAX_REPAIR_ATTEMPTS}: cargo test {}; session summary: {summary}",
                if tests_ok { "PASS" } else { "FAIL" }
            ),
        );
    }
    if !tests_ok {
        log_line(&log, format!("tests still failing after {attempts} repair attempt(s); keeping worktree"));
    }
    let status = git(&worktree, &["status", "--porcelain"])?;
    let diff_stat = git(&worktree, &["diff", "--stat", "HEAD"])?;

    let log_path = write_run_log(
        &repo_root,
        &issue,
        &log,
        final_message.as_deref(),
        &diff_stat,
        tests_ok,
        &test_tail,
    )?;

    println!("\n================ run summary ================");
    println!("final message:");
    println!("{}", final_message.as_deref().unwrap_or("(no final message)"));
    println!("run log:  {}", log_path.display());
    println!("issue:    #{} {}", issue.number, issue.title);
    println!("branch:   {branch}");
    println!("worktree: {}", worktree.display());
    println!("changed files (git status --porcelain):");
    println!("{}", if status.trim().is_empty() { "  (none)" } else { &status });
    println!("diff --stat vs HEAD:");
    println!("{}", if diff_stat.trim().is_empty() { "  (none)" } else { &diff_stat });
    println!("cargo test: {}", if tests_ok { "PASS" } else { "FAIL" });
    println!("--- cargo test output (tail) ---\n{test_tail}");
    println!("next step: review the worktree, then commit/push/open a PR manually.");
    println!("=============================================");

    if !tests_ok {
        std::process::exit(1);
    }
    Ok(())
}

/// Accept `3`, `#3`, or a full issue URL (`.../issues/3`).
fn parse_issue_ref(s: &str) -> Result<u64, Box<dyn std::error::Error>> {
    let trimmed = s.trim().trim_end_matches('/');
    let last = trimmed.rsplit('/').next().unwrap_or(trimmed);
    let digits: String = last
        .trim_start_matches('#')
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits
        .parse::<u64>()
        .map_err(|_| format!("cannot parse an issue number from `{s}`").into())
}

pub(crate) fn repo_root() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let out = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()?;
    if !out.status.success() {
        return Err("not inside a git repository (run actmaton from the local clone)".into());
    }
    Ok(PathBuf::from(String::from_utf8(out.stdout)?.trim()))
}

fn fetch_issue(number: u64) -> Result<Issue, Box<dyn std::error::Error>> {
    let out = Command::new("gh")
        .args([
            "issue",
            "view",
            &number.to_string(),
            "--repo",
            REPO,
            "--json",
            "number,title,body,url,state,comments",
        ])
        .output()?;
    if !out.status.success() {
        return Err(format!(
            "gh issue view {number} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )
        .into());
    }
    let v: Value = serde_json::from_slice(&out.stdout)?;
    if v["state"].as_str() != Some("OPEN") {
        return Err(format!("issue #{number} is not OPEN (state: {})", v["state"]).into());
    }
    Ok(Issue {
        number: v["number"].as_u64().unwrap_or(number),
        title: v["title"].as_str().unwrap_or_default().to_string(),
        body: v["body"].as_str().unwrap_or_default().to_string(),
        url: v["url"].as_str().unwrap_or_default().to_string(),
        comments: v["comments"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|c| Comment {
                        author: c["author"]["login"].as_str().unwrap_or("unknown").to_string(),
                        body: c["body"].as_str().unwrap_or_default().to_string(),
                    })
                    .collect()
            })
            .unwrap_or_default(),
    })
}

/// Create branch `actmaton/issue-<n>` and a sibling worktree directory
/// based on the clone's current HEAD. Fails if either already exists, so
/// a stale task is never silently reused.
fn setup_worktree(
    repo_root: &Path,
    number: u64,
) -> Result<(String, PathBuf), Box<dyn std::error::Error>> {
    let branch = format!("actmaton/issue-{number}");
    let worktree = repo_root
        .parent()
        .ok_or("repo root has no parent directory")?
        .join(format!("actmaton-issue-{number}"));
    let branch_exists = git(
        repo_root,
        &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")],
    )
    .is_ok();
    if worktree.exists() || branch_exists {
        let dirty = if worktree.exists() {
            !git(&worktree, &["status", "--porcelain"])?.is_empty()
        } else {
            false
        };
        let ahead: u64 = if branch_exists {
            git(repo_root, &["rev-list", "--count", &format!("HEAD..{branch}")])?.parse()?
        } else {
            0
        };
        match leftover_decision(dirty, ahead) {
            Leftover::Recreate => {
                println!("removing clean leftover worktree/branch for issue {number}");
                if worktree.exists() {
                    git(
                        repo_root,
                        &[
                            "worktree",
                            "remove",
                            worktree.to_str().ok_or("worktree path is not UTF-8")?,
                        ],
                    )?;
                }
                git(repo_root, &["worktree", "prune"])?;
                if branch_exists {
                    git(repo_root, &["branch", "-D", &branch])?;
                }
            }
            Leftover::Refuse(reason) => {
                return Err(format!(
                    "leftover state from a previous attempt: worktree {} / branch {branch} {reason}. \
                     Nothing was deleted. Review it, then commit/push the work or remove it manually \
                     (`git worktree remove --force {}` and `git branch -D {branch}`) and re-run.",
                    worktree.display(),
                    worktree.display()
                )
                .into());
            }
        }
    }
    let out = Command::new("git")
        .args([
            "worktree",
            "add",
            "-b",
            &branch,
            worktree.to_str().ok_or("worktree path is not UTF-8")?,
            "HEAD",
        ])
        .current_dir(repo_root)
        .output()?;
    if !out.status.success() {
        return Err(format!(
            "git worktree add failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )
        .into());
    }
    Ok((branch, worktree))
}

#[derive(Debug, PartialEq)]
enum Leftover {
    Recreate,
    Refuse(String),
}

/// Decide what to do with a leftover worktree/branch: only a clean one with
/// no commits beyond the base may be discarded.
fn leftover_decision(dirty: bool, ahead: u64) -> Leftover {
    match (dirty, ahead) {
        (false, 0) => Leftover::Recreate,
        (true, 0) => Leftover::Refuse("has uncommitted changes".into()),
        (false, n) => Leftover::Refuse(format!("has {n} commit(s) beyond the base")),
        (true, n) => Leftover::Refuse(format!(
            "has uncommitted changes and {n} commit(s) beyond the base"
        )),
    }
}

type RunLog = Arc<Mutex<Vec<String>>>;

fn log_line(log: &RunLog, line: String) {
    if let Ok(mut l) = log.lock() {
        l.push(line);
    }
}

/// Plain-text content of an assistant message event, if it has any.
pub fn final_message_text(event: &SessionEvent) -> Option<String> {
    event
        .data
        .get("content")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Write the run log to `<repo-parent>/actmaton-runs/issue-<n>-<unix-ts>.log`.
fn write_run_log(
    repo_root: &Path,
    issue: &Issue,
    log: &RunLog,
    final_message: Option<&str>,
    diff_stat: &str,
    tests_ok: bool,
    test_tail: &str,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let dir = repo_root
        .parent()
        .ok_or("repo root has no parent directory")?
        .join("actmaton-runs");
    std::fs::create_dir_all(&dir)?;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let path = dir.join(format!("issue-{}-{ts}.log", issue.number));
    let steps = log.lock().map(|l| l.join("\n")).unwrap_or_default();
    let text = format!(
        "issue #{} {}\n\n== steps and permission decisions ==\n{steps}\n\n\
         == final message ==\n{}\n\n== diff --stat vs HEAD ==\n{diff_stat}\n\n\
         == cargo test: {} ==\n{test_tail}\n",
        issue.number,
        issue.title,
        final_message.unwrap_or("(no final message)"),
        if tests_ok { "PASS" } else { "FAIL" },
    );
    std::fs::write(&path, text)?;
    Ok(path)
}

/// The exact prompt sent to the agent (shared by the real run and `--dry-run`).
fn build_prompt(issue: &Issue) -> String {
    format!(
        "You are implementing a GitHub issue in an isolated git worktree of the \
         actmaton repository. Your current working directory is the worktree root.\n\n\
         Issue #{number}: {title}\n\n{body}\n\n{comments}\
         Rules:\n\
         - Make all changes inside this worktree only. Do not read or write paths outside it.\n\
         - Do NOT run git commit, git push, or gh commands. Leave changes uncommitted.\n\
         - Keep the change minimal and focused on the issue's acceptance criteria.\n\
         - After implementing, run `cargo test` and make sure it passes before finishing.\n\n\
         Surgical-change rules:\n\
         - Prefer deletion over addition: if the task can be met by removing or simplifying code, do that.\n\
         - Before writing new code, check whether the code (or a helper) already exists and reuse it.\n\
         - Touch only what the task requires; every changed line must trace to the issue.\n\
         - Remove only the orphans (unused imports/functions) your own change created.\n\
         - Do not clean up, refactor, or reformat anything else; cleanup beyond the task's own \
         orphans belongs in its own issue, not in this diff.\n\n\
         How to edit (important):\n\
         - Edit files ONLY with the write/edit tools, in small steps (one file or one function at a time).\n\
         - NEVER modify files through shell commands: no heredocs, no sed/awk/perl/python \
         scripts that rewrite files, no echo/cat redirection into files. Shell edits like that \
         are denied by the permission policy.\n\
         - Use shell only for `cargo build` / `cargo test` and small read-only inspections \
         (git status/diff/log, ls, grep).\n\
         - If a shell command is denied, do not retry it in another form and do not give up: \
         switch to the write/edit tools and continue.\n\
         - If you truly cannot complete the task, say exactly what blocked you in your \
         final message instead of finishing silently.",
        number = issue.number,
        title = issue.title,
        body = issue.body,
        comments = comments_section(&issue.comments),
    )
}

/// Human-readable description of `scoped_permission`'s policy.
fn permission_policy_summary() -> &'static str {
    "- read/write: auto-approved only when the target path is inside the task worktree\n\
     - shell: auto-approved unless the command escapes the worktree \
     (absolute paths outside it, `..` traversal, `git push`) or edits files \
     through the shell (heredocs, redirection to files, `sed -i`, `tee`, `dd of=`); \
     denials carry feedback telling the session what to do instead\n\
     - everything else (url, mcp, custom tools, memory, hooks): denied"
}

async fn run_session(
    issue: &Issue,
    prompt: String,
    worktree: &Path,
    log: &RunLog,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let root = worktree
        .canonicalize()
        .unwrap_or_else(|_| worktree.to_path_buf());

    let perm_log = log.clone();
    let client = Client::start(ClientOptions::default()).await?;
    let session = client
        .create_session(
            SessionConfig::default()
                .with_working_directory(worktree)
                .with_permission_handler(Arc::new(ScopedPermissionHandler {
                    root,
                    log: perm_log,
                })),
        )
        .await?;

    println!("copilot session started; sending task for issue #{}", issue.number);
    log_line(log, "copilot session started; task sent".to_string());
    let reply = session
        .send_and_wait(MessageOptions::new(prompt).with_wait_timeout(SESSION_TIMEOUT))
        .await?;
    log_line(log, "session finished".to_string());

    session.disconnect().await?;
    client.stop().await.ok();
    Ok(reply.as_ref().and_then(final_message_text))
}

// ---------------------------------------------------------------------------
// Permission scoping
//
// The SDK lets a session approve permission requests via a predicate on
// PermissionRequestData (kind + raw request payload). We scope to the
// task worktree:
//   - write/read: approved only when the target path is inside the worktree
//   - shell: approved unless the command text obviously escapes the
//     worktree (absolute paths outside it, `..` traversal, `git push`)
//   - everything else (url, mcp, custom tools, memory, hooks): denied
// Every decision is logged to stderr for audit. Shell scoping is
// necessarily heuristic — see the M0 notes.
// ---------------------------------------------------------------------------

/// A permission decision. Denials carry feedback that is forwarded to the
/// model (the SDK's reject feedback), so the session can recover — e.g.
/// switch from a denied shell edit to the write tools — instead of giving
/// up silently. This is a provisional guard: the real fix is OS-level
/// sandboxing of tool execution; until that is available, deterministic
/// detection + instructive denials keep runs from dying on the known
/// failure mode (one giant shell heredoc that rewrites files).
#[derive(Debug, PartialEq)]
enum Decision {
    Approve,
    Deny(Option<String>),
}

const FEEDBACK_SHELL_EDIT: &str = "Denied by actmaton's permission policy: files must not be created or modified through shell commands (no heredocs, output redirection to files, sed -i, tee, or dd of=). Use the write/edit tools to change files, in small steps; use shell only for cargo build/test and read-only inspection.";
const FEEDBACK_OUTSIDE_WORKTREE: &str = "Denied by actmaton's permission policy: that path is outside the task worktree. All reads and writes must stay inside the worktree.";
const FEEDBACK_GIT_PUSH: &str =
    "Denied by actmaton's permission policy: git push is not allowed during a run.";
const FEEDBACK_ESCAPE: &str = "Denied by actmaton's permission policy: the command appears to escape the task worktree (absolute path outside it or `..` traversal). Work only inside the worktree.";
const FEEDBACK_KIND: &str =
    "Denied by actmaton's permission policy: this permission kind is not allowed during a run.";
const FEEDBACK_NO_PATH: &str =
    "Denied by actmaton's permission policy: the request carries no target path to scope.";

struct ScopedPermissionHandler {
    root: PathBuf,
    log: RunLog,
}

#[async_trait::async_trait]
impl PermissionHandler for ScopedPermissionHandler {
    async fn handle(
        &self,
        _session_id: SessionId,
        _request_id: RequestId,
        data: PermissionRequestData,
    ) -> PermissionResult {
        let (decision, line) = scoped_permission(&self.root, &data);
        log_line(&self.log, line);
        match decision {
            Decision::Approve => PermissionResult::approve_once(),
            Decision::Deny(feedback) => PermissionResult::reject(feedback),
        }
    }
}

fn scoped_permission(root: &Path, data: &PermissionRequestData) -> (Decision, String) {
    let payload = data
        .extra
        .get("permissionRequest")
        .cloned()
        .unwrap_or_else(|| data.extra.clone());
    let (decision, detail) = match data.kind {
        Some(PermissionRequestKind::Write) | Some(PermissionRequestKind::Read) => {
            match payload_target_path(&payload) {
                Some(p) => {
                    if path_within(root, Path::new(p)) {
                        (Decision::Approve, format!("path={p}"))
                    } else {
                        (
                            Decision::Deny(Some(FEEDBACK_OUTSIDE_WORKTREE.to_string())),
                            format!("path={p}"),
                        )
                    }
                }
                None => (
                    Decision::Deny(Some(FEEDBACK_NO_PATH.to_string())),
                    format!("no path in payload: {payload}"),
                ),
            }
        }
        Some(PermissionRequestKind::Shell) => {
            let cmd = payload
                .get("fullCommandText")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| match payload.get("command") {
                    Some(Value::String(s)) => Some(s.clone()),
                    Some(Value::Array(a)) => Some(
                        a.iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(" "),
                    ),
                    _ => None,
                })
                .unwrap_or_default();
            (shell_decision(root, &cmd), format!("cmd={cmd}"))
        }
        other => (
            Decision::Deny(Some(FEEDBACK_KIND.to_string())),
            format!("kind={other:?}"),
        ),
    };
    let allowed = decision == Decision::Approve;
    let line = format!(
        "[permission] {} kind={:?} {detail}",
        if allowed { "APPROVE" } else { "DENY" },
        data.kind
    );
    eprintln!("{line}");
    (decision, line)
}

/// Decide a shell command. Order matters: the file-editing check comes
/// first so its denial carries the actionable feedback (use the write
/// tools), then the hard bans, then the worktree-escape heuristics.
fn shell_decision(root: &Path, cmd: &str) -> Decision {
    if shell_edit_attempt(cmd) {
        return Decision::Deny(Some(FEEDBACK_SHELL_EDIT.to_string()));
    }
    if cmd.contains("git push") {
        return Decision::Deny(Some(FEEDBACK_GIT_PUSH.to_string()));
    }
    if shell_allowed(root, cmd) {
        Decision::Approve
    } else {
        Decision::Deny(Some(FEEDBACK_ESCAPE.to_string()))
    }
}

/// Deterministic detection of shell commands that create or modify files:
/// heredocs, output redirection to a file (other than /dev/null or an fd),
/// `sed -i`, `tee`, and `dd of=`. These are exactly the shapes sessions
/// reach for when they try to apply a whole implementation as one giant
/// shell command — the recurring run failure mode. Read-only uses
/// (`2>&1`, `>/dev/null`, `sed -n ...p`) stay allowed.
fn shell_edit_attempt(cmd: &str) -> bool {
    if cmd.contains("<<") {
        return true; // heredoc or herestring
    }
    // Output redirection: find '>' (outside quotes) and inspect its target.
    let bytes = cmd.as_bytes();
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' if !in_double => in_single = !in_single,
            b'"' if !in_single => in_double = !in_double,
            _ => {}
        }
        if bytes[i] == b'>' && !in_single && !in_double {
            let mut j = i + 1;
            if j < bytes.len() && bytes[j] == b'>' {
                j += 1; // '>>'
            }
            while j < bytes.len() && (bytes[j] as char).is_whitespace() {
                j += 1;
            }
            let start = j;
            while j < bytes.len() && !(bytes[j] as char).is_whitespace() {
                j += 1;
            }
            let target = &cmd[start..j];
            if !target.is_empty() && !target.starts_with('&') && target != "/dev/null" {
                return true;
            }
            i = j.max(i + 1);
        } else {
            i += 1;
        }
    }
    let tokens: Vec<&str> = cmd.split_whitespace().collect();
    let has_word = |w: &str| {
        tokens
            .iter()
            .any(|t| t.trim_matches(|c| matches!(c, ';' | '|' | '&' | '(' | ')')) == w)
    };
    if has_word("tee") || has_word("truncate") {
        return true;
    }
    if has_word("sed")
        && tokens
            .iter()
            .any(|t| *t == "-i" || t.starts_with("-i") || *t == "--in-place")
    {
        return true;
    }
    if has_word("dd") && tokens.iter().any(|t| t.starts_with("of=")) {
        return true;
    }
    false
}

/// Is `candidate` (possibly relative to `root`) inside `root`?
/// Extract the target file path from a read/write permission payload.
/// Read requests carry it as `path`; write requests from the Copilot SDK
/// carry it as `fileName`. Both go through the same worktree scoping.
pub(crate) fn payload_target_path(payload: &Value) -> Option<&str> {
    payload
        .get("path")
        .and_then(Value::as_str)
        .or_else(|| payload.get("fileName").and_then(Value::as_str))
}

pub(crate) fn path_within(root: &Path, candidate: &Path) -> bool {
    let abs = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        root.join(candidate)
    };
    canonicalize_lenient(&abs).starts_with(root)
}

/// Canonicalize as much of the path as exists (the leaf of a write target
/// usually does not exist yet).
fn canonicalize_lenient(p: &Path) -> PathBuf {
    if let Ok(c) = p.canonicalize() {
        return c;
    }
    if let (Some(parent), Some(name)) = (p.parent(), p.file_name()) {
        if let Ok(pc) = parent.canonicalize() {
            return pc.join(name);
        }
    }
    p.to_path_buf()
}

fn shell_allowed(root: &Path, cmd: &str) -> bool {
    if cmd.contains("git push") {
        return false;
    }
    for raw in cmd.split_whitespace() {
        let token = raw.trim_matches(|c| matches!(c, '"' | '\'' | ';' | '|' | '&' | '(' | ')'));
        let token = token.trim_start_matches(|c: char| c.is_ascii_digit() || c == '>');
        if token == ".." || token.starts_with("../") {
            return false;
        }
        if let Some(rest) = token.strip_prefix('/') {
            if rest.is_empty() {
                return false; // bare "/"
            }
            let p = Path::new(token);
            if path_within(root, p)
                || token.starts_with("/usr/")
                || token.starts_with("/bin/")
                || token.starts_with("/sbin/")
                || token.starts_with("/dev/")
            {
                continue;
            }
            return false;
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Small process helpers
// ---------------------------------------------------------------------------

fn git(dir: &Path, args: &[&str]) -> Result<String, Box<dyn std::error::Error>> {
    let out = Command::new("git").args(args).current_dir(dir).output()?;
    if !out.status.success() {
        return Err(format!(
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )
        .into());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

/// Last `max` characters of `text`, on a line boundary, with a truncation marker.
fn tail_chars(text: &str, max: usize) -> String {
    let total = text.chars().count();
    if total <= max {
        return text.to_string();
    }
    let tail: String = text.chars().skip(total - max).collect();
    let tail = tail.split_once('\n').map_or(tail.as_str(), |(_, rest)| rest);
    format!("[output truncated to the last part]\n{tail}")
}

/// One-line summary of a session's final message (its last non-empty line).
fn one_line_summary(message: Option<&str>) -> String {
    let line = message
        .and_then(|m| m.lines().map(str::trim).rev().find(|l| !l.is_empty()))
        .unwrap_or("(no final message)");
    let mut s: String = line.chars().take(200).collect();
    if line.chars().count() > 200 {
        s.push('…');
    }
    s
}

/// What to do after a test run, given how many repair attempts were already made.
#[derive(Debug, PartialEq)]
enum RepairStep {
    Done,
    Repair(u32),
    GiveUp,
}

fn repair_decision(tests_ok: bool, attempts_made: u32) -> RepairStep {
    if tests_ok {
        RepairStep::Done
    } else if attempts_made < MAX_REPAIR_ATTEMPTS {
        RepairStep::Repair(attempts_made + 1)
    } else {
        RepairStep::GiveUp
    }
}

fn build_repair_prompt(issue: &Issue, attempt: u32, test_output: &str) -> String {
    format!(
        "You are continuing work on issue #{number} ({title}) in the same git worktree \
         (your working directory). Your implementation was verified with `cargo test` and it \
         FAILED. This is repair attempt {attempt} of {max}.\n\n\
         Failing `cargo test` output:\n```\n{test_output}\n```\n\n\
         Fix these failures without expanding scope: change only what is needed to make the \
         tests pass, do not refactor or clean up unrelated code, and do not rewrite or delete \
         tests just to make them pass unless the test itself is wrong. The same rules as before \
         apply: stay inside the worktree, no git commit/push/gh, edit files only with the \
         write/edit tools (never via shell), and use shell only for cargo build/test and \
         read-only inspection. Run `cargo test` before finishing, and end with a one-line \
         summary of what you changed.",
        number = issue.number,
        title = issue.title,
        max = MAX_REPAIR_ATTEMPTS,
    )
}

fn cargo_test(dir: &Path) -> (bool, String) {
    match Command::new("cargo").arg("test").current_dir(dir).output() {
        Ok(out) => {
            let mut text = String::from_utf8_lossy(&out.stderr).into_owned();
            text.push_str(&String::from_utf8_lossy(&out.stdout));
            (out.status.success(), tail_chars(&text, MAX_TEST_OUTPUT_CHARS))
        }
        Err(e) => (false, format!("failed to spawn cargo: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repair_decision_stops_on_pass() {
        assert_eq!(repair_decision(true, 0), RepairStep::Done);
        assert_eq!(repair_decision(true, MAX_REPAIR_ATTEMPTS), RepairStep::Done);
    }

    #[test]
    fn repair_decision_continues_on_fail_until_limit() {
        assert_eq!(repair_decision(false, 0), RepairStep::Repair(1));
        assert_eq!(
            repair_decision(false, MAX_REPAIR_ATTEMPTS - 1),
            RepairStep::Repair(MAX_REPAIR_ATTEMPTS)
        );
        assert_eq!(repair_decision(false, MAX_REPAIR_ATTEMPTS), RepairStep::GiveUp);
    }

    #[test]
    fn tail_and_summary_helpers() {
        assert_eq!(tail_chars("a\nb", 10), "a\nb");
        assert!(tail_chars("aaaa\nbbbb\ncccc", 6).ends_with("cccc"));
        assert_eq!(one_line_summary(Some("x\nfixed it\n\n")), "fixed it");
        assert_eq!(one_line_summary(None), "(no final message)");
    }

    #[test]
    fn shell_edit_attempt_detects_file_writes() {
        // The recurring failure mode: implementation-as-heredoc.
        assert!(shell_edit_attempt("python3 - <<'EOF'\nprint(1)\nEOF"));
        assert!(shell_edit_attempt("cat > src/main.rs <<'EOF'\nx\nEOF"));
        assert!(shell_edit_attempt("echo hello > out.txt"));
        assert!(shell_edit_attempt("echo hello >> out.txt"));
        assert!(shell_edit_attempt("cargo test 2>test.log"));
        assert!(shell_edit_attempt("sed -i 's/a/b/' src/lib.rs"));
        assert!(shell_edit_attempt("sed --in-place 's/a/b/' src/lib.rs"));
        assert!(shell_edit_attempt("ls | tee files.txt"));
        assert!(shell_edit_attempt("dd if=/dev/zero of=out.bin"));
        // Read-only shapes stay allowed.
        assert!(!shell_edit_attempt("cargo test"));
        assert!(!shell_edit_attempt("cargo test 2>&1 | head -20"));
        assert!(!shell_edit_attempt("echo warn >/dev/null"));
        assert!(!shell_edit_attempt("grep -n 'a>b' src/lib.rs"));
        assert!(!shell_edit_attempt("sed -n '1,5p' src/lib.rs"));
        assert!(!shell_edit_attempt("git diff HEAD~1"));
    }

    #[test]
    fn shell_decision_gives_actionable_feedback() {
        let root = Path::new("/tmp/worktree");
        match shell_decision(root, "python3 - <<'EOF'\nx\nEOF") {
            Decision::Deny(Some(f)) => assert!(f.contains("write/edit tools")),
            other => panic!("expected shell-edit denial, got {other:?}"),
        }
        match shell_decision(root, "git push origin main") {
            Decision::Deny(Some(f)) => assert!(f.contains("git push")),
            other => panic!("expected git-push denial, got {other:?}"),
        }
        assert_eq!(shell_decision(root, "cargo test"), Decision::Approve);
    }

    #[test]
    fn leftover_decision_recreates_only_clean() {
        assert_eq!(leftover_decision(false, 0), Leftover::Recreate);
        assert!(matches!(leftover_decision(true, 0), Leftover::Refuse(_)));
        assert!(matches!(leftover_decision(false, 2), Leftover::Refuse(_)));
        assert!(matches!(leftover_decision(true, 2), Leftover::Refuse(_)));
    }

    #[test]
    fn prompt_and_policy_summary() {
        let issue = Issue {
            number: 7,
            title: "Do thing".into(),
            body: "Body text".into(),
            url: "u".into(),
            comments: vec![],
        };
        let p = build_prompt(&issue);
        assert!(!p.contains("Issue comments"));
        assert!(p.contains("Issue #7: Do thing\n\nBody text"));
        assert!(p.contains("Do NOT run git commit"));
        let s = permission_policy_summary();
        assert!(s.contains("read/write") && s.contains("shell") && s.contains("denied"));
    }

    #[test]
    fn prompt_includes_and_bounds_comments() {
        let mut comments: Vec<Comment> = (0..25)
            .map(|i| Comment { author: "bob".into(), body: format!("c{i}x") })
            .collect();
        comments.push(Comment { author: "alice".into(), body: "y".repeat(5000) });
        let issue = Issue {
            number: 1,
            title: "t".into(),
            body: "b".into(),
            url: "u".into(),
            comments,
        };
        let p = build_prompt(&issue);
        assert!(p.contains("## Issue comments (chronological)"));
        assert!(p.contains("the comment wins"));
        assert!(p.contains("6 older comment(s) omitted"));
        assert!(!p.contains("c5x") && p.contains("c6x"));
        assert!(p.contains("### @alice") && p.contains("[comment truncated"));
        assert!(!p.contains(&"y".repeat(MAX_COMMENT_CHARS + 1)));
        assert!(p.find("Issue comments").unwrap() < p.find("Rules:").unwrap());
    }

    #[test]
    fn parses_issue_refs() {
        assert_eq!(parse_issue_ref("3").unwrap(), 3);
        assert_eq!(parse_issue_ref("#3").unwrap(), 3);
        assert_eq!(
            parse_issue_ref("https://github.com/piriwata/actmaton/issues/3").unwrap(),
            3
        );
        assert!(parse_issue_ref("nope").is_err());
    }

    #[test]
    fn final_message_text_extracts_content() {
        let event: SessionEvent = serde_json::from_value(serde_json::json!({
            "id": "1",
            "timestamp": "2026-01-01T00:00:00Z",
            "parentId": null,
            "type": "assistant.message",
            "data": {"content": "All done."}
        }))
        .unwrap();
        assert_eq!(final_message_text(&event).as_deref(), Some("All done."));
        let other: SessionEvent = serde_json::from_value(serde_json::json!({
            "id": "2", "timestamp": "t", "parentId": null,
            "type": "session.idle", "data": {}
        }))
        .unwrap();
        assert_eq!(final_message_text(&other), None);
    }

    fn event_with_data(data: serde_json::Value) -> SessionEvent {
        serde_json::from_value(serde_json::json!({
            "id": "1", "timestamp": "t", "parentId": null,
            "type": "assistant.message", "data": data
        }))
        .unwrap()
    }

    #[test]
    fn final_message_text_returns_empty_string_for_empty_content() {
        let event = event_with_data(serde_json::json!({"content": ""}));
        assert_eq!(final_message_text(&event), Some(String::new()));
    }

    #[test]
    fn final_message_text_ignores_missing_or_non_string_content() {
        let missing = event_with_data(serde_json::json!({"other": "x"}));
        assert_eq!(final_message_text(&missing), None);
        let null = event_with_data(serde_json::json!({"content": null}));
        assert_eq!(final_message_text(&null), None);
        let number = event_with_data(serde_json::json!({"content": 5}));
        assert_eq!(final_message_text(&number), None);
    }

    #[test]
    fn final_message_text_uses_content_not_other_fields_and_keeps_text_verbatim() {
        let event = event_with_data(serde_json::json!({
            "message": "wrong",
            "content": "  line one\n\nline two  "
        }));
        assert_eq!(
            final_message_text(&event).as_deref(),
            Some("  line one\n\nline two  ")
        );
    }

    #[test]
    fn payload_target_path_accepts_path_and_file_name() {
        let read = serde_json::json!({"path": "/wt/src/main.rs"});
        assert_eq!(payload_target_path(&read), Some("/wt/src/main.rs"));
        // Write requests from the Copilot SDK use `fileName`, not `path`.
        let write = serde_json::json!({"fileName": "/wt/src/run_loop.rs"});
        assert_eq!(payload_target_path(&write), Some("/wt/src/run_loop.rs"));
        let neither = serde_json::json!({"command": "ls"});
        assert_eq!(payload_target_path(&neither), None);
    }

    #[test]
    fn shell_policy_scopes_to_worktree() {
        let root = Path::new("/repo/wt");
        assert!(shell_allowed(root, "cargo test"));
        assert!(shell_allowed(root, "cargo test 2>/dev/null"));
        assert!(!shell_allowed(root, "git push origin main"));
        assert!(!shell_allowed(root, "cat /etc/passwd > /tmp/x"));
        assert!(!shell_allowed(root, "cd .. && ls"));
    }
}
