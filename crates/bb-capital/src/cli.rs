//! `bot-status`, `cash-distributor`, `capital-pool` and `auto-convert` commands.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use bb_binance::{BinanceClient, BinanceConfig};
use rust_decimal::Decimal;
use serde_json::{Value, json};

use crate::advisor::{AllocationContext, FileProposalProvider, GuardedAdvisor, guard_proposal};
use crate::allocator::{AllocationPlan, Scoring};
use crate::claude::{AdviceReport, ClaudeAllocator, LiveFleet};
use crate::config::{
    AdvisorConfig, AutoConvertConfig, CapitalPoolConfig, ClaudeAllocatorConfig, parse_assets,
};
use crate::convert::{ConvertMode, FundingRequest, audit_row, ensure_quote_for_order};
use crate::distributor::{
    DistributorConfig, build_default_snapshots, compute_plan, fleet_scorecards, render_plan,
};
use crate::pool::CapitalPool;
use crate::status::{
    Fx, collect_snapshot, default_data_dir, render_json, render_plaintext, send_telegram_status,
    signed, wallet_client,
};
use crate::wallet::fetch_wallet;

/// Capital commands.
#[derive(clap::Subcommand, Debug)]
pub enum Command {
    /// Unified status snapshot across every deployed bot.
    BotStatus(BotStatusArgs),
    /// Compute an advisory cash-distribution plan across all running bots.
    CashDistributor(CashDistributorArgs),
    /// Show the capital pool plan and persisted state; --advise asks the Claude allocator once.
    CapitalPool(CapitalPoolArgs),
    /// Fund a quote shortfall by market-selling an allowed asset (order/test unless --submit-live).
    AutoConvert(AutoConvertArgs),
}

#[derive(clap::Args, Debug, Clone)]
pub struct BotStatusArgs {
    #[arg(long, default_value_os_t = default_data_dir())]
    pub data_dir: PathBuf,
    /// Emit JSON instead of plaintext.
    #[arg(long)]
    pub json: bool,
    /// Send the rendered snapshot to the configured Telegram chat.
    #[arg(long)]
    pub telegram: bool,
}

#[derive(clap::Args, Debug, Clone)]
pub struct CashDistributorArgs {
    #[arg(long, default_value_os_t = default_data_dir())]
    pub data_dir: PathBuf,
    /// Total treasury value; if omitted, queries the Binance wallet.
    #[arg(long)]
    pub treasury_usdt: Option<Decimal>,
    #[arg(long)]
    pub json: bool,
    /// Suggested-action threshold in $.
    #[arg(long, default_value = "5.0")]
    pub threshold_usdt: Decimal,
}

#[derive(clap::Args, Debug, Clone)]
pub struct CapitalPoolArgs {
    #[arg(long, default_value_os_t = default_data_dir())]
    pub data_dir: PathBuf,
    /// Pool state file (default CAPITAL_POOL_STATE_FILE or data/capital-pool-state.json).
    #[arg(long)]
    pub state_file: Option<PathBuf>,
    /// Total equity; if omitted, the Binance wallet total (spot + USD-M free USDT).
    #[arg(long)]
    pub equity_usdt: Option<Decimal>,
    /// Modules to plan (repeatable); default: persisted modules plus the live fleet bots.
    #[arg(long = "module")]
    pub modules: Vec<String>,
    /// Scoring policy: scorecard or lifetime-pnl.
    #[arg(long, default_value = "scorecard")]
    pub scoring: Scoring,
    /// Ask the Claude allocator (omp) once and show the guarded result.
    #[arg(long)]
    pub advise: bool,
    #[arg(long)]
    pub json: bool,
}

#[derive(clap::Args, Debug, Clone)]
pub struct AutoConvertArgs {
    /// Asset to sell (default AUTO_CONVERT_ASSETS).
    #[arg(long)]
    pub asset: Option<String>,
    /// Quote asset to fund (default USDT).
    #[arg(long)]
    pub quote: Option<String>,
    /// Quote value of the buy to fund.
    #[arg(long)]
    pub amount: Decimal,
    /// Target buy symbol (its base asset is never sold).
    #[arg(long)]
    pub symbol: Option<String>,
    /// Quote that must stay free (min_quote_balance).
    #[arg(long, default_value = "0")]
    pub keep_quote: Decimal,
    /// Plan with Binance order/test only (default).
    #[arg(long)]
    pub plan_only: bool,
    /// Submit the market sell (needs CONFIRM_LIVE_TRADING=yes and AUTO_CONVERT_TO_QUOTE=true).
    #[arg(long)]
    pub submit_live: bool,
}

pub async fn run(cmd: Command) -> anyhow::Result<ExitCode> {
    match cmd {
        Command::BotStatus(args) => bot_status(args).await,
        Command::CashDistributor(args) => cash_distributor(args).await,
        Command::CapitalPool(args) => capital_pool(args).await,
        Command::AutoConvert(args) => auto_convert(args).await,
    }
}

async fn bot_status(args: BotStatusArgs) -> anyhow::Result<ExitCode> {
    let client = wallet_client();
    let snapshot = collect_snapshot(client.as_ref(), &args.data_dir).await;
    let rendered = if args.json {
        serde_json::to_string_pretty(&render_json(&snapshot))?
    } else {
        render_plaintext(&snapshot)
    };
    println!("{rendered}");
    if args.telegram {
        let message = if args.json {
            "Bot status JSON attached"
        } else {
            rendered.as_str()
        };
        if !send_telegram_status(message).await {
            println!("(Telegram delivery failed or alerts disabled)");
            return Ok(ExitCode::from(2));
        }
    }
    Ok(ExitCode::SUCCESS)
}

async fn cash_distributor(args: CashDistributorArgs) -> anyhow::Result<ExitCode> {
    let config = DistributorConfig::default();
    let (treasury, free) = match args.treasury_usdt {
        Some(treasury) => (treasury, treasury),
        None => {
            let wallet = match wallet_client() {
                Some(client) => fetch_wallet(&client).await,
                None => Default::default(),
            };
            (
                wallet.total_value_usdt,
                wallet.spot_usdt_free + wallet.futures_usdt_free,
            )
        }
    };
    // Per-bot inventory attribution does not exist yet: every current value is 0.
    let snapshots = build_default_snapshots(&args.data_dir, &BTreeMap::new(), &config);
    let plan = compute_plan(&snapshots, treasury, free);
    if args.json {
        println!("{}", serde_json::to_string_pretty(&plan)?);
    } else {
        println!("{}", render_plan(&plan, args.threshold_usdt));
    }
    Ok(ExitCode::SUCCESS)
}

/// Advisor outcome shown by `capital-pool`.
enum Advice {
    None,
    File {
        path: PathBuf,
        proposal: Option<Value>,
    },
    Claude(Box<AdviceReport>),
}

async fn capital_pool(args: CapitalPoolArgs) -> anyhow::Result<ExitCode> {
    let pool_config = CapitalPoolConfig::from_env()?;
    let advisor_config = AdvisorConfig::from_env()?;
    let state_file = args
        .state_file
        .clone()
        .unwrap_or_else(crate::config::state_file);
    let distributor = DistributorConfig::default();

    let mut pool = CapitalPool::new(pool_config.clone())
        .with_scoring(args.scoring)
        .with_unit_shares(distributor.all_shares());
    let loaded = pool.load(&state_file)?;
    let mut scorecards = pool.scorecards().clone();
    let fleet_cards = fleet_scorecards(&args.data_dir);
    let fleet_ids: Vec<String> = fleet_cards.keys().cloned().collect();
    scorecards.extend(fleet_cards);
    pool.update_scorecards(scorecards);

    let module_ids: Vec<String> = if args.modules.is_empty() {
        let mut ids = BTreeSet::new();
        ids.extend(pool.scorecards().keys().cloned());
        ids.extend(pool.ledger().keys().cloned());
        if let Some(plan) = pool.last_plan() {
            ids.extend(plan.allocations.iter().map(|a| a.module_id.clone()));
        }
        ids.extend(fleet_ids);
        ids.into_iter().collect()
    } else {
        args.modules.clone()
    };

    let client = wallet_client();
    let equity = match (args.equity_usdt, &client) {
        (Some(equity), _) => equity,
        (None, Some(client)) => fetch_wallet(client).await.total_value_usdt,
        (None, None) => Decimal::ZERO,
    };
    let context = pool.context(equity, &module_ids);

    let (plan, advice) = if args.advise {
        let claude_config = ClaudeAllocatorConfig::from_env()?;
        let fleet = Arc::new(LiveFleet {
            client: client.clone(),
            data_dir: args.data_dir.clone(),
        });
        let allocator = ClaudeAllocator::new(claude_config, fleet);
        let report = allocator.advise(&context).await;
        allocator.shutdown().await;
        let plan = guard_proposal(
            report.proposal.as_ref(),
            &context.fallback_plan,
            &pool_config,
        );
        (plan, Advice::Claude(Box::new(report)))
    } else if let (true, Some(path)) =
        (advisor_config.enabled, advisor_config.proposal_file.clone())
    {
        let advisor = GuardedAdvisor::new(
            Arc::new(FileProposalProvider::new(&path)),
            Duration::from_secs(1),
        );
        let advised = advisor.build_plan(&context).await;
        (
            advised.plan,
            Advice::File {
                path,
                proposal: advised.proposal,
            },
        )
    } else {
        (context.fallback_plan.clone(), Advice::None)
    };

    if args.json {
        let advisor = match &advice {
            Advice::None => Value::Null,
            Advice::File { path, proposal } => {
                json!({"source": "file", "path": path, "proposal": proposal})
            }
            Advice::Claude(report) => json!({"source": "claude", "report": report}),
        };
        let document = json!({
            "state_file": state_file,
            "state_loaded": loaded,
            "pool_enabled": pool_config.enabled,
            "scoring": args.scoring.as_str(),
            "context": context.to_json(),
            "plan": plan,
            "advisor_applied": plan != context.fallback_plan,
            "advisor": advisor,
            "ledger": pool.ledger(),
            "recent_leases": pool.lease_history().rev().take(5).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&document)?);
    } else {
        println!(
            "{}",
            render_pool(&state_file, loaded, &pool, &context, &plan, &advice)
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn render_plan_table(out: &mut String, plan: &AllocationPlan) {
    if plan.allocations.is_empty() {
        out.push_str("  (no allocations)\n");
    }
    for a in &plan.allocations {
        let _ = writeln!(
            out,
            "  {:28} {:8} ${:>10}  score {:>5.2}  share {:>6}%  {}",
            a.module_id,
            a.status.as_str(),
            format!("{:.4}", Fx(a.max_usdt)),
            a.score,
            format!("{:.2}", Fx(a.final_share_pct)),
            a.reason
        );
    }
    for note in &plan.notes {
        let _ = writeln!(out, "  note: {note}");
    }
}

fn render_pool(
    state_file: &std::path::Path,
    loaded: bool,
    pool: &CapitalPool,
    context: &AllocationContext,
    plan: &AllocationPlan,
    advice: &Advice,
) -> String {
    let config = pool.config();
    let mut out = String::new();
    let _ = writeln!(out, "{}", "=".repeat(72));
    let _ = writeln!(
        out,
        "  CAPITAL POOL  (pool {}, scoring {}, state {} {})",
        if config.enabled {
            "enabled"
        } else {
            "disabled"
        },
        pool.scoring(),
        state_file.display(),
        if loaded { "loaded" } else { "absent" }
    );
    let _ = writeln!(out, "{}", "=".repeat(72));
    let _ = writeln!(
        out,
        "  Equity ${:.4}  reserve ${:.4} ({}%)  allocatable ${:.4}  module min ${} max {}",
        Fx(plan.total_equity_usdt),
        Fx(plan.reserve_usdt),
        config.reserve_pct,
        Fx(plan.allocatable_usdt),
        config.min_module_allocation_usdt,
        config
            .max_module_allocation_usdt
            .map_or_else(|| "none".into(), |m| format!("${m}"))
    );
    out.push_str("\nPLAN\n");
    render_plan_table(&mut out, plan);
    match advice {
        Advice::None => {}
        Advice::File { path, proposal } => {
            let _ = writeln!(
                out,
                "\nADVISOR: file {} ({})",
                path.display(),
                if proposal.is_some() {
                    "proposal read"
                } else {
                    "no proposal; fallback"
                }
            );
        }
        Advice::Claude(report) => {
            let _ = writeln!(
                out,
                "\nADVISOR: Claude via omp — {} (session {}{}), {} in / {} out tokens, ${}",
                report.status,
                if report.session_id.is_empty() {
                    "-"
                } else {
                    &report.session_id
                },
                if report.resumed { ", resumed" } else { "" },
                report.input_tokens,
                report.output_tokens,
                report.cost_usd.round_dp(6).normalize()
            );
            if let Some(error) = &report.error {
                let _ = writeln!(out, "  error: {error}");
            }
            for call in &report.tool_calls {
                let _ = writeln!(
                    out,
                    "  tool {}{} ({} ms) args {}",
                    call.name,
                    if call.is_error { " [error]" } else { "" },
                    call.ms,
                    call.args
                );
                if call.is_error {
                    let _ = writeln!(out, "    → {}", call.result);
                }
            }
            if let Some(text) = &report.final_text {
                let _ = writeln!(out, "  final: {}", text.trim());
            }
            let applied = *plan != context.fallback_plan;
            let _ = writeln!(
                out,
                "  proposal: {}",
                match (&report.proposal, applied) {
                    (None, _) => "none — deterministic fallback kept".to_owned(),
                    (Some(_), true) => "applied within the deterministic bounds".to_owned(),
                    (Some(_), false) => "identical to the fallback".to_owned(),
                }
            );
            out.push_str("\nDETERMINISTIC FALLBACK\n");
            render_plan_table(&mut out, &context.fallback_plan);
        }
    }
    if !pool.ledger().is_empty() {
        out.push_str("\nLEDGER\n");
        for entry in pool.ledger().values() {
            let _ = writeln!(
                out,
                "  {:28} {:11} allocated ${:.4} used ${:.4} free ${:.4} pnl ${} trades {}",
                entry.module_id,
                entry.status,
                Fx(entry.allocated_usdt),
                Fx(entry.used_usdt),
                Fx(entry.free_usdt),
                signed(entry.realized_pnl_usdt, 4),
                entry.trades
            );
        }
    }
    let leases: Vec<_> = pool.lease_history().rev().take(5).collect();
    if !leases.is_empty() {
        out.push_str("\nRECENT LEASES\n");
        for lease in leases {
            let _ = writeln!(
                out,
                "  {} {} {} {} requested ${:.4} approved ${:.4} ({:?}: {})",
                bb_core::time::from_ms(lease.created_at_ms)
                    .map_or_else(String::new, bb_core::time::iso),
                lease.module_id,
                lease.side,
                lease.symbol,
                Fx(lease.requested_notional_usdt),
                Fx(lease.approved_notional_usdt),
                lease.status,
                lease.reason
            );
        }
    }
    out.trim_end_matches('\n').to_owned()
}

async fn auto_convert(args: AutoConvertArgs) -> anyhow::Result<ExitCode> {
    let mut config = AutoConvertConfig::from_env()?;
    if let Some(asset) = &args.asset {
        config.allowed_assets = parse_assets(asset)?;
    }
    if let Some(quote) = &args.quote {
        config.quote_asset = quote.trim().to_uppercase();
    }
    let live = args.submit_live && !args.plan_only;
    let mode = if live {
        let mut missing = Vec::new();
        if !bb_core::env::confirmed(&["CONFIRM_LIVE_TRADING"]) {
            missing.push("CONFIRM_LIVE_TRADING=yes");
        }
        if !config.enabled {
            missing.push("AUTO_CONVERT_TO_QUOTE=true");
        }
        if !missing.is_empty() {
            eprintln!(
                "auto-convert --submit-live refused: set {}",
                missing.join(" and ")
            );
            return Ok(ExitCode::from(2));
        }
        ConvertMode::Live
    } else {
        // Planning is always allowed; it only calls order/test.
        config.enabled = true;
        ConvertMode::PlanOnly
    };

    let binance = BinanceConfig::from_env(false)?;
    binance.require_credentials()?;
    let client = BinanceClient::new(binance)?;
    let mut request = match &args.symbol {
        Some(symbol) => {
            let mut request = FundingRequest::for_order(symbol, "buy", args.amount, Decimal::ONE)?;
            request.requested_notional = args.amount;
            request
        }
        None => FundingRequest {
            symbol: None,
            side: "buy".into(),
            quote_asset: config.quote_asset.clone(),
            requested_notional: args.amount,
            min_quote_balance: Decimal::ZERO,
            metadata: Value::Null,
        },
    };
    request.min_quote_balance = args.keep_quote;
    request.metadata = json!({"source": "auto-convert-cli"});
    let result = ensure_quote_for_order(&client, &config, &request, mode).await?;
    if mode == ConvertMode::Live && result.attempted {
        let log = bb_core::audit::JsonlLog::new(crate::config::convert_audit_path());
        if let Err(error) = log.append(&audit_row(&request, &result, mode)) {
            tracing::error!(%error, "auto-convert audit write failed");
        }
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "mode": if live { "live" } else { "plan_only" },
            "request": {
                "symbol": request.symbol,
                "quote_asset": request.quote_asset,
                "requested_notional": crate::scorecard::decimal_value(request.requested_notional),
                "min_quote_balance": crate::scorecard::decimal_value(request.min_quote_balance),
                "allowed_assets": config.allowed_assets,
            },
            "result": result,
        }))?
    );
    Ok(ExitCode::SUCCESS)
}
