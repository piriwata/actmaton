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
use std::time::Duration;

use github_copilot_sdk::{
    Client, ClientOptions, MessageOptions, PermissionRequestData, PermissionRequestKind,
    SessionConfig,
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
pub async fn run(issue_ref: &str) -> Result<(), Box<dyn std::error::Error>> {
    let number = parse_issue_ref(issue_ref)?;
    let repo_root = repo_root()?;
    println!("repo root: {}", repo_root.display());

    let issue = fetch_issue(number)?;
    println!("issue #{}: {}", issue.number, issue.title);
    println!("         {}", issue.url);

    let (branch, worktree) = setup_worktree(&repo_root, number)?;
    println!("branch:   {branch}");
    println!("worktree: {}", worktree.display());

    run_session(&issue, &worktree).await?;

    // Post-session verification, run by us (not the agent).
    let status = git(&worktree, &["status", "--porcelain"])?;
    let diff_stat = git(&worktree, &["diff", "--stat", "HEAD"])?;
    let (tests_ok, test_tail) = cargo_test(&worktree);

    println!("\n================ run summary ================");
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

fn repo_root() -> Result<PathBuf, Box<dyn std::error::Error>> {
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
    if worktree.exists() {
        return Err(format!(
            "worktree path {} already exists; remove it (and branch {branch}) before re-running",
            worktree.display()
        )
        .into());
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

async fn run_session(issue: &Issue, worktree: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let prompt = format!(
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
    );

    let root = worktree
        .canonicalize()
        .unwrap_or_else(|_| worktree.to_path_buf());

    let client = Client::start(ClientOptions::default()).await?;
    let session = client
        .create_session(
            SessionConfig::default()
                .with_working_directory(worktree)
                .approve_permissions_if(move |data| scoped_permission(&root, data)),
        )
        .await?;

    println!("copilot session started; sending task for issue #{}", issue.number);
    let reply = session
        .send_and_wait(MessageOptions::new(prompt).with_wait_timeout(SESSION_TIMEOUT))
        .await?;
    println!("session finished. final reply: {reply:?}");

    session.disconnect().await?;
    client.stop().await.ok();
    Ok(())
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

fn scoped_permission(root: &Path, data: &PermissionRequestData) -> bool {
    let payload = data
        .extra
        .get("permissionRequest")
        .cloned()
        .unwrap_or_else(|| data.extra.clone());
    let (allowed, detail) = match data.kind {
        Some(PermissionRequestKind::Write) | Some(PermissionRequestKind::Read) => {
            match payload.get("path").and_then(Value::as_str) {
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
    eprintln!(
        "[permission] {} kind={:?} {detail}",
        if allowed { "APPROVE" } else { "DENY" },
        data.kind
    );
    allowed
}

/// Is `candidate` (possibly relative to `root`) inside `root`?
fn path_within(root: &Path, candidate: &Path) -> bool {
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
    fn shell_policy_scopes_to_worktree() {
        let root = Path::new("/repo/wt");
        assert!(shell_allowed(root, "cargo test"));
        assert!(shell_allowed(root, "cargo test 2>/dev/null"));
        assert!(!shell_allowed(root, "git push origin main"));
        assert!(!shell_allowed(root, "cat /etc/passwd > /tmp/x"));
        assert!(!shell_allowed(root, "cd .. && ls"));
    }
}
