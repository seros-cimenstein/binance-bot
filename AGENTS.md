# Agent Instructions

Repo: the Binance Spot algorithm trader (Rust binary `bb`) and its read-only
dashboard (`cryptobot-gui`). Live host: the Pi (`ssh pi-wired`).

Always read, in this order (short files):

1. `CODEX.md`: the rules (workflow, live-money safety). Binding.
2. `PROGRESS.md`: the current state: is the bot live, what is deployed, what
   is next. The only place that state is kept.
3. `IMPLEMENTATION_PLAN.md`: the active phase and its checkboxes.
4. `LOOP_*.md` at the repo root, if one exists: an active loop and its
   Protocol.

Read only when the task needs it:

| Question | File |
|---|---|
| How do I run, deploy, stop or inspect something? | `OPERATIONS.md` (jump to the section) |
| What does the binary do, which commands exist? | `README.md` |
| Why is a rule the way it is; exact operator wording? | `docs/decisions.md` |
| History of a finished phase (1-50)? | `docs/archive/implementation-plan-phases-1-50.md` |
| Phase 52 (nonstop algorithm trader, /biz/ lead-study verdict)? | `docs/archive/implementation-plan-phase-52.md` |
| Phase 53 (tags feed contract, scoreboard, rule file, sleeves)? | `docs/archive/implementation-plan-phase-53.md` |
| The old LLM trader's rules, decisions, Phase 51? | `docs/archive/llm-trader-era-to-2026-10-29.md` |
| What happened on a past date? | git log, then `docs/archive/progress-2026-05-25_to_2026-10-29.md` |
| A finished loop ledger? | `docs/archive/loops/` |

Do not read `docs/archive/` by default. Keep `PROGRESS.md`, the plan boxes and
`docs/decisions.md` current as `CODEX.md`, "Required Workflow", says.
