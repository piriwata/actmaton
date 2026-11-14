//! `actmaton propose` — M2 self-proposal, first slice.
//!
//! Gathers a snapshot of the repository state, asks a read-only Copilot
//! session for ONE concrete next-task proposal, and files it as a GitHub
//! issue labeled `actmaton-proposal`. Implementation still happens only
//! through `actmaton run <n>`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use github_copilot_sdk::{
    Client, ClientOptions, MessageOptions, PermissionRequestData, PermissionRequestKind,
    SessionConfig,
};
use serde_json::Value;

use crate::run_loop::{final_message_text, path_within, payload_target_path, repo_root};

const REPO: &str = "piriwata/actmaton";
const LABEL: &str = "actmaton-proposal";
const SESSION_TIMEOUT: Duration = Duration::from_secs(600);

pub async fn propose() -> Result<(), Box<dyn std::error::Error>> {
    let root = repo_root()?;
    println!("repo root: {}", root.display());

    let snapshot = gather_snapshot(&root)?;
    let prompt = build_prompt(&snapshot);

    let reply = run_session(&root, &prompt).await?;
    let log_path = write_log(&root, &prompt, reply.as_deref())?;
    println!("propose log: {}", log_path.display());

    let reply = reply.ok_or("session returned no final message")?;
    let (title, mut body) = extract_proposal(&reply)?;
    let unverified = check_references(&body, crate::USAGE, &|p| root.join(p).exists());
    append_log(
        &log_path,
        &format!(
            "\n== reference check ==\n{}\n",
            if unverified.is_empty() {
                "all references verified".to_string()
            } else {
                unverified.join("\n")
            }
        ),
    );
    if !unverified.is_empty() {
        body.push_str(&unverified_note(&unverified));
    }
    let url = create_issue(&title, &body)?;

    println!("\n================ propose summary ================");
    println!("title: {title}");
    println!("issue: {url}");
    println!("next step: review the proposal; close it, or run `actmaton run <n>`.");
    println!("=================================================");
    Ok(())
}

fn command_output(cmd: &mut Command, what: &str) -> Result<String, Box<dyn std::error::Error>> {
    let out = cmd.output()?;
    if !out.status.success() {
        return Err(format!(
            "{what} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )
        .into());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

fn gather_snapshot(root: &Path) -> Result<String, Box<dyn std::error::Error>> {
    let issues = command_output(
        Command::new("gh").args([
            "issue", "list", "--repo", REPO, "--state", "open", "--json",
            "number,title,labels",
        ]),
        "gh issue list",
    )?;
    let prs = command_output(
        Command::new("gh").args([
            "pr", "list", "--repo", REPO, "--state", "open", "--json", "number,title",
        ]),
        "gh pr list",
    )?;
    let log = command_output(
        Command::new("git").arg("-C").arg(root).args(["log", "--oneline", "-15"]),
        "git log",
    )?;
    let files = command_output(
        Command::new("git").arg("-C").arg(root).arg("ls-files"),
        "git ls-files",
    )?;
    let files: Vec<&str> = files.lines().take(200).collect();
    let todos = scan_todos(&root.join("src"), root);
    let readme = std::fs::read_to_string(root.join("README.md")).unwrap_or_default();
    let headers = readme_headers(&readme);
    let fns = pub_fns(&root.join("src"), root);

    Ok(format!(
        "## Open issues (JSON)\n{issues}\n\n## Open pull requests (JSON)\n{prs}\n\n\
         ## Recent commits\n{log}\n\n## Tracked files\n{}\n\n## TODO/FIXME in src/\n{}\n\n\
         ## Current CLI usage (what `actmaton` prints)\n{}\n\n\
         ## README section headers\n{}\n\n## pub fn names per src/ file\n{}",
        files.join("\n"),
        if todos.is_empty() { "(none)".to_string() } else { todos.join("\n") },
        crate::USAGE,
        if headers.is_empty() { "(none)".to_string() } else { headers.join("\n") },
        if fns.is_empty() { "(none)".to_string() } else { fns.join("\n") },
    ))
}

/// Markdown header lines (`# ...`) of a README, outside code fences.
fn readme_headers(text: &str) -> Vec<String> {
    let mut in_fence = false;
    let mut out = Vec::new();
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
        } else if !in_fence && line.starts_with('#') {
            out.push(line.trim().to_string());
        }
    }
    out
}

/// `file: name, name` of `pub fn` names for each .rs file under `dir`.
fn pub_fns(dir: &Path, root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    for p in paths {
        if p.is_dir() {
            out.extend(pub_fns(&p, root));
        } else if p.extension().is_some_and(|e| e == "rs") {
            let text = std::fs::read_to_string(&p).unwrap_or_default();
            let names = text
                .lines()
                .filter_map(|l| {
                    let rest = l.trim_start().strip_prefix("pub ")?;
                    let rest = rest.strip_prefix("async ").unwrap_or(rest);
                    let rest = rest.strip_prefix("fn ")?;
                    Some(rest.split(|c: char| !c.is_alphanumeric() && c != '_').next()?.to_string())
                })
                .collect::<Vec<_>>();
            if !names.is_empty() {
                let rel = p.strip_prefix(root).unwrap_or(&p).display().to_string();
                out.push(format!("{rel}: {}", names.join(", ")));
            }
        }
    }
    out
}

fn is_word_present(line: &str, words: &[&str]) -> bool {
    line.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .any(|w| words.contains(&w))
}

/// Check backtick-quoted paths, flags and `actmaton ...` commands in `body`
/// against the repo (`path_exists`) and the CLI `usage`. Returns one
/// description per reference that does not exist and is not presented as new
/// (a line containing add/new/create/introduce/implement).
fn check_references(body: &str, usage: &str, path_exists: &dyn Fn(&str) -> bool) -> Vec<String> {
    let known_flags: Vec<&str> = usage.split_whitespace().filter(|w| w.starts_with("--")).collect();
    let known_cmds: Vec<&str> = usage
        .lines()
        .filter_map(|l| {
            let rest = l.split("actmaton ").nth(1)?;
            let w = rest.split_whitespace().next()?;
            Some(w).filter(|w| !w.starts_with('<'))
        })
        .collect();
    let new_words = ["add", "adds", "new", "create", "introduce", "implement", "support"];
    let mut out: Vec<String> = Vec::new();
    for line in body.lines() {
        if is_word_present(line, &new_words) {
            continue;
        }
        for (i, tok) in line.split('`').enumerate() {
            if i % 2 == 0 || tok.trim().is_empty() {
                continue;
            }
            let tok = tok.trim();
            let mut problems = Vec::new();
            let words: Vec<&str> = tok.split_whitespace().collect();
            if words[0] == "actmaton" {
                if let Some(c) = words.get(1).filter(|c| !c.starts_with('-') && !c.starts_with('<')) {
                    if !known_cmds.contains(c) {
                        problems.push(format!("command `actmaton {c}`"));
                    }
                }
            }
            for w in words.iter().filter(|w| w.starts_with("--")) {
                let flag = w.split('=').next().unwrap_or(w);
                if !known_flags.contains(&flag) {
                    problems.push(format!("flag `{flag}`"));
                }
            }
            if words.len() == 1 && tok.starts_with("src/") {
                let path = tok.split(':').next().unwrap_or(tok);
                if !path_exists(path) {
                    problems.push(format!("path `{path}`"));
                }
            }
            for p in problems {
                if !out.contains(&p) {
                    out.push(p);
                }
            }
        }
    }
    out
}

fn unverified_note(items: &[String]) -> String {
    format!(
        "\n\n---\n⚠ unverified references: the following are not present in the current \
         repo/CLI and are not phrased as new additions: {}",
        items.join(", ")
    )
}

fn append_log(path: &Path, text: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(path) {
        let _ = f.write_all(text.as_bytes());
    }
}

/// `file:line: text` for each TODO/FIXME under `dir`.
fn scan_todos(dir: &Path, root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    for p in paths {
        if p.is_dir() {
            out.extend(scan_todos(&p, root));
        } else if let Ok(text) = std::fs::read_to_string(&p) {
            let rel = p.strip_prefix(root).unwrap_or(&p).display().to_string();
            for (i, line) in text.lines().enumerate() {
                if line.contains("TODO") || line.contains("FIXME") {
                    out.push(format!("{rel}:{}: {}", i + 1, line.trim()));
                }
            }
        }
    }
    out
}

fn build_prompt(snapshot: &str) -> String {
    format!(
        "You are proposing the next task for the actmaton repository ({REPO}). You have \
         read-only access to the repository (working directory = the clone); you cannot \
         write files or run shell commands, and everything you need is in the snapshot below.\n\n\
         {snapshot}\n\n\
         Propose exactly ONE concrete next task.\n\
         - Do not duplicate any open issue or open pull request.\n\
         - Prefer a small, verifiable improvement to actmaton itself.\n\
         - If the roadmap (M2 self-proposal -> M3 repeated loop -> M4 autonomy/local models) \
         suggests a natural next step, prefer it.\n\
         - Reference only files, commands, flags, and document sections that appear in the \
         snapshot. Anything else must be phrased explicitly as a new addition (e.g. \"add a \
         --help flag\"), never as an existing thing to modify.\n\n\
         Reply with ONLY a JSON object: {{\"title\": \"...\", \"body\": \"...\"}}. The body is \
         Markdown with the sections: Motivation, Scope, Acceptance criteria."
    )
}

/// Parse `{"title", "body"}` from a reply, tolerating a surrounding code fence
/// or stray prose around the object.
fn extract_proposal(reply: &str) -> Result<(String, String), Box<dyn std::error::Error>> {
    let text = reply.trim();
    let start = text.find('{').ok_or("no JSON object in reply")?;
    let end = text.rfind('}').ok_or("no JSON object in reply")?;
    if end < start {
        return Err("no JSON object in reply".into());
    }
    let v: Value = serde_json::from_str(&text[start..=end])?;
    let field = |k: &str| {
        v.get(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .ok_or_else(|| format!("proposal JSON is missing a non-empty `{k}`"))
    };
    Ok((field("title")?, field("body")?))
}

fn create_issue(title: &str, body: &str) -> Result<String, Box<dyn std::error::Error>> {
    command_output(
        Command::new("gh").args([
            "issue", "create", "--repo", REPO, "--title", title, "--body", body, "--label",
            LABEL,
        ]),
        "gh issue create",
    )
}

fn write_log(
    root: &Path,
    prompt: &str,
    reply: Option<&str>,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let dir = root
        .parent()
        .ok_or("repo root has no parent directory")?
        .join("actmaton-runs");
    std::fs::create_dir_all(&dir)?;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let path = dir.join(format!("propose-{ts}.log"));
    std::fs::write(
        &path,
        format!(
            "== prompt ==\n{prompt}\n\n== reply ==\n{}\n",
            reply.unwrap_or("(no final message)")
        ),
    )?;
    Ok(path)
}

async fn run_session(root: &Path, prompt: &str) -> Result<Option<String>, Box<dyn std::error::Error>> {
    let scope = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let client = Client::start(ClientOptions::default()).await?;
    let session = client
        .create_session(
            SessionConfig::default()
                .with_working_directory(root)
                .approve_permissions_if(move |data| {
                    let (allowed, line) = propose_permission(&scope, data);
                    eprintln!("{line}");
                    allowed
                }),
        )
        .await?;
    println!("copilot session started (read-only); requesting a proposal");
    let reply = session
        .send_and_wait(MessageOptions::new(prompt).with_wait_timeout(SESSION_TIMEOUT))
        .await?;
    session.disconnect().await?;
    client.stop().await.ok();
    Ok(reply.as_ref().and_then(final_message_text))
}

fn propose_permission(root: &Path, data: &PermissionRequestData) -> (bool, String) {
    let payload = data
        .extra
        .get("permissionRequest")
        .cloned()
        .unwrap_or_else(|| data.extra.clone());
    let path = payload_target_path(&payload);
    let allowed = decide(root, data.kind.as_ref(), path);
    (
        allowed,
        format!(
            "[propose-permission] {} kind={:?} path={path:?}",
            if allowed { "APPROVE" } else { "DENY" },
            data.kind
        ),
    )
}

/// Read inside the repo only; write, shell, and everything else denied.
fn decide(root: &Path, kind: Option<&PermissionRequestKind>, path: Option<&str>) -> bool {
    match kind {
        Some(PermissionRequestKind::Read) => {
            path.is_some_and(|p| path_within(root, Path::new(p)))
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_parts() {
        let h = readme_headers("# A\ntext\n```\n# not\n```\n## B");
        assert_eq!(h, vec!["# A", "## B"]);
        assert!(crate::USAGE.contains("actmaton propose"));
    }

    #[test]
    fn reference_check() {
        let usage = crate::USAGE;
        let exists = |p: &str| p == "src/main.rs";
        let check = |b: &str| check_references(b, usage, &exists);
        assert!(check("Edit `src/main.rs` and run `actmaton run --dry-run`.").is_empty());
        assert_eq!(check("Edit `src/nope.rs`."), vec!["path `src/nope.rs`"]);
        assert_eq!(check("`actmaton run --help` documents it"), vec!["flag `--help`"]);
        assert!(check("Add a `--help` flag").is_empty());
        assert_eq!(check("Run `actmaton foo`"), vec!["command `actmaton foo`"]);
    }

    #[test]
    fn extracts_plain_json() {
        let (t, b) = extract_proposal(r#"{"title": "T", "body": "B"}"#).unwrap();
        assert_eq!((t.as_str(), b.as_str()), ("T", "B"));
    }

    #[test]
    fn extracts_fenced_json() {
        let reply = "Here:\n```json\n{\"title\": \"T\", \"body\": \"## Motivation\\nx\"}\n```\n";
        let (t, b) = extract_proposal(reply).unwrap();
        assert_eq!(t, "T");
        assert!(b.starts_with("## Motivation"));
    }

    #[test]
    fn rejects_bad_replies() {
        assert!(extract_proposal("no json").is_err());
        assert!(extract_proposal(r#"{"title": "", "body": "B"}"#).is_err());
    }

    #[test]
    fn denies_write_and_shell_allows_scoped_read() {
        let root = std::env::current_dir().unwrap().canonicalize().unwrap();
        assert!(!decide(&root, Some(&PermissionRequestKind::Write), Some("src/x.rs")));
        assert!(!decide(&root, Some(&PermissionRequestKind::Shell), None));
        assert!(decide(&root, Some(&PermissionRequestKind::Read), Some("src/main.rs")));
        assert!(!decide(&root, Some(&PermissionRequestKind::Read), Some("/etc/passwd")));
        assert!(!decide(&root, None, Some("src/main.rs")));
    }
}
