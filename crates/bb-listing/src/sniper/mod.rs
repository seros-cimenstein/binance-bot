//! Listing sniper daemon (port of `src/listing_sniper.py`).
//!
//! Polls `GET /api/v3/exchangeInfo` on a tight cycle, diffs the TRADING
//! universe for the quote asset against the previous snapshot (plus symbols
//! pre-armed from the shared watchlist), and fires paper or live round trips
//! with a flat, trailing or momentum exit.

pub mod cycle;
pub mod execute;
pub mod exit;
pub mod settings;
pub mod state;

pub use cycle::{
    CycleAction, CycleReport, SkipAction, SniperMode, Venue, preflight, run_cycle, run_daemon,
};
pub use execute::{
    FireResult, LiveOutcome, PaperOutcome, fire_live, fire_paper, format_live_fire_alert,
};
pub use exit::{HoldOutcome, hold, momentum_should_exit};
pub use settings::{HoldMode, SniperSettings};
pub use state::{DailySpend, SniperState};
