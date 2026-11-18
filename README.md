# actmaton
**Build while you sleep. Steer when you wake.**

Actmaton is a local-first, always-on development orchestrator that puts otherwise idle computing power—like a Mac at home overnight—to work. It uses local models for implementation and testing, and powerful cloud models for planning and difficult decisions, keeping development moving while optimizing cloud LLM costs. Humans steer the product through periodic feedback rather than approving every task.

Actmaton's goal is to become a good autonomous coding agent. As an early proof, it develops itself: dogfooding on its own codebase from the very start.

## How it works

Actmaton uses a Rust/Tokio control loop and the [GitHub Copilot SDK](https://github.com/github/copilot-sdk) to run one development task at a time:

1. **Direct:** a cloud LLM discovers or prioritizes work, records it in a GitHub Issue, and defines acceptance criteria.
2. **Develop:** an LLM implements and tests the change in an isolated Git worktree. The first loop uses a cloud model; local models come later.
3. **Review:** Actmaton opens a draft pull request; CI checks the changes and a cloud LLM reviews when useful.
4. **Continue:** human feedback in GitHub informs the next decision, while routine development proceeds between check-ins.

GitHub Issues, PRs, and CI hold the backlog, decisions, and results. The initial target is a **16 GB Apple Silicon Mac mini or MacBook**, using [TUFF](https://github.com/tuff-app/TUFF) to serve the local coding model.

### Loop verdict

`actmaton loop` records one verdict per iteration: **FAIL** when `cargo test` fails; **PASS** when tests pass and every item under the issue's "Done criteria"/"Acceptance" section is reported `VERIFIED` by the session (also when the issue states none); otherwise **PARTIAL**. The session must end with one `CRITERION <n>: VERIFIED|NOT-VERIFIED|HUMAN-CHECK - <how/why>` line per criterion; unaddressed criteria count as not verified. PARTIAL runs still open a draft PR, with the unverified criteria (marked `human-check` where not machine-checkable) listed in the loop report and PR body.

### Loop resource notes

Memory, not CPU, is the binding constraint on small hosts (an 8 GB VM hit the OOM killer twice in unattended runs). The loop's own `cargo test` verification therefore runs with `CARGO_BUILD_JOBS=1` and `CARGO_PROFILE_DEV_DEBUG=0` by default, and the run log records the settings used. This trades wall time (slower builds) for lower peak memory. Set either variable in the environment to override it, e.g. `CARGO_BUILD_JOBS=8` on a roomy machine.

Measured limit: the single `rustc` compiling `github-copilot-sdk` peaks at about 5.1 GB RSS regardless of these settings. Disabling the SDK's default features (`default-features = false`) only lowered it to about 4.3 GB, so it was not adopted. To avoid paying that peak repeatedly, the loop shares one `CARGO_TARGET_DIR` across all issue worktrees by default: `actmaton-target`, next to the clone (alongside the `actmaton-issue-N` worktrees). Dependencies such as the SDK compile once and later runs build incrementally. The loop's verification and the sessions it spawns both use it, and the run log records it. Set `CARGO_TARGET_DIR` in the environment to use another location. The loop never cleans this directory; prune it manually if it grows too large.

## Roadmap

- [x] **M0 — Seed:** the minimal agent loop, built from outside Actmaton: read one Issue, edit in a worktree, test, open a draft PR.
- [x] **M1 — First self-improvement:** Actmaton improves Actmaton itself; a human reviews and merges.
- [ ] **Sandbox:** OS-level isolation for the agent's tool execution (no credentials, no network, worktree-only writes), so runs can go unattended.
- [x] **M2 — Self-proposal:** Actmaton files its own improvement Issues; a human selects.
- [x] **M3 — Repeated loop:** self-improvement becomes repeatable, with idempotency, bounded retries, and cost/time records.
- [ ] **M4 — Autonomy and local models:** always-on operation, richer direction, and local model support for the 16 GB Apple Silicon target.
