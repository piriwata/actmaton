# actmaton
**Build while you sleep. Steer when you wake.**

Actmaton is a local-first, always-on development orchestrator that puts otherwise idle computing power—like a Mac at home overnight—to work. It uses local models for implementation and testing, and powerful cloud models for planning and difficult decisions, keeping development moving while optimizing cloud LLM costs. Humans steer the product through periodic feedback rather than approving every task.

## How it works

Actmaton uses a Rust/Tokio control loop and the [GitHub Copilot SDK](https://github.com/github/copilot-sdk) to run one development task at a time:

1. **Direct:** a cloud LLM discovers or prioritizes work, records it in a GitHub Issue, and defines acceptance criteria.
2. **Develop:** a local LLM implements and tests the change in an isolated Git worktree.
3. **Review:** Actmaton opens a draft pull request; CI checks the changes and a cloud LLM reviews when useful.
4. **Continue:** human feedback in GitHub informs the next decision, while routine development proceeds between check-ins.

GitHub Issues, PRs, and CI hold the backlog, decisions, and results. Actmaton uses the [`gh` CLI](https://cli.github.com/) for routine GitHub operations and the [GitHub MCP Server](https://github.com/github/github-mcp-server) for agent-driven interactions. `launchd` keeps the Rust process running on macOS.

The initial target is a **16 GB Apple Silicon Mac mini or MacBook**, using [TUFF](https://github.com/tuff-app/TUFF) to serve the local coding model. The model endpoint is configurable so other local runtimes can also be used.

## Roadmap

### Milestone 1 — Prove the agent loop

- [ ] Connect the Rust Copilot SDK to a local coding model, initially served with TUFF.
- [ ] Verify streaming, tool calls, code edits, and tests in a representative repository.
- [ ] Read one GitHub Issue and execute it in a dedicated worktree.
- [ ] Publish test evidence and create a draft PR.
- [ ] Confirm that restart/retry does not duplicate external work.

### Milestone 2 — Cloud direction and review

- [ ] Add configurable Director and Reviewer sessions using cloud models.
- [ ] Make decisions and acceptance criteria structured and inspectable.
- [ ] Continue from one ready issue to the next without human approval for routine work.
- [ ] Add failure classification, bounded retries, and escalation.

### Milestone 3 — Human steering and continuous operation

- [ ] Consume new human directions from GitHub comments or discussions.
- [ ] Produce scheduled progress and direction briefings.
- [ ] Record model usage, cost, completion time, review findings, and rework.
- [ ] Run unattended via `launchd` and test safe restart behavior.
