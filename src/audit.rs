//! Audit-style proposal support for `actmaton propose`: findings, vetting
//! against the repository, the rejection ledger, and the health/direction mix.

use std::path::Path;

use serde_json::{Value, json};

const CATEGORIES: [&str; 6] = [
    "correctness",
    "performance",
    "test-coverage",
    "tech-debt",
    "docs-drift",
    "direction",
];

#[derive(Debug, Clone, PartialEq)]
pub struct Finding {
    pub title: String,
    pub category: String,
    pub claim: String,
    pub evidence: Vec<String>,
    pub quote: String,
    pub internal_evidence: String,
    /// Issue body; present only for findings the model offers to file.
    pub body: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Disposition {
    Filed,
    /// Vetted and valid, but not filed this run (one proposal per run).
    Vetted,
    Dropped(String),
    /// Matches a previously rejected finding or a closed-as-rejected issue.
    Ledger(String),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Slate {
    Health,
    Direction,
}

pub fn slate_of(category: &str) -> Slate {
    if category == "direction" { Slate::Direction } else { Slate::Health }
}

pub fn parse_findings(v: &Value) -> Vec<Finding> {
    let s = |f: &Value, k: &str| f.get(k).and_then(Value::as_str).unwrap_or("").trim().to_string();
    v.get("findings")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|f| Finding {
                    title: s(f, "title"),
                    category: s(f, "category").to_lowercase(),
                    claim: s(f, "claim"),
                    evidence: f
                        .get("evidence")
                        .and_then(Value::as_array)
                        .map(|e| e.iter().filter_map(|x| x.as_str().map(|x| x.trim().to_string())).collect())
                        .unwrap_or_default(),
                    quote: s(f, "quote"),
                    internal_evidence: s(f, "internal_evidence"),
                    body: Some(s(f, "body")).filter(|b| !b.is_empty()),
                })
                .collect()
        })
        .unwrap_or_default()
}

fn words(s: &str) -> Vec<String> {
    let mut w: Vec<String> = s
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() > 2)
        .map(str::to_string)
        .collect();
    w.sort();
    w.dedup();
    w
}

/// Stable key for a finding title.
pub fn title_key(title: &str) -> String {
    words(title).join(" ")
}

/// True when two titles are the same finding (word-set similarity >= 0.75).
pub fn same_finding(a: &str, b: &str) -> bool {
    let (a, b) = (words(a), words(b));
    if a.is_empty() || b.is_empty() {
        return false;
    }
    let inter = a.iter().filter(|w| b.contains(w)).count() as f64;
    let union = (a.len() + b.len()) as f64 - inter;
    inter / union >= 0.75
}

/// Ledger entries as `(title, reason)`, one JSON object per line.
pub fn read_ledger(path: &Path) -> Vec<(String, String)> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter_map(|v| {
            Some((
                v.get("title")?.as_str()?.to_string(),
                v.get("reason").and_then(Value::as_str).unwrap_or("").to_string(),
            ))
        })
        .collect()
}

pub fn append_ledger(path: &Path, entries: &[(String, String, String)]) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    for (title, category, reason) in entries {
        let line = json!({"key": title_key(title), "title": title, "category": category, "reason": reason});
        writeln!(f, "{line}")?;
    }
    Ok(())
}

/// Resolve `path:line` or `path:start-end` and return the cited text.
fn read_cited(files: &[(String, String)], cite: &str) -> Result<String, String> {
    let (path, range) = cite.rsplit_once(':').ok_or_else(|| format!("`{cite}` is not file:line"))?;
    let (start, end) = match range.split_once('-') {
        Some((a, b)) => (a, b),
        None => (range, range),
    };
    let (start, end): (usize, usize) = match (start.trim().parse(), end.trim().parse()) {
        (Ok(a), Ok(b)) if a >= 1 && b >= a => (a, b),
        _ => return Err(format!("`{cite}` has an invalid line range")),
    };
    let text = &files
        .iter()
        .find(|(p, _)| p == path)
        .ok_or_else(|| format!("`{path}` not found"))?
        .1;
    let lines: Vec<&str> = text.lines().collect();
    if end > lines.len() {
        return Err(format!("`{cite}` is past end of file ({} lines)", lines.len()));
    }
    Ok(lines[start - 1..end].join("\n"))
}

fn has_new_word(line: &str) -> bool {
    let l = line.to_lowercase();
    l.split(|c: char| !c.is_alphanumeric())
        .any(|w| ["add", "adds", "new", "create", "introduce", "implement", "support"].contains(&w))
}

/// Premise check: negative claims about current state ("X does not exist",
/// "no tests for Y") that the repository contradicts. `files` are
/// `(path, contents)`. Lines phrased as new additions are skipped.
pub fn check_state_claims(text: &str, files: &[(String, String)]) -> Vec<String> {
    const NEGATIVES: [&str; 10] = [
        "does not exist", "doesn't exist", "do not exist", "there is no", "there are no",
        "has no", "have no", "is missing", "lacks", "not implemented",
    ];
    let mut out = Vec::new();
    for line in text.lines() {
        let lower = line.to_lowercase();
        if has_new_word(line) || !NEGATIVES.iter().any(|n| lower.contains(n)) {
            continue;
        }
        let toks: Vec<&str> = line.split('`').skip(1).step_by(2).map(str::trim).collect();
        let mentions_tests = lower.contains("test");
        for tok in &toks {
            let path = tok.split(':').next().unwrap_or(tok);
            if let Some((_, text)) = files.iter().find(|(p, _)| p == path) {
                if mentions_tests && text.contains("#[cfg(test)]") {
                    out.push(format!("claims no tests for `{path}`, but it has a #[cfg(test)] module"));
                } else if !mentions_tests && lower.contains("exist") {
                    out.push(format!("claims `{path}` does not exist, but it does"));
                }
            } else if tok.len() > 2 && !tok.contains(' ') && !tok.contains('/') {
                let sym = tok.trim_start_matches('-').trim_end_matches("()");
                if let Some((p, _)) = files.iter().find(|(_, t)| t.contains(sym)) {
                    out.push(format!("claims `{tok}` is absent, but `{sym}` appears in {p}"));
                }
            }
        }
        if mentions_tests && toks.is_empty() && files.iter().any(|(_, t)| t.contains("#[cfg(test)]")) {
            out.push(format!("claims missing tests (\"{}\"), but test modules exist", line.trim()));
        }
    }
    out.dedup();
    out
}

fn is_internal_marker(text: &str) -> bool {
    let l = text.to_lowercase();
    ["todo", "fixme", "roadmap", "workaround", "m2", "m3", "m4"].iter().any(|m| l.contains(m))
}

/// Vet every finding and decide the single one to file. `files` are the
/// repo's `(path, contents)`; `rejected` is the ledger plus closed-as-rejected
/// issue titles. Returns one disposition per finding, in order.
pub fn vet(
    findings: &[Finding],
    files: &[(String, String)],
    rejected: &[(String, String)],
) -> Vec<Disposition> {
    let mut disp: Vec<Option<Disposition>> = vec![None; findings.len()];
    let mut direction_seen = false;
    for (i, f) in findings.iter().enumerate() {
        let drop = |r: String| Some(Disposition::Dropped(r));
        disp[i] = if !CATEGORIES.contains(&f.category.as_str()) {
            drop(format!("unknown category `{}`", f.category))
        } else if f.title.is_empty() {
            drop("empty title".into())
        } else if let Some((t, r)) = rejected.iter().find(|(t, _)| same_finding(t, &f.title)) {
            Some(Disposition::Ledger(format!("matches rejected \"{t}\" ({r})")))
        } else if f.evidence.is_empty() {
            drop("no file:line evidence".into())
        } else {
            let mut reason = None;
            let mut cited = Vec::new();
            for e in &f.evidence {
                match read_cited(files, e) {
                    Ok(t) => cited.push(t),
                    Err(m) => {
                        reason = Some(format!("evidence unresolved: {m}"));
                        break;
                    }
                }
            }
            if reason.is_none() && !f.quote.is_empty() {
                let q = f.quote.trim();
                if !cited.iter().any(|c| c.contains(q)) {
                    reason = Some(format!("quote \"{q}\" not found at cited lines"));
                }
            }
            if reason.is_none() {
                let mut text = f.claim.clone();
                if let Some(b) = &f.body {
                    text.push('\n');
                    text.push_str(b);
                }
                let bad = check_state_claims(&text, files);
                if !bad.is_empty() {
                    reason = Some(format!("false premise: {}", bad.join("; ")));
                }
            }
            if reason.is_none() && slate_of(&f.category) == Slate::Direction {
                let internal = !f.internal_evidence.is_empty() && cited.iter().any(|c| is_internal_marker(c));
                if !internal {
                    reason = Some("direction without repo-internal evidence (TODO/roadmap/workaround)".into());
                } else if direction_seen {
                    reason = Some("direction cap: at most one direction proposal per run".into());
                } else {
                    direction_seen = true;
                }
            }
            reason.map(Disposition::Dropped)
        };
    }
    let mut disp: Vec<Disposition> = disp.into_iter().map(|d| d.unwrap_or(Disposition::Vetted)).collect();
    // Health first, then direction.
    let pick = [Slate::Health, Slate::Direction].into_iter().find_map(|s| {
        findings.iter().enumerate().position(|(i, f)| {
            disp[i] == Disposition::Vetted && f.body.is_some() && slate_of(&f.category) == s
        })
    });
    if let Some(i) = pick {
        disp[i] = Disposition::Filed;
    }
    disp
}

pub fn render_dispositions(findings: &[Finding], disp: &[Disposition]) -> String {
    let mut out = String::new();
    let mut cats: Vec<&str> = findings.iter().map(|f| f.category.as_str()).collect();
    cats.sort();
    cats.dedup();
    out.push_str(&format!("categories scanned: {}\n", CATEGORIES.join(", ")));
    out.push_str(&format!("categories with findings: {}\n", cats.join(", ")));
    out.push_str(&format!("findings vetted: {}\n", findings.len()));
    for (f, d) in findings.iter().zip(disp) {
        let slate = match slate_of(&f.category) {
            Slate::Health => "health",
            Slate::Direction => "direction",
        };
        let d = match d {
            Disposition::Filed => "FILED".to_string(),
            Disposition::Vetted => "vetted (not filed this run)".to_string(),
            Disposition::Dropped(r) => format!("DROPPED: {r}"),
            Disposition::Ledger(r) => format!("LEDGER: {r}"),
        };
        out.push_str(&format!("- [{slate}/{}] {} -> {d}\n", f.category, f.title));
    }
    out
}

/// Ledger rows `(title, category, reason)` for the findings dropped this run.
pub fn ledger_entries(findings: &[Finding], disp: &[Disposition]) -> Vec<(String, String, String)> {
    findings
        .iter()
        .zip(disp)
        .filter_map(|(f, d)| match d {
            Disposition::Dropped(r) => Some((f.title.clone(), f.category.clone(), r.clone())),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files() -> Vec<(String, String)> {
        vec![
            ("src/a.rs".into(), "fn x() {}\n// TODO: roadmap M3 loop\n#[cfg(test)]\nmod t {}\n".into()),
            ("src/b.rs".into(), "fn helper_fn() {}\n".into()),
        ]
    }

    fn finding(title: &str, cat: &str, ev: &str) -> Finding {
        Finding {
            title: title.into(),
            category: cat.into(),
            claim: "c".into(),
            evidence: vec![ev.into()],
            quote: String::new(),
            internal_evidence: String::new(),
            body: Some("body".into()),
        }
    }

    #[test]
    fn premise_check() {
        let f = files();
        assert!(!check_state_claims("There are no tests for `src/a.rs`.", &f).is_empty());
        assert!(check_state_claims("There are no tests for `src/b.rs`.", &f).is_empty());
        assert!(!check_state_claims("`helper_fn` does not exist", &f).is_empty());
        assert!(check_state_claims("`other_fn` does not exist", &f).is_empty());
        assert!(check_state_claims("Add a new test module for `src/a.rs`; there is no `--help`", &f).is_empty());
    }

    #[test]
    fn ledger_round_trip_and_dedup() {
        let path = std::env::temp_dir().join(format!("actmaton-ledger-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);
        append_ledger(&path, &[("Remove dead helper in b.rs".into(), "tech-debt".into(), "false premise".into())]).unwrap();
        let ledger = read_ledger(&path);
        std::fs::remove_file(&path).ok();
        assert_eq!(ledger, vec![("Remove dead helper in b.rs".to_string(), "false premise".to_string())]);
        let d = vet(&[finding("Remove the dead helper in b.rs", "tech-debt", "src/b.rs:1")], &files(), &ledger);
        assert!(matches!(d[0], Disposition::Ledger(_)));
        let d = vet(&[finding("Unrelated cleanup", "tech-debt", "src/b.rs:1")], &files(), &ledger);
        assert_eq!(d[0], Disposition::Filed);
    }

    #[test]
    fn slate_classification_and_mix() {
        assert_eq!(slate_of("direction"), Slate::Direction);
        assert_eq!(slate_of("tech-debt"), Slate::Health);
        let mut d1 = finding("Loop feature one", "direction", "src/a.rs:2");
        d1.internal_evidence = "TODO in a.rs".into();
        let mut d2 = finding("Loop feature two", "direction", "src/a.rs:2");
        d2.internal_evidence = "TODO in a.rs".into();
        let generic = finding("Generic idea", "direction", "src/b.rs:1");
        let health = finding("Tidy b", "tech-debt", "src/b.rs:1");
        let fs = vec![d1, d2, generic, health];
        let d = vet(&fs, &files(), &[]);
        assert_eq!(d[0], Disposition::Vetted);
        assert!(matches!(&d[1], Disposition::Dropped(r) if r.contains("cap")));
        assert!(matches!(&d[2], Disposition::Dropped(r) if r.contains("repo-internal")));
        assert_eq!(d[3], Disposition::Filed); // health beats direction
    }

    #[test]
    fn bad_evidence_dropped() {
        let d = vet(&[finding("x thing", "correctness", "src/b.rs:99")], &files(), &[]);
        assert!(matches!(&d[0], Disposition::Dropped(r) if r.contains("unresolved")));
    }
}
