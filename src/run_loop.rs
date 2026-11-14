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

use github_copilot_sdk::{
    Client, ClientOptions, MessageOptions, PermissionRequestData, PermissionRequestKind,
    SessionConfig, SessionEvent,
};
use serde_json::Value;

const REPO: &str = "piriwata/actmaton";
const SESSION_TIMEOUT: Duration = Duration::from_secs(1200);

struct Issue {
    number: u64,
    title: String,
    body: String,
    url: String,
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

    let final_message = run_session(&issue, &worktree, &log).await?;

    // Post-session verification, run by us (not the agent).
    let status = git(&worktree, &["status", "--porcelain"])?;
    let diff_stat = git(&worktree, &["diff", "--stat", "HEAD"])?;
    log_line(&log, "running cargo test in worktree".to_string());
    let (tests_ok, test_tail) = cargo_test(&worktree);

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
            "number,title,body,url,state",
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
         Issue #{number}: {title}\n\n{body}\n\n\
         Rules:\n\
         - Make all changes inside this worktree only. Do not read or write paths outside it.\n\
         - Do NOT run git commit, git push, or gh commands. Leave changes uncommitted.\n\
         - Keep the change minimal and focused on the issue's acceptance criteria.\n\
         - After implementing, run `cargo test` and make sure it passes before finishing.",
        number = issue.number,
        title = issue.title,
        body = issue.body,
    )
}

/// Human-readable description of `scoped_permission`'s policy.
fn permission_policy_summary() -> &'static str {
    "- read/write: auto-approved only when the target path is inside the task worktree\n\
     - shell: auto-approved unless the command escapes the worktree \
     (absolute paths outside it, `..` traversal, `git push`)\n\
     - everything else (url, mcp, custom tools, memory, hooks): denied"
}

async fn run_session(
    issue: &Issue,
    worktree: &Path,
    log: &RunLog,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let prompt = build_prompt(issue);

    let root = worktree
        .canonicalize()
        .unwrap_or_else(|_| worktree.to_path_buf());

    let perm_log = log.clone();
    let client = Client::start(ClientOptions::default()).await?;
    let session = client
        .create_session(
            SessionConfig::default()
                .with_working_directory(worktree)
                .approve_permissions_if(move |data| {
                    let (allowed, line) = scoped_permission(&root, data);
                    log_line(&perm_log, line);
                    allowed
                }),
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

fn scoped_permission(root: &Path, data: &PermissionRequestData) -> (bool, String) {
    let payload = data
        .extra
        .get("permissionRequest")
        .cloned()
        .unwrap_or_else(|| data.extra.clone());
    let (allowed, detail) = match data.kind {
        Some(PermissionRequestKind::Write) | Some(PermissionRequestKind::Read) => {
            match payload_target_path(&payload) {
                Some(p) => (path_within(root, Path::new(p)), format!("path={p}")),
                None => (false, format!("no path in payload: {payload}")),
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
            (shell_allowed(root, &cmd), format!("cmd={cmd}"))
        }
        other => (false, format!("kind={other:?}")),
    };
    let line = format!(
        "[permission] {} kind={:?} {detail}",
        if allowed { "APPROVE" } else { "DENY" },
        data.kind
    );
    eprintln!("{line}");
    (allowed, line)
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

fn cargo_test(dir: &Path) -> (bool, String) {
    match Command::new("cargo").arg("test").current_dir(dir).output() {
        Ok(out) => {
            let mut text = String::from_utf8_lossy(&out.stderr).into_owned();
            text.push_str(&String::from_utf8_lossy(&out.stdout));
            let lines: Vec<&str> = text.lines().collect();
            let tail = lines[lines.len().saturating_sub(30)..].join("\n");
            (out.status.success(), tail)
        }
        Err(e) => (false, format!("failed to spawn cargo: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        };
        let p = build_prompt(&issue);
        assert!(p.contains("Issue #7: Do thing\n\nBody text"));
        assert!(p.contains("Do NOT run git commit"));
        let s = permission_policy_summary();
        assert!(s.contains("read/write") && s.contains("shell") && s.contains("denied"));
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
