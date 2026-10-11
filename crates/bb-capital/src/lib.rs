//! Capital: the shared allocator, per-order capital pool, Claude allocation advisor (omp), quote
//! auto-convert, fleet status and the advisory cash distributor.
//!
//! - [`config`]: `CAPITAL_*`, `CAPITAL_AI_ADVISOR_*`, `CLAUDE_ALLOCATOR_*`, `AUTO_CONVERT_*`.
//! - [`scorecard`]: module scorecards from fills or JSONL ledgers (one ledger reader).
//! - [`allocator`]: the one deterministic allocator (reserve, floors, score weights, caps).
//! - [`pool`]: [`pool::CapitalPool`] — per-buy leases, usage ledger, JSON state.
//! - [`advisor`]: guarded proposals (file / Claude), schema validation, clamping.
//! - [`claude`]: the persistent omp allocator session and its host tools.
//! - [`convert`]: [`convert::ensure_quote_for_order`] for runtimes that place buys.
//! - [`wallet`], [`status`]: wallet snapshot and fleet status (`bot-status`).
//! - [`distributor`]: advisory share plan (`cash-distributor`). [`cli`]: commands.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod advisor;
pub mod allocator;
pub mod claude;
pub mod cli;
pub mod config;
pub mod convert;
pub mod distributor;
pub mod error;
pub mod pool;
pub mod scorecard;
pub mod status;
pub mod wallet;

pub use advisor::{AllocationContext, GuardedAdvisor, ProposalProvider, guard_proposal};
pub use allocator::{
    AllocationPlan, AllocationStatus, AllocationUnit, ModuleAllocation, Scoring, build_plan,
};
pub use claude::{ClaudeAllocator, FleetSource, LiveFleet, guarded_advisor_from_env};
pub use config::{AdvisorConfig, AutoConvertConfig, CapitalPoolConfig, ClaudeAllocatorConfig};
pub use convert::{
    AutoConversionResult, ConvertExchange, ConvertMode, FundingRequest, ensure_quote_for_order,
};
pub use error::{CapitalError, Result};
pub use pool::{AllocationLease, CapitalPool, LeaseRequest, LeaseStatus, ModuleLedgerEntry};
pub use scorecard::{ModuleScorecard, OrderFill, scorecards_from_orders};
