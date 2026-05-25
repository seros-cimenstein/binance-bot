# Codex Working Notes

## Session Pre-Prompt

At the start of every new Codex session in this repository, read these files
before making changes:

1. `CODEX.md`
2. `IMPLEMENTATION_PLAN.md`
3. `PROGRESS.md`

Treat `IMPLEMENTATION_PLAN.md` as the durable roadmap and historical scope.
Treat `PROGRESS.md` as the active cross-session handoff checklist. Update
`PROGRESS.md` at the end of every meaningful session with current status,
commands run, remaining tasks, and blockers. Only update
`IMPLEMENTATION_PLAN.md` when roadmap scope changes or a roadmap checkbox/status
needs correction.

Future Codex sessions should treat this repository as a roadmap-and-progress
driven project.

## Required Workflow

- Keep `IMPLEMENTATION_PLAN.md` as the source of truth for planned roadmap work.
- Keep `PROGRESS.md` as the source of truth for the current handoff state.
- Before starting a meaningful roadmap change, make sure the relevant plan
  section has checkbox items.
- As roadmap work completes, tick each finished item in `IMPLEMENTATION_PLAN.md`.
- As session work completes, update `PROGRESS.md` with the current state.
- Add a dated progress note when a phase or major slice is completed.
- Do not claim a plan item is complete unless tests or a concrete code path back
  up the claim.
- Prefer small implementation batches that end with verification.
- Run the relevant targeted tests first, then the full suite before handing off.
- Mention any tests that could not be run.

## Current Verification Commands

```bash
.venv/bin/python -m pytest -q
.venv/bin/python test_pipeline.py
```
