//! The live trader's schizo state (Phase 49, `docs/archive/loops/LOOP_SCHIZO_LIVE.md` C2).
//!
//! Each cycle runs the backtest `schizo` persona's rules on live inputs, as information for
//! the LLM trader (it still decides and acts only through the host tools):
//!
//! - **Mood ring, following GCP2** ([`next_mood`]): the active mood is one of the four
//!   base personas. When GCP2's zone changed (up or down) since the previous cycle the mood
//!   switches to another one picked by the GCP2 dice; when the zone is the same, or a
//!   reading is missing now or at the previous cycle, the mood is kept. The first reading
//!   picks the mood with the dice. No timer, no seed.
//! - **Dice:** the GCP2 dice of the latest minute ([`crate::gcp2::dice`]).
//! - **Votes** ([`Votes::cast`]): the four base personas decide on the live book; the
//!   active mood's vote counts 2.
//!
//! The mood and the previous cycle's zone are persisted next to the agent
//! state ([`STATE_FILE`]) after the cycle's audit row is written, so they survive restarts.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::gcp2::Gcp2View;
use crate::book::{Book, Snapshot};
use crate::persona_backtest::personas::{
    MoodStep, Votes, base_personas, next_mood,
};

/// The persisted schizo memory, next to `bot-spot-agent-state.json`.
pub const STATE_FILE: &str = "bot-spot-schizo-state.json";

/// [`STATE_FILE`] in the directory of the agent state file (`BOT_SPOT_STATE_PATH`).
pub fn state_path(agent_state_path: &Path) -> PathBuf {
    agent_state_path.with_file_name(STATE_FILE)
}

/// What one cycle hands the next.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchizoMemory {
    /// The active mood (`None` before the first GCP2 reading).
    pub mood: Option<String>,
    /// The cycle's GCP2 zone index (`None` when it had no reading).
    pub zone: Option<usize>,
    pub zone_label: Option<String>,
    pub updated_at: String,
}

impl SchizoMemory {
    /// A missing file is a fresh memory; an unreadable one is an error.
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read(path) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        Ok(bb_core::fsutil::write_json_atomic(path, self)?)
    }
}

/// The GCP2 reading as the state records it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Gcp2Brief {
    pub value: f64,
    pub zone: String,
    /// The previous cycle's zone (`None` when it had no reading or there was none).
    pub previous_zone: Option<String>,
    /// Last second of the hour the value covers (epoch seconds).
    pub end_s: i64,
}

/// One cycle's schizo state; serialized as the audit row's `schizo` object.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SchizoState {
    pub mood: Option<String>,
    pub previous_mood: Option<String>,
    /// `initial`, `switched`, `held` or `no reading`.
    pub mood_step: &'static str,
    pub mood_reason: String,
    /// `None`: GCP2 unavailable this cycle.
    pub gcp2: Option<Gcp2Brief>,
    pub dice: Option<u64>,
    /// `None` when the market breadth (the personas' view) was unavailable.
    pub votes: Option<Votes>,
    /// Flat book: the coin the weighted votes buy.
    pub vote_buy: Option<String>,
    /// Held coins with at least 2 weighted sell votes.
    pub vote_sell: Vec<String>,
    /// Saved after the cycle's audit row is written.
    #[serde(skip)]
    pub memory: SchizoMemory,
}

impl SchizoState {
    /// Advance `memory` by one cycle with this cycle's GCP2 reading, personas' view and
    /// live book.
    pub fn next(
        memory: &SchizoMemory,
        gcp2: Option<&Gcp2View>,
        snap: Option<&Snapshot>,
        book: &Book,
        now: String,
    ) -> Result<Self> {
        let mut voters = base_personas()?;
        let names: Vec<String> = voters.iter().map(|v| v.name().to_string()).collect();
        let active = memory
            .mood
            .as_deref()
            .and_then(|m| names.iter().position(|n| n == m));
        let (next, step) = next_mood(
            active,
            names.len(),
            gcp2.map(|v| v.zone),
            memory.zone,
            gcp2.map(|v| v.dice),
        );
        let mood = next.map(|i| names[i].clone());
        let previous_mood = active.map(|i| names[i].clone());
        let mood_reason = reason(
            step,
            gcp2,
            memory,
            mood.as_deref(),
            previous_mood.as_deref(),
        );

        let votes = snap.map(|s| Votes::cast(&mut voters, next, s, book));
        let (vote_buy, vote_sell) = match (&votes, snap) {
            (Some(v), Some(s)) if book.is_flat() => (v.pick(s).cloned(), Vec::new()),
            (Some(v), _) => (None, v.sells(book).into_iter().cloned().collect()),
            _ => (None, Vec::new()),
        };

        Ok(Self {
            mood: mood.clone(),
            previous_mood,
            mood_step: step.as_str(),
            mood_reason,
            gcp2: gcp2.map(|v| Gcp2Brief {
                value: v.value(),
                zone: v.zone_label.clone(),
                previous_zone: memory.zone_label.clone(),
                end_s: v.end_s,
            }),
            dice: gcp2.map(|v| v.dice),
            votes,
            vote_buy,
            vote_sell,
            memory: SchizoMemory {
                mood,
                zone: gcp2.map(|v| v.zone),
                zone_label: gcp2.map(|v| v.zone_label.clone()),
                updated_at: now,
            },
        })
    }
}

fn reason(
    step: MoodStep,
    gcp2: Option<&Gcp2View>,
    memory: &SchizoMemory,
    mood: Option<&str>,
    previous: Option<&str>,
) -> String {
    let mood = mood.unwrap_or("none");
    let zone = gcp2.map_or("?", |v| v.zone_label.as_str());
    let dice = gcp2.map_or(0, |v| v.dice);
    match step {
        MoodStep::Initial => {
            format!("first GCP2 reading ({zone}): the GCP2 dice {dice} picked {mood}")
        }
        MoodStep::Switched => format!(
            "GCP2 moved {} -> {zone} since the last cycle, so the mood switches: the GCP2 dice {dice} turned {} into {mood}",
            memory.zone_label.as_deref().unwrap_or("?"),
            previous.unwrap_or("none")
        ),
        MoodStep::Held => format!(
            "GCP2 stayed {zone} since the last cycle, so the mood holds: still {mood}"
        ),
        MoodStep::NoReading if gcp2.is_none() => {
            format!("GCP2 unavailable this cycle: no mood switch; still {mood}")
        }
        MoodStep::NoReading => {
            format!("no GCP2 reading at the last cycle to compare with ({zone} now): still {mood}")
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;


    use super::*;
    use crate::gcp2::{ColorZones, Gcp2Reading, parse_color_zones};
    use crate::book::{CoinView, Holding};

    const T: i64 = 1_792_487_580_000;

    fn zones() -> ColorZones {
        parse_color_zones(
            &serde_json::json!({"colorzones": [
                {"label": "Normal", "range_start": 0, "range_end": 140},
                {"label": "Elevated", "range_start": 140, "range_end": 166},
                {"label": "High", "range_start": 166, "range_end": 219},
                {"label": "Very High", "range_start": 219, "range_end": 279},
                {"label": "Extreme", "range_start": 279, "range_end": 10000}
            ]}),
            "t".into(),
        )
        .unwrap()
    }

    fn view(value: f64, end_s: i64) -> Gcp2View {
        let units = (value * 10_000.0).round() as u64;
        Gcp2View::from_reading(Gcp2Reading { end_s, units }, &zones())
    }

    fn coin(r15: f64, r1h: f64, r4h: f64, volume: f64) -> CoinView {
        CoinView {
            ret_15m: Some(r15),
            ret_1h: Some(r1h),
            ret_4h: Some(r4h),
            quote_volume_24h: volume,
        }
    }

    /// A bullish market: SOL leads 1h, ETH leads 4h; breadth +40.
    fn snap() -> Snapshot {
        let coins: BTreeMap<String, CoinView> = [
            ("BTC", coin(0.1, 0.3, 1.5, 9e9)),
            ("ETH", coin(0.2, 0.6, 3.0, 5e9)),
            ("SOL", coin(0.4, 2.0, 2.5, 2e9)),
        ]
        .into_iter()
        .map(|(a, c)| (a.to_string(), c))
        .collect();
        Snapshot {
            t: T,
            score: Some(40.0),
            horizons: vec![crate::market_mood::HorizonBreadth {
                name: "4h",
                weight: 0.3,
                up: 3,
                down: 0,
                flat: 0,
                breadth: 1.0,
                median_return_pct: Some(2.5),
            }],
            coins,
            liquid_top: vec!["BTC".into(), "ETH".into(), "SOL".into()],
            gcp2: None,
        }
    }

    fn flat() -> Book {
        Book {
            t: T,
            cash: 100.0,
            equity: 100.0,
            holdings: BTreeMap::new(),
        }
    }

    fn holding(asset: &str) -> Book {
        Book {
            t: T,
            cash: 0.0,
            equity: 100.0,
            holdings: [(
                asset.to_string(),
                Holding {
                    qty: 1.0,
                    value: 100.0,
                    entry: None,
                },
            )]
            .into(),
        }
    }

    fn step(memory: &SchizoMemory, gcp2: Option<&Gcp2View>, book: &Book) -> SchizoState {
        SchizoState::next(memory, gcp2, Some(&snap()), book, "now".into()).unwrap()
    }

    fn remembered(mood: &str, zone_value: f64) -> SchizoMemory {
        let v = view(zone_value, 0);
        SchizoMemory {
            mood: Some(mood.into()),
            zone: Some(v.zone),
            zone_label: Some(v.zone_label),
            ..SchizoMemory::default()
        }
    }

    const NAMES: [&str; 4] = ["v13", "contrarian", "detached", "high_variance"];

    #[test]
    fn the_same_gcp2_minute_rolls_the_same_dice_and_picks_the_same_mood() {
        let a = view(169.1705, 100);
        let b = view(169.1705, 100);
        let other = view(169.1706, 100);
        assert_eq!(a.dice, b.dice);
        assert_ne!(a.dice, other.dice);
        let first = step(&SchizoMemory::default(), Some(&a), &flat());
        let again = step(&SchizoMemory::default(), Some(&b), &flat());
        assert_eq!(first.mood_step, "initial");
        assert_eq!(first.dice, Some(a.dice));
        assert_eq!(first.mood, again.mood);
        assert_eq!(first.mood.as_deref(), Some(NAMES[(a.dice % 4) as usize]));
    }

    #[test]
    fn a_zone_change_up_or_down_switches_the_mood_to_the_dice_pick() {
        let others = ["contrarian", "detached", "high_variance"];
        for (now, label) in [(view(230.0, 100), "High -> Very High"), (view(100.0, 100), "High -> Normal")] {
            let state = step(&remembered("v13", 170.0), Some(&now), &flat());
            assert_eq!(state.mood_step, "switched");
            assert_eq!(state.mood.as_deref(), Some(others[(now.dice % 3) as usize]));
            assert_eq!(state.previous_mood.as_deref(), Some("v13"));
            assert!(state.mood_reason.contains(label), "{}", state.mood_reason);
        }
    }

    #[test]
    fn a_held_zone_or_a_missing_reading_keeps_the_mood() {
        // GCP2 stays High (as remembered) for cycle after cycle: the mood never moves.
        let mut memory = remembered("contrarian", 170.0);
        for (value, minute) in [(180.0, 100), (181.0, 160), (175.5, 220)] {
            let state = step(&memory, Some(&view(value, minute)), &flat());
            assert_eq!(
                (state.mood_step, state.mood.as_deref()),
                ("held", Some("contrarian"))
            );
            assert!(state.mood_reason.contains("stayed High"), "{}", state.mood_reason);
            memory = state.memory;
        }

        // GCP2 unavailable: kept, no dice, and the next cycle has nothing to compare.
        let missing = step(&remembered("high_variance", 230.0), None, &flat());
        assert_eq!(
            (missing.mood_step, missing.mood.as_deref()),
            ("no reading", Some("high_variance"))
        );
        assert_eq!((missing.dice, missing.gcp2.as_ref()), (None, None));
        assert!(missing.mood_reason.contains("GCP2 unavailable"));
        assert_eq!(missing.memory.zone, None);
        let after = step(&missing.memory, Some(&view(100.0, 100)), &flat());
        assert_eq!(
            (after.mood_step, after.mood.as_deref()),
            ("no reading", Some("high_variance"))
        );
    }

    #[test]
    fn the_active_mood_votes_double() {
        // v13 buys ETH (best 4h with 1h > 0) in this bullish market; high_variance buys SOL.
        let v13 = step(
            &remembered("high_variance", 100.0),
            Some(&view(120.0, 100)),
            &flat(),
        );
        assert_eq!(
            v13.mood.as_deref(),
            Some("high_variance"),
            "zone unchanged: held"
        );
        let votes = v13.votes.as_ref().unwrap();
        let weight = |p: &str| {
            votes
                .by_persona
                .iter()
                .find(|v| v.persona == p)
                .unwrap()
                .weight
        };
        assert_eq!((weight("high_variance"), weight("v13")), (2, 1));
        assert!(votes.buy["SOL"] >= 2);
        // A held coin with two weighted sell votes is named.
        let mut bear = snap();
        bear.score = Some(-40.0);
        let state = SchizoState::next(
            &remembered("v13", 100.0),
            Some(&view(120.0, 100)),
            Some(&bear),
            &holding("BTC"),
            "n".into(),
        )
        .unwrap();
        assert_eq!(state.vote_sell, vec!["BTC".to_string()]);
        assert_eq!(state.vote_buy, None);
        // Without market breadth there are no votes.
        let blind = SchizoState::next(
            &SchizoMemory::default(),
            Some(&view(170.0, 100)),
            None,
            &flat(),
            "n".into(),
        )
        .unwrap();
        assert_eq!((blind.votes, blind.vote_buy), (None, None));
    }

    #[test]
    fn the_state_persists_across_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = state_path(&dir.path().join("bot-spot-agent-state.json"));
        assert_eq!(path, dir.path().join(STATE_FILE));
        assert_eq!(SchizoMemory::load(&path).unwrap(), SchizoMemory::default());
        let first = step(&SchizoMemory::default(), Some(&view(180.0, 100)), &flat());
        first.memory.save(&path).unwrap();
        // A restarted trader reads the file: the same next cycle as without the restart.
        let restored = SchizoMemory::load(&path).unwrap();
        assert_eq!(restored, first.memory);
        let next = view(185.0, 160);
        let after_restart = step(&restored, Some(&next), &flat());
        let without = step(&first.memory, Some(&next), &flat());
        assert_eq!(after_restart, without);
        assert_eq!(after_restart.mood_step, "held");
        assert_eq!(after_restart.previous_mood, first.mood);
        std::fs::write(&path, "{oops").unwrap();
        assert!(SchizoMemory::load(&path).is_err());
        // A file written before YOLO was removed (2026-10-21) still loads.
        std::fs::write(
            &path,
            r#"{"mood":"high_variance","zone":0,"zone_label":"Normal","yolo":false,"yolo_asset":null,"updated_at":"t"}"#,
        )
        .unwrap();
        assert_eq!(SchizoMemory::load(&path).unwrap().mood.as_deref(), Some("high_variance"));
    }

    #[test]
    fn the_audit_object_carries_mood_reason_gcp2_dice_and_votes() {
        let state = step(&remembered("v13", 170.0), Some(&view(230.5, 100)), &flat());
        let json = serde_json::to_value(&state).unwrap();
        assert_eq!(json["mood"], serde_json::json!(state.mood));
        assert_ne!(state.mood.as_deref(), Some("v13"), "High -> Very High switches");
        assert_eq!(json["mood_reason"], state.mood_reason);
        assert_eq!(json["gcp2"]["value"], 230.5);
        assert_eq!(json["gcp2"]["zone"], "Very High");
        assert_eq!(json["gcp2"]["previous_zone"], "High");
        assert_eq!(json["dice"], serde_json::json!(view(230.5, 1).dice));
        assert_eq!(json["votes"]["by_persona"].as_array().unwrap().len(), 4);
        assert!(json.get("memory").is_none());
    }

    #[test]
    fn the_live_snapshot_is_built_from_the_breadth_read() {
        use crate::market_mood::{CoinMove, MarketView};
        let coin = |asset: &str, r: [f64; 4], volume: Option<f64>| CoinMove {
            asset: asset.into(),
            returns_pct: r.iter().map(|v| Some(*v)).collect(),
            quote_volume_24h: volume,
        };
        let mut mood = MarketView {
            score: Some(22.5),
            coins: (0..12)
                .map(|i| {
                    coin(
                        &format!("C{i:02}"),
                        [0.1, 0.2 * i as f64, 1.0, 3.0],
                        Some(f64::from(i) * 1e6),
                    )
                })
                .chain([coin("NOVOL", [1.0, 9.0, 9.0, 9.0], None)])
                .collect(),
            ..MarketView::default()
        };
        let gcp2 = view(180.0, 100);
        let snap = Snapshot::from_breadth(T, &mood, Some(gcp2.clone())).unwrap();
        assert_eq!(
            (snap.t, snap.score, snap.gcp2.as_ref()),
            (T, Some(22.5), Some(&gcp2))
        );
        // 15m / 1h / 4h from the breadth returns; the 24h volume from the tickers.
        let c05 = snap.coin("C05").unwrap();
        assert_eq!(
            (c05.ret_15m, c05.ret_1h, c05.ret_4h),
            (Some(0.1), Some(1.0), Some(1.0))
        );
        assert_eq!(c05.quote_volume_24h, 5e6);
        // Liquid top: the 10 largest volumes, largest first; no volume, no liquid top.
        assert_eq!(snap.liquid_top.len(), 10);
        assert_eq!(snap.liquid_top[0], "C11");
        assert!(!snap.liquid_top.contains(&"NOVOL".to_string()));
        assert!(
            snap.coin("NOVOL").is_some(),
            "still in the price-only universe"
        );
        mood.error = Some("24h tickers unavailable".into());
        assert!(Snapshot::from_breadth(T, &mood, None).is_none());
    }
}
