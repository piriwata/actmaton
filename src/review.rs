//! Post-iteration self-review: deterministic checks of an iteration's claims
//! against ground truth, a reviewer prompt/parser where evidence is mandatory,
//! and a findings ledger whose recurring classes escalate into filed issues.

use std::path::Path;

use serde_json::{Value, json};

pub const CHECKLIST: &str = include_str!("../docs/review-checklist.md");

/// Same finding class seen in this many distinct issues is escalated.
const RECURRENCE: usize = 2;

#[derive(Debug, Clone, PartialEq)]
pub struct Finding {
    pub class: String,
    pub claim: String,
    pub evidence: String,
}

/// Everything the self-review looks at for one iteration.
pub struct Artifacts<'a> {
    pub issue_body: &'a str,
    pub final_message: &'a str,
    pub pr_body: &'a str,
    pub commit_message: &'a str,
    pub run_log: &'a str,
    pub report_line: &'a str,
    pub verdict: &'a str,
    /// The branch was committed and pushed before the PR body was written.
    pub pushed: bool,
    pub issue_number: u64,
    /// Whether a path exists on `origin/main` (checked against git).
    pub on_main: &'a dyn Fn(&str) -> bool,
}

fn finding(class: &str, claim: &str, evidence: String) -> Finding {
    Finding { class: class.into(), claim: claim.into(), evidence }
}

/// The model the run log says the session actually resolved to.
pub fn logged_model(run_log: &str) -> Option<String> {
    run_log
        .lines()
        .find_map(|l| l.trim().strip_prefix("resolved model: "))
        .map(str::trim)
        .filter(|m| !m.is_empty() && !m.starts_with('('))
        .map(String::from)
}

fn report_model(line: &str) -> Option<&str> {
    line.split(" | ").find_map(|p| p.trim().strip_prefix("model: "))
}

/// Bullet lines under the issue's `Acceptance` heading.
fn criteria(issue_body: &str) -> Vec<String> {
    let mut in_section = false;
    let mut out = Vec::new();
    for line in issue_body.lines() {
        if line.starts_with('#') {
            in_section = line.to_lowercase().contains("acceptance");
        } else if in_section {
            if let Some(c) = line.trim().strip_prefix("- ").or_else(|| line.trim().strip_prefix("* ")) {
                out.push(c.to_string());
            }
        }
    }
    out
}

const STATUS_WORDS: [&str; 7] = ["PASS", "FAIL", "PARTIAL", "UNCHECKED", "UNVERIFIED", "NOT VERIFIED", "VERIFIED"];
const UNCHECKED_WORDS: [&str; 4] = ["UNCHECKED", "UNVERIFIED", "NOT VERIFIED", "PARTIAL"];

/// Lines of the final message that start with a per-criterion status.
fn status_lines(message: &str) -> Vec<&str> {
    message
        .lines()
        .map(|l| l.trim().trim_start_matches(['-', '*', ' ']).trim_start_matches('[').trim())
        .map(|l| match l.to_uppercase().strip_prefix("CRITERION") {
            // the prompted form `CRITERION <n>: VERIFIED - ...`
            Some(_) => l.split_once(':').map_or(l, |(_, rest)| rest.trim()),
            None => l,
        })
        .filter(|l| {
            let up = l.to_uppercase();
            STATUS_WORDS.iter().any(|w| up.starts_with(w))
        })
        .collect()
}

/// Deterministic checks: claims in the artifacts versus verifiable facts.
pub fn check(a: &Artifacts) -> Vec<Finding> {
    let mut out = Vec::new();

    if a.pushed {
        if let Some(l) = a.pr_body.lines().find(|l| l.to_lowercase().contains("uncommitted")) {
            out.push(finding(
                "stale-status-wording",
                "PR body says changes are uncommitted, but the branch was committed and pushed",
                format!("PR body line: `{}`", l.trim()),
            ));
        }
    }

    if let (Some(field), Some(real)) = (report_model(a.report_line), logged_model(a.run_log)) {
        if field != real {
            out.push(finding(
                "report-run-log-mismatch",
                "loop report model field differs from the model resolved in the run log",
                format!("report `model: {field}` vs run log `resolved model: {real}`"),
            ));
        }
    }

    for l in a.final_message.lines() {
        let low = l.to_lowercase();
        if !["on main", "into main", "on `main`", "into `main`", "in main"].iter().any(|p| low.contains(p)) {
            continue;
        }
        let missing: Vec<&str> = l
            .split_whitespace()
            .map(|t| t.trim_matches(|c: char| !(c.is_alphanumeric() || "/_.-".contains(c))))
            .filter(|t| t.contains('/') && t.contains('.') && !t.ends_with('.'))
            .filter(|t| !(a.on_main)(t))
            .collect();
        if !missing.is_empty() {
            out.push(finding(
                "unverified-landing-claim",
                "final message says a feature is on main, but git shows its files are not on origin/main",
                format!("final message line: `{}`; absent from origin/main: {}", l.trim(), missing.join(", ")),
            ));
        }
    }

    let n = a.issue_number;
    let closes = |t: &str| t.to_lowercase().contains(&format!("closes #{n}"));
    if !closes(a.pr_body) && !closes(a.commit_message) {
        out.push(finding(
            "closing-keyword",
            "neither PR body nor commit uses `Closes #n`, so merging will not close the issue",
            format!("PR body/commit reference issue #{n} without a closing keyword"),
        ));
    }

    let crit = criteria(a.issue_body);
    let status = status_lines(a.final_message);
    if status.len() < crit.len() {
        out.push(finding(
            "missing-criterion-status",
            "final message lacks a per-criterion status line for every stated done criterion (verdict should be PARTIAL)",
            format!("issue states {} criteria; final message has {} status line(s)", crit.len(), status.len()),
        ));
    }
    if a.verdict == "PASS" {
        if let Some(l) = status.iter().find(|l| {
            let up = l.to_uppercase();
            UNCHECKED_WORDS.iter().any(|w| up.starts_with(w))
        }) {
            out.push(finding(
                "unchecked-criterion-passed",
                "verdict is PASS although the final message admits a criterion is unchecked",
                format!("final message line: `{l}`"),
            ));
        }
    }
    out
}

pub fn reviewer_prompt(a: &Artifacts) -> String {
    format!(
        "You are a skeptical, read-only reviewer of one actmaton loop iteration. Judge the \
         artifacts below ONLY against the checklist. Every finding MUST cite checkable evidence \
         (a quoted line, file:line, or log line); findings without evidence are discarded.\n\n\
         Reply with only a JSON array of {{\"class\": str, \"claim\": str, \"evidence\": str}} \
         (empty array if none).\n\n## Checklist\n{CHECKLIST}\n\n## Issue\n{}\n\n## Final message\n{}\n\n\
         ## PR body\n{}\n\n## Report line\n{}\n",
        a.issue_body, a.final_message, a.pr_body, a.report_line
    )
}

/// Parse the reviewer's reply; evidence-free findings are dropped.
pub fn parse_reviewer_findings(reply: &str) -> Vec<Finding> {
    let (Some(s), Some(e)) = (reply.find('['), reply.rfind(']')) else { return vec![] };
    let Ok(Value::Array(items)) = serde_json::from_str::<Value>(&reply[s..=e]) else { return vec![] };
    let field = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").trim().to_string();
    items
        .iter()
        .map(|v| Finding { class: field(v, "class"), claim: field(v, "claim"), evidence: field(v, "evidence") })
        .filter(|f| !f.class.is_empty() && !f.evidence.is_empty())
        .collect()
}

#[derive(Debug, Clone, PartialEq)]
pub struct LedgerEntry {
    pub class: String,
    pub issue: u64,
    pub evidence: String,
    pub escalated: bool,
}

pub fn read_ledger(path: &Path) -> Vec<LedgerEntry> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter_map(|v| {
            Some(LedgerEntry {
                class: v.get("class")?.as_str()?.to_string(),
                issue: v.get("issue")?.as_u64()?,
                evidence: v.get("evidence").and_then(Value::as_str).unwrap_or("").to_string(),
                escalated: v.get("escalated").and_then(Value::as_bool).unwrap_or(false),
            })
        })
        .collect()
}

pub fn append_ledger(path: &Path, entries: &[LedgerEntry]) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    for e in entries {
        let line = json!({"class": e.class, "issue": e.issue, "evidence": e.evidence, "escalated": e.escalated});
        writeln!(f, "{line}")?;
    }
    Ok(())
}

/// Classes (with their evidence) that, counting `new`, now span at least
/// `RECURRENCE` distinct issues and have not been escalated yet.
pub fn to_escalate(ledger: &[LedgerEntry], new: &[LedgerEntry]) -> Vec<(String, Vec<LedgerEntry>)> {
    let mut classes: Vec<&str> = new.iter().map(|e| e.class.as_str()).collect();
    classes.dedup();
    let mut out = Vec::new();
    for class in classes {
        if out.iter().any(|(c, _): &(String, _)| c == class)
            || ledger.iter().any(|e| e.class == class && e.escalated)
        {
            continue;
        }
        let all: Vec<LedgerEntry> =
            ledger.iter().chain(new).filter(|e| e.class == class && !e.escalated).cloned().collect();
        let mut issues: Vec<u64> = all.iter().map(|e| e.issue).collect();
        issues.sort_unstable();
        issues.dedup();
        if issues.len() >= RECURRENCE {
            out.push((class.to_string(), all));
        }
    }
    out
}

pub fn escalation_issue(class: &str, entries: &[LedgerEntry]) -> (String, String) {
    let mut body = format!(
        "Capability gap: actmaton's self-review caught the finding class `{class}` in {RECURRENCE}+ \
         iterations, so it is systemic rather than a one-off. Decide what the loop, propose, or the \
         review checklist (`docs/review-checklist.md`) must change so it is prevented, not just caught.\n\n## Evidence\n"
    );
    for e in entries {
        body.push_str(&format!("- issue #{}: {}\n", e.issue, e.evidence));
    }
    (format!("Self-review: recurring finding class `{class}`"), body)
}

pub fn render(findings: &[Finding]) -> String {
    findings
        .iter()
        .map(|f| format!("  - review finding [{}]: {} (evidence: {})\n", f.class, f.claim, f.evidence))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ISSUE: &str = "## Acceptance\n- caught\n- cited\n- tests pass\n";

    fn base<'a>() -> Artifacts<'a> {
        Artifacts {
            issue_body: ISSUE,
            final_message: "- PASS caught\n- PASS cited\n- PASS tests pass",
            pr_body: "Closes #7\n\ndone",
            commit_message: "t\n\nCloses #7",
            run_log: "resolved model: claude-x\n",
            report_line: "- iteration 1: issue #7 | model: claude-x | verdict: PASS | PR: u | s",
            verdict: "PASS",
            pushed: true,
            issue_number: 7,
            on_main: &|_| true,
        }
    }

    #[test]
    fn fixture_dev_feature_claimed_on_main() {
        let a = Artifacts {
            final_message: "- PASS caught\n- PASS cited\n- PASS tests pass\nThe dev flow is on main now: `src/devflow.rs`, PR #67.",
            on_main: &|p| p != "src/devflow.rs",
            ..base()
        };
        let f = check(&a);
        assert_eq!(f[0].class, "unverified-landing-claim");
        assert!(f[0].evidence.contains("src/devflow.rs"));
        assert!(classes(&Artifacts { on_main: &|_| true, ..a }).is_empty());
    }

    fn classes(a: &Artifacts) -> Vec<String> {
        check(a).into_iter().map(|f| f.class).collect()
    }

    #[test]
    fn clean_iteration_has_no_findings() {
        assert!(check(&base()).is_empty());
    }

    // Fixture corpus: real past failures from loop run 1. Add new ones as found.
    #[test]
    fn fixture_stale_uncommitted_pr_wording() {
        let a = Artifacts { pr_body: "Closes #7\n\nChanges are left uncommitted in the worktree.", ..base() };
        let f = check(&a);
        assert_eq!(f[0].class, "stale-status-wording");
        assert!(f[0].evidence.contains("uncommitted"));
    }

    #[test]
    fn fixture_unchecked_criterion_reported_pass() {
        let a = Artifacts {
            final_message: "- PASS caught\n- PASS cited\n- UNCHECKED tests pass (not run)",
            ..base()
        };
        assert_eq!(classes(&a), ["unchecked-criterion-passed"]);
    }

    #[test]
    fn fixture_dropped_resolved_model() {
        let a = Artifacts {
            report_line: "- iteration 1: issue #7 | model: default | verdict: PASS | PR: u | s",
            ..base()
        };
        let f = check(&a);
        assert_eq!(f[0].class, "report-run-log-mismatch");
        assert!(f[0].evidence.contains("claude-x"));
    }

    #[test]
    fn fixture_refs_instead_of_closes() {
        let a = Artifacts { pr_body: "Refs #7", commit_message: "t\n\nRefs #7", ..base() };
        assert_eq!(classes(&a), ["closing-keyword"]);
    }

    #[test]
    fn fixture_missing_criterion_status_lines() {
        let a = Artifacts { final_message: "All done, everything verified.", ..base() };
        assert_eq!(classes(&a), ["missing-criterion-status"]);
    }

    #[test]
    fn prompted_criterion_lines_count_as_status_lines() {
        let a = Artifacts { final_message: "CRITERION 1: VERIFIED - ran it\n- CRITERION 2: VERIFIED - ok\nCRITERION 3: VERIFIED - x", ..base() };
        assert!(!classes(&a).contains(&"missing-criterion-status".to_string()));
    }

    #[test]
    fn reviewer_prompt_carries_checklist_and_fixture_classes_are_listed() {
        let p = reviewer_prompt(&base());
        assert!(p.contains(CHECKLIST));
        for c in [
            "stale-status-wording",
            "report-run-log-mismatch",
            "closing-keyword",
            "missing-criterion-status",
            "unchecked-criterion-passed",
            "unverified-landing-claim",
        ] {
            assert!(CHECKLIST.contains(c), "checklist lacks {c}");
        }
    }

    #[test]
    fn evidence_free_reviewer_findings_are_dropped() {
        let reply = "```json\n[{\"class\":\"a\",\"claim\":\"x\",\"evidence\":\"\"},\
                     {\"class\":\"b\",\"claim\":\"y\",\"evidence\":\"log:3\"},\
                     {\"class\":\"c\",\"claim\":\"z\"}]\n```";
        let f = parse_reviewer_findings(reply);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].class, "b");
        assert!(parse_reviewer_findings("no json").is_empty());
    }

    fn entry(class: &str, issue: u64, escalated: bool) -> LedgerEntry {
        LedgerEntry { class: class.into(), issue, evidence: format!("e{issue}"), escalated }
    }

    #[test]
    fn class_seen_twice_escalates_once() {
        let seeded = vec![entry("closing-keyword", 3, false)];
        let same_issue = [entry("closing-keyword", 3, false)];
        assert!(to_escalate(&seeded, &same_issue).is_empty());

        let new = [entry("closing-keyword", 5, false), entry("other", 5, false)];
        let esc = to_escalate(&seeded, &new);
        assert_eq!(esc.len(), 1);
        assert_eq!(esc[0].0, "closing-keyword");
        assert_eq!(esc[0].1.len(), 2);
        let (title, body) = escalation_issue(&esc[0].0, &esc[0].1);
        assert!(title.contains("closing-keyword") && body.contains("issue #3: e3"));

        let done = vec![seeded[0].clone(), entry("closing-keyword", 5, false), entry("closing-keyword", 0, true)];
        assert!(to_escalate(&done, &[entry("closing-keyword", 6, false)]).is_empty());
    }

    #[test]
    fn ledger_roundtrip() {
        let p = std::env::temp_dir().join(format!("actmaton-review-ledger-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&p);
        let e = vec![entry("c", 1, false), entry("c", 2, true)];
        append_ledger(&p, &e).unwrap();
        assert_eq!(read_ledger(&p), e);
        let _ = std::fs::remove_file(&p);
    }
}
