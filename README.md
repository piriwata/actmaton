# actmaton
**Build while you sleep. Steer when you wake.**

Actmaton is a local-first, always-on development orchestrator that puts otherwise idle computing power—like a Mac at home overnight—to work. It uses local models for implementation and testing, and powerful cloud models for planning and difficult decisions, keeping development moving while optimizing cloud LLM costs. Humans steer the product through periodic feedback rather than approving every task.

Actmaton's goal is to become a good autonomous coding agent. Actmaton improving Actmaton itself is not that goal — it is the story: a dogfooding route that puts the agent to work on a real codebase, its own, from the very beginning. Development is therefore ordered by the shortest path to that first self-improvement loop, because a loop that can improve Actmaton is strong evidence that the underlying autonomous coding agent works. The first loop starts with a cloud model, targets only this repository, and relies on human review before anything is merged. Local-model support is deliberately not a blocker for that first loop.

## How it works

Actmaton uses a Rust/Tokio control loop and the [GitHub Copilot SDK](https://github.com/github/copilot-sdk) to run one development task at a time:

1. **Direct:** a cloud LLM discovers or prioritizes work, records it in a GitHub Issue, and defines acceptance criteria.
2. **Develop:** an LLM implements and tests the change in an isolated Git worktree.
3. **Review:** Actmaton opens a draft pull request; CI checks the changes and a cloud LLM reviews when useful.
4. **Continue:** human feedback in GitHub informs the next decision, while routine development proceeds between check-ins.

GitHub Issues, PRs, and CI hold the backlog, decisions, and results. Actmaton uses the [`gh` CLI](https://cli.github.com/) for routine GitHub operations and the [GitHub MCP Server](https://github.com/github/github-mcp-server) for agent-driven interactions. `launchd` keeps the Rust process running on macOS.

The initial target is a **16 GB Apple Silicon Mac mini or MacBook**, using [TUFF](https://github.com/tuff-app/TUFF) to serve the local coding model. The model endpoint is configurable so other local runtimes can also be used.

## Roadmap

The roadmap serves the goal above — a good autonomous coding agent. It is ordered by the shortest path to the self-improvement story, with Actmaton working on Actmaton as an early, end-to-end proof of the agent loop on a real codebase, rather than as the goal itself.

Early safety relies on simple guardrails instead of a complete autonomy framework:

- Actmaton targets only the `actmaton` repository.
- Pull requests are always drafts.
- A human decides whether a pull request is merged.

### Milestone 0 — Seed

Build the minimal implementation from outside Actmaton, so Actmaton has something to improve.

- [ ] Scaffold the Rust/Tokio project and its configuration.
- [ ] Connect the Rust Copilot SDK to a cloud model and verify streaming and tool calls. Local models are explicitly out of scope for this milestone.
- [ ] Read one specified GitHub Issue.
- [ ] Create an isolated worktree and branch, make the change, and run `cargo test`.
- [ ] Publish test evidence and open a draft pull request.

### Milestone 1 — First self-improvement

Close the loop for the first time.

- [ ] Actmaton processes an Issue in the `actmaton` repository and opens a draft pull request that improves Actmaton itself.
- [ ] A human reviews and merges the pull request.
- [ ] A later run uses the merged improvement. When this happens, the self-improvement loop is closed.

The first tasks should be small improvements that make Actmaton itself easier to run, observe, or test — for example, adding a test, improving an error message, or improving logging.

### Milestone 2 — Self-proposal

Actmaton stops being only an executor.

- [ ] Actmaton creates GitHub Issues proposing its own improvements, with acceptance criteria.
- [ ] A human selects which proposals Actmaton should execute.

### Milestone 3 — Repeated loop

Make self-improvement repeatable instead of a one-off demonstration.

- [ ] Complete several distinct self-improvements in a row without duplicate or runaway work.
- [ ] Add idempotency, bounded retries, and failure classification.
- [ ] Record model usage, cost, completion time, review findings, and rework.

### Milestone 4 — Autonomy and local models

Expand autonomy only after the loop above is proven.

- [ ] Add configurable Director and Reviewer sessions using cloud models, and make decisions and acceptance criteria structured and inspectable.
- [ ] Consume new human directions from GitHub comments or discussions.
- [ ] Produce scheduled progress and direction briefings.
- [ ] Run unattended via `launchd` and test safe restart behaviour.
- [ ] Add local model support through BYOK (for example, served with TUFF) and optimise for the 16 GB Apple Silicon target.
