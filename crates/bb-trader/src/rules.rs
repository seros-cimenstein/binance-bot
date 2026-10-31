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
//! - `style`: `fixed` (the weights are the operator's numbers).
//! - per sleeve: `name`; `share` of the portfolio (the shares sum to 1); `horizon` filter
//!   of its candidates (`any`, `short`, `swing`, `long`); `defensive` assets it may hold
//!   when risk-off; `risk_off_below` (market-wide score level, `null` = never); `buy_above`
//!   and `sell_below` (score levels, sell not above buy); `min_posters` (posters a window
//!   needs before `direction`, `agreement` and `stance.6h` count); `weights` by signal
//!   name ([`crate::signals`]; an unknown name is an error).
//!
//! A sleeve's score for an asset is `Σ weight × value` over its weights.
//!
//! Until sleeves are booked and decided separately (Phase 53 D3-D5) the trader runs one
//! sleeve: a file with more than one sleeve holding a share, a horizon filter other than
//! `any`, or a `risk_off_below` is refused, so no file promises what the trader does not do.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::signals::Name;

/// How far the shares may sum away from 1 (float noise in a hand-written file).
const SHARE_TOLERANCE: f64 = 1e-9;

/// Where a rule's weights come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Style {
    /// The operator's numbers.
    Fixed,
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
    /// Weight per signal, in catalog order.
    pub weights: BTreeMap<Name, f64>,
}

impl fmt::Display for Sleeve {
    /// `fast: talk.6h +0, stance.6h +1, momentum.1h +1; buy at score ≥ +0.5, sell below -0.5`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let weights: Vec<String> = self
            .weights
            .iter()
            .map(|(name, w)| format!("{name} {w:+}"))
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
    pub sleeves: Vec<Sleeve>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRule {
    rules_version: String,
    style: Style,
    sleeves: Vec<RawSleeve>,
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
}

impl RawSleeve {
    fn check(self) -> Result<Sleeve, String> {
        let name = self.name.trim().to_owned();
        if name.is_empty() {
            return Err("a sleeve has a blank name".into());
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
        })
    }
}

impl Rule {
    /// Parse and check a rule file's text (see the module docs for what is refused).
    pub fn parse(text: &str) -> Result<Self, String> {
        let raw: RawRule = serde_json::from_str(text).map_err(|e| e.to_string())?;
        let rules_version = raw.rules_version.trim().to_owned();
        if rules_version.is_empty() {
            return Err("rules_version must not be blank".into());
        }
        if raw.sleeves.is_empty() {
            return Err("no sleeve".into());
        }
        let mut names = BTreeSet::new();
        let mut sleeves = Vec::new();
        for raw in raw.sleeves {
            let sleeve = raw.check()?;
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
            sleeves,
        };
        rule.check_supported()?;
        Ok(rule)
    }

    /// Read and check the rule file at `path`.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        Self::parse(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Refuse what the trader cannot carry out yet (see the module docs).
    fn check_supported(&self) -> Result<(), String> {
        let funded: Vec<&str> = self
            .sleeves
            .iter()
            .filter(|s| s.share > 0.0)
            .map(|s| s.name.as_str())
            .collect();
        if funded.len() > 1 {
            return Err(format!(
                "sleeves {} all hold a share: the trader runs one sleeve until sleeve holdings are booked (Phase 53 D3-D5)",
                funded.join(", ")
            ));
        }
        for sleeve in &self.sleeves {
            if sleeve.horizon != Horizon::Any {
                return Err(format!(
                    "sleeve {}: horizon filters come with per-sleeve decisions (Phase 53 D5); use \"any\"",
                    sleeve.name
                ));
            }
            if sleeve.risk_off_below.is_some() {
                return Err(format!(
                    "sleeve {}: risk-off comes with per-sleeve decisions (Phase 53 D5); use null",
                    sleeve.name
                ));
            }
        }
        Ok(())
    }

    /// The sleeve the trader runs: the one holding the share (the shares sum to 1 and only
    /// one sleeve may hold any, so there is exactly one).
    pub fn trading_sleeve(&self) -> &Sleeve {
        self.sleeves
            .iter()
            .find(|s| s.share > 0.0)
            .unwrap_or(&self.sleeves[0])
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
                "unknown variant `edge`",
                Box::new(|v| v["style"] = "edge".into()),
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
            // Not carried out before per-sleeve decisions: refused, not ignored.
            (
                "all hold a share",
                Box::new(|v| {
                    v["sleeves"][0]["share"] = 0.5.into();
                    let mut extra = core.clone();
                    extra["share"] = 0.5.into();
                    v["sleeves"].as_array_mut().unwrap().push(extra);
                }),
            ),
            (
                "horizon filters",
                Box::new(|v| v["sleeves"][0]["horizon"] = "long".into()),
            ),
            (
                "risk-off",
                Box::new(|v| v["sleeves"][0]["risk_off_below"] = (-0.2).into()),
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
        let err = Rule::load(Path::new("/nonexistent/rule.json")).unwrap_err();
        assert!(err.contains("/nonexistent/rule.json"), "{err}");
    }
}
