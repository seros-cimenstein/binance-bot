//! The operator's rule file (`BOT_SPOT_ALGO_RULE`, Phase 53): what the trader scores and
//! when it trades, as JSON.
//!
//! ```json
//! {
//!   "rules_version": "algo-2026-10-29",
//!   "style": "fixed",
//!   "sleeves": [
//!     {"name": "fast", "share": 1.0, "horizon": "any", "defensive": ["USDT"],
//!      "risk_off_below": null, "buy_above": 0.5, "sell_below": -0.5, "min_posters": 2,
//!      "weights": {"talk.6h": 0, "stance.6h": 1, "momentum.1h": 1}}
//!   ]
//! }
//! ```
//!
//! - `rules_version`: the operator's name for the rule, recorded on every audit row.
//! - `style`: `fixed` (the weights are the operator's numbers) or `edge` (the weights
//!   follow the measured edge of the scoreboard, [`crate::edge`]).
//! - per sleeve: `name`; `share` of the portfolio (the shares sum to 1); `horizon` filter
//!   of its candidates (`any`, `short`, `swing`, `long`); `defensive` assets it may hold
//!   when risk-off; `risk_off_below` (market-wide score level, `null` = never); `buy_above`
//!   and `sell_below` (score levels, sell not above buy); `min_posters` (posters a window
//!   needs before `direction`, `agreement` and `stance.6h` count); `weights` by signal
//!   name ([`crate::signals`]; an unknown name is an error).
//!
//! A sleeve's score for an asset is `Σ weight × value` over its weights.
//!
//! Style `edge` (operator 2026-10-30 "Both, later decide"; fields chosen 2026-10-31) needs,
//! at the rule level, `edge_scale`, `edge_cap`, `edge_refresh_hours` and `edge_min_t`
//! (no defaults: they are trading choices) and optionally `edge_scoreboard` (default
//! `data/scoreboard/latest.json`), and per sleeve `edge_horizon` (`1h`, `24h` or `7d`)
//! with empty `weights`. Style `fixed` refuses every `edge_*` field.
//!
//! Each sleeve decides on its own ([`crate::decider`], Phase 53 D5): its `horizon` filters
//! its candidates by their own asset tag's dominant 7d horizon, and while its market-wide
//! score (its weighted `market.*` names) is below `risk_off_below` it moves to its
//! best-scoring `defensive` asset.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::scoreboard::HORIZONS;
use crate::signals::Name;

/// The scoreboard an edge rule reads when `edge_scoreboard` is not set.
pub const DEFAULT_EDGE_SCOREBOARD: &str = "data/scoreboard/latest.json";

/// How far the shares may sum away from 1 (float noise in a hand-written file).
const SHARE_TOLERANCE: f64 = 1e-9;

/// Where a rule's weights come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Style {
    /// The operator's numbers.
    Fixed,
    /// The scoreboard's measured edge, scaled and capped ([`crate::edge`]).
    Edge,
}

/// The edge style's settings (see the module docs).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Edge {
    #[serde(rename = "edge_scoreboard")]
    pub scoreboard: PathBuf,
    /// weight = clamp(scale × excess %, −cap, +cap).
    #[serde(rename = "edge_scale")]
    pub scale: f64,
    #[serde(rename = "edge_cap")]
    pub cap: f64,
    /// The scoreboard file is read again after this many hours.
    #[serde(rename = "edge_refresh_hours")]
    pub refresh_hours: f64,
    /// Rows under this |t| give no weight.
    #[serde(rename = "edge_min_t")]
    pub min_t: f64,
}

/// The posts' horizon a sleeve's candidates must lean to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Horizon {
    Any,
    Short,
    Swing,
    Long,
}

/// One sleeve of the rule (see the module docs).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Sleeve {
    pub name: String,
    pub share: f64,
    pub horizon: Horizon,
    pub defensive: Vec<String>,
    pub risk_off_below: Option<f64>,
    pub buy_above: f64,
    pub sell_below: f64,
    pub min_posters: u64,
    /// Weight per signal, in catalog order (edge style: empty in the file, filled from the
    /// scoreboard at decision time).
    pub weights: BTreeMap<Name, f64>,
    /// Edge style: the scoreboard horizon whose rows weight this sleeve.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edge_horizon: Option<String>,
}

impl fmt::Display for Sleeve {
    /// `fast: talk.6h +0, stance.6h +1, momentum.1h +1; buy at score ≥ +0.5, sell below -0.5`
    /// (weights to 3 decimals: edge weights are measured, not typed).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let weights: Vec<String> = self
            .weights
            .iter()
            .map(|(name, w)| format!("{name} {:+}", (w * 1000.0).round() / 1000.0))
            .collect();
        write!(
            f,
            "{}: {}; buy at score ≥ {:+}, sell below {:+}",
            self.name,
            if weights.is_empty() {
                "no weights".to_owned()
            } else {
                weights.join(", ")
            },
            self.buy_above,
            self.sell_below
        )
    }
}

/// The operator's rule (see the module docs).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Rule {
    pub rules_version: String,
    pub style: Style,
    /// Style `edge` only.
    #[serde(flatten)]
    pub edge: Option<Edge>,
    pub sleeves: Vec<Sleeve>,
    /// sha256 (hex) of the rule file's text, recorded in `run_config.rule` so a row names
    /// the exact file that decided it.
    #[serde(skip)]
    pub sha256: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRule {
    rules_version: String,
    style: Style,
    edge_scoreboard: Option<PathBuf>,
    edge_scale: Option<f64>,
    edge_cap: Option<f64>,
    edge_refresh_hours: Option<f64>,
    edge_min_t: Option<f64>,
    sleeves: Vec<RawSleeve>,
}

impl RawRule {
    /// The edge settings: required with style `edge`, refused with `fixed`.
    fn edge(&self) -> Result<Option<Edge>, String> {
        let fields = [
            ("edge_scale", self.edge_scale),
            ("edge_cap", self.edge_cap),
            ("edge_refresh_hours", self.edge_refresh_hours),
            ("edge_min_t", self.edge_min_t),
        ];
        if self.style == Style::Fixed {
            if let Some((name, _)) = fields.iter().find(|(_, v)| v.is_some()) {
                return Err(format!("{name} is set but style is fixed"));
            }
            if self.edge_scoreboard.is_some() {
                return Err("edge_scoreboard is set but style is fixed".into());
            }
            return Ok(None);
        }
        let [scale, cap, refresh_hours, min_t] = fields.map(|(name, v)| match v {
            Some(v) if v.is_finite() => Ok(v),
            Some(v) => Err(format!("{name} {v} is not a number")),
            None => Err(format!("style edge needs {name}")),
        });
        let (scale, cap, refresh_hours, min_t) = (scale?, cap?, refresh_hours?, min_t?);
        if cap <= 0.0 {
            return Err(format!("edge_cap {cap} must be positive"));
        }
        if refresh_hours <= 0.0 {
            return Err(format!(
                "edge_refresh_hours {refresh_hours} must be positive"
            ));
        }
        if min_t < 0.0 {
            return Err(format!("edge_min_t {min_t} must not be negative"));
        }
        Ok(Some(Edge {
            scoreboard: self
                .edge_scoreboard
                .clone()
                .unwrap_or_else(|| PathBuf::from(DEFAULT_EDGE_SCOREBOARD)),
            scale,
            cap,
            refresh_hours,
            min_t,
        }))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSleeve {
    name: String,
    share: f64,
    horizon: Horizon,
    defensive: Vec<String>,
    risk_off_below: Option<f64>,
    buy_above: f64,
    sell_below: f64,
    min_posters: u64,
    weights: BTreeMap<String, f64>,
    edge_horizon: Option<String>,
}

impl RawSleeve {
    fn check(self, style: Style) -> Result<Sleeve, String> {
        let name = self.name.trim().to_owned();
        if name.is_empty() {
            return Err("a sleeve has a blank name".into());
        }
        if !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return Err(format!("sleeve name {name:?}: use letters, digits, _ or -"));
        }
        let at = |what: String| format!("sleeve {name}: {what}");
        if !(0.0..=1.0).contains(&self.share) {
            return Err(at(format!("share {} is not between 0 and 1", self.share)));
        }
        if self.sell_below > self.buy_above {
            return Err(at(format!(
                "sell_below {} is above buy_above {}",
                self.sell_below, self.buy_above
            )));
        }
        if self.defensive.is_empty() {
            return Err(at("no defensive asset (USDT at least)".into()));
        }
        let mut defensive = Vec::new();
        for asset in &self.defensive {
            let asset = asset.trim().to_ascii_uppercase();
            if asset.is_empty() || !asset.bytes().all(|b| b.is_ascii_alphanumeric()) {
                return Err(at(format!("bad defensive asset {asset:?}")));
            }
            if defensive.contains(&asset) {
                return Err(at(format!("defensive asset {asset} listed twice")));
            }
            defensive.push(asset);
        }
        let mut weights = BTreeMap::new();
        for (text, weight) in self.weights {
            let signal: Name = text.parse().map_err(at)?;
            if weights.insert(signal.clone(), weight).is_some() {
                return Err(at(format!("{signal} weighted twice")));
            }
        }
        let edge_horizon = match (style, self.edge_horizon) {
            (Style::Fixed, None) => None,
            (Style::Fixed, Some(_)) => {
                return Err(at("edge_horizon is set but style is fixed".into()));
            }
            (Style::Edge, None) => return Err(at("style edge needs edge_horizon".into())),
            (Style::Edge, Some(h)) => {
                if !HORIZONS.iter().any(|span| span.as_str() == h) {
                    return Err(at(format!("edge_horizon {h:?} is not 1h, 24h or 7d")));
                }
                Some(h)
            }
        };
        if style == Style::Edge && !weights.is_empty() {
            return Err(at(
                "style edge takes its weights from the scoreboard: weights must be {}".into(),
            ));
        }
        Ok(Sleeve {
            name,
            share: self.share,
            horizon: self.horizon,
            defensive,
            risk_off_below: self.risk_off_below,
            buy_above: self.buy_above,
            sell_below: self.sell_below,
            min_posters: self.min_posters,
            weights,
            edge_horizon,
        })
    }
}

impl Rule {
    /// Parse and check a rule file's text (see the module docs for what is refused).
    pub fn parse(text: &str) -> Result<Self, String> {
        let raw: RawRule = serde_json::from_str(text).map_err(|e| e.to_string())?;
        let edge = raw.edge()?;
        let rules_version = raw.rules_version.trim().to_owned();
        if rules_version.is_empty() {
            return Err("rules_version must not be blank".into());
        }
        if raw.sleeves.is_empty() {
            return Err("no sleeve".into());
        }
        let mut names = BTreeSet::new();
        let mut sleeves = Vec::new();
        for sleeve in raw.sleeves {
            let sleeve = sleeve.check(raw.style)?;
            if !names.insert(sleeve.name.clone()) {
                return Err(format!("sleeve {} listed twice", sleeve.name));
            }
            sleeves.push(sleeve);
        }
        let total: f64 = sleeves.iter().map(|s| s.share).sum();
        if (total - 1.0).abs() > SHARE_TOLERANCE {
            return Err(format!("the sleeves' shares sum to {total}, not 1"));
        }
        let rule = Self {
            rules_version,
            style: raw.style,
            edge,
            sleeves,
            sha256: hex::encode(Sha256::digest(text.as_bytes())),
        };
        Ok(rule)
    }

    /// Read and check the rule file at `path`.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        Self::parse(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// The first sleeve holding a share: the sleeve `--evidence-only` reports and the one a
    /// lone pre-sleeve holding migrates to. The trader and the backtest decide every active
    /// sleeve ([`Sleeve::active`]).
    pub fn trading_sleeve(&self) -> &Sleeve {
        self.sleeves
            .iter()
            .find(|s| s.share > 0.0)
            .unwrap_or(&self.sleeves[0])
    }
}

impl Sleeve {
    /// Whether the trader decides this sleeve: it has a share of the portfolio or holds
    /// something (a 0-share sleeve with nothing is skipped and writes no rows).
    pub fn active(&self, holds: bool) -> bool {
        self.share > 0.0 || holds
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The shipped `algo-2026-10-29` rule file.
    pub(crate) const SHIPPED: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../config/rules/algo-2026-10-29.json"
    );

    /// The `snapback-2026-10-31` rule file (live 2026-10-31..11-02).
    const SNAPBACK: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../config/rules/snapback-2026-10-31.json"
    );

    /// A one-sleeve rule file's text: `weights` by name, the levels, 2 posters.
    pub(crate) fn one_sleeve(weights: &[(&str, f64)], buy_above: f64, sell_below: f64) -> String {
        let weights: serde_json::Map<String, serde_json::Value> = weights
            .iter()
            .map(|(name, w)| ((*name).to_owned(), serde_json::json!(w)))
            .collect();
        serde_json::json!({"rules_version": "test-rules", "style": "fixed", "sleeves": [
            {"name": "fast", "share": 1.0, "horizon": "any", "defensive": ["USDT"],
             "risk_off_below": null, "buy_above": buy_above, "sell_below": sell_below,
             "min_posters": 2, "weights": weights}]})
        .to_string()
    }

    /// Write [`one_sleeve`] into `dir` and return its path.
    pub(crate) fn write(
        dir: &Path,
        weights: &[(&str, f64)],
        buy_above: f64,
        sell_below: f64,
    ) -> String {
        let path = dir.join("rule.json");
        std::fs::write(&path, one_sleeve(weights, buy_above, sell_below)).unwrap();
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn the_shipped_rule_is_algo_2026_10_29() {
        let rule = Rule::load(Path::new(SHIPPED)).unwrap();
        assert_eq!(
            (rule.rules_version.as_str(), rule.style),
            ("algo-2026-10-29", Style::Fixed)
        );
        let fast = rule.trading_sleeve();
        assert_eq!((fast.name.as_str(), fast.share), ("fast", 1.0));
        let weights: Vec<(String, f64)> = fast
            .weights
            .iter()
            .map(|(n, w)| (n.to_string(), *w))
            .collect();
        assert_eq!(
            weights,
            [
                ("talk.6h".to_owned(), 0.0),
                ("stance.6h".to_owned(), 1.0),
                ("momentum.1h".to_owned(), 1.0)
            ]
        );
        assert_eq!(
            (fast.buy_above, fast.sell_below, fast.min_posters),
            (0.5, -0.5, 2)
        );
        assert_eq!(
            (fast.horizon, fast.risk_off_below, fast.defensive.as_slice()),
            (Horizon::Any, None, ["USDT".to_owned()].as_slice())
        );
        let core = &rule.sleeves[1];
        assert_eq!(
            (core.name.as_str(), core.share, core.weights.len()),
            ("core", 0.0, 0)
        );
        assert_eq!(
            fast.to_string(),
            "fast: talk.6h +0, stance.6h +1, momentum.1h +1; buy at score ≥ +0.5, sell below -0.5"
        );
    }

    #[test]
    fn the_snapback_rule_is_snapback_2026_10_31() {
        let rule = Rule::load(Path::new(SNAPBACK)).unwrap();
        assert_eq!(
            (rule.rules_version.as_str(), rule.style),
            ("snapback-2026-10-31", Style::Fixed)
        );
        let fast = rule.trading_sleeve();
        assert_eq!((fast.name.as_str(), fast.share), ("fast", 1.0));
        assert_eq!(
            (fast.horizon, fast.risk_off_below, fast.defensive.as_slice()),
            (Horizon::Any, None, ["USDT".to_owned()].as_slice())
        );
        assert_eq!(
            fast.to_string(),
            "fast: momentum.24h -1; buy at score ≥ +0.8, sell below -0.3"
        );
        let core = &rule.sleeves[1];
        assert_eq!(
            (core.name.as_str(), core.share, core.weights.len()),
            ("core", 0.0, 0)
        );
    }

    #[test]
    fn the_live_rule_is_tagsnap_2026_11_01() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../config/rules/tagsnap-2026-11-01.json"
        );
        let rule = Rule::load(Path::new(path)).unwrap();
        assert_eq!(
            (rule.rules_version.as_str(), rule.style),
            ("tagsnap-2026-11-01", Style::Fixed)
        );
        let fast = rule.trading_sleeve();
        assert_eq!(
            (fast.name.as_str(), fast.share, fast.min_posters),
            ("fast", 1.0, 2)
        );
        assert_eq!(
            fast.to_string(),
            "fast: momentum.24h -1, asset.direction.24h +0.5; buy at score ≥ +1, sell below -0.3"
        );
        assert_eq!((rule.sleeves[1].share, rule.sleeves[1].weights.len()), (0.0, 0));
    }

    #[test]
    fn a_bad_rule_file_is_refused_with_the_reason() {
        let base: serde_json::Value =
            serde_json::from_str(&one_sleeve(&[("momentum.1h", 1.0)], 0.5, 0.0)).unwrap();
        let with = |edit: &dyn Fn(&mut serde_json::Value)| {
            let mut v = base.clone();
            edit(&mut v);
            Rule::parse(&v.to_string())
        };
        let core = serde_json::json!({"name": "core", "share": 0.0, "horizon": "any", "defensive": ["USDT"],
            "risk_off_below": null, "buy_above": 0.5, "sell_below": -0.5, "min_posters": 2, "weights": {}});
        type Edit<'a> = Box<dyn Fn(&mut serde_json::Value) + 'a>;
        let cases: Vec<(&str, Edit<'_>)> = vec![
            (
                "unknown signal",
                Box::new(|v| v["sleeves"][0]["weights"]["talk"] = 1.into()),
            ),
            (
                "unknown signal",
                Box::new(|v| v["sleeves"][0]["weights"]["asset.vibes.6h"] = 1.into()),
            ),
            (
                "sum to 0.6",
                Box::new(|v| v["sleeves"][0]["share"] = 0.6.into()),
            ),
            (
                "sum to 1.5",
                Box::new(|v| {
                    let mut extra = core.clone();
                    extra["share"] = 0.5.into();
                    v["sleeves"].as_array_mut().unwrap().push(extra);
                }),
            ),
            (
                "sell_below 0.6 is above buy_above 0.5",
                Box::new(|v| v["sleeves"][0]["sell_below"] = 0.6.into()),
            ),
            (
                "not between 0 and 1",
                Box::new(|v| v["sleeves"][0]["share"] = 1.5.into()),
            ),
            (
                "rules_version must not be blank",
                Box::new(|v| v["rules_version"] = " ".into()),
            ),
            (
                "sleeve name \"*\"",
                Box::new(|v| v["sleeves"][0]["name"] = "*".into()),
            ),
            (
                "unknown variant `vibes`",
                Box::new(|v| v["style"] = "vibes".into()),
            ),
            (
                "style edge needs edge_scale",
                Box::new(|v| v["style"] = "edge".into()),
            ),
            (
                "edge_cap is set but style is fixed",
                Box::new(|v| v["edge_cap"] = 2.0.into()),
            ),
            (
                "edge_horizon is set but style is fixed",
                Box::new(|v| v["sleeves"][0]["edge_horizon"] = "24h".into()),
            ),
            (
                "unknown field `weight`",
                Box::new(|v| v["sleeves"][0]["weight"] = 1.into()),
            ),
            (
                "missing field `buy_above`",
                Box::new(|v| {
                    v["sleeves"][0].as_object_mut().unwrap().remove("buy_above");
                }),
            ),
            (
                "no sleeve",
                Box::new(|v| v["sleeves"] = serde_json::json!([])),
            ),
            (
                "listed twice",
                Box::new(|v| v["sleeves"][0]["defensive"] = serde_json::json!(["USDT", "usdt"])),
            ),
            (
                "no defensive asset",
                Box::new(|v| v["sleeves"][0]["defensive"] = serde_json::json!([])),
            ),
            (
                "sleeve fast listed twice",
                Box::new(|v| {
                    let mut twin = v["sleeves"][0].clone();
                    twin["share"] = 0.0.into();
                    v["sleeves"].as_array_mut().unwrap().push(twin);
                }),
            ),
        ];
        for (reason, edit) in &cases {
            let err = with(edit.as_ref()).unwrap_err();
            assert!(err.contains(reason), "{reason:?} not in {err:?}");
        }
        // Two sleeves, one funded, a sell level equal to the buy level: fine.
        let ok = with(&|v| {
            v["sleeves"][0]["sell_below"] = 0.5.into();
            v["sleeves"].as_array_mut().unwrap().push(core.clone());
        })
        .unwrap();
        assert_eq!(ok.trading_sleeve().name, "fast");
        // Phase 53 D5: two funded sleeves, a horizon filter and a risk-off level load.
        let split = with(&|v| {
            v["sleeves"][0]["share"] = 0.6.into();
            v["sleeves"][0]["horizon"] = "short".into();
            let mut core = core.clone();
            core["share"] = 0.4.into();
            core["horizon"] = "long".into();
            core["risk_off_below"] = (-0.2).into();
            core["defensive"] = serde_json::json!(["USDT", "PAXG", "XAUT"]);
            v["sleeves"].as_array_mut().unwrap().push(core);
        })
        .unwrap();
        let core = &split.sleeves[1];
        assert_eq!(
            (core.share, core.horizon, core.risk_off_below),
            (0.4, Horizon::Long, Some(-0.2))
        );
        assert_eq!(core.defensive, ["USDT", "PAXG", "XAUT"]);
        assert!(core.active(false) && !ok.sleeves[1].active(false) && ok.sleeves[1].active(true));
        let err = Rule::load(Path::new("/nonexistent/rule.json")).unwrap_err();
        assert!(err.contains("/nonexistent/rule.json"), "{err}");
    }

    #[test]
    fn an_edge_rule_needs_its_settings_and_takes_no_weights() {
        let text = crate::edge::tests::edge_rule_text("board.json", "24h");
        let rule = Rule::parse(&text).unwrap();
        let edge = rule.edge.as_ref().unwrap();
        assert_eq!(
            (
                rule.style,
                edge.scale,
                edge.cap,
                edge.refresh_hours,
                edge.min_t
            ),
            (Style::Edge, 3.0, 2.0, 6.0, 2.0)
        );
        assert_eq!(rule.trading_sleeve().edge_horizon.as_deref(), Some("24h"));
        // run_config shows the settings flat, as in the file.
        let shown = serde_json::to_value(&rule).unwrap();
        assert_eq!(
            (
                shown["edge_cap"].as_f64(),
                shown["edge_scoreboard"].as_str()
            ),
            (Some(2.0), Some("board.json"))
        );
        let base: serde_json::Value = serde_json::from_str(&text).unwrap();
        let without_path = {
            let mut v = base.clone();
            v.as_object_mut().unwrap().remove("edge_scoreboard");
            Rule::parse(&v.to_string()).unwrap()
        };
        assert_eq!(
            without_path.edge.unwrap().scoreboard,
            PathBuf::from(DEFAULT_EDGE_SCOREBOARD)
        );
        type Edit<'a> = Box<dyn Fn(&mut serde_json::Value) + 'a>;
        let cases: Vec<(&str, Edit<'_>)> = vec![
            (
                "style edge needs edge_min_t",
                Box::new(|v| {
                    v.as_object_mut().unwrap().remove("edge_min_t");
                }),
            ),
            (
                "edge_cap 0 must be positive",
                Box::new(|v| v["edge_cap"] = 0.0.into()),
            ),
            (
                "edge_refresh_hours -1 must be positive",
                Box::new(|v| v["edge_refresh_hours"] = (-1.0).into()),
            ),
            (
                "edge_min_t -0.5 must not be negative",
                Box::new(|v| v["edge_min_t"] = (-0.5).into()),
            ),
            (
                "style edge needs edge_horizon",
                Box::new(|v| {
                    v["sleeves"][0]
                        .as_object_mut()
                        .unwrap()
                        .remove("edge_horizon");
                }),
            ),
            (
                "edge_horizon \"6h\" is not 1h, 24h or 7d",
                Box::new(|v| v["sleeves"][0]["edge_horizon"] = "6h".into()),
            ),
            (
                "weights must be {}",
                Box::new(|v| v["sleeves"][0]["weights"]["stance.6h"] = 1.into()),
            ),
        ];
        for (reason, edit) in &cases {
            let mut v = base.clone();
            edit(&mut v);
            let err = Rule::parse(&v.to_string()).unwrap_err();
            assert!(err.contains(reason), "{reason:?} not in {err:?}");
        }
    }
}
