# Agent Instructions

Always read, in this order (short files):

1. `CODEX.md`: workflow and the live-money safety rules. Binding.
2. `PROGRESS.md`: the current state (is the bot live, what is deployed, what
   is next). The only place that state is kept.
3. `IMPLEMENTATION_PLAN.md`: the active phase and its checkboxes.

Read only when the task needs it:

| Question | File |
|---|---|
| How do I run, deploy, stop or inspect something? | `OPERATIONS.md` (jump to the section) |
| What does the binary do, which commands exist? | `README.md` |
| Why is a rule the way it is; exact operator wording? | `docs/decisions.md` |
| Touching legacy runtime, pool, canary, margin, futures, transfer, withdrawal, prediction, capital pool? | `docs/rules-other-modules.md` (binding) |
| History of a finished phase (1-50)? | `docs/archive/implementation-plan-phases-1-50.md` |
| What happened on a past date? | git log, then `docs/archive/progress-2026-05-25_to_2026-10-29.md` |
| An old loop ledger (`LOOP_*`)? | `docs/archive/loops/` |

Do not read the archive by default. Update `PROGRESS.md`, plan boxes and
`docs/decisions.md` as `CODEX.md`, "Required Workflow", describes, in the
same commit as the work.
