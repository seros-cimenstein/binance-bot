"""Curated catalog of currently-farmable airdrop opportunities.

Each entry describes a protocol, what the operator needs to do, on what
cadence, and what kind of activity counts toward the airdrop. This is
not financial advice and the list will rot — review before each season
and update via the ``opportunities`` constant.

The catalog is intentionally hand-curated rather than scraped: scraped
airdrop lists are dominated by farming-yourself scams that direct
users to malicious contracts. A small list of established protocols
with publicly-documented points programs is the honest answer.

Structure of each ``AirdropOpportunity``:

* ``name``:        human label
* ``chain``:       which chain (must match the wallet's chain)
* ``url``:         official protocol URL for verification
* ``status``:      "active" | "speculative" | "ended"
* ``activity_kind``: the kind of action that earns points
* ``min_actions_per_week``: how many times per week
* ``min_size_usd``: minimum activity size that counts
* ``confidence``:  "high" | "medium" | "low"
                    (operator confidence the airdrop will actually happen)
* ``notes``:       free-form caveats / strategy notes
"""

from __future__ import annotations

from dataclasses import dataclass


@dataclass(frozen=True, slots=True)
class AirdropOpportunity:
    name: str
    chain: str
    url: str
    status: str
    activity_kind: str
    min_actions_per_week: int
    min_size_usd: float
    confidence: str
    notes: str = ""


# Hand-curated list of well-known programs operator-validated as of build time.
# Update before each new season. Confidence ratings reflect track record of
# (a) the team and (b) the chain ecosystem actually delivering airdrops.
opportunities: tuple[AirdropOpportunity, ...] = (
    # ---- Solana ecosystem ----
    AirdropOpportunity(
        name="Kamino Finance",
        chain="solana",
        url="https://app.kamino.finance/",
        status="active",
        activity_kind="supply / borrow stablecoins",
        min_actions_per_week=1,
        min_size_usd=10.0,
        confidence="medium",
        notes="Points accumulate per $-hour. Higher confidence if you keep "
              "the deposit live rather than rotating in and out.",
    ),
    AirdropOpportunity(
        name="MarginFi",
        chain="solana",
        url="https://app.marginfi.com/",
        status="active",
        activity_kind="supply assets and refer",
        min_actions_per_week=1,
        min_size_usd=10.0,
        confidence="medium",
        notes="MFI airdrop already happened; current rewards point at a "
              "secondary distribution.",
    ),
    AirdropOpportunity(
        name="Jito Network",
        chain="solana",
        url="https://jito.network/",
        status="speculative",
        activity_kind="stake SOL via JitoSOL",
        min_actions_per_week=0,  # one-time + hold
        min_size_usd=10.0,
        confidence="low",
        notes="JTO already airdropped once. Further distributions to "
              "liquid stakers are speculative.",
    ),
    AirdropOpportunity(
        name="Drift Protocol",
        chain="solana",
        url="https://app.drift.trade/",
        status="active",
        activity_kind="perp trade volume",
        min_actions_per_week=2,
        min_size_usd=20.0,
        confidence="medium",
        notes="DRIFT distributed Phase 1; Phase 2 is open. Volume from "
              "small accounts counts but is rate-limited.",
    ),

    # ---- EVM L2s (Base / Arbitrum / Optimism / zkSync / Linea) ----
    AirdropOpportunity(
        name="Base ecosystem activity",
        chain="evm",
        url="https://base.org/",
        status="speculative",
        activity_kind="bridge in + use 2-3 Base DEXs/lending per month",
        min_actions_per_week=1,
        min_size_usd=10.0,
        confidence="medium",
        notes="No confirmed BASE token. Activity points toward speculative "
              "future drop. Aerodrome / Aave on Base are the canonical "
              "venues to interact with.",
    ),
    AirdropOpportunity(
        name="Linea Voyage / Surge",
        chain="evm",
        url="https://linea.build/",
        status="active",
        activity_kind="LXP-L points: bridge + DEX swaps + liquidity",
        min_actions_per_week=1,
        min_size_usd=10.0,
        confidence="medium",
        notes="ConsenSys-backed. Voyage closed; LXP-L (DeFi season) is "
              "the current track.",
    ),
    AirdropOpportunity(
        name="Scroll Marks",
        chain="evm",
        url="https://scroll.io/",
        status="active",
        activity_kind="bridge in + use Scroll DEX/lending",
        min_actions_per_week=1,
        min_size_usd=10.0,
        confidence="medium",
        notes="Scroll Marks is the on-chain points program — eligible to "
              "be converted to a future token at TBD ratios.",
    ),
    AirdropOpportunity(
        name="zkSync Era usage (post-ZK)",
        chain="evm",
        url="https://zksync.io/",
        status="speculative",
        activity_kind="bridge + DEX swaps + LP",
        min_actions_per_week=1,
        min_size_usd=10.0,
        confidence="low",
        notes="ZK already airdropped. Further drops to ongoing users are "
              "speculative.",
    ),

    # ---- Pendle / EigenLayer / restaking ----
    AirdropOpportunity(
        name="EigenLayer + LRT season 2",
        chain="evm",
        url="https://app.eigenlayer.xyz/",
        status="speculative",
        activity_kind="restake ETH via an LRT (Renzo / EtherFi / Kelp)",
        min_actions_per_week=0,  # passive
        min_size_usd=50.0,
        confidence="medium",
        notes="EIGEN season 1 already paid. Season 2 is the current play. "
              "Higher minimum because restaking gas only makes sense above "
              "$50 deployed.",
    ),
    AirdropOpportunity(
        name="Pendle PT/YT activity",
        chain="evm",
        url="https://www.pendle.finance/",
        status="active",
        activity_kind="hold PTs or LP on Pendle",
        min_actions_per_week=0,
        min_size_usd=20.0,
        confidence="medium",
        notes="Pendle airdrops downstream protocol points (Eigen, Karak, "
              "Etherex, etc.) at a multiplier. Low effort once positioned.",
    ),
    AirdropOpportunity(
        name="Karak L2 / Restaking",
        chain="evm",
        url="https://karak.network/",
        status="speculative",
        activity_kind="restake into Karak vaults",
        min_actions_per_week=0,
        min_size_usd=20.0,
        confidence="low",
        notes="No confirmed token. Lower confidence than EigenLayer.",
    ),
)


def filter_by(
    *, chain: str | None = None, status: str | None = None,
    min_confidence: str | None = None,
) -> tuple[AirdropOpportunity, ...]:
    """Return the subset of opportunities matching the filters."""
    confidence_order = {"low": 0, "medium": 1, "high": 2}
    out: list[AirdropOpportunity] = []
    min_rank = confidence_order.get(min_confidence or "low", 0)
    for opp in opportunities:
        if chain and opp.chain != chain:
            continue
        if status and opp.status != status:
            continue
        if confidence_order.get(opp.confidence, 0) < min_rank:
            continue
        out.append(opp)
    return tuple(out)
