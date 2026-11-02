# Tagger audit, 2026-11-02

Operator, 2026-11-02: "lets address these issues" (AI accuracy was one of them;
`docs/decisions.md`, "2026-11-02 tagging fixes").

## Method

- Sample: 200 /biz/ posts from the last 7 days (seed 20261102): 100 the AI
  tagged with a tradable Binance coin, 100 at random.
- Reference: an agent labelled each post blind (it never saw the AI tags):
  assets with stance up/down/neutral, sarcasm, the poster's own mood.
  **Agent labels, not human labels**; 5 posts it marked sarcastic, and it noted
  its doubt on 10 (protocol vs token, ironic one-word replies, unnamed metals).
- Compared: the cached `tags-1` tags against the same posts re-tagged with
  `tags-2` (two calls of 80 and 120 posts, Gemini 3.8 Flash).
- Asset names matched after upper-casing, dropping `$` and a few aliases
  (Bitcoin to BTC, SPX to SPY); coins are those with a Binance spot market.
- Files: `~/bb-tuning/tagger-audit/` (`audit-sample.json`, `audit-key.json`,
  `labels.json`, `score.py`, `scores.json`).

## Result

| Measure | tags-1 | tags-2 |
|---|---|---|
| Coin precision | 84% | 91% |
| Coin recall | 95% | 93% |
| Coin direction agreement | 77% (75/97) | 82% (78/95) |
| Asset direction agreement, all assets | 83% | 86% |
| Up against down, opposite calls | 4 | 3 |
| Mood polarity agreement | 86% | 90% |
| Bearish : bullish moods (labels 9 : 12) | 16 : 2 | 14 : 6 |
| Moods on posts the labels call mood-free | 12 | 10 |

- tags-1's coin direction misses are mostly softness, not sign errors: the
  labels' "up" read as neutral (9 of 22), neutral read as down (6).
- Sarcasm: tags-1 read 2 of the 5 sarcastic posts the wrong way round (38,
  63); tags-2 fixes 63 and keeps 38 (a reply mocking a bitcoin-death link,
  which the labeller itself flagged as two-way).
- A first tags-2 draft tagged 30 bare calls ("Bullish", "so bullish xrp",
  "-10% day tomorrow") as moods; the shipped prompt names those cases as not
  moods.
- Macro risk: of 14 macro tags in the sample, 8 carry a side (5 down, 3 up),
  for example "war easing" up and "recession growing" down.

## Limits

- One run per prompt: a model rerun moves single posts, so differences of
  2 or 3 posts are within noise. The bearish skew of tags-1 moods is the clear
  finding.
- One labeller, an agent. The operator may spot-check any of the 200 posts.
