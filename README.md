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

## Roadmap

- **M0 — Seed:** the minimal agent loop, built from outside Actmaton: read one Issue, edit in a worktree, test, open a draft PR.
- **M1 — First self-improvement:** Actmaton improves Actmaton itself; a human reviews and merges.
- **M2 — Self-proposal:** Actmaton files its own improvement Issues; a human selects.
- **M3 — Repeated loop:** self-improvement becomes repeatable, with idempotency, bounded retries, and cost/time records.
- **M4 — Autonomy and local models:** always-on operation, richer direction, and local model support for the 16 GB Apple Silicon target.
