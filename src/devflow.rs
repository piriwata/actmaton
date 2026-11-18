//! Dev-branch flow: the loop auto-merges green work into `dev`; the human
//! merges `dev` → `main` once, via a standing PR whose body is built here.
//! Merging into main is never done by actmaton.

pub const DEV: &str = "dev";

/// Merge into dev only when the run passed, the PR targets dev, git sees no
/// conflict, and dev's tree is green.
pub fn should_merge(verdict_pass: bool, base: &str, mergeable: &str, dev_green: bool) -> bool {
    verdict_pass && base == DEV && mergeable == "MERGEABLE" && dev_green
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
    /// What the orchestrator did on a human PR's behalf (conflict repair).
    pub repair_note: Option<String>,
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

fn render_item(i: &Item) -> String {
    let who = if i.human { "（人が作成）" } else { "" };
    let mut s = format!("### #{} {}{}\n- リンク: {}\n", i.pr, i.title, who, i.url);
    s.push_str(&format!("- やったこと: {}\n", or_none(&i.implemented)));
    s.push_str(&format!("- 判断したこと: {}\n", or_none(&i.decisions)));
    s.push_str(&format!("- 仮定したこと: {}\n", or_none(&i.assumptions)));
    if i.repair_attempts > 0 {
        s.push_str(&format!("- テストの直しが {} 回ありました。\n", i.repair_attempts));
    }
    if let Some(n) = &i.repair_note {
        s.push_str(&format!("- 代わりに直したこと: {n}\n"));
    }
    s.push('\n');
    s
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
        let group: Vec<&Item> =
            m.items.iter().filter(|i| classify(&i.files, i.repair_attempts) == stratum).collect();
        if group.is_empty() {
            continue;
        }
        s.push_str(title);
        s.push_str("\n\n");
        for i in group {
            s.push_str(&render_item(i));
        }
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
    fn merge_only_when_all_conditions_hold() {
        assert!(should_merge(true, "dev", "MERGEABLE", true));
        assert!(!should_merge(false, "dev", "MERGEABLE", true));
        assert!(!should_merge(true, "main", "MERGEABLE", true));
        assert!(!should_merge(true, "dev", "CONFLICTING", true));
        assert!(!should_merge(true, "dev", "UNKNOWN", true));
        assert!(!should_merge(true, "dev", "MERGEABLE", false));
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
