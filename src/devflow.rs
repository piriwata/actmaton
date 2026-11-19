//! Dev-branch flow: the loop auto-merges green work into `dev`; the human
//! merges `dev` → `main` once, via a standing PR whose body is built here.
//! Merging into main is never done by actmaton.

pub const DEV: &str = "dev";

/// Merge into dev only when the run passed (or is a human-only PARTIAL, whose
/// human checks are deferred to the standing PR), the PR targets dev, git sees
/// no conflict, and dev's tree is green.
pub fn should_merge(verdict: &crate::run_loop::Verdict, base: &str, mergeable: &str, dev_green: bool) -> bool {
    let qualifies = *verdict == crate::run_loop::Verdict::Pass || verdict.human_only();
    qualifies && base == DEV && mergeable == "MERGEABLE" && dev_green
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Stratum {
    AsIs,
    Check,
    Attention,
}

const GUARD_FILES: [&str; 2] = ["src/permission", "src/devflow.rs"];
const CORE_FILES: [&str; 5] =
    ["src/run_loop.rs", "src/propose.rs", "src/loop_cmd.rs", "src/audit.rs", "src/model.rs"];

/// Group by blast radius: guard/verifier surface > core files or repairs > rest.
/// The permission policy lives in run_loop.rs, so shell/permission code is
/// recognised by name as well.
pub fn classify(files: &[String], repair_attempts: u32) -> Stratum {
    let lower = |f: &String| f.to_lowercase();
    if files.iter().any(|f| {
        GUARD_FILES.iter().any(|g| f.starts_with(g)) || lower(f).contains("credential") || lower(f).contains("secret")
    }) {
        Stratum::Attention
    } else if repair_attempts > 0 || files.iter().any(|f| CORE_FILES.contains(&f.as_str())) {
        Stratum::Check
    } else {
        Stratum::AsIs
    }
}

#[derive(Debug, Clone, Default)]
pub struct Item {
    pub pr: u64,
    pub title: String,
    pub url: String,
    pub human: bool,
    pub implemented: String,
    pub decisions: String,
    pub assumptions: String,
    pub files: Vec<String>,
    pub repair_attempts: u32,
    /// Outstanding human-check criteria of a merged human-only PARTIAL.
    pub human_checks: Vec<String>,
    /// What the orchestrator did on a human PR's behalf (conflict repair).
    pub repair_note: Option<String>,
    /// Issues this PR closes; GitHub only auto-closes on the default branch,
    /// so the standing PR repeats them.
    pub closes: Vec<u64>,
}

/// Issue numbers named by closing keywords (`Closes #4`, `fixes #5`, `Resolved #6`).
pub fn closes_with_retry<E: std::fmt::Display>(mut fetch: impl FnMut() -> Result<String, E>) -> Result<Vec<u64>, String> {
    fetch()
        .or_else(|_| fetch())
        .map(|b| closing_issues(&b))
        .map_err(|e| e.to_string())
}

/// Problems-section line for PRs whose closing-issue lookup failed.
pub fn lookup_failure_note(failed: &[u64]) -> String {
    let list: Vec<String> = failed.iter().map(|n| format!("#{n}")).collect();
    format!("Closes の導出が不完全です: {} の本文を取得できませんでした", list.join(", "))
}

pub fn closing_issues(text: &str) -> Vec<u64> {
    let mut out = Vec::new();
    let words: Vec<&str> = text.split_whitespace().collect();
    for w in words.windows(2) {
        let kw = w[0].trim_end_matches(':').to_lowercase();
        let is_kw = ["close", "closes", "closed", "fix", "fixes", "fixed", "resolve", "resolves", "resolved"].contains(&kw.as_str());
        if let (true, Some(n)) = (is_kw, w[1].strip_prefix('#').and_then(|d| d.trim_end_matches(|c: char| !c.is_ascii_digit()).parse::<u64>().ok())) {
            if !out.contains(&n) {
                out.push(n);
            }
        }
    }
    out
}

#[derive(Debug, Clone, Default)]
pub struct Morning {
    pub items: Vec<Item>,
    pub remaining: Vec<String>,
    pub dev_head: String,
    pub dev_green: bool,
    pub iterations: u32,
    pub human_touches: u32,
    pub yomiyasu_note: Option<String>,
    /// Repairs and escalations the loop performed on human PRs targeting dev.
    pub human_notes: Vec<String>,
}

/// Text under the first heading/label line containing any of `names`
/// (`## Decisions and assumptions`, `**Decisions**:` ...), up to the next heading.
pub fn extract_section(text: &str, names: &[&str]) -> String {
    let mut out = Vec::new();
    let mut on = false;
    for line in text.lines() {
        let t = line.trim();
        let lower = t.to_lowercase();
        let is_head = t.starts_with('#') || (t.starts_with("**") && t.contains("**"));
        if is_head {
            on = names.iter().any(|n| lower.contains(n));
            continue;
        }
        if on && !t.is_empty() {
            out.push(t.trim_start_matches(['-', '*']).trim().to_string());
        }
    }
    out.join(" / ")
}

fn or_none(s: &str) -> &str {
    if s.trim().is_empty() {
        "（記載なし）"
    } else {
        s
    }
}

/// PR number and title from a commit subject such as `title (#12)` or
/// `Merge pull request #12 from x/y`.
pub fn pr_from_subject(subject: &str) -> Option<(u64, String)> {
    let at = subject.find('#')?;
    let digits: String = subject[at + 1..].chars().take_while(|c| c.is_ascii_digit()).collect();
    let pr = digits.parse().ok()?;
    let title = subject[..at].trim_end().trim_end_matches('(').trim_end();
    let title = if title.is_empty() || title.starts_with("Merge pull request") { subject } else { title };
    Some((pr, title.to_string()))
}

/// Reconcile recorded loop items with the PRs actually in dev but not in main:
/// drop items no longer ahead, and add ahead PRs that entered dev by another route.
pub fn merge_shipped(items: Vec<Item>, shipped: Vec<Item>) -> Vec<Item> {
    let mut out: Vec<Item> = items.into_iter().filter(|i| shipped.iter().any(|s| s.pr == i.pr)).collect();
    // Closing issues come from live PR state; stored ones only fill in when live lookup found none.
    for i in &mut out {
        if let Some(s) = shipped.iter().find(|s| s.pr == i.pr) {
            if !s.closes.is_empty() {
                i.closes = s.closes.clone();
            }
        }
    }
    for s in shipped {
        if !out.iter().any(|i| i.pr == s.pr) {
            out.push(Item { human: true, ..s });
        }
    }
    out
}

fn render_item(i: &Item) -> String {
    let who = if i.human { "（人が作成）" } else { "" };
    let mut s = format!("### #{} {}{}\n- リンク: {}\n", i.pr, i.title, who, i.url);
    s.push_str(&format!("- やったこと: {}\n", or_none(&i.implemented)));
    s.push_str(&format!("- 判断したこと: {}\n", or_none(&i.decisions)));
    s.push_str(&format!("- 仮定したこと: {}\n", or_none(&i.assumptions)));
    if i.repair_attempts > 0 {
        s.push_str(&format!("- テストの直しが {} 回ありました。\n", i.repair_attempts));
    }
    if !i.human_checks.is_empty() {
        s.push_str("- 人が確認すること（未確認のまま dev に入りました）:\n");
        for c in &i.human_checks {
            s.push_str(&format!("  - {c}\n"));
        }
    }
    if let Some(n) = &i.repair_note {
        s.push_str(&format!("- 代わりに直したこと: {n}\n"));
    }
    s.push('\n');
    s
}

pub const NIHONGO_NOTE: &str = "nihongo を読み込めなかったため、この文章は文体ルールだけで書いています。";

/// Prompt for the drafting session: the nihongo skill text plus the
/// deterministic body, whose facts must be kept as they are.
pub fn nihongo_prompt(skill: &str, body: &str) -> String {
    format!(
        "以下の日本語文章スキル（nihongo）に従って、PR 本文の日本語だけを読みやすく直してください。\n\
         事実は変えないでください。PR 番号、リンク、コミット、数値、テストの状態、見出し、項目は、追加も削除もしません。\n\
         返答は直した本文だけにしてください（前置きやコードフェンスは付けません）。\n\n\
         ## スキル\n{skill}\n\n## 本文\n{body}\n"
    )
}

fn fact_tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for w in text.split(|c: char| c.is_whitespace() || "()（）、。：,".contains(c)) {
        if w.starts_with("https://") || (w.starts_with('#') && w.len() > 1 && w[1..].chars().all(|c| c.is_ascii_digit())) {
            out.push(w.to_string());
        }
    }
    for (i, span) in text.split('`').enumerate() {
        if i % 2 == 1 && !span.is_empty() {
            out.push(format!("`{span}`"));
        }
    }
    out
}

/// Whether every link, PR number and code span of `original` survives in `refined`.
pub fn facts_preserved(original: &str, refined: &str) -> bool {
    fact_tokens(original).iter().all(|t| refined.contains(t.as_str()))
}

/// Strip a wrapping code fence the session may add around the body.
pub fn unfence(reply: &str) -> &str {
    let t = reply.trim();
    t.strip_prefix("```markdown")
        .or_else(|| t.strip_prefix("```md"))
        .or_else(|| t.strip_prefix("```"))
        .and_then(|r| r.strip_suffix("```"))
        .map(str::trim)
        .unwrap_or(t)
}

/// Whether a standing PR body names `head` as its dev head (see `build_body`).
pub fn body_names_head(body: &str, head: &str) -> bool {
    body.contains(&format!("dev の最新: `{head}`"))
}

/// The standing dev → main PR body: plain Japanese, short sentences, bullets.
pub fn build_body(m: &Morning) -> String {
    let mut s = String::from("# 朝のまとめ（dev → main）\n\n");
    s.push_str("このPRをマージすると、dev の内容がすべて main に入ります。判断はこの1回だけです。\n\n");
    if let Some(n) = &m.yomiyasu_note {
        s.push_str(&format!("> 注意: {n}\n\n"));
    }
    s.push_str("## いまの状態\n");
    s.push_str(&format!(
        "- dev の最新: `{}`\n- テスト: {}\n- 取り込んだ PR: {} 件\n",
        m.dev_head,
        if m.dev_green { "通っています" } else { "**失敗しています。マージしないでください**" },
        m.items.len()
    ));
    if m.iterations > 0 {
        s.push_str(&format!(
            "- 人が手を入れた割合: {}/{} 回（{}%）\n",
            m.human_touches,
            m.iterations,
            m.human_touches * 100 / m.iterations
        ));
    }
    s.push('\n');
    for (stratum, title) in [
        (Stratum::Attention, "## 特に注意（ガードや検証の仕組みに触れています）"),
        (Stratum::Check, "## 要確認（直しが入った、または中心のファイルに触れています）"),
        (Stratum::AsIs, "## そのまま（独立した変更で、一度でテストが通りました）"),
    ] {
        let group: Vec<&Item> = m
            .items
            .iter()
            .filter(|i| {
                let s = classify(&i.files, i.repair_attempts);
                let s = if s == Stratum::AsIs && !i.human_checks.is_empty() { Stratum::Check } else { s };
                s == stratum
            })
            .collect();
        if group.is_empty() {
            continue;
        }
        s.push_str(title);
        s.push_str("\n\n");
        for i in group {
            s.push_str(&render_item(i));
        }
    }
    if !m.human_notes.is_empty() {
        s.push_str("## 人が作った PR に対して行ったこと\n");
        for n in &m.human_notes {
            s.push_str(&format!("- {n}\n"));
        }
        s.push('\n');
    }
    let mut closes: Vec<u64> = m.items.iter().flat_map(|i| i.closes.iter().copied()).collect();
    closes.sort_unstable();
    closes.dedup();
    if !closes.is_empty() {
        s.push_str("## このマージで閉じる Issue\n");
        for n in closes {
            s.push_str(&format!("Closes #{n}\n"));
        }
        s.push('\n');
    }
    s.push_str("## 残っている問題と制限\n");
    if m.remaining.is_empty() {
        s.push_str("- 今回の実行では、記録された問題はありません。\n");
    }
    for r in &m.remaining {
        s.push_str(&format!("- {r}\n"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_closes_lookup_is_retried_then_surfaced() {
        let mut calls = 0;
        let ok = closes_with_retry(|| { calls += 1; if calls == 1 { Err("boom") } else { Ok("Closes #70".to_string()) } });
        assert_eq!(ok, Ok(vec![70]));
        let mut calls = 0;
        let err = closes_with_retry(|| { calls += 1; Err::<String, _>("boom") });
        assert_eq!((calls, err), (2, Err("boom".to_string())));
        let m = Morning { remaining: vec![lookup_failure_note(&[78, 81])], ..Default::default() };
        let body = build_body(&m);
        assert!(body.contains("#78, #81") && !body.contains("記録された問題はありません"));
    }

    #[test]
    fn nihongo_refinement_must_keep_facts() {
        let body = "- dev の最新: `abc123`\n### #12 t\n- リンク: https://github.com/o/r/pull/12\n";
        assert!(nihongo_prompt("SKILL", body).contains("SKILL"));
        let ok = "- dev の最新は `abc123` です。\n### #12 t\n- リンク: https://github.com/o/r/pull/12\n";
        assert!(facts_preserved(body, ok));
        assert!(!facts_preserved(body, "- dev の最新は `abc123` です。\n### #12 t\n"));
        assert!(!facts_preserved(body, "### #12 t\nhttps://github.com/o/r/pull/12"));
        assert_eq!(unfence("```markdown\nx\n```"), "x");
    }

    #[test]
    fn merge_only_when_all_conditions_hold() {
        use crate::run_loop::Verdict;
        let pass = Verdict::Pass;
        assert!(should_merge(&pass, "dev", "MERGEABLE", true));
        assert!(!should_merge(&Verdict::Fail, "dev", "MERGEABLE", true));
        assert!(!should_merge(&pass, "main", "MERGEABLE", true));
        assert!(!should_merge(&pass, "dev", "CONFLICTING", true));
        assert!(!should_merge(&pass, "dev", "UNKNOWN", true));
        assert!(!should_merge(&pass, "dev", "MERGEABLE", false));
    }

    #[test]
    fn human_only_partial_merges_blocked_partial_does_not() {
        use crate::run_loop::Verdict;
        let human = Verdict::Partial(vec![("a".into(), true)]);
        let blocked = Verdict::Partial(vec![("a".into(), true), ("b".into(), false)]);
        assert!(should_merge(&human, "dev", "MERGEABLE", true));
        assert!(!should_merge(&human, "main", "MERGEABLE", true));
        assert!(!should_merge(&blocked, "dev", "MERGEABLE", true));
    }

    #[test]
    fn human_only_item_listed_under_check_with_items() {
        let m = Morning {
            items: vec![Item {
                pr: 9,
                files: vec!["docs/a.md".into()],
                human_checks: vec!["文章が自然か".into()],
                ..Default::default()
            }],
            ..Default::default()
        };
        let b = build_body(&m);
        assert!(b.contains("## 要確認") && b.contains("文章が自然か"), "{b}");
        assert!(!b.contains("## そのまま"));
    }

    #[test]
    fn classifies_by_blast_radius() {
        let f = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(classify(&f(&["docs/index.html"]), 0), Stratum::AsIs);
        assert_eq!(classify(&f(&["docs/index.html"]), 1), Stratum::Check);
        assert_eq!(classify(&f(&["src/loop_cmd.rs"]), 0), Stratum::Check);
        assert_eq!(classify(&f(&["src/devflow.rs", "src/loop_cmd.rs"]), 0), Stratum::Attention);
    }

    #[test]
    fn extracts_sections() {
        let t = "## Summary\nx\n## Decisions\n- chose A\n## Assumptions\n- B holds\n## Other\nz";
        assert_eq!(extract_section(t, &["decision"]), "chose A");
        assert_eq!(extract_section(t, &["assumption"]), "B holds");
    }

    #[test]
    fn body_lists_loop_merged_and_manually_merged_prs() {
        let loop_item = Item { pr: 7, title: "ループの変更".into(), url: "https://example/pull/7".into(), files: vec!["docs/a.md".into()], ..Default::default() };
        let (pr, title) = pr_from_subject("手動の変更 (#67)").unwrap();
        let manual = Item { pr, title, url: "https://example/pull/67".into(), files: vec!["src/model.rs".into()], ..Default::default() };
        let stale = Item { pr: 5, ..Default::default() };
        let shipped = vec![loop_item.clone(), manual];
        let items = merge_shipped(vec![loop_item, stale], shipped);
        let b = build_body(&Morning { items, dev_green: true, ..Default::default() });
        assert!(b.contains("取り込んだ PR: 2 件"), "{b}");
        assert!(b.contains("### #7 ループの変更"), "{b}");
        assert!(b.contains("### #67 手動の変更"), "{b}");
        assert!(!b.contains("### #5 "), "{b}");
    }

    #[test]
    fn body_closes_only_issues_of_promoted_prs() {
        assert_eq!(closing_issues("Closes #44\n\nfixes #52, see #9"), vec![44, 52]);
        let it = |pr, closes: Vec<u64>| Item { pr, closes, ..Default::default() };
        let shipped = vec![it(67, vec![44]), it(68, vec![52]), it(74, vec![73])];
        let stale = it(60, vec![10]);
        let items = merge_shipped(vec![stale], shipped);
        let b = build_body(&Morning { items, ..Default::default() });
        for n in [44, 52, 73] {
            assert!(b.contains(&format!("Closes #{n}\n")), "{b}");
        }
        assert!(!b.contains("Closes #10"), "{b}");
        // Items recorded by an older binary carry no closes; live state fills them in.
        let old = vec![it(78, vec![]), it(81, vec![])];
        let live = vec![it(78, vec![70]), it(81, vec![76])];
        let b = build_body(&Morning { items: merge_shipped(old, live), ..Default::default() });
        assert!(b.contains("Closes #70\n") && b.contains("Closes #76\n"), "{b}");
        assert!(!build_body(&Morning::default()).contains("Closes"));
    }

    #[test]
    fn body_records_human_pr_repairs_and_escalations() {
        let notes = vec!["#9: 競合を直しました".to_string(), "#10: 人の確認待ち".to_string()];
        let b = build_body(&Morning { human_notes: notes, ..Default::default() });
        assert!(b.contains("人が作った PR に対して行ったこと") && b.contains("#9: 競合") && b.contains("#10: 人の"), "{b}");
        assert!(!build_body(&Morning::default()).contains("人が作った PR に対して"));
    }

    #[test]
    fn body_staleness_detected_by_named_dev_head() {
        let b = build_body(&Morning { dev_head: "abc123".into(), ..Default::default() });
        assert!(body_names_head(&b, "abc123"));
        assert!(!body_names_head(&b, "def456"));
    }

    #[test]
    fn morning_body_has_all_sections_in_japanese() {
        let m = Morning {
            items: vec![
                Item {
                    pr: 7,
                    title: "小さな変更".into(),
                    url: "https://example/pull/7".into(),
                    implemented: "ヘルプを追加".into(),
                    decisions: "名前を短くした".into(),
                    assumptions: "dev は常に緑".into(),
                    files: vec!["docs/a.md".into()],
                    ..Default::default()
                },
                Item {
                    pr: 8,
                    title: "人のPR".into(),
                    human: true,
                    files: vec!["src/model.rs".into()],
                    repair_note: Some("dev をマージして衝突を解消。テストは成功".into()),
                    ..Default::default()
                },
            ],
            remaining: vec!["シェル検査は仮の実装です".into()],
            dev_head: "abc123".into(),
            dev_green: true,
            iterations: 4,
            human_touches: 1,
            yomiyasu_note: None,
            human_notes: Vec::new(),
        };
        let b = build_body(&m);
        for needle in [
            "やったこと: ヘルプを追加",
            "判断したこと: 名前を短くした",
            "仮定したこと: dev は常に緑",
            "## 残っている問題と制限",
            "シェル検査は仮の実装です",
            "（人が作成）",
            "代わりに直したこと: dev をマージ",
            "通っています",
            "1/4 回（25%）",
            "## そのまま",
            "## 要確認",
        ] {
            assert!(b.contains(needle), "missing {needle}\n{b}");
        }
        let red = build_body(&Morning { dev_green: false, ..m });
        assert!(red.contains("マージしないでください"));
    }
}
