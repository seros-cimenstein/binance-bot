//! `airdrop-scheduler` command (former `python -m src.airdrop.scheduler`).

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::Context as _;
use serde::Serialize;

use crate::catalog::{Chain, Confidence, Status};
use crate::error::AirdropError;
use crate::reminder::{ReminderPaths, ReminderSettings, http_client, run_reminder};
use crate::scheduler::{
    TaskKey, build_checklist, iso_week_label, load_state, render_checklist, save_state,
};
use crate::state::{ACTIVITY_FILE, STATE_FILE, STORE_FILE, default_data_dir};
use crate::wallet::{add_wallet, load_store, read_passphrase, save_store};

#[derive(clap::Subcommand, Debug)]
pub enum Command {
    /// Manage the airdrop wallet store and produce a weekly checklist.
    AirdropScheduler(SchedulerArgs),
}

/// State files default to `$AIRDROP_DATA_DIR` (else
/// `~/.local/share/binance-bot/data`); explicit path flags win.
#[derive(clap::Args, Debug)]
pub struct SchedulerArgs {
    #[arg(long)]
    pub store_path: Option<PathBuf>,
    #[arg(long)]
    pub state_path: Option<PathBuf>,
    #[command(subcommand)]
    pub action: Action,
}

#[derive(clap::Subcommand, Debug)]
pub enum Action {
    /// Generate + add a new wallet.
    Add { name: String, chain: WalletChain },
    /// List known wallets (no decryption).
    List,
    /// Render this week's per-wallet checklist.
    Checklist {
        #[arg(long, value_enum)]
        min_confidence: Option<Confidence>,
        #[arg(long, value_enum, default_value_t = StatusFilter::Active)]
        status_filter: StatusFilter,
        #[arg(long)]
        json: bool,
    },
    /// Mark a task as done this week.
    Done {
        wallet_name: String,
        opportunity_name: String,
    },
    /// Reset a task back to pending.
    Undone {
        wallet_name: String,
        opportunity_name: String,
    },
    /// Snapshot wallet activity, auto-mark active wallets done, and push the
    /// pending digest to Telegram.
    Remind {
        #[arg(long)]
        activity_path: Option<PathBuf>,
    },
}

#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalletChain {
    Solana,
    Evm,
}

impl From<WalletChain> for Chain {
    fn from(c: WalletChain) -> Self {
        match c {
            WalletChain::Solana => Self::Solana,
            WalletChain::Evm => Self::Evm,
        }
    }
}

#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusFilter {
    Active,
    Speculative,
    Ended,
    Any,
}

impl StatusFilter {
    fn status(self) -> Option<Status> {
        match self {
            Self::Active => Some(Status::Active),
            Self::Speculative => Some(Status::Speculative),
            Self::Ended => Some(Status::Ended),
            Self::Any => None,
        }
    }
}

pub async fn run(cmd: Command) -> anyhow::Result<ExitCode> {
    match cmd {
        Command::AirdropScheduler(args) => run_scheduler(args, &default_data_dir()).await,
    }
}

#[derive(Serialize)]
struct Added<'a> {
    added: &'a str,
    chain: Chain,
    address: &'a str,
}

#[derive(Serialize)]
struct WalletListing<'a> {
    wallets: Vec<WalletEntry<'a>>,
}

#[derive(Serialize)]
struct WalletEntry<'a> {
    name: &'a str,
    chain: Chain,
    address: &'a str,
}

#[derive(Serialize)]
struct ChecklistJson<'a> {
    iso_week: &'a str,
    rows: &'a [crate::scheduler::ChecklistRow],
}

#[derive(Serialize)]
struct TaskUpdate<'a> {
    task: String,
    command: &'a str,
    iso_week: &'a str,
}

fn print_json<T: Serialize>(value: &T) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn fail(err: &AirdropError, code: u8) -> ExitCode {
    eprintln!("airdrop scheduler: {err}");
    ExitCode::from(code)
}

/// Run against an explicit default data dir (resolved from the environment by [`run`]).
pub async fn run_scheduler(args: SchedulerArgs, data_dir: &Path) -> anyhow::Result<ExitCode> {
    let store_path = args.store_path.unwrap_or_else(|| data_dir.join(STORE_FILE));
    let state_path = args.state_path.unwrap_or_else(|| data_dir.join(STATE_FILE));

    let (mut store, mut state) =
        match load_store(&store_path).and_then(|s| Ok((s, load_state(&state_path)?))) {
            Ok(loaded) => loaded,
            Err(err) => return Ok(fail(&err, 1)),
        };

    let now = bb_core::time::now();
    let iso = iso_week_label(now);
    match args.action {
        Action::Add { name, chain } => {
            let added = read_passphrase("Airdrop wallet store passphrase: ")
                .map(zeroize::Zeroizing::new)
                .and_then(|pp| add_wallet(&mut store, &name, chain.into(), &pp))
                .and_then(|w| save_store(&store, &store_path).map(|()| w));
            match added {
                Ok(w) => print_json(&Added {
                    added: &w.name,
                    chain: w.chain,
                    address: &w.address,
                })?,
                Err(err) => return Ok(fail(&err, 2)),
            }
        }
        Action::List => print_json(&WalletListing {
            wallets: store
                .wallets
                .iter()
                .map(|w| WalletEntry {
                    name: &w.name,
                    chain: w.chain,
                    address: &w.address,
                })
                .collect(),
        })?,
        Action::Checklist {
            min_confidence,
            status_filter,
            json,
        } => {
            let rows =
                build_checklist(&store, &state, &iso, min_confidence, status_filter.status());
            if json {
                print_json(&ChecklistJson {
                    iso_week: &iso,
                    rows: &rows,
                })?;
            } else {
                println!("{}", render_checklist(&rows, &iso));
            }
        }
        Action::Done {
            wallet_name,
            opportunity_name,
        } => {
            let key = TaskKey::new(wallet_name, opportunity_name);
            state.mark(&key, &iso);
            save_state(&state, &state_path)?;
            print_json(&TaskUpdate {
                task: key.as_key(),
                command: "done",
                iso_week: &iso,
            })?;
        }
        Action::Undone {
            wallet_name,
            opportunity_name,
        } => {
            let key = TaskKey::new(wallet_name, opportunity_name);
            state.unmark(&key);
            save_state(&state, &state_path)?;
            print_json(&TaskUpdate {
                task: key.as_key(),
                command: "undone",
                iso_week: &iso,
            })?;
        }
        Action::Remind { activity_path } => {
            let activity_path = activity_path.unwrap_or_else(|| data_dir.join(ACTIVITY_FILE));
            let settings = ReminderSettings::from_env();
            let http = http_client()?;
            let result = run_reminder(
                &http,
                &settings,
                &ReminderPaths {
                    store: &store_path,
                    state: &state_path,
                    activity: &activity_path,
                },
                now,
            )
            .await
            .context("airdrop reminder failed")?;
            if settings.telegram.is_none() {
                tracing::info!(
                    "airdrop reminder: TELEGRAM_BOT_TOKEN / TELEGRAM_CHAT_ID unset — skipping push and printing the digest instead."
                );
                println!("{}", result.digest);
            }
            print_json(&result)?;
        }
    }
    Ok(ExitCode::SUCCESS)
}
