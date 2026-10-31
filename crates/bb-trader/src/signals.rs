//! The signal catalog (Phase 53): every number a rule may weight, by name.
//!
//! Names (an unknown name is an error, [`Name::from_str`]):
//!
//! | Name | Range | Meaning |
//! |---|---|---|
//! | `talk.6h` | −1..3 | legacy: the coin's own asset tag, 6h mention rate over its 24h rate − 1; 0 under [`SURGE_MIN_MENTIONS`] mentions in 24h |
//! | `stance.6h` | −1..1 | legacy: (up − down) / (up + down) of the own asset tag's 6h AI tags; 0 under `min_posters` posters or without tags |
//! | `momentum.<1h\|24h\|7d>` | −1..1 | the coin's return over the horizon capped at ±[`MOMENTUM_CAP_PCT`] (5%), ±10%, ±20% and divided by the cap |
//! | `<kind>.<signal>[.<window>]` | see below | per tradable asset: the tag signal over every [`crate::reach`] path from a tag of `<kind>`, averaged by path weight |
//! | `market.<macro\|mood>.<signal>[.<window>]` | see below | market-wide: the tag signal over every live macro or mood tag, averaged by the tags' posters |
//!
//! `<kind>` is a tag kind (`asset`, `narrative`, `macro`, `event`, `entity`, `claim`,
//! `mood`); `asset` covers the coin's own tag, its proxies' source tags (GOLD for PAXG) and
//! other assets linked to it. `<window>` is `1h`, `6h`, `24h`, `7d` or `30d`.
//!
//! | Signal | Windows | Range | Per tag over a window |
//! |---|---|---|---|
//! | `direction` | all | −1..1 | (up − down) / (up + down); 0 under `min_posters` posters or without tags |
//! | `level` | all | 0..1 | posters / the posters of the feed's loudest tag in the window |
//! | `surge` | 1h, 6h, 24h, 7d | −1..1 | (r − R) / (r + R) of the mention rate per hour `r` against the next longer window's `R` (1h and 6h against 24h, 24h against 7d, 7d against 30d); 0 under [`SURGE_MIN_MENTIONS`] mentions in that longer window |
//! | `novelty` | none | 0..1 | 1 − days since the tag was first seen / 30 |
//! | `breadth` | all | 0..1 | distinct posters / mentions |
//! | `persistence` | none | 0..1 | share of the 6h, 24h and 7d windows whose mention rate is at least half the 30d rate; 0 under [`SURGE_MIN_MENTIONS`] mentions in 30d |
//! | `agreement` | all | 0..1 | \|up − down\| / (up + down), same gate as `direction` |
//! | `horizon` | all | −1..1 | (long − short) / (short + swing + long) of the posts' horizon mix (+1: long theses) |
//! | `price_confirmed` | 1h, 24h, 7d | −1..1 | per asset only: `<kind>.direction.<w>` × `momentum.<w>` (talk and price agree: positive) |
//!
//! A link path carries the link's own posters and up/down (posts tying the tag to that
//! asset: "war rising" can be "gold up" and "stocks down") for `direction` and
//! `agreement`; every other signal reads the tag's own windows. The legacy names reproduce
//! the `algo-2026-10-29` rule exactly; the decider reads them from here.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use crate::book::CoinView;
use crate::reach::{Reach, Via};
use crate::tags::{Feed, LinkWindow, LinkWindows, Tag, Window, Windows};

/// 24h mentions a coin needs before its legacy talk counts; also the longer-window
/// minimum of `surge` and the 30d minimum of `persistence`.
pub const SURGE_MIN_MENTIONS: u64 = 4;
/// 1h move that saturates `momentum.1h` (and the legacy momentum term).
pub const MOMENTUM_CAP_PCT: f64 = 5.0;
/// 24h and 7d moves that saturate `momentum.24h` and `momentum.7d`.
pub const MOMENTUM_CAP_24H_PCT: f64 = 10.0;
pub const MOMENTUM_CAP_7D_PCT: f64 = 20.0;
/// Days after which a tag's `novelty` is 0.
pub const NOVELTY_DAYS: f64 = 30.0;

/// Tag kinds a per-asset name may select.
pub const KINDS: [&str; 7] = [
    "asset",
    "narrative",
    "macro",
    "event",
    "entity",
    "claim",
    "mood",
];
/// Tag kinds a market-wide name may select.
pub const MARKET_KINDS: [&str; 2] = ["macro", "mood"];

/// A feed window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Span {
    H1,
    H6,
    H24,
    D7,
    D30,
}

impl Span {
    pub const ALL: [Span; 5] = [Span::H1, Span::H6, Span::H24, Span::D7, Span::D30];

    pub fn as_str(self) -> &'static str {
        match self {
            Span::H1 => "1h",
            Span::H6 => "6h",
            Span::H24 => "24h",
            Span::D7 => "7d",
            Span::D30 => "30d",
        }
    }

    fn hours(self) -> f64 {
        match self {
            Span::H1 => 1.0,
            Span::H6 => 6.0,
            Span::H24 => 24.0,
            Span::D7 => 168.0,
            Span::D30 => 720.0,
        }
    }

    /// The window `surge` compares against.
    fn parent(self) -> Option<Span> {
        match self {
            Span::H1 | Span::H6 => Some(Span::H24),
            Span::H24 => Some(Span::D7),
            Span::D7 => Some(Span::D30),
            Span::D30 => None,
        }
    }

    /// The cap of the price move over this window, for the horizons the trader has.
    fn momentum_cap(self) -> Option<f64> {
        match self {
            Span::H1 => Some(MOMENTUM_CAP_PCT),
            Span::H24 => Some(MOMENTUM_CAP_24H_PCT),
            Span::D7 => Some(MOMENTUM_CAP_7D_PCT),
            Span::H6 | Span::D30 => None,
        }
    }

    fn of(self, w: &Windows) -> &Window {
        match self {
            Span::H1 => &w.h1,
            Span::H6 => &w.h6,
            Span::H24 => &w.h24,
            Span::D7 => &w.d7,
            Span::D30 => &w.d30,
        }
    }

    fn of_link(self, w: &LinkWindows) -> &LinkWindow {
        match self {
            Span::H1 => &w.h1,
            Span::H6 => &w.h6,
            Span::H24 => &w.h24,
            Span::D7 => &w.d7,
            Span::D30 => &w.d30,
        }
    }

    fn parse(s: &str) -> Option<Span> {
        Span::ALL.into_iter().find(|span| span.as_str() == s)
    }
}

/// A per-tag signal (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Signal {
    Direction,
    Level,
    Surge,
    Novelty,
    Breadth,
    Persistence,
    Agreement,
    Horizon,
    PriceConfirmed,
}

impl Signal {
    pub const ALL: [Signal; 9] = [
        Signal::Direction,
        Signal::Level,
        Signal::Surge,
        Signal::Novelty,
        Signal::Breadth,
        Signal::Persistence,
        Signal::Agreement,
        Signal::Horizon,
        Signal::PriceConfirmed,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Signal::Direction => "direction",
            Signal::Level => "level",
            Signal::Surge => "surge",
            Signal::Novelty => "novelty",
            Signal::Breadth => "breadth",
            Signal::Persistence => "persistence",
            Signal::Agreement => "agreement",
            Signal::Horizon => "horizon",
            Signal::PriceConfirmed => "price_confirmed",
        }
    }

    /// The windows the signal takes; empty for window-less signals.
    pub fn spans(self) -> &'static [Span] {
        match self {
            Signal::Novelty | Signal::Persistence => &[],
            Signal::Surge => &[Span::H1, Span::H6, Span::H24, Span::D7],
            Signal::PriceConfirmed => &[Span::H1, Span::H24, Span::D7],
            _ => &Span::ALL,
        }
    }

    pub fn range(self) -> (f64, f64) {
        match self {
            Signal::Level
            | Signal::Novelty
            | Signal::Breadth
            | Signal::Persistence
            | Signal::Agreement => (0.0, 1.0),
            _ => (-1.0, 1.0),
        }
    }

    fn parse(s: &str) -> Option<Signal> {
        Signal::ALL.into_iter().find(|signal| signal.as_str() == s)
    }
}

/// A signal name a rule may weight (see the module docs).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Name {
    Talk,
    Stance,
    Momentum(Span),
    Asset {
        kind: &'static str,
        signal: Signal,
        span: Option<Span>,
    },
    Market {
        kind: &'static str,
        signal: Signal,
        span: Option<Span>,
    },
}

impl Name {
    /// Every valid name.
    pub fn all() -> Vec<Name> {
        let mut out = vec![Name::Talk, Name::Stance];
        out.extend([Span::H1, Span::H24, Span::D7].map(Name::Momentum));
        let with_spans = |signal: Signal| -> Vec<Option<Span>> {
            if signal.spans().is_empty() {
                vec![None]
            } else {
                signal.spans().iter().copied().map(Some).collect()
            }
        };
        for kind in KINDS {
            for signal in Signal::ALL {
                for span in with_spans(signal) {
                    out.push(Name::Asset { kind, signal, span });
                }
            }
        }
        for kind in MARKET_KINDS {
            for signal in Signal::ALL
                .into_iter()
                .filter(|s| *s != Signal::PriceConfirmed)
            {
                for span in with_spans(signal) {
                    out.push(Name::Market { kind, signal, span });
                }
            }
        }
        out
    }

    pub fn range(&self) -> (f64, f64) {
        match self {
            Name::Talk => (-1.0, 3.0),
            Name::Stance | Name::Momentum(_) => (-1.0, 1.0),
            Name::Asset { signal, .. } | Name::Market { signal, .. } => signal.range(),
        }
    }
}

fn tag_part(
    kind: &'static str,
    rest: &[&str],
    market: bool,
    name: &str,
) -> Result<(Signal, Option<Span>), String> {
    let (signal, span) = match rest {
        [signal] => (*signal, None),
        [signal, span] => (*signal, Some(*span)),
        _ => {
            return Err(format!(
                "unknown signal {name:?}: expected {kind}.<signal>[.<window>]"
            ));
        }
    };
    let signal = Signal::parse(signal)
        .filter(|s| !(market && *s == Signal::PriceConfirmed))
        .ok_or_else(|| format!("unknown signal {name:?}: no signal {signal:?} here"))?;
    let span = match span {
        None if signal.spans().is_empty() => None,
        Some(span) => match Span::parse(span) {
            Some(span) if signal.spans().contains(&span) => Some(span),
            _ => {
                return Err(format!(
                    "unknown signal {name:?}: {} takes {}",
                    signal.as_str(),
                    window_list(signal)
                ));
            }
        },
        None => {
            return Err(format!(
                "unknown signal {name:?}: {} takes {}",
                signal.as_str(),
                window_list(signal)
            ));
        }
    };
    Ok((signal, span))
}

fn window_list(signal: Signal) -> String {
    if signal.spans().is_empty() {
        "no window".into()
    } else {
        let spans: Vec<&str> = signal.spans().iter().map(|s| s.as_str()).collect();
        format!("a window of {}", spans.join(", "))
    }
}

impl FromStr for Name {
    type Err = String;

    fn from_str(name: &str) -> Result<Self, String> {
        let parts: Vec<&str> = name.trim().split('.').collect();
        match parts.as_slice() {
            ["talk", "6h"] => Ok(Name::Talk),
            ["stance", "6h"] => Ok(Name::Stance),
            ["momentum", span] => match Span::parse(span) {
                Some(span) if span.momentum_cap().is_some() => Ok(Name::Momentum(span)),
                _ => Err(format!(
                    "unknown signal {name:?}: momentum takes 1h, 24h or 7d"
                )),
            },
            ["market", kind, rest @ ..] => {
                let kind = MARKET_KINDS
                    .into_iter()
                    .find(|k| k == kind)
                    .ok_or_else(|| {
                        format!(
                            "unknown signal {name:?}: market signals come from macro or mood tags"
                        )
                    })?;
                let (signal, span) = tag_part(kind, rest, true, name)?;
                Ok(Name::Market { kind, signal, span })
            }
            [kind, rest @ ..] if !rest.is_empty() => {
                let kind = KINDS
                    .into_iter()
                    .find(|k| k == kind)
                    .ok_or_else(|| format!("unknown signal {name:?}: unknown tag kind {kind:?}"))?;
                let (signal, span) = tag_part(kind, rest, false, name)?;
                Ok(Name::Asset { kind, signal, span })
            }
            _ => Err(format!(
                "unknown signal {name:?}: expected talk.6h, stance.6h, momentum.<h>, <kind>.<signal>[.<window>] or market.<kind>.<signal>[.<window>]"
            )),
        }
    }
}

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let tail = |signal: &Signal, span: &Option<Span>| match span {
            Some(span) => format!("{}.{}", signal.as_str(), span.as_str()),
            None => signal.as_str().to_owned(),
        };
        match self {
            Name::Talk => f.write_str("talk.6h"),
            Name::Stance => f.write_str("stance.6h"),
            Name::Momentum(span) => write!(f, "momentum.{}", span.as_str()),
            Name::Asset { kind, signal, span } => write!(f, "{kind}.{}", tail(signal, span)),
            Name::Market { kind, signal, span } => {
                write!(f, "market.{kind}.{}", tail(signal, span))
            }
        }
    }
}

impl serde::Serialize for Name {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

/// Legacy `talk.6h` of a coin's own asset tag (see the module docs).
pub fn talk(windows: &Windows) -> f64 {
    if windows.h24.mentions < SURGE_MIN_MENTIONS {
        return 0.0;
    }
    (windows.h6.mentions as f64 * 4.0 / windows.h24.mentions as f64 - 1.0).clamp(-1.0, 3.0)
}

/// Legacy `stance.6h` of a coin's own asset tag (see the module docs).
pub fn stance(windows: &Windows, min_posters: u64) -> f64 {
    lean(
        windows.h6.posters,
        windows.h6.up,
        windows.h6.down,
        min_posters,
    )
}

/// `momentum.<span>` from a coin's view; 0 without a price or a return.
pub fn momentum(coin: Option<&CoinView>, span: Span) -> f64 {
    let (Some(coin), Some(cap)) = (coin, span.momentum_cap()) else {
        return 0.0;
    };
    let ret = match span {
        Span::H1 => coin.ret_1h,
        Span::H24 => coin.ret_24h,
        Span::D7 => coin.ret_7d,
        Span::H6 | Span::D30 => None,
    };
    ret.map_or(0.0, |r| r.clamp(-cap, cap) / cap)
}

/// The names that read only the coin's own asset tag and price (`talk.6h`, `stance.6h`,
/// `momentum.*`); `None` for the tag and market names, which need the whole feed
/// ([`Signals`]).
pub fn own_value(
    name: &Name,
    own: Option<&Windows>,
    coin: Option<&CoinView>,
    min_posters: u64,
) -> Option<f64> {
    match name {
        Name::Talk => Some(own.map_or(0.0, talk)),
        Name::Stance => Some(own.map_or(0.0, |w| stance(w, min_posters))),
        Name::Momentum(span) => Some(momentum(coin, *span)),
        Name::Asset { .. } | Name::Market { .. } => None,
    }
}

/// (up − down) / (up + down) with the poster gate.
fn lean(posters: u64, up: f64, down: f64, min_posters: u64) -> f64 {
    let tagged = up + down;
    if posters < min_posters || tagged <= 0.0 {
        0.0
    } else {
        (up - down) / tagged
    }
}

/// Posters, up and down a path's `direction` and `agreement` read.
#[derive(Debug, Clone, Copy)]
struct Opinion {
    posters: u64,
    up: f64,
    down: f64,
}

/// The signals of one fresh feed (see the module docs).
pub struct Signals<'a> {
    feed: &'a Feed,
    reach: &'a Reach,
    min_posters: u64,
    /// The loudest tag's posters per window, for `level`.
    loudest: BTreeMap<Span, u64>,
}

impl<'a> Signals<'a> {
    /// `min_posters` gates `direction`, `agreement` and the legacy `stance.6h`.
    pub fn new(feed: &'a Feed, reach: &'a Reach, min_posters: u64) -> Self {
        let loudest = Span::ALL
            .into_iter()
            .map(|span| {
                let max = feed
                    .tags
                    .values()
                    .map(|t| span.of(&t.windows).posters)
                    .max()
                    .unwrap_or(0);
                (span, max)
            })
            .collect();
        Self {
            feed,
            reach,
            min_posters,
            loudest,
        }
    }

    /// `signal` of `tag` over `span`, with `opinion` for direction and agreement.
    fn tag_signal(&self, tag: &Tag, signal: Signal, span: Option<Span>, opinion: Opinion) -> f64 {
        let w = |s: Span| s.of(&tag.windows);
        let rate = |s: Span| w(s).mentions as f64 / s.hours();
        match (signal, span) {
            (Signal::Direction, Some(_)) => {
                lean(opinion.posters, opinion.up, opinion.down, self.min_posters)
            }
            (Signal::Agreement, Some(_)) => {
                lean(opinion.posters, opinion.up, opinion.down, self.min_posters).abs()
            }
            (Signal::Level, Some(s)) => match self.loudest.get(&s).copied().unwrap_or(0) {
                0 => 0.0,
                max => w(s).posters as f64 / max as f64,
            },
            (Signal::Surge, Some(s)) => match s.parent() {
                Some(p) if w(p).mentions >= SURGE_MIN_MENTIONS => {
                    let (r, base) = (rate(s), rate(p));
                    (r - base) / (r + base)
                }
                _ => 0.0,
            },
            (Signal::Breadth, Some(s)) => match w(s).mentions {
                0 => 0.0,
                m => (w(s).posters as f64 / m as f64).min(1.0),
            },
            (Signal::Horizon, Some(s)) => {
                let h = w(s).horizon;
                let total = h.short + h.swing + h.long;
                if total > 0.0 {
                    (h.long - h.short) / total
                } else {
                    0.0
                }
            }
            (Signal::Novelty, None) => tag
                .first_seen
                .as_deref()
                .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                .map_or(0.0, |first| {
                    let days =
                        (self.feed.as_of_ms - first.timestamp_millis()) as f64 / 86_400_000.0;
                    (1.0 - days / NOVELTY_DAYS).clamp(0.0, 1.0)
                }),
            (Signal::Persistence, None) => {
                if w(Span::D30).mentions < SURGE_MIN_MENTIONS {
                    return 0.0;
                }
                let base = rate(Span::D30);
                let steady = [Span::H6, Span::H24, Span::D7]
                    .into_iter()
                    .filter(|s| rate(*s) >= 0.5 * base)
                    .count();
                steady as f64 / 3.0
            }
            _ => 0.0,
        }
    }

    /// The tag's own opinion over `span` (no window: 30d).
    fn own_opinion(tag: &Tag, span: Option<Span>) -> Opinion {
        let w = span.unwrap_or(Span::D30).of(&tag.windows);
        Opinion {
            posters: w.posters,
            up: w.up,
            down: w.down,
        }
    }

    /// `kind.signal.span` of `asset`: the path-weighted mean over its paths from tags of
    /// `kind`; 0 without such paths.
    fn through_reach(&self, asset: &str, kind: &str, signal: Signal, span: Option<Span>) -> f64 {
        let Some(paths) = self.reach.paths.get(asset) else {
            return 0.0;
        };
        let (mut sum, mut weight) = (0.0, 0.0);
        for path in paths {
            let Some(tag) = self.feed.tags.get(&path.tag).filter(|t| t.kind == kind) else {
                continue;
            };
            let opinion = match (path.via, span) {
                (Via::Link, Some(s)) => self
                    .feed
                    .links
                    .get(&path.tag)
                    .and_then(|assets| assets.get(&path.asset_tag))
                    .map_or(
                        Opinion {
                            posters: 0,
                            up: 0.0,
                            down: 0.0,
                        },
                        |l| {
                            let lw = s.of_link(l);
                            Opinion {
                                posters: lw.posters,
                                up: lw.up,
                                down: lw.down,
                            }
                        },
                    ),
                _ => Self::own_opinion(tag, span),
            };
            sum += path.weight * self.tag_signal(tag, signal, span, opinion);
            weight += path.weight;
        }
        if weight > 0.0 { sum / weight } else { 0.0 }
    }

    /// The value of `name` for `asset` (its own asset tag windows in `own`, its price view
    /// in `coin`); market names ignore the asset.
    pub fn value(
        &self,
        name: &Name,
        asset: &str,
        own: Option<&Windows>,
        coin: Option<&CoinView>,
    ) -> f64 {
        match name {
            Name::Talk | Name::Stance | Name::Momentum(_) => {
                own_value(name, own, coin, self.min_posters).unwrap_or(0.0)
            }
            Name::Asset {
                kind,
                signal: Signal::PriceConfirmed,
                span: Some(span),
            } => {
                self.through_reach(asset, kind, Signal::Direction, Some(*span))
                    * momentum(coin, *span)
            }
            Name::Asset { kind, signal, span } => self.through_reach(asset, kind, *signal, *span),
            Name::Market { kind, signal, span } => self.market(kind, *signal, *span),
        }
    }

    /// The posters-weighted mean of `signal` over every live tag of `kind` (posters over
    /// the window, or 30d for window-less signals).
    fn market(&self, kind: &str, signal: Signal, span: Option<Span>) -> f64 {
        let (mut sum, mut weight) = (0.0, 0.0);
        for tag in self.feed.tags.values().filter(|t| t.kind == kind) {
            let posters = span.unwrap_or(Span::D30).of(&tag.windows).posters as f64;
            if posters <= 0.0 {
                continue;
            }
            sum += posters * self.tag_signal(tag, signal, span, Self::own_opinion(tag, span));
            weight += posters;
        }
        if weight > 0.0 { sum / weight } else { 0.0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reach::Proxies;
    use crate::tags::Horizon;

    fn win(mentions: u64, posters: u64, up: f64, down: f64) -> Window {
        Window {
            mentions,
            posters,
            threads: posters,
            up,
            down,
            horizon: Horizon::default(),
        }
    }

    fn view(r1: f64, r24: f64, r7: f64) -> CoinView {
        CoinView {
            ret_15m: None,
            ret_1h: Some(r1),
            ret_4h: None,
            ret_24h: Some(r24),
            ret_7d: Some(r7),
            quote_volume_24h: 0.0,
        }
    }

    #[test]
    fn the_legacy_terms_keep_their_exact_formulas_and_thresholds() {
        let w = |m6: u64, m24: u64, posters: u64, up: f64, down: f64| Windows {
            h6: win(m6, posters, up, down),
            h24: win(m24, posters, up, down),
            ..Default::default()
        };
        // talk: 6h rate against 24h rate − 1, capped −1..3, from SURGE_MIN_MENTIONS (4) in 24h.
        assert!((talk(&w(6, 12, 3, 0.0, 0.0)) - 1.0).abs() < 1e-12);
        assert_eq!(talk(&w(3, 3, 3, 0.0, 0.0)), 0.0);
        assert!((talk(&w(1, 4, 1, 0.0, 0.0)) - 0.0).abs() < 1e-12);
        assert_eq!(talk(&w(0, 4, 0, 0.0, 0.0)), -1.0);
        assert_eq!(talk(&w(9, 4, 3, 0.0, 0.0)), 3.0);
        // stance: (up − down) / tagged of the 6h window with min_posters posters.
        assert!((stance(&w(4, 8, 3, 3.0, 1.0), 2) - 0.5).abs() < 1e-12);
        assert!((stance(&w(4, 8, 2, 3.0, 1.0), 2) - 0.5).abs() < 1e-12);
        assert_eq!(stance(&w(4, 8, 1, 3.0, 1.0), 2), 0.0);
        assert_eq!(stance(&w(4, 8, 3, 0.0, 0.0), 2), 0.0);
        // momentum.1h: 1h return capped at MOMENTUM_CAP_PCT (5) over 5.
        assert_eq!(MOMENTUM_CAP_PCT, 5.0);
        assert!((momentum(Some(&view(2.5, 0.0, 0.0)), Span::H1) - 0.5).abs() < 1e-12);
        assert_eq!(momentum(Some(&view(-7.0, 0.0, 0.0)), Span::H1), -1.0);
        assert_eq!(momentum(None, Span::H1), 0.0);
        // The longer horizons cap at 10% and 20%.
        assert!((momentum(Some(&view(0.0, 5.0, -30.0)), Span::H24) - 0.5).abs() < 1e-12);
        assert_eq!(momentum(Some(&view(0.0, 5.0, -30.0)), Span::D7), -1.0);
    }

    #[test]
    fn names_parse_and_print_back_and_unknown_names_are_errors() {
        let all = Name::all();
        for name in &all {
            assert_eq!(&name.to_string().parse::<Name>().unwrap(), name, "{name}");
        }
        // 2 legacy + 3 momentum + 7 kinds × 34 + 2 market kinds × 31 (no price_confirmed).
        assert_eq!(all.len(), 2 + 3 + 7 * 34 + 2 * 31);
        for good in [
            "talk.6h",
            "stance.6h",
            "momentum.7d",
            "asset.direction.6h",
            "narrative.surge.24h",
            "mood.novelty",
            "macro.price_confirmed.24h",
            "market.mood.direction.24h",
            "market.macro.persistence",
        ] {
            assert!(good.parse::<Name>().is_ok(), "{good}");
        }
        for bad in [
            "",
            "talk",
            "stance",
            "talk.24h",
            "stance.1h",
            "momentum.6h",
            "momentum",
            "stock.direction.6h",
            "asset.vibes.6h",
            "asset.direction",
            "asset.direction.2h",
            "asset.surge.30d",
            "asset.novelty.6h",
            "asset.price_confirmed.6h",
            "market.narrative.direction.24h",
            "market.mood.price_confirmed.24h",
            "asset.direction.6h.x",
            "market.mood",
        ] {
            assert!(bad.parse::<Name>().is_err(), "{bad:?} should be refused");
        }
    }

    #[test]
    fn each_tag_signal_on_hand_built_windows() {
        let feed_text = serde_json::json!({
            "schema": "tags-feed-v1", "as_of": "2026-10-31T00:00:00Z",
            "tags": {
                "asset:SOL": {"kind": "asset", "name": "SOL", "first_seen": "2026-10-16T00:00:00Z", "windows": {
                    "1h": {"mentions": 6, "posters": 3, "up": 3.0, "down": 1.0},
                    "6h": {"mentions": 6, "posters": 4, "up": 6.0, "down": 2.0,
                           "horizon": {"short": 1.0, "swing": 1.0, "long": 2.0, "unstated": 9.0}},
                    "24h": {"mentions": 24, "posters": 8, "up": 9.0, "down": 3.0},
                    "7d": {"mentions": 84, "posters": 20},
                    "30d": {"mentions": 240, "posters": 40}}},
                "asset:BTC": {"kind": "asset", "name": "BTC", "first_seen": "2026-09-01T00:00:00Z", "windows": {
                    "6h": {"mentions": 10, "posters": 8}, "24h": {"mentions": 40, "posters": 16}}}
            }
        })
        .to_string();
        let feed = crate::tags::parse_raw(&feed_text).unwrap();
        let reach = crate::reach::build(&feed, &Proxies::default(), |a| a == "SOL" || a == "BTC");
        let s = Signals::new(&feed, &reach, 2);
        let sol = |name: &str| {
            s.value(
                &name.parse().unwrap(),
                "SOL",
                None,
                Some(&view(2.5, -5.0, 0.0)),
            )
        };
        let close = |a: f64, b: f64| (a - b).abs() < 1e-12;
        // direction 6h: (6 − 2) / 8; agreement its size.
        assert!(close(sol("asset.direction.6h"), 0.5));
        assert!(close(sol("asset.agreement.6h"), 0.5));
        // level 6h: 4 posters against BTC's 8.
        assert!(close(sol("asset.level.6h"), 0.5));
        // surge 1h: 6/h against 24h's 1/h -> (6 − 1) / 7; 24h against 7d: 1/h vs 0.5/h.
        assert!(close(sol("asset.surge.1h"), 5.0 / 7.0));
        assert!(close(sol("asset.surge.24h"), 0.5 / 1.5));
        // breadth 24h: 8 posters / 24 mentions.
        assert!(close(sol("asset.breadth.24h"), 1.0 / 3.0));
        // horizon 6h: (long 2 − short 1) / 4, unstated left out.
        assert!(close(sol("asset.horizon.6h"), 0.25));
        // novelty: first seen 15 days before as_of.
        assert!(close(sol("asset.novelty"), 0.5));
        // persistence: 30d rate 1/3 per h; 6h (1), 24h (1), 7d (0.5) all at least half.
        assert!(close(sol("asset.persistence"), 1.0));
        // price_confirmed 1h: direction 1h (0.5) × momentum 1h (0.5); 24h: 0.5 × −0.5.
        assert!(close(sol("asset.price_confirmed.1h"), 0.25));
        assert!(close(sol("asset.price_confirmed.24h"), -0.25));
        // Gates: 1 poster gives no direction; under 4 mentions in the longer window, no surge.
        let quiet = feed_text.replace(r#""posters":4"#, r#""posters":1"#);
        let feed = crate::tags::parse_raw(&quiet).unwrap();
        let s = Signals::new(&feed, &reach, 2);
        assert_eq!(
            s.value(&"asset.direction.6h".parse().unwrap(), "SOL", None, None),
            0.0
        );
        // No paths from a kind: 0.
        assert_eq!(
            s.value(
                &"narrative.direction.6h".parse().unwrap(),
                "SOL",
                None,
                None
            ),
            0.0
        );
    }

    #[test]
    fn asset_signals_average_over_reach_and_links_carry_their_own_opinion() {
        let text = serde_json::json!({
            "schema": "tags-feed-v1", "as_of": "2026-10-31T00:00:00Z",
            "tags": {
                "asset:GOLD": {"kind": "asset", "name": "GOLD", "windows": {"24h": {"mentions": 10, "posters": 5, "up": 4.0, "down": 0.0}}},
                "asset:PAXG": {"kind": "asset", "name": "PAXG", "windows": {"24h": {"mentions": 4, "posters": 3, "up": 0.0, "down": 2.0}}},
                "narrative:hard money": {"kind": "narrative", "name": "hard money", "windows": {"24h": {"mentions": 9, "posters": 6, "up": 3.0, "down": 1.0}}},
                "narrative:AI": {"kind": "narrative", "name": "AI", "windows": {"24h": {"mentions": 9, "posters": 6, "up": 0.0, "down": 3.0}}},
                "macro:war": {"kind": "macro", "name": "war", "windows": {"24h": {"mentions": 30, "posters": 20, "up": 9.0, "down": 1.0}}},
                "macro:rates": {"kind": "macro", "name": "rates", "windows": {"24h": {"mentions": 8, "posters": 5, "up": 0.0, "down": 5.0}}},
                "mood:fear": {"kind": "mood", "name": "fear", "windows": {"24h": {"posters": 0}}}
            },
            "members": {"narrative:hard money": {"asset:GOLD": 0.75}, "narrative:AI": {"asset:PAXG": 0.25}},
            "links": {"macro:war": {"asset:GOLD": {"24h": {"posters": 4, "up": 0.0, "down": 4.0}}}}
        })
        .to_string();
        let feed = crate::tags::parse_raw(&text).unwrap();
        let proxies = Proxies::parse(r#"{"GOLD": ["PAXG"]}"#).unwrap();
        let reach = crate::reach::build(&feed, &proxies, |a| a == "PAXG");
        let s = Signals::new(&feed, &reach, 2);
        let paxg = |name: &str| s.value(&name.parse().unwrap(), "PAXG", None, None);
        let close = |a: f64, b: f64| (a - b).abs() < 1e-12;
        // asset kind: PAXG's own tag (−1) and GOLD through the proxy (+1), weight 1 each.
        assert!(close(paxg("asset.direction.24h"), 0.0));
        // narrative kind: hard money (+0.5, weight 0.75 via GOLD) and AI (−1, weight 0.25).
        assert!(close(
            paxg("narrative.direction.24h"),
            (0.75 * 0.5 - 0.25) / 1.0
        ));
        // macro kind: the war link says "gold down" (−1), though war itself is rising (+0.8).
        assert!(close(paxg("macro.direction.24h"), -1.0));
        // Market-wide macro direction: war +0.8 (20 posters) and rates −1 (5), by posters;
        // a silent mood tag gives 0.
        assert!(close(
            s.value(
                &"market.macro.direction.24h".parse().unwrap(),
                "",
                None,
                None
            ),
            (20.0 * 0.8 - 5.0) / 25.0
        ));
        assert_eq!(
            s.value(
                &"market.mood.direction.24h".parse().unwrap(),
                "",
                None,
                None
            ),
            0.0
        );
        // The legacy names read only the coin's own asset tag.
        let own = feed.tags["asset:PAXG"].windows;
        assert_eq!(s.value(&Name::Stance, "PAXG", Some(&own), None), 0.0); // 3 posters, 6h empty
    }

    #[test]
    fn every_name_stays_in_its_range_on_the_real_feed() {
        let text = include_str!("../tests/fixtures/tags-feed-v1.json");
        let feed = crate::tags::parse_raw(text).unwrap();
        let proxies = Proxies::parse(r#"{"GOLD": ["PAXG", "XAUT"]}"#).unwrap();
        let reach = crate::reach::build(&feed, &proxies, |a| {
            matches!(a, "BTC" | "ETH" | "SOL" | "XRP" | "PAXG" | "XAUT")
        });
        let s = Signals::new(&feed, &reach, 2);
        let coin = view(12.0, -30.0, 50.0);
        let mut nonzero = 0;
        for asset in ["BTC", "ETH", "SOL", "XRP", "PAXG", "XAUT"] {
            let own = feed.assets().find(|(n, _)| *n == asset).map(|(_, w)| *w);
            for name in Name::all() {
                let v = s.value(&name, asset, own.as_ref(), Some(&coin));
                let (lo, hi) = name.range();
                assert!(
                    v.is_finite() && v >= lo - 1e-12 && v <= hi + 1e-12,
                    "{asset} {name} = {v}"
                );
                nonzero += usize::from(v != 0.0);
            }
        }
        assert!(
            nonzero > 100,
            "the real feed moves many signals ({nonzero})"
        );
    }
}
