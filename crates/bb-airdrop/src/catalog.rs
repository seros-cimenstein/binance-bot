//! Curated catalog of currently-farmable airdrop opportunities.
//!
//! Each entry describes a protocol, what the operator needs to do, on what
//! cadence, and what kind of activity counts toward the airdrop. This is not
//! financial advice and the list will rot — review before each season and
//! update [`OPPORTUNITIES`].
//!
//! The catalog is intentionally hand-curated rather than scraped: scraped
//! airdrop lists are dominated by farming-yourself scams that direct users to
//! malicious contracts. A curated list of programs with publicly-documented
//! points mechanics — each entry linking only the official site/docs — is the
//! honest answer.
//!
//! Last reviewed: 2026-10-01. [`Status::Ended`] entries are kept deliberately:
//! they document drops that already happened so the operator does not re-farm
//! dead campaigns (and recognises phishing pages that resurrect them).

use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};

/// Where an opportunity (or a stored wallet) lives.
///
/// `Solana` / `Evm` are on-chain wallets; `Cex` is an exchange-account program
/// (e.g. Binance HODLer Airdrops) that needs no keypair in the wallet store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Chain {
    Solana,
    Evm,
    Cex,
}

impl Chain {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Solana => "solana",
            Self::Evm => "evm",
            Self::Cex => "cex",
        }
    }
}

impl std::fmt::Display for Chain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Active,
    Speculative,
    Ended,
}

/// Operator confidence the airdrop will actually happen. Ordered low < high.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    clap::ValueEnum,
)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    Low,
    Medium,
    High,
}

impl Confidence {
    /// Lenient parse matching the Python rank lookup: unknown labels rank as `Low`.
    pub fn parse_or_low(raw: &str) -> Self {
        match raw {
            "medium" => Self::Medium,
            "high" => Self::High,
            _ => Self::Low,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

impl std::fmt::Display for Confidence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AirdropOpportunity {
    pub name: &'static str,
    pub chain: Chain,
    /// Official protocol URL for verification.
    pub url: &'static str,
    pub status: Status,
    /// The kind of action that earns points.
    pub activity_kind: &'static str,
    pub min_actions_per_week: u32,
    /// Minimum activity size (USD) that counts.
    pub min_size_usd: Decimal,
    pub confidence: Confidence,
    /// Free-form caveats / strategy notes.
    pub notes: &'static str,
}

/// Hand-curated list of well-known programs, operator-validated as of 2026-10-01.
/// Update before each new season. Confidence ratings reflect track record of
/// (a) the team and (b) the ecosystem actually delivering airdrops, plus how
/// concrete the current points/token commitment is.
pub const OPPORTUNITIES: &[AirdropOpportunity] = &[
    // ---- Solana ecosystem ----
    AirdropOpportunity {
        name: "Pacifica Points",
        chain: Chain::Solana,
        url: "https://docs.pacifica.fi/programs/points-program",
        status: Status::Active,
        activity_kind: "perp trade volume (GUI or API)",
        min_actions_per_week: 2,
        min_size_usd: dec!(20.0),
        confidence: Confidence::High,
        notes: "Weekly snapshots Thursday 00:00 UTC; 10M points/week. Formula opaque by design; self-trading/sybil excluded. No token or conversion commitment yet — never trade points off-platform.",
    },
    AirdropOpportunity {
        name: "Loopscale Points",
        chain: Chain::Solana,
        url: "https://docs.loopscale.com/using-loopscale/points",
        status: Status::Active,
        activity_kind: "lend / borrow / yield loops",
        min_actions_per_week: 0,
        min_size_usd: dec!(20.0),
        confidence: Confidence::High,
        notes: "Points accrue on deposited capital; partner points (Fragmetric, Meteora, marginfi) stack. No token announced in docs; loops carry liquidation risk.",
    },
    AirdropOpportunity {
        name: "Project 0 (marginfi) points",
        chain: Chain::Solana,
        url: "https://blog.0.xyz/articles/introducing-project-0",
        status: Status::Active,
        activity_kind: "keep a live deposit on Project 0",
        min_actions_per_week: 0,
        min_size_usd: dec!(20.0),
        confidence: Confidence::Medium,
        notes: "MRGN points migrated 1:1; a live Project 0 deposit is required to activate past/future points. Token announced with >50% of supply reserved for users, but TGE date still unannounced as of 2026-10-01. Ignore copycat MRGN/P0 tokens.",
    },
    AirdropOpportunity {
        name: "Jito Points",
        chain: Chain::Solana,
        url: "https://www.jito.network/points/",
        status: Status::Active,
        activity_kind: "hold JitoSOL / stake JTO via StakeNet",
        min_actions_per_week: 0,
        min_size_usd: dec!(10.0),
        confidence: Confidence::Medium,
        notes: "JTO is already live; points promise 'future exclusive benefits' with no conversion formula or timeline published. Sparse official detail — treat as a low-effort hold, not a thesis.",
    },
    AirdropOpportunity {
        name: "Kamino Rewards Season 5",
        chain: Chain::Solana,
        url: "https://gov.kamino.finance/t/introducing-kamino-season-5/854",
        status: Status::Ended,
        activity_kind: "(ended) supply / borrow + KMNO staking boosts",
        min_actions_per_week: 0,
        min_size_usd: dec!(10.0),
        confidence: Confidence::High,
        notes: "Season 5 concluded ~Feb-Mar 2026 (KMNO live, 6-month vesting runs through late 2026); no Season 6 announced. Ongoing product incentives are APY-based, not airdrop points.",
    },
    AirdropOpportunity {
        name: "Drift Protocol points",
        chain: Chain::Solana,
        url: "https://www.drift.trade/",
        status: Status::Ended,
        activity_kind: "(ended) perp volume / FUEL season",
        min_actions_per_week: 0,
        min_size_usd: dec!(20.0),
        confidence: Confidence::High,
        notes: "DRIFT TGE ran May 2024; FUEL claims closed June 2025. An April 2026 exploit pushed the venue toward a Velocity/DFX recovery plan. Any 'Drift points season 3' claim page is phishing.",
    },
    // ---- EVM L2s / DeFi (Base / Arbitrum / Ethereum / Robinhood Chain) ----
    AirdropOpportunity {
        name: "Variational Omni points",
        chain: Chain::Evm,
        url: "https://docs.variational.io/omni/rewards/points",
        status: Status::Active,
        activity_kind: "perp / swap volume on Omni (USDC margin)",
        min_actions_per_week: 2,
        min_size_usd: dec!(50.0),
        confidence: Confidence::High,
        notes: "Weekly distributions every Friday stop at the Q4-2026 TGE; 32% of $VAR supply goes to points holders at the Genesis Distribution. US/Canada restricted. $VAR is not live — ignore any VAR token or claim link.",
    },
    AirdropOpportunity {
        name: "Symbiotic Points",
        chain: Chain::Evm,
        url: "https://docs.symbiotic.fi/get-started/resources/points-program",
        status: Status::Active,
        activity_kind: "deposit into vaults that delegate stake to networks",
        min_actions_per_week: 0,
        min_size_usd: dec!(100.0),
        confidence: Confidence::High,
        notes: "Core V2 (July 2026) pays points only on capital actively delegated; over-concentrated restaking is penalised. Economics need roughly $100+. Docs reference no token — avoid copycats.",
    },
    AirdropOpportunity {
        name: "MetaMask Rewards",
        chain: Chain::Evm,
        url: "https://metamask.io/rewards",
        status: Status::Active,
        activity_kind: "swap / perp trade inside MetaMask Mobile (opt-in)",
        min_actions_per_week: 1,
        min_size_usd: dec!(25.0),
        confidence: Confidence::High,
        notes: "Import the wallet into MetaMask Mobile (v7.57+) and opt in; level rewards pay a share of $30M LINEA plus 'future token allocations'. Points reset each season (bonuses carry over). Unavailable in the UK.",
    },
    AirdropOpportunity {
        name: "Karak V2 restaking",
        chain: Chain::Evm,
        url: "https://v2.karak.network/",
        status: Status::Active,
        activity_kind: "stake ETH / LSTs / USDC in Karak vaults",
        min_actions_per_week: 0,
        min_size_usd: dec!(50.0),
        confidence: Confidence::Medium,
        notes: "The app itself states no Karak token is live; points accrue on staked assets. Ignore KAR tokens and 'KAR launch' claims — official site warns about them.",
    },
    AirdropOpportunity {
        name: "Lighter on Robinhood Chain points",
        chain: Chain::Evm,
        url: "https://docs.lighter.xyz/points-program/lighter-on-robinhood-chain-points.md",
        status: Status::Active,
        activity_kind: "perp trade on Lighter (Robinhood Chain)",
        min_actions_per_week: 2,
        min_size_usd: dec!(50.0),
        confidence: Confidence::Medium,
        notes: "Real-time points plus a weekly Friday drop. LIT is already live and conversion of these points is unconfirmed. Strict wash-trade/sybil rules; long restricted list (US/UK/CA/SG/CH/UAE and more).",
    },
    AirdropOpportunity {
        name: "Pendle PT/YT activity",
        chain: Chain::Evm,
        url: "https://www.pendle.finance/",
        status: Status::Active,
        activity_kind: "hold PTs or provide liquidity on Pendle",
        min_actions_per_week: 0,
        min_size_usd: dec!(20.0),
        confidence: Confidence::Medium,
        notes: "Strategy entry, not a pending Pendle drop (PENDLE is live): holding PTs/LPs stacks downstream points (Symbiotic, ether.fi, Karak and similar) on top of the yield.",
    },
    // ---- Speculative EVM plays (no confirmed program or token) ----
    AirdropOpportunity {
        name: "Base ecosystem activity",
        chain: Chain::Evm,
        url: "https://base.app/official-referral-terms.html",
        status: Status::Speculative,
        activity_kind: "use Base apps; Base App referral fee-share",
        min_actions_per_week: 1,
        min_size_usd: dec!(10.0),
        confidence: Confidence::Medium,
        notes: "Still no BASE token: Coinbase said 'exploring' in Sep 2025 after years of 'no plans'. The live program is Base App referral fee-share (25% of referred fees in USDC), not points. Treat 'BASE airdrop' checkers as scams.",
    },
    AirdropOpportunity {
        name: "Abstract Portal XP",
        chain: Chain::Evm,
        url: "https://docs.abs.xyz/portal/overview.md",
        status: Status::Speculative,
        activity_kind: "use Abstract apps + Portal quests/badges",
        min_actions_per_week: 0,
        min_size_usd: dec!(10.0),
        confidence: Confidence::Medium,
        notes: "XP is tracked weekly and can be lost for cheating; conversion or token is unconfirmed. Abstract Global Wallet is a smart-contract wallet (no exportable key), so farm with the in-app wallet rather than the stored EOA.",
    },
    AirdropOpportunity {
        name: "EigenLayer future stakedrops",
        chain: Chain::Evm,
        url: "https://docs.eigenfoundation.org/faq/overview",
        status: Status::Speculative,
        activity_kind: "restake ETH / EIGEN if a season opens",
        min_actions_per_week: 0,
        min_size_usd: dec!(50.0),
        confidence: Confidence::Medium,
        notes: "No season open as of 2026-10-01. The foundation has reserved 15% of initial supply for future community stakedrops — wait for an official announcement instead of farming on rumor.",
    },
    // ---- Ended on-chain campaigns (kept to recognise dead/phishing pages) ----
    AirdropOpportunity {
        name: "Linea LXP-L / Surge",
        chain: Chain::Evm,
        url: "https://linea.build/",
        status: Status::Ended,
        activity_kind: "(ended) bridge + DEX / LP activity",
        min_actions_per_week: 0,
        min_size_usd: dec!(10.0),
        confidence: Confidence::High,
        notes: "LINEA TGE (Sep 2025) closed out LXP-L; no successor program announced. Any site advertising 'ongoing Surge / LXP' is a scam.",
    },
    AirdropOpportunity {
        name: "Scroll Sessions / Marks",
        chain: Chain::Evm,
        url: "https://portal.scroll.io/sessions",
        status: Status::Ended,
        activity_kind: "(ended) bridge + DeFi activity",
        min_actions_per_week: 0,
        min_size_usd: dec!(10.0),
        confidence: Confidence::High,
        notes: "Session 2 sunset in February 2025; Marks are no longer tracked. Scroll now funds builders and targeted ecosystem incentives, not retail points.",
    },
    AirdropOpportunity {
        name: "zkSync Era usage (post-ZK)",
        chain: Chain::Evm,
        url: "https://docs.zknation.io/zk-token/zk-airdrop",
        status: Status::Ended,
        activity_kind: "(ended) bridge + DEX / LP activity",
        min_actions_per_week: 0,
        min_size_usd: dec!(10.0),
        confidence: Confidence::High,
        notes: "Official ZK Nation docs describe only the 2024 airdrop (claim window closed January 2025). No further retail distribution is documented — '2026 zkSync airdrop' pages are phishing.",
    },
    // ---- Binance account programs (no wallet needed) ----
    AirdropOpportunity {
        name: "Binance HODLer Airdrops",
        chain: Chain::Cex,
        url: "https://www.binance.com/en/launchpool/hodler-airdrop",
        status: Status::Active,
        activity_kind: "hold BNB in Simple Earn (Flexible/Locked) or On-Chain Yields",
        min_actions_per_week: 0,
        min_size_usd: dec!(200.0),
        confidence: Confidence::High,
        notes: "Retroactive snapshots: BNB must already be subscribed before a project announcement (hourly balance averages). Rewards land in Spot within ~5h, no claim step; per-user cap 4% of average BNB. Program live but no new project since OPG (2026-06-30).",
    },
    AirdropOpportunity {
        name: "Binance Launchpool",
        chain: Chain::Cex,
        url: "https://www.binance.com/en/launchpool",
        status: Status::Active,
        activity_kind: "lock BNB / FDUSD / USDC / USD1 into the live pool",
        min_actions_per_week: 1,
        min_size_usd: dec!(100.0),
        confidence: Confidence::High,
        notes: "No live project as of 2026-10-01 ('New projects coming soon'); BNB held in Simple Earn auto-enrols in the next pool. Principal unlocks instantly and rewards accrue hourly.",
    },
    AirdropOpportunity {
        name: "Binance Alpha Points & airdrops",
        chain: Chain::Cex,
        url: "https://www.binance.com/en/support/faq/detail/12e7f2e555704f9c8e852d1c1afb032a",
        status: Status::Active,
        activity_kind: "hold eligible assets + buy Alpha tokens, then claim events",
        min_actions_per_week: 3,
        min_size_usd: dec!(100.0),
        confidence: Confidence::High,
        notes: "Points expire after 15 days (rolling); claims are first-come with a decreasing threshold and spend points non-refundably. Balance under $100 zeroes volume points. Only Binance Keyless wallet trades count; sub-account splitting is banned.",
    },
    AirdropOpportunity {
        name: "Binance Wallet Booster",
        chain: Chain::Cex,
        url: "https://www.binance.com/en/support/faq/detail/5ee2a485f6434f13b1a3581a2d4d7129",
        status: Status::Active,
        activity_kind: "complete weekly quests in Binance Wallet (Keyless)",
        min_actions_per_week: 1,
        min_size_usd: dec!(50.0),
        confidence: Confidence::High,
        notes: "Alpha Points threshold is checked once at join and NOT consumed (some Pre-TGE subscriptions do spend points). Rewards stay locked until the project's TGE. No campaign live on 2026-10-01 (TermMax ended 2026-08-24).",
    },
    AirdropOpportunity {
        name: "Binance Megadrop",
        chain: Chain::Cex,
        url: "https://www.binance.com/en/megadrop",
        status: Status::Speculative,
        activity_kind: "lock BNB (Simple Earn Locked) + Web3 quests",
        min_actions_per_week: 0,
        min_size_usd: dec!(100.0),
        confidence: Confidence::Medium,
        notes: "Page is live and locked BNB still accrues scores, but no project has shipped since KernelDAO (2025-04) — dormant, not dead.",
    },
];

/// Subset of [`OPPORTUNITIES`] matching every given filter (`None` = no filter).
pub fn filter_by(
    chain: Option<Chain>,
    status: Option<Status>,
    min_confidence: Option<Confidence>,
) -> Vec<&'static AirdropOpportunity> {
    let min_rank = min_confidence.unwrap_or(Confidence::Low);
    OPPORTUNITIES
        .iter()
        .filter(|opp| chain.is_none_or(|c| opp.chain == c))
        .filter(|opp| status.is_none_or(|s| opp.status == s))
        .filter(|opp| opp.confidence >= min_rank)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_has_solana_evm_and_exchange_entries() {
        for chain in [Chain::Solana, Chain::Evm, Chain::Cex] {
            assert!(OPPORTUNITIES.iter().any(|o| o.chain == chain), "{chain}");
        }
        assert!(
            filter_by(Some(Chain::Cex), None, None)
                .iter()
                .all(|o| o.url.starts_with("https://"))
        );
    }

    #[test]
    fn opportunity_names_are_unique() {
        let names: std::collections::HashSet<_> = OPPORTUNITIES.iter().map(|o| o.name).collect();
        assert_eq!(names.len(), OPPORTUNITIES.len());
    }

    #[test]
    fn filters_apply() {
        let sol = filter_by(Some(Chain::Solana), None, None);
        assert!(!sol.is_empty() && sol.iter().all(|o| o.chain == Chain::Solana));
        assert!(
            filter_by(None, Some(Status::Active), None)
                .iter()
                .all(|o| o.status == Status::Active)
        );
        assert!(
            filter_by(None, None, Some(Confidence::Medium))
                .iter()
                .all(|o| o.confidence >= Confidence::Medium)
        );
        assert_eq!(filter_by(None, None, None).len(), OPPORTUNITIES.len());
    }

    /// Verbatim with the 2026-10 Python catalog (fixture dumped from `src.airdrop.catalog`).
    #[test]
    fn matches_python_catalog_verbatim() {
        let py: Vec<serde_json::Value> =
            serde_json::from_str(include_str!("../tests/fixtures/python-catalog.json")).unwrap();
        assert_eq!(py.len(), OPPORTUNITIES.len());
        for (p, o) in py.iter().zip(OPPORTUNITIES) {
            assert_eq!(p["name"], o.name);
            assert_eq!(p["chain"], o.chain.as_str());
            assert_eq!(p["url"], o.url);
            assert_eq!(p["status"], serde_json::to_value(o.status).unwrap());
            assert_eq!(p["activity_kind"], o.activity_kind);
            assert_eq!(p["min_actions_per_week"], o.min_actions_per_week);
            assert_eq!(
                p["min_size_usd"].to_string().parse::<Decimal>().unwrap(),
                o.min_size_usd
            );
            assert_eq!(p["confidence"], o.confidence.as_str());
            assert_eq!(p["notes"], o.notes);
        }
    }
}
