# Self-review checklist

Used by the post-iteration self-review (deterministic checks in `src/review.rs`
and the skeptical reviewer prompt). Each class must cite a real past failure;
new classes enter only with repo-internal evidence (no speculative rules), and
their failure is added as a fixture test in `src/review.rs`.

- `stale-status-wording` — PR body says changes are "uncommitted" though the branch was committed and pushed (loop run 1).
- `report-run-log-mismatch` — loop report says `model: default` while the run log has a resolved model (loop run 1).
- `closing-keyword` — PR/commit uses `Refs #n` instead of `Closes #n`, so merge does not close the issue (loop run 1).
- `missing-criterion-status` — the final message lacks a per-criterion status line for every stated done criterion; the verdict should be PARTIAL (#48, #44, #56). Prevented by: the repair prompt re-states the `CRITERION <n>: …` format (the repair reply replaces the final message), and the check accepts that prompted form.
- `unchecked-criterion-passed` — the final message admits an unchecked criterion but the verdict is PASS (loop run 1, #48).
- `stale-standing-body` — the standing PR body names a dev head that no longer matches `origin/dev`, so its claims may already be stale (loop run 22, #80).
- `unverified-landing-claim` — the final message says a feature/file is "on main" but git shows the file is absent from `origin/main` (merged into dev, not main; loop run 11, #71).

Every finding must cite evidence (a quoted line, file:line, or log line). No evidence, no finding.
