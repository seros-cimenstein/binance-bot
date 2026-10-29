# Operator Decision Log

The full wording and history of the operator decisions behind the Bot Spot
rules. `CODEX.md` holds the rules in force; this file says why and when.
Newest phase last. Append a new entry when the operator makes a decision; do
not rewrite old entries (mark one superseded instead).

These entries were moved verbatim from `CODEX.md` on 2026-10-29: "below" and
"above" in them mean the rules in `CODEX.md`.

## Bot Spot model and fallback

- Bot Spot model fallback (operator decisions 2026-10-13, 2026-10-15,
  2026-10-16 and 2026-10-19): the primary model (`BOT_SPOT_MODEL`) is
  `openai-codex/gpt-6.1-sol` at thinking `high` since 2026-10-16 (before
  that `anthropic/claude-sonnet-5-5` from 2026-10-14 and
  `anthropic/claude-haiku-5-5`), on a 20-minute deployed cycle since
  2026-10-15 so it fits the subscription allowance. Since 2026-10-19 there is
  no fallback model (operator: "remove backup", after the
  `google-antigravity/gemini-4-argon-high` fallback answered "quota
  reached"): `BOT_SPOT_FALLBACK_MODELS` is empty and the Pi's
  `omp-agent/config.yml` has `retry.modelFallback: false` and no
  `fallbackChains`. A failed turn is not retried on another model; the
  brain-down alert (`BOT_SPOT_BRAIN_DOWN_ALERT_CYCLES`) still fires. The
  failover code stays: when a fallback is configured, a fallback model is
  the trader for that turn with the same host tools, the same
  `execute_route` path and every hard-safety item below unchanged. Each
  login is made on the Pi with `omp login <provider>`, never a copied token.
  Adding a fallback or another model or provider needs a new operator
  decision.

## Phase 45 exit plans and prompt tags v11-v14

- Phase 45 bot-chosen exit plans (operator approval 2026-10-14): The bot may
  pick a sell plan per position from a fixed, deterministic menu (follow the
  peak by percent or ATR, take part, stretch above the Bollinger band,
  slowdown, momentum fade, deadline) by setting its parameters; The bot
  never writes or runs code. Approved: the historical replay
  (`bb bot-spot-exit-replay`) and, after the operator reviewed it, the
  paper trial: the optional `set_exit_plan` host tool and the
  `bot-spot-exit-watch` watcher, which records would-be sells and must
  never place an order or hold an order path. The prompt may offer the tool
  but must not require exit plans.
- Phase 45 live exit plans (operator decision 2026-10-15, before a paper
  run, after the 40-trade backtest): with `BOT_SPOT_EXIT_PLANS_LIVE=true`
  the `bot-spot` daemon follows positions between cycles and sells a
  position into USDT when its plan (The bot's from `set_exit_plan`, or the
  default `BOT_SPOT_DEFAULT_EXIT_PLAN`, deployed `deadline=120` since
  2026-10-17 (was `stretch=2:1,atr=3:1h`),
  for positions opened from `BOT_SPOT_DEFAULT_EXIT_PLAN_FROM`) fires.
  These sells use `Runtime::exit_sell`: the cycle lock (never during a
  bot route), live arming, the emergency stop file, the failed-route
  halt, operator scope, `order/test` per leg, exchange filters, fill
  reconciliation, and one durable audit row (`cycle_id` `exit-plan`) before
  the alert; an ambiguous sell halts live trading like any route. Only the
  trader writes the watch files then; the paper watcher refuses to start.
  Exit plans only sell, never buy. Changing the default plan or the menu
  needs a new operator decision. Operator decision 2026-10-17 (branch
  `old-gold-way`): back to the v7 system after the fixed plan (2026-10-16)
  and locked rules (2026-10-17) cut winners short. The bot's `set_exit_plan`
  (full menu, deadline included; setting again replaces, `clear` removes)
  overrides the default per position, and the system prompt is the v7 text
  (tag `bot-spot-v11-agentic`). Kept from 2026-10-17: `stretch` sells
  only when its line is above break-even (entry plus buy and sell fees at
  the trip's fee rate), and the cycle prompt and trade history show the bot
  which rule sold each position and the result after fees (information
  only). The trader session was restored to the v7 conversation up to the
  last gpt-6.1-sol cycle before Sonnet took over (cycle 542, 2026-10-16
  05:02 UTC), with the journal cut at the same point. Tag
  `bot-spot-v12-agentic` (2026-10-18) keeps the v7 system prompt and changes
  only information in the cycle message: the route table's trailing momentum
  is no longer labelled "edge" (it is a past move, not a forecast). Tag
  `bot-spot-v13-agentic` (operator approval 2026-10-18, Phase 47) adds 20-coin
  market breadth (`15m/1h/4h/24h`), hold-duration buckets and `+4h` entry timing
  to the cycle prompt, switches the candidate table to `liquid_top_value`
  (excluding stablecoins, fiat and `TRD_GRP_261` tokenized stocks as buy
  targets), updates the default exit plan to `atr=3.5:1h,deadline=360`, and
  replaces the `"Holding is a decision, not a default"` line in the system
  prompt with asymmetric regime/swing guidance (information and guidance only,
  never a blocking gate). Tag `bot-spot-v14-agentic` (operator decision
  2026-10-19) changes only information in the cycle message: live, the
  exit-plan section ends with the real results of the positions a plan sold
  ("positions sold by a plan: N (W wins), net X USDT after fees") instead of
  "plan vs your own exits", which in live mode only measured trigger price
  against fill.

## Phases 49-50 schizo trader and long-term value voice

- Phase 49 schizo trader (operator decisions 2026-10-20,
  `docs/archive/loops/LOOP_SCHIZO_LIVE.md`).
  The operator asked for a "schizo" Bot Spot: "it is not schizo enough", "ai
  needs to be schizoified or become schizo thats it", "on top of those it looks
  at gcp2 data for reference …", and "Backtest, then trade live as schizo", with
  GCP2 as "Mood ring" and "YOLO button".
  - The LLM trader stays the trader. It becomes schizo through its prompt and
    a per-cycle state: an active mood (one of the Phase 48 personas), dice and
    the personas' votes. Its behaviour is not replaced by rules, and it still
    acts only through the host tools; `execute_route` stays the only order path.
  - GCP2 (gcp2.net, Global Consciousness Project 2.0) public data is read-only
    information. It may be read without login (public colour zones,
    per-minute aggregate, monthly history) and must never be posted to or
    logged into. Mood switches are bound to GCP2 only ("nothing hardcoded …
    we are going to be gcp bound in mood switches"): no timer, no fixed
    switch probability, no fixed seed. The zone bounds come from GCP2's own
    `getcolorzones`, and the dice come from GCP2 readings.
  - The mood follows GCP2 (operator decision 2026-10-20, after the first
    inverted deploy: "reverse mood change meaning if gcp is normal he is
    normal aight inverse of what it is rn"): the bot switches mood when the
    GCP2 zone changed between cycles (up or down) and keeps it while the zone
    stays the same. The first deploy ran it inverted ("mood is inverted not
    yolo": switch on the same zone, keep on a change). A failed GCP2 read means
    no mood switch for that cycle.
  - YOLO is removed (operator decision 2026-10-21: "remove yolo as well if it
    still persists it is a stupid decision making way"). Until then, while
    GCP2 was Very High or Extreme, the bot was told to sell other holdings and
    go all-in on the hottest liquid coin. Bringing it back needs a new operator
    decision.
  - There is no panic sell ("no panic sell"). The 2026-10-14 no-stop-loss rule
    above stays in force, and exit plans keep the existing menu and default.
  - Go-live is not gated on the backtest ("go live no tiny shit whatever we
    said earlier will be live"): the schizo trader runs on the full wallet with
    no size cap.
  - Every Bot Spot hard-safety item below stays unchanged.
  - Tag `bot-spot-v15-schizo` (`BOT_SPOT_GCP2_ENABLED=true`, the default) was
    the first schizo trader: the system prompt replaces the v13 regime/swing
    bullets with the four personalities, the inverted GCP2 mood ring, the
    voices (votes), the one-coin all-cash bet and the YOLO order, and the
    cycle message opens with `## Your schizo state`. Its no-stop-loss,
    exit-plan, hard-safety and tool text is the v14 text. Tag
    `bot-spot-v16-schizo` changes only the mood ring: the mood follows GCP2.
    `BOT_SPOT_GCP2_ENABLED=false` gives the v14 prompts.
- Phase 50 long-term value voice (operator decisions 2026-10-21): "we need to
  create a reward function alike system that has gamma value around 0.6 or
  something that would make him more wanting to achieve longer term rewards
  that influences directly the ai's decisions", "we will use markov's stuff
  closer to it", "backtest as well". Chosen through the ask tool: γ = 0.95 per
  20-minute cycle (the operator's 0.6 per cycle would have a 50-minute
  horizon, shorter than the trader's holds), a strong voice in the prompt, and
  live directly.
  - `bb bot-spot-mdp-fit` fits a Markov decision process on cached 1m candles
    (`crate::mdp`): states of a held coin (gain since entry, time held, 1h
    move, market breadth), hold or sell, rewards in percent after costs, value
    iteration with γ. The trader reads the model file
    (`BOT_SPOT_MDP_MODEL_PATH`) every cycle and shows Q(hold) against Q(sell)
    for the held coin and the value of buying each liquid coin
    (`## Your long-term value`, audit field `mdp`).
  - The voice is information and guidance for the LLM, never a gate: the
    system prompt calls it the strongest voice and asks for a written reason
    to act against it, and the bot still decides. A missing or unreadable
    model means "voice unavailable" for that cycle.
  - Tag `bot-spot-v17-schizo` (`BOT_SPOT_MDP_ENABLED=true`, the default) is
    v16 plus this voice and minus YOLO. The no-stop-loss rule, exit plans and
    hard safety are unchanged.
  - The backtest persona `schizo_mdp` (offline only) lets the model overrule
    the votes; its model must be fitted on data that ends before the window.

## Phase 51 rule-based decider

Active: the decision wording and the rules are in `CODEX.md`, "Phase 51
rule-based decider". Move them here when the phase is finished.
