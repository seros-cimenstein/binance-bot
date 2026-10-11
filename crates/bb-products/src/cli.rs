//! `bb` subcommands for the product modules (former `python -m src.X` entry points).
//!
//! Flags, defaults, messages and exit codes follow the Python argparse CLIs.

use std::path::PathBuf;
use std::process::ExitCode;

use rust_decimal::Decimal;

use crate::alerts::TelegramAlerter;
use crate::api_permissions::{self, Feature, InspectOptions};
use crate::common::print_json;
use crate::futures::{
    self, FuturesOrderIntent, FuturesOrderType, FuturesRiskLimits, OrderSide, PositionSide,
};
use crate::futures_canary::{
    self, FuturesCanaryControls, FuturesCanaryFlags, FuturesExitPlanIntent,
};
use crate::live_probe;
use crate::live_spot_allocator::{self, LiveSpotConfirmations, LiveSpotIntent, LiveSpotLimits};
use crate::margin::{
    self, AccountType, MarginBorrowRequest, MarginDryRunInputs, MarginRepayRequest,
    MarginRiskLimits,
};
use crate::margin_canary::{self, MarginCanaryFlags, MarginCanaryLimits};
use crate::prediction::{self, Direction, PredictionIntent, PredictionRiskLimits};
use crate::settings::Settings;
use crate::spot_canary::{self, SpotCanaryConfirmations, SpotCanaryLimits};
use crate::transfer_canary::{self, TransferCanaryFlags, TransferCanaryLimits};
use crate::transfers::{self, TransferRequest, TransferRiskLimits};
use crate::withdrawals::{self, WithdrawalAddress, WithdrawalRequest, WithdrawalSafetyPolicy};
use crate::{advanced_readiness, product_modules, scope_audit};

#[derive(clap::Subcommand, Debug)]
pub enum Command {
    /// Inspect Binance API permissions without placing orders.
    ApiPermissions(ApiPermissionsArgs),
    /// Audit local API symbol/asset whitelist scope.
    ScopeAudit(ScopeAuditArgs),
    /// Inspect local advanced Binance product module gates.
    ProductModules(ProductModulesArgs),
    /// Review local evidence for advanced-product canary plans.
    AdvancedReadiness(AdvancedReadinessArgs),
    /// Run read-only margin discovery and local risk modeling.
    Margin(MarginArgs),
    /// Plan one foreground margin canary using read-only margin data; live margin actions are disabled.
    MarginCanary(MarginCanaryArgs),
    /// Run read-only USD-M futures discovery and risk modeling.
    Futures(FuturesArgs),
    /// Plan a foreground USD-M futures canary; live execution is disabled.
    FuturesCanary(FuturesCanaryArgs),
    /// Plan Binance internal transfers without submitting them.
    Transfers(TransfersArgs),
    /// Build a foreground Binance universal-transfer canary plan. Live transfer submission is not implemented.
    TransferCanary(TransferCanaryArgs),
    /// Plan Binance withdrawals without submitting them.
    Withdrawals(WithdrawalsArgs),
    /// Run read-only prediction market parsing and local risk modeling.
    Prediction(PredictionArgs),
    /// Plan or submit one tightly gated spot live canary order.
    SpotCanary(SpotCanaryArgs),
    /// Plan or submit one guarded live spot allocation.
    LiveSpotAllocator(LiveSpotAllocatorArgs),
    /// Submit one tightly gated real-money live probe order.
    LiveProbe(LiveProbeArgs),
}

pub async fn run(cmd: Command) -> anyhow::Result<ExitCode> {
    Ok(match cmd {
        Command::ApiPermissions(args) => report(
            "API permission check failed",
            api_permissions_cmd(args).await,
        ),
        Command::ScopeAudit(args) => report("API scope audit failed", scope_audit_cmd(&args)),
        Command::ProductModules(args) => {
            report("Product module check failed", product_modules_cmd(&args))
        }
        Command::AdvancedReadiness(args) => report(
            "advanced readiness check failed",
            advanced_readiness_cmd(&args),
        ),
        Command::Margin(args) => margin_cmd(args).await,
        Command::MarginCanary(args) => margin_canary_cmd(args).await,
        Command::Futures(args) => futures_cmd(args).await,
        Command::FuturesCanary(args) => futures_canary_cmd(args).await,
        Command::Transfers(args) => transfers_cmd(args).await,
        Command::TransferCanary(args) => {
            report("transfer canary failed", transfer_canary_cmd(args).await)
        }
        Command::Withdrawals(args) => withdrawals_cmd(args),
        Command::Prediction(args) => prediction_cmd(args),
        Command::SpotCanary(args) => spot_canary_cmd(args).await,
        Command::LiveSpotAllocator(args) => live_spot_allocator_cmd(args).await,
        Command::LiveProbe(args) => live_probe_cmd(args).await,
    })
}

/// Print `"{prefix}: {error}"` on failure (Python `print(f"...: {exc}")`).
fn report(prefix: &str, result: anyhow::Result<ExitCode>) -> ExitCode {
    result.unwrap_or_else(|err| fail(prefix, err))
}

fn fail(prefix: &str, err: impl std::fmt::Display) -> ExitCode {
    println!("{prefix}: {err}");
    ExitCode::FAILURE
}

fn exit(ok: bool) -> ExitCode {
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

// ---------------------------------------------------------------- api-permissions

#[derive(clap::Args, Debug, Clone)]
pub struct ApiPermissionsArgs {
    /// Feature to evaluate; may be passed more than once.
    #[arg(long = "feature", value_enum)]
    pub feature: Vec<Feature>,
    /// Spot symbol for the safe /api/v3/order/test probe.
    #[arg(long)]
    pub spot_test_symbol: Option<String>,
    /// Skip the safe TRADE endpoint probe and report spot trading as skipped.
    #[arg(long)]
    pub skip_spot_order_test: bool,
    /// Print the diagnostic report as JSON.
    #[arg(long)]
    pub json: bool,
}

async fn api_permissions_cmd(args: ApiPermissionsArgs) -> anyhow::Result<ExitCode> {
    let settings = Settings::from_env()?;
    let client = settings.binance_client(0)?;
    let options = InspectOptions {
        spot_test_symbol: args.spot_test_symbol,
        include_spot_order_test: !args.skip_spot_order_test,
        features: args.feature.clone(),
    };
    let report = api_permissions::inspect_api_permissions(&settings, &client, &options).await?;
    if args.json {
        print_json(&report.to_json());
    } else {
        println!("{}", api_permissions::render_report(&report));
    }
    for feature in &args.feature {
        api_permissions::ensure_feature_allowed(&report, *feature)?;
    }
    Ok(ExitCode::SUCCESS)
}

// ---------------------------------------------------------------- scope-audit

#[derive(clap::Args, Debug, Clone)]
pub struct ScopeAuditArgs {
    /// Symbol to check.
    #[arg(long = "symbol")]
    pub symbol: Vec<String>,
    /// Asset to check.
    #[arg(long = "asset")]
    pub asset: Vec<String>,
    /// Return a non-zero exit code if any requested scope is blocked.
    #[arg(long)]
    pub fail_on_blocked: bool,
    /// Print JSON output.
    #[arg(long)]
    pub json: bool,
}

fn scope_audit_cmd(args: &ScopeAuditArgs) -> anyhow::Result<ExitCode> {
    let settings = Settings::from_env()?;
    let report = scope_audit::cli_report(&settings, &args.symbol, &args.asset)?;
    if args.json {
        print_json(&report.to_json());
    } else {
        println!("{}", scope_audit::render_scope_report(&report));
    }
    if args.fail_on_blocked {
        scope_audit::ensure_scope_allowed(&report)?;
    }
    Ok(ExitCode::SUCCESS)
}

// ---------------------------------------------------------------- product-modules

#[derive(clap::Args, Debug, Clone)]
pub struct ProductModulesArgs {
    /// Module id to report; may be passed more than once.
    #[arg(long = "module")]
    pub module: Vec<String>,
    /// Print module report as JSON.
    #[arg(long)]
    pub json: bool,
}

fn product_modules_cmd(args: &ProductModulesArgs) -> anyhow::Result<ExitCode> {
    let checks = product_modules::evaluate_product_modules(None, &args.module)?;
    if args.json {
        print_json(&checks);
    } else {
        println!("{}", product_modules::render_product_module_report(&checks));
    }
    Ok(ExitCode::SUCCESS)
}

// ---------------------------------------------------------------- advanced-readiness

#[derive(clap::ValueEnum, Debug, Clone, Copy)]
#[value(rename_all = "snake_case")]
pub enum CanaryPlanModule {
    MarginCanaryPlan,
    FuturesCanaryPlan,
    TransferCanaryPlan,
}

impl CanaryPlanModule {
    fn as_str(self) -> &'static str {
        match self {
            Self::MarginCanaryPlan => "margin_canary_plan",
            Self::FuturesCanaryPlan => "futures_canary_plan",
            Self::TransferCanaryPlan => "transfer_canary_plan",
        }
    }
}

#[derive(clap::Args, Debug, Clone)]
pub struct AdvancedReadinessArgs {
    /// JSON evidence mapping.
    #[arg(long)]
    pub evidence_file: Option<PathBuf>,
    /// Record a proposed mutable canary for serial-execution review.
    #[arg(long = "requested-live-canary", value_enum)]
    pub requested_live_canary: Vec<CanaryPlanModule>,
    /// Return non-zero when more than one mutable canary is proposed.
    #[arg(long)]
    pub fail_on_serial_conflict: bool,
    /// Print JSON output.
    #[arg(long)]
    pub json: bool,
}

fn advanced_readiness_cmd(args: &AdvancedReadinessArgs) -> anyhow::Result<ExitCode> {
    let evidence = advanced_readiness::load_evidence(args.evidence_file.as_deref())?;
    let requested: Vec<String> = args
        .requested_live_canary
        .iter()
        .map(|m| m.as_str().to_owned())
        .collect();
    let report = advanced_readiness::build_advanced_readiness_report(&evidence, &requested)?;
    if args.json {
        print_json(&report.to_json());
    } else {
        println!(
            "{}",
            advanced_readiness::render_advanced_readiness_report(&report)
        );
    }
    if args.fail_on_serial_conflict {
        advanced_readiness::ensure_serial_live_canary_execution(&report)?;
    }
    Ok(ExitCode::SUCCESS)
}

// ---------------------------------------------------------------- margin

#[derive(clap::Args, Debug, Clone)]
pub struct MarginLimitArgs {
    #[arg(long, default_value = "2.0")]
    pub min_margin_level: Decimal,
    #[arg(long, default_value = "1.1")]
    pub liquidation_margin_level: Decimal,
    #[arg(long, default_value = "25.0")]
    pub min_liquidation_buffer_percent: Decimal,
    #[arg(long, default_value = "50.0")]
    pub max_borrow_utilization_percent: Decimal,
    #[arg(long, default_value = "0.05")]
    pub max_hourly_interest_usdt: Decimal,
    #[arg(long, default_value = "25.0")]
    pub max_total_liability_usdt: Decimal,
}

impl MarginLimitArgs {
    fn limits(&self) -> MarginRiskLimits {
        MarginRiskLimits {
            min_margin_level: self.min_margin_level,
            liquidation_margin_level: self.liquidation_margin_level,
            min_liquidation_buffer_percent: self.min_liquidation_buffer_percent,
            max_borrow_utilization_percent: self.max_borrow_utilization_percent,
            max_hourly_interest_usdt: self.max_hourly_interest_usdt,
            max_total_liability_usdt: self.max_total_liability_usdt,
        }
    }
}

#[derive(clap::Args, Debug, Clone)]
pub struct MarginRequestArgs {
    #[arg(long, value_enum, default_value = "cross")]
    pub account_type: AccountType,
    /// Isolated margin symbol.
    #[arg(long)]
    pub isolated_symbol: Option<String>,
    /// Also query isolated margin account info.
    #[arg(long)]
    pub include_isolated: bool,
    /// Asset to model for borrow or repay.
    #[arg(long, default_value = "USDT")]
    pub asset: String,
    /// Borrow amount to model.
    #[arg(long)]
    pub borrow_amount: Option<Decimal>,
    /// Repay amount to model.
    #[arg(long)]
    pub repay_amount: Option<Decimal>,
    /// USDT price for non-stable assets when applying liability caps.
    #[arg(long)]
    pub asset_usdt_price: Option<Decimal>,
}

impl MarginRequestArgs {
    fn borrow(&self) -> Option<MarginBorrowRequest> {
        self.borrow_amount.map(|amount| MarginBorrowRequest {
            account_type: self.account_type,
            asset: self.asset.clone(),
            requested_amount: amount,
            isolated_symbol: self.isolated_symbol.clone(),
            asset_usdt_price: self.asset_usdt_price,
        })
    }

    fn repay(&self) -> Option<MarginRepayRequest> {
        self.repay_amount.map(|amount| MarginRepayRequest {
            account_type: self.account_type,
            asset: self.asset.clone(),
            requested_amount: amount,
            isolated_symbol: self.isolated_symbol.clone(),
        })
    }

    fn include_isolated(&self) -> bool {
        self.include_isolated || self.account_type == AccountType::Isolated
    }
}

#[derive(clap::Args, Debug, Clone)]
pub struct MarginArgs {
    /// Required; confirms no live margin write action will be attempted.
    #[arg(long)]
    pub dry_run: bool,
    /// Always fails; live margin activation is not implemented.
    #[arg(long)]
    pub confirm_live_margin: bool,
    #[command(flatten)]
    pub request: MarginRequestArgs,
    #[command(flatten)]
    pub limits: MarginLimitArgs,
    /// Print JSON output.
    #[arg(long)]
    pub json: bool,
}

async fn margin_cmd(args: MarginArgs) -> ExitCode {
    if !args.dry_run {
        println!("margin check failed: --dry-run is required; live margin is disabled");
        return ExitCode::FAILURE;
    }
    let result: anyhow::Result<()> = async {
        margin::assert_margin_live_disabled(args.confirm_live_margin)?;
        let settings = Settings::from_env()?;
        let include_isolated = args.request.include_isolated();
        let isolated_symbols = match &args.request.isolated_symbol {
            Some(symbol) => vec![symbol.clone()],
            None if include_isolated => vec![settings.trading_symbol.clone()],
            None => Vec::new(),
        };
        let inputs = MarginDryRunInputs {
            include_isolated,
            isolated_symbols,
            borrow_request: args.request.borrow(),
            repay_request: args.request.repay(),
            limits: args.limits.limits(),
        };
        let client = settings.binance_client(0)?;
        let report = margin::build_margin_dry_run_report(&settings, &client, &inputs).await?;
        if args.json {
            print_json(&report);
        } else {
            println!("{}", margin::render_margin_report(&report));
        }
        Ok(())
    }
    .await;
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => fail("margin check failed", err),
    }
}

#[derive(clap::Args, Debug, Clone)]
pub struct MarginCanaryArgs {
    /// Required; build and audit a read-only plan without mutation.
    #[arg(long)]
    pub plan_only: bool,
    /// Always fails; margin action submission is not implemented.
    #[arg(long)]
    pub submit_action: bool,
    /// Always fails; live margin execution is not approved.
    #[arg(long)]
    pub confirm_live_margin: bool,
    #[command(flatten)]
    pub request: MarginRequestArgs,
    /// Symbol intended for margin planning and local scope auditing.
    #[arg(long = "margin-symbol")]
    pub margin_symbol: Vec<String>,
    #[command(flatten)]
    pub limits: MarginLimitArgs,
    #[arg(long, env = "MARGIN_CANARY_AUDIT_PATH", default_value = margin_canary::DEFAULT_AUDIT_PATH)]
    pub audit_path: PathBuf,
    #[arg(long, env = "MARGIN_CANARY_STOP_PATH", default_value = margin_canary::DEFAULT_STOP_PATH)]
    pub emergency_stop_path: PathBuf,
    /// Print JSON output.
    #[arg(long)]
    pub json: bool,
}

async fn margin_canary_cmd(args: MarginCanaryArgs) -> ExitCode {
    if !args.plan_only {
        println!("Margin canary failed: --plan-only is required; live margin is disabled");
        return ExitCode::FAILURE;
    }
    let result: anyhow::Result<ExitCode> = async {
        let flags = MarginCanaryFlags {
            submit_action: args.submit_action,
            confirm_live_margin: args.confirm_live_margin,
        };
        margin_canary::preflight(flags)?;
        let settings = Settings::from_env()?;
        let inputs = MarginDryRunInputs {
            include_isolated: args.request.include_isolated(),
            isolated_symbols: args.request.isolated_symbol.iter().cloned().collect(),
            borrow_request: args.request.borrow(),
            repay_request: args.request.repay(),
            limits: args.limits.limits(),
        };
        let limits = MarginCanaryLimits {
            audit_path: args.audit_path.clone(),
            emergency_stop_path: args.emergency_stop_path.clone(),
        };
        let client = settings.binance_client(0)?;
        let alerter = TelegramAlerter::from_settings(&settings);
        let result = margin_canary::run_margin_canary(
            &settings,
            &client,
            &alerter,
            flags,
            &inputs,
            &args.margin_symbol,
            &limits,
        )
        .await?;
        if args.json {
            print_json(&result);
        } else {
            println!("{}", margin_canary::render_margin_canary_result(&result));
        }
        Ok(exit(result.status == "planned"))
    }
    .await;
    result.unwrap_or_else(|err| fail("Margin canary failed", err))
}

// ---------------------------------------------------------------- futures

#[derive(clap::Args, Debug, Clone)]
pub struct FuturesLimitArgs {
    #[arg(long, default_value = "3.0")]
    pub max_leverage: Decimal,
    #[arg(long, default_value = "25.0")]
    pub max_notional_usdt: Decimal,
    #[arg(long, default_value = "25.0")]
    pub max_account_notional_percent: Decimal,
    #[arg(long, default_value = "10.0")]
    pub max_initial_margin_usdt: Decimal,
    #[arg(long, default_value = "5.0")]
    pub min_free_balance_after_initial_margin_usdt: Decimal,
    #[arg(long, default_value = "20.0")]
    pub min_liquidation_distance_percent: Decimal,
    #[arg(long, default_value = "0.001")]
    pub max_abs_funding_rate: Decimal,
    #[arg(long, default_value = "0.05")]
    pub max_abs_funding_payment_usdt: Decimal,
    #[arg(long, default_value = "50.0")]
    pub min_maintenance_buffer_percent: Decimal,
}

impl FuturesLimitArgs {
    fn limits(&self) -> FuturesRiskLimits {
        FuturesRiskLimits {
            max_leverage: self.max_leverage,
            max_notional_usdt: self.max_notional_usdt,
            max_account_notional_percent: self.max_account_notional_percent,
            max_initial_margin_usdt: self.max_initial_margin_usdt,
            min_free_balance_after_initial_margin_usdt: self
                .min_free_balance_after_initial_margin_usdt,
            min_liquidation_distance_percent: self.min_liquidation_distance_percent,
            max_abs_funding_rate: self.max_abs_funding_rate,
            max_abs_funding_payment_usdt: self.max_abs_funding_payment_usdt,
            min_maintenance_buffer_percent: self.min_maintenance_buffer_percent,
            ..FuturesRiskLimits::default()
        }
    }
}

#[derive(clap::Args, Debug, Clone)]
pub struct FuturesArgs {
    /// Required; confirms no live futures write action will be attempted.
    #[arg(long)]
    pub dry_run: bool,
    /// Always fails; live futures activation is not implemented.
    #[arg(long)]
    pub confirm_live_futures: bool,
    /// USD-M futures symbol, for example BTC/USDT.
    #[arg(long)]
    pub symbol: Option<String>,
    #[arg(long, value_enum, default_value = "BUY")]
    pub side: OrderSide,
    /// Quantity to model.
    #[arg(long)]
    pub quantity: Option<Decimal>,
    /// Limit price to model.
    #[arg(long)]
    pub price: Option<Decimal>,
    #[arg(long, value_enum, default_value = "MARKET")]
    pub order_type: FuturesOrderType,
    #[arg(long, value_enum, default_value = "BOTH")]
    pub position_side: PositionSide,
    #[arg(long)]
    pub reduce_only: bool,
    #[arg(long)]
    pub requested_leverage: Option<Decimal>,
    #[command(flatten)]
    pub limits: FuturesLimitArgs,
    /// Print JSON output.
    #[arg(long)]
    pub json: bool,
}

async fn futures_cmd(args: FuturesArgs) -> ExitCode {
    if !args.dry_run {
        println!("futures check failed: --dry-run is required; live futures is disabled");
        return ExitCode::FAILURE;
    }
    let result: anyhow::Result<()> = async {
        futures::assert_futures_live_disabled(args.confirm_live_futures)?;
        let settings = Settings::from_env()?;
        let symbol = bb_binance::normalize_symbol(
            args.symbol.as_deref().unwrap_or(&settings.trading_symbol),
        )?;
        let intent = args.quantity.map(|quantity| FuturesOrderIntent {
            symbol: symbol.clone(),
            side: args.side,
            quantity,
            order_type: args.order_type,
            price: args.price,
            position_side: args.position_side,
            reduce_only: args.reduce_only,
            requested_leverage: args.requested_leverage,
        });
        let client = settings.binance_client(0)?;
        let report = futures::build_futures_dry_run_report(
            &settings,
            &client,
            &[symbol],
            intent.as_ref(),
            &args.limits.limits(),
        )
        .await?;
        if args.json {
            print_json(&report);
        } else {
            println!("{}", futures::render_futures_report(&report));
        }
        Ok(())
    }
    .await;
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => fail("futures check failed", err),
    }
}

#[derive(clap::Args, Debug, Clone)]
pub struct FuturesCanaryArgs {
    /// Required; permits read-only planning and JSONL audit output.
    #[arg(long = "plan-only", visible_alias = "dry-run")]
    pub plan_only: bool,
    /// Always fails; this slice has no futures submission path.
    #[arg(long = "submit-order", visible_alias = "submit")]
    pub submit_order: bool,
    /// Always fails; live futures execution is not approved.
    #[arg(long)]
    pub confirm_live_futures: bool,
    /// USD-M symbol, for example BTC/USDT.
    #[arg(long)]
    pub symbol: Option<String>,
    #[arg(long, value_enum, default_value = "BUY")]
    pub side: OrderSide,
    /// Entry quantity to plan.
    #[arg(long)]
    pub quantity: Option<Decimal>,
    #[arg(long, value_enum, default_value = "MARKET")]
    pub order_type: FuturesOrderType,
    /// Entry limit price.
    #[arg(long)]
    pub price: Option<Decimal>,
    #[arg(long, value_enum, default_value = "BOTH")]
    pub position_side: PositionSide,
    #[arg(long)]
    pub requested_leverage: Option<Decimal>,
    /// Required opposite-side closure intent.
    #[arg(long, value_enum)]
    pub exit_side: Option<OrderSide>,
    /// Defaults to entry quantity.
    #[arg(long)]
    pub exit_quantity: Option<Decimal>,
    #[arg(long, value_enum, default_value = "MARKET")]
    pub exit_order_type: FuturesOrderType,
    /// Exit limit price.
    #[arg(long)]
    pub exit_price: Option<Decimal>,
    /// Required; documents that the exit may only reduce the canary position.
    #[arg(long)]
    pub exit_reduce_only: bool,
    /// Required operator-readable trigger or condition for the exit intent.
    #[arg(long)]
    pub exit_trigger: Option<String>,
    #[arg(long, default_value = futures_canary::DEFAULT_AUDIT_PATH)]
    pub audit_path: PathBuf,
    #[arg(long, default_value = futures_canary::DEFAULT_STOP_PATH)]
    pub emergency_stop_path: PathBuf,
    #[command(flatten)]
    pub limits: FuturesLimitArgs,
    /// Print JSON output.
    #[arg(long)]
    pub json: bool,
}

async fn futures_canary_cmd(args: FuturesCanaryArgs) -> ExitCode {
    if args.submit_order || args.confirm_live_futures {
        println!("Futures canary failed: live futures submission is disabled");
        return ExitCode::FAILURE;
    }
    if !args.plan_only {
        println!("Futures canary failed: --plan-only is required");
        return ExitCode::FAILURE;
    }
    let Some(quantity) = args.quantity else {
        println!("Futures canary failed: --quantity is required");
        return ExitCode::FAILURE;
    };
    let (Some(exit_side), true, Some(trigger)) = (
        args.exit_side,
        args.exit_reduce_only,
        args.exit_trigger.clone(),
    ) else {
        println!(
            "Futures canary failed: --exit-side, --exit-reduce-only, and --exit-trigger are required"
        );
        return ExitCode::FAILURE;
    };
    let result: anyhow::Result<ExitCode> = async {
        let settings = Settings::from_env()?;
        let symbol = futures_canary::canary_symbol(
            args.symbol.as_deref().unwrap_or(&settings.trading_symbol),
        )?;
        let intent = FuturesOrderIntent {
            symbol: symbol.clone(),
            side: args.side,
            quantity,
            order_type: args.order_type,
            price: args.price,
            position_side: args.position_side,
            reduce_only: false,
            requested_leverage: args.requested_leverage,
        };
        let exit_plan = FuturesExitPlanIntent {
            symbol,
            side: exit_side,
            quantity: args.exit_quantity.unwrap_or(quantity),
            trigger,
            order_type: args.exit_order_type,
            price: args.exit_price,
            position_side: args.position_side,
            reduce_only: args.exit_reduce_only,
        };
        let controls = FuturesCanaryControls {
            audit_path: args.audit_path.clone(),
            emergency_stop_path: args.emergency_stop_path.clone(),
        };
        let client = settings.binance_client(0)?;
        let result = futures_canary::run_futures_canary(
            &settings,
            &client,
            &intent,
            &exit_plan,
            &args.limits.limits(),
            &controls,
            FuturesCanaryFlags::default(),
        )
        .await?;
        if args.json {
            print_json(&result);
        } else {
            println!("{}", futures_canary::render_futures_canary_result(&result));
        }
        Ok(exit(result.status == "planned"))
    }
    .await;
    result.unwrap_or_else(|err| fail("Futures canary failed", err))
}

// ---------------------------------------------------------------- transfers

#[derive(clap::Args, Debug, Clone)]
pub struct TransferRequestArgs {
    #[arg(long, default_value = transfers::WALLET_SPOT)]
    pub source_wallet: String,
    #[arg(long, default_value = transfers::WALLET_FUNDING)]
    pub destination_wallet: String,
    #[arg(long, default_value = "USDT")]
    pub asset: String,
    #[arg(long)]
    pub amount: Option<Decimal>,
    #[arg(long)]
    pub asset_usdt_price: Option<Decimal>,
    #[arg(long)]
    pub from_symbol: Option<String>,
    #[arg(long)]
    pub to_symbol: Option<String>,
    #[arg(long, default_value = "10.0")]
    pub max_transfer_amount: Decimal,
    #[arg(long, default_value = "25.0")]
    pub max_transfer_value_usdt: Decimal,
    #[arg(long, default_value = "0.0")]
    pub min_source_free_after: Decimal,
}

impl TransferRequestArgs {
    fn request(&self, amount: Decimal, reason: &str) -> TransferRequest {
        TransferRequest {
            reason: reason.to_owned(),
            asset_usdt_price: self.asset_usdt_price,
            from_symbol: self.from_symbol.clone(),
            to_symbol: self.to_symbol.clone(),
            ..TransferRequest::new(
                &self.source_wallet,
                &self.destination_wallet,
                &self.asset,
                amount,
            )
        }
    }

    fn limits(&self) -> TransferRiskLimits {
        TransferRiskLimits {
            max_transfer_amount: self.max_transfer_amount,
            max_transfer_value_usdt: self.max_transfer_value_usdt,
            min_source_free_after: self.min_source_free_after,
            ..TransferRiskLimits::default()
        }
    }
}

#[derive(clap::Args, Debug, Clone)]
pub struct TransfersArgs {
    /// Required.
    #[arg(long)]
    pub dry_run: bool,
    /// Always fails; live transfer activation is not implemented.
    #[arg(long)]
    pub confirm_live_transfer: bool,
    #[command(flatten)]
    pub request: TransferRequestArgs,
    #[arg(long, default_value = "manual dry-run transfer plan")]
    pub reason: String,
    #[arg(long, default_value = transfers::DEFAULT_AUDIT_PATH)]
    pub audit_path: PathBuf,
    #[arg(long)]
    pub no_audit: bool,
    #[arg(long)]
    pub json: bool,
}

async fn transfers_cmd(args: TransfersArgs) -> ExitCode {
    if !args.dry_run {
        println!("transfer check failed: --dry-run is required; live transfer is disabled");
        return ExitCode::FAILURE;
    }
    let result: anyhow::Result<()> = async {
        transfers::assert_transfer_live_disabled(args.confirm_live_transfer)?;
        let amount = args
            .request
            .amount
            .ok_or_else(|| anyhow::anyhow!("--amount is required for transfer planning"))?;
        let settings = Settings::from_env()?;
        let client = settings.binance_client(0)?;
        let audit = (!args.no_audit).then_some(args.audit_path.as_path());
        let report = transfers::build_transfer_dry_run_report(
            &settings,
            &client,
            &args.request.request(amount, &args.reason),
            &args.request.limits(),
            audit,
        )
        .await?;
        if args.json {
            print_json(&report);
        } else {
            println!("{}", transfers::render_transfer_report(&report));
        }
        Ok(())
    }
    .await;
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => fail("transfer check failed", err),
    }
}

#[derive(clap::Args, Debug, Clone)]
pub struct TransferCanaryArgs {
    /// Required; run read-only discovery and local transfer planning.
    #[arg(long)]
    pub plan_only: bool,
    /// Always fails; universal-transfer submission is disabled.
    #[arg(long)]
    pub submit_transfer: bool,
    /// Always fails; live universal-transfer activation is disabled.
    #[arg(long)]
    pub confirm_live_transfer: bool,
    #[command(flatten)]
    pub request: TransferRequestArgs,
    #[arg(long, default_value = "foreground plan-only transfer canary")]
    pub reason: String,
    #[arg(long, default_value = transfer_canary::DEFAULT_AUDIT_PATH)]
    pub audit_path: PathBuf,
    #[arg(long, default_value = transfer_canary::DEFAULT_STOP_PATH)]
    pub emergency_stop_path: PathBuf,
    #[arg(long)]
    pub json: bool,
}

async fn transfer_canary_cmd(args: TransferCanaryArgs) -> anyhow::Result<ExitCode> {
    if !args.plan_only {
        anyhow::bail!("--plan-only is required; submission is disabled");
    }
    let flags = TransferCanaryFlags {
        submit_transfer: args.submit_transfer,
        confirm_live_transfer: args.confirm_live_transfer,
    };
    transfer_canary::preflight(flags, &args.emergency_stop_path)?;
    let amount = args
        .request
        .amount
        .ok_or_else(|| anyhow::anyhow!("--amount is required for transfer planning"))?;
    let settings = Settings::from_env()?;
    let client = settings.binance_client(0)?;
    let limits = TransferCanaryLimits {
        risk_limits: args.request.limits(),
        audit_path: args.audit_path.clone(),
        emergency_stop_path: args.emergency_stop_path.clone(),
    };
    let request = args.request.request(amount, &args.reason);
    let result =
        transfer_canary::run_transfer_canary(&settings, &client, &request, flags, &limits).await?;
    if args.json {
        print_json(&result);
    } else {
        println!(
            "{}",
            transfer_canary::render_transfer_canary_result(&result)
        );
    }
    Ok(ExitCode::SUCCESS)
}

// ---------------------------------------------------------------- withdrawals

#[derive(clap::Args, Debug, Clone)]
pub struct WithdrawalsArgs {
    /// Required.
    #[arg(long)]
    pub dry_run: bool,
    /// Always fails; live withdrawal activation is not implemented.
    #[arg(long)]
    pub confirm_live_withdrawal: bool,
    #[arg(long, default_value = "USDT")]
    pub asset: String,
    #[arg(long)]
    pub amount: Option<Decimal>,
    #[arg(long)]
    pub address: Option<String>,
    #[arg(long)]
    pub network: Option<String>,
    #[arg(long)]
    pub memo: Option<String>,
    #[arg(long, default_value = "")]
    pub operator_id: String,
    #[arg(long, default_value = "manual dry-run withdrawal plan")]
    pub reason: String,
    #[arg(long = "allow-address")]
    pub allow_address: Vec<String>,
    #[arg(long = "allow-network")]
    pub allow_network: Vec<String>,
    #[arg(long)]
    pub allow_memo: Option<String>,
    #[arg(long, default_value = "")]
    pub allow_label: String,
    #[arg(long = "allowed-operator")]
    pub allowed_operator: Vec<String>,
    #[arg(long = "approval")]
    pub approval: Vec<String>,
    #[arg(long, default_value_t = 2)]
    pub approvals_required: i64,
    #[arg(long, default_value = "10.0")]
    pub max_per_request_amount: Decimal,
    #[arg(long, default_value = "25.0")]
    pub max_daily_amount: Decimal,
    #[arg(long, default_value = "0.0")]
    pub daily_used_amount: Decimal,
    #[arg(long, default_value_t = 3600)]
    pub cooldown_seconds: i64,
    #[arg(long)]
    pub last_withdrawal_at: Option<String>,
    #[arg(long)]
    pub emergency_stop: bool,
    #[arg(long, default_value = withdrawals::DEFAULT_AUDIT_PATH)]
    pub audit_path: PathBuf,
    #[arg(long)]
    pub no_audit: bool,
    #[arg(long)]
    pub json: bool,
}

fn withdrawals_cmd(args: WithdrawalsArgs) -> ExitCode {
    if !args.dry_run {
        println!("withdrawal check failed: --dry-run is required; live withdrawal is disabled");
        return ExitCode::FAILURE;
    }
    let result: anyhow::Result<()> = (|| {
        withdrawals::assert_withdrawal_live_disabled(args.confirm_live_withdrawal)?;
        let amount = args
            .amount
            .ok_or_else(|| anyhow::anyhow!("--amount is required for withdrawal planning"))?;
        let address = args
            .address
            .clone()
            .filter(|a| !a.is_empty())
            .ok_or_else(|| anyhow::anyhow!("--address is required for withdrawal planning"))?;
        let settings = Settings::from_env()?;
        let networks = args
            .allow_network
            .iter()
            .map(|n| withdrawals::network(n))
            .collect::<Result<Vec<_>, _>>()?;
        let allowlist = args
            .allow_address
            .iter()
            .map(|addr| WithdrawalAddress {
                asset: args.asset.clone(),
                address: addr.clone(),
                networks: networks.clone(),
                label: args.allow_label.clone(),
                memo: withdrawals::optional_text(args.allow_memo.as_deref()),
                active: true,
            })
            .collect();
        let request = WithdrawalRequest {
            asset: args.asset.clone(),
            amount,
            address,
            network: args.network.clone(),
            memo: args.memo.clone(),
            operator_id: args.operator_id.clone(),
            reason: args.reason.clone(),
        };
        let policy = WithdrawalSafetyPolicy {
            allowlist,
            max_per_request_amount: args.max_per_request_amount,
            max_daily_amount: args.max_daily_amount,
            daily_used_amount: args.daily_used_amount,
            approvals_required: args.approvals_required,
            approvals: args.approval.clone(),
            cooldown_seconds: args.cooldown_seconds,
            last_withdrawal_at: args.last_withdrawal_at.clone(),
            emergency_stop: args.emergency_stop,
            allowed_operators: args.allowed_operator.clone(),
            require_operator_identity: true,
        };
        let audit = (!args.no_audit).then_some(args.audit_path.as_path());
        let report =
            withdrawals::build_withdrawal_dry_run_report(&settings, &request, &policy, audit)?;
        if args.json {
            print_json(&report);
        } else {
            println!("{}", withdrawals::render_withdrawal_report(&report));
        }
        Ok(())
    })();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => fail("withdrawal check failed", err),
    }
}

// ---------------------------------------------------------------- prediction

#[derive(clap::Args, Debug, Clone)]
pub struct PredictionArgs {
    /// Required.
    #[arg(long)]
    pub dry_run: bool,
    /// Always fails; live prediction activation is not implemented.
    #[arg(long)]
    pub confirm_live_prediction: bool,
    /// Local prediction market JSON payload.
    #[arg(long)]
    pub market_data_file: Option<PathBuf>,
    /// Prediction symbol, for example BTC/USDT.
    #[arg(long)]
    pub symbol: Option<String>,
    /// Market id, round id, or symbol to model.
    #[arg(long)]
    pub market: Option<String>,
    #[arg(long, value_enum)]
    pub direction: Option<Direction>,
    #[arg(long)]
    pub stake_usdt: Option<Decimal>,
    #[arg(long)]
    pub bankroll_usdt: Option<Decimal>,
    #[arg(long, default_value = "0.0")]
    pub current_open_stake_usdt: Decimal,
    #[arg(long, default_value = "0.0")]
    pub current_drawdown_percent: Decimal,
    #[arg(long)]
    pub emergency_stop: bool,
    #[arg(long, default_value = "5.0")]
    pub max_stake_usdt: Decimal,
    #[arg(long, default_value = "10.0")]
    pub max_total_stake_usdt: Decimal,
    #[arg(long, default_value = "5.0")]
    pub max_stake_bankroll_percent: Decimal,
    #[arg(long, default_value_t = 60)]
    pub min_seconds_to_lock: i64,
    #[arg(long, default_value_t = 3600)]
    pub max_seconds_to_settlement: i64,
    #[arg(long, default_value = "25.0")]
    pub min_total_pool_usdt: Decimal,
    #[arg(long, default_value = "80.0")]
    pub max_pool_imbalance_percent: Decimal,
    #[arg(long, default_value = "50.0")]
    pub max_settlement_risk_score: Decimal,
    #[arg(long, default_value = "10.0")]
    pub max_drawdown_percent: Decimal,
    /// Print JSON output.
    #[arg(long)]
    pub json: bool,
}

fn prediction_cmd(args: PredictionArgs) -> ExitCode {
    if !args.dry_run {
        println!("prediction check failed: --dry-run is required; live prediction is disabled");
        return ExitCode::FAILURE;
    }
    let result: anyhow::Result<()> = (|| {
        prediction::assert_prediction_live_disabled(args.confirm_live_prediction)?;
        let payload = prediction::load_json_payload(args.market_data_file.as_deref())?;
        let settings = Settings::from_env()?;
        let symbol = bb_binance::normalize_symbol(
            args.symbol.as_deref().unwrap_or(&settings.trading_symbol),
        )?;
        let limits = PredictionRiskLimits {
            max_stake_usdt: args.max_stake_usdt,
            max_total_stake_usdt: args.max_total_stake_usdt,
            max_stake_bankroll_percent: args.max_stake_bankroll_percent,
            min_seconds_to_lock: args.min_seconds_to_lock,
            max_seconds_to_settlement: args.max_seconds_to_settlement,
            min_total_pool_usdt: args.min_total_pool_usdt,
            max_pool_imbalance_percent: args.max_pool_imbalance_percent,
            max_settlement_risk_score: args.max_settlement_risk_score,
            max_drawdown_percent: args.max_drawdown_percent,
            ..PredictionRiskLimits::default()
        };
        let wants_plan =
            args.direction.is_some() || args.stake_usdt.is_some() || args.bankroll_usdt.is_some();
        let intent = wants_plan.then(|| PredictionIntent {
            market: args.market.clone().unwrap_or_else(|| symbol.clone()),
            direction: args.direction,
            stake_usdt: args.stake_usdt,
            bankroll_usdt: args.bankroll_usdt,
            current_open_stake_usdt: args.current_open_stake_usdt,
            current_drawdown_percent: args.current_drawdown_percent,
            emergency_stop: args.emergency_stop,
            now: None,
        });
        let report = prediction::build_prediction_dry_run_report(
            &settings,
            &payload,
            &[symbol],
            intent.as_ref(),
            &limits,
        )?;
        if args.json {
            print_json(&report);
        } else {
            println!("{}", prediction::render_prediction_report(&report));
        }
        Ok(())
    })();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => fail("prediction check failed", err),
    }
}

// ---------------------------------------------------------------- spot-canary

#[derive(clap::Args, Debug, Clone)]
pub struct SpotCanaryArgs {
    #[arg(long, default_value = "BNB")]
    pub source_asset: String,
    #[arg(long, default_value = "USDT")]
    pub destination_asset: String,
    #[arg(long, default_value = "6.0")]
    pub max_order_notional_usdt: Decimal,
    #[arg(long, default_value = "6.0")]
    pub max_daily_notional_usdt: Decimal,
    #[arg(long, default_value_t = 1)]
    pub max_daily_orders: u32,
    #[arg(long, default_value = "50.0")]
    pub max_spread_bps: Decimal,
    #[arg(long, env = "SPOT_CANARY_AUDIT_PATH", default_value = spot_canary::DEFAULT_AUDIT_PATH)]
    pub audit_path: PathBuf,
    #[arg(long, env = "SPOT_CANARY_STOP_PATH", default_value = spot_canary::DEFAULT_STOP_PATH)]
    pub emergency_stop_path: PathBuf,
    /// Write a proposed canary audit record without submitting an order.
    #[arg(long)]
    pub plan_only: bool,
    /// Required unless --plan-only is used.
    #[arg(long)]
    pub confirm_live_canary: bool,
    /// Required unless --plan-only is used.
    #[arg(long)]
    pub confirm_replay_passed: bool,
    /// Required unless --plan-only is used.
    #[arg(long)]
    pub confirm_paper_warmup_reviewed: bool,
}

async fn spot_canary_cmd(args: SpotCanaryArgs) -> ExitCode {
    let submit_order = !args.plan_only;
    if submit_order && !args.confirm_live_canary {
        println!("Spot canary failed: spot canary requires --confirm-live-canary");
        return ExitCode::FAILURE;
    }
    let result: anyhow::Result<()> = async {
        let limits = SpotCanaryLimits {
            max_order_notional_usdt: args.max_order_notional_usdt,
            max_daily_notional_usdt: args.max_daily_notional_usdt,
            max_daily_orders: args.max_daily_orders,
            max_spread_bps: args.max_spread_bps,
            audit_path: args.audit_path.clone(),
            emergency_stop_path: args.emergency_stop_path.clone(),
        };
        let confirmations = SpotCanaryConfirmations {
            submit_order,
            confirm_live_canary: args.confirm_live_canary,
            confirm_replay_passed: args.confirm_replay_passed,
            confirm_paper_warmup_reviewed: args.confirm_paper_warmup_reviewed,
        };
        let settings = Settings::from_env()?;
        let client = settings.binance_client(0)?;
        let alerter = TelegramAlerter::from_settings(&settings);
        let result = spot_canary::run_spot_canary(
            &settings,
            &client,
            &alerter,
            confirmations,
            &args.source_asset,
            &args.destination_asset,
            &limits,
        )
        .await?;
        print_json(&result);
        Ok(())
    }
    .await;
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => fail("Spot canary failed", err),
    }
}

// ---------------------------------------------------------------- live-spot-allocator

#[derive(clap::Args, Debug, Clone)]
pub struct LiveSpotAllocatorArgs {
    #[arg(long, default_value = "BNB")]
    pub source_asset: String,
    #[arg(long, default_value = "USDT")]
    pub destination_asset: String,
    #[arg(long)]
    pub requested_source_amount: Option<Decimal>,
    #[arg(long, default_value = "manual foreground live spot allocation")]
    pub reason: String,
    #[arg(long, default_value = "6.0")]
    pub max_order_notional_usdt: Decimal,
    #[arg(long, default_value = "12.0")]
    pub max_daily_notional_usdt: Decimal,
    #[arg(long, default_value_t = 2)]
    pub max_daily_orders: u32,
    #[arg(long, default_value = "50.0")]
    pub max_spread_bps: Decimal,
    #[arg(long, default_value = "25.0")]
    pub slippage_buffer_bps: Decimal,
    #[arg(long, default_value = "100.0")]
    pub max_turnover_percent: Decimal,
    #[arg(long, default_value = "100.0")]
    pub max_asset_exposure_percent: Decimal,
    #[arg(long, default_value = "0.0")]
    pub max_total_exposure_usdt: Decimal,
    #[arg(long, default_value = "0.0")]
    pub daily_loss_limit_usdt: Decimal,
    #[arg(long, default_value = "0.0")]
    pub max_drawdown_percent: Decimal,
    #[arg(long, default_value = "0.00005")]
    pub min_bnb_reserve: Decimal,
    #[arg(long, env = "LIVE_SPOT_ALLOCATOR_AUDIT_PATH", default_value = live_spot_allocator::DEFAULT_AUDIT_PATH)]
    pub audit_path: PathBuf,
    #[arg(long, env = "LIVE_SPOT_ALLOCATOR_STOP_PATH", default_value = live_spot_allocator::DEFAULT_STOP_PATH)]
    pub emergency_stop_path: PathBuf,
    /// Write a proposed allocation audit record without submitting an order.
    #[arg(long)]
    pub plan_only: bool,
    #[arg(long)]
    pub confirm_live_allocator: bool,
    #[arg(long)]
    pub confirm_canary_reviewed: bool,
    #[arg(long)]
    pub confirm_replay_passed: bool,
    #[arg(long)]
    pub confirm_paper_warmup_reviewed: bool,
    #[arg(long)]
    pub confirm_permission_diagnostics: bool,
}

async fn live_spot_allocator_cmd(args: LiveSpotAllocatorArgs) -> ExitCode {
    let submit_order = !args.plan_only;
    if submit_order && !args.confirm_live_allocator {
        println!(
            "Live spot allocator failed: live spot allocator requires --confirm-live-allocator"
        );
        return ExitCode::FAILURE;
    }
    let result: anyhow::Result<()> = async {
        let limits = LiveSpotLimits {
            max_order_notional_usdt: args.max_order_notional_usdt,
            max_daily_notional_usdt: args.max_daily_notional_usdt,
            max_daily_orders: args.max_daily_orders,
            max_spread_bps: args.max_spread_bps,
            slippage_buffer_bps: args.slippage_buffer_bps,
            max_turnover_percent: args.max_turnover_percent,
            max_asset_exposure_percent: args.max_asset_exposure_percent,
            max_total_exposure_usdt: args.max_total_exposure_usdt,
            daily_loss_limit_usdt: args.daily_loss_limit_usdt,
            max_drawdown_percent: args.max_drawdown_percent,
            min_bnb_reserve: args.min_bnb_reserve,
            max_route_legs: 1,
            audit_path: args.audit_path.clone(),
            emergency_stop_path: args.emergency_stop_path.clone(),
        };
        let intent = LiveSpotIntent {
            source_asset: args.source_asset.clone(),
            destination_asset: args.destination_asset.clone(),
            requested_source_amount: args.requested_source_amount,
            reason: args.reason.clone(),
        };
        let confirmations = LiveSpotConfirmations {
            submit_order,
            confirm_live_allocator: args.confirm_live_allocator,
            confirm_canary_reviewed: args.confirm_canary_reviewed,
            confirm_replay_passed: args.confirm_replay_passed,
            confirm_paper_warmup_reviewed: args.confirm_paper_warmup_reviewed,
            confirm_permission_diagnostics: args.confirm_permission_diagnostics,
        };
        let settings = Settings::from_env()?;
        let client = settings.binance_client(0)?;
        let alerter = TelegramAlerter::from_settings(&settings);
        let result = live_spot_allocator::run_live_spot_allocator(
            &settings,
            &client,
            &alerter,
            confirmations,
            &intent,
            &limits,
        )
        .await?;
        print_json(&result);
        Ok(())
    }
    .await;
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => fail("Live spot allocator failed", err),
    }
}

// ---------------------------------------------------------------- live-probe

#[derive(clap::Args, Debug, Clone)]
pub struct LiveProbeArgs {
    /// Required; confirms this command may submit one real market sell.
    #[arg(long)]
    pub confirm_real_order: bool,
    /// Preferred USDT cap; Binance minimums may exceed it for tiny-balance probes.
    #[arg(long, default_value = "6.0")]
    pub max_notional_usdt: Decimal,
    /// JSONL audit path for submitted probe orders.
    #[arg(long, env = "LIVE_PROBE_AUDIT_PATH", default_value = live_probe::DEFAULT_AUDIT_PATH)]
    pub audit_path: PathBuf,
}

async fn live_probe_cmd(args: LiveProbeArgs) -> ExitCode {
    if !args.confirm_real_order {
        println!("Live probe failed: live probe requires --confirm-real-order");
        return ExitCode::FAILURE;
    }
    let result: anyhow::Result<()> = async {
        let settings = Settings::from_env()?;
        let client = settings.binance_client(0)?;
        let alerter = TelegramAlerter::from_settings(&settings);
        let result = live_probe::run_live_probe(
            &settings,
            &client,
            &alerter,
            args.confirm_real_order,
            args.max_notional_usdt,
            &args.audit_path,
        )
        .await?;
        print_json(&result);
        Ok(())
    }
    .await;
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => fail("Live probe failed", err),
    }
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser};

    use super::*;

    #[derive(clap::Parser, Debug)]
    struct Cli {
        #[command(subcommand)]
        command: Command,
    }

    #[test]
    fn cli_definition_is_valid_and_parses_readme_examples() {
        Cli::command().debug_assert();
        for argv in [
            vec![
                "bb",
                "api-permissions",
                "--feature",
                "claude_spot_autonomous",
            ],
            vec![
                "bb",
                "scope-audit",
                "--symbol",
                "BNB/USDT",
                "--asset",
                "BNB",
            ],
            vec!["bb", "product-modules"],
            vec![
                "bb",
                "advanced-readiness",
                "--requested-live-canary",
                "margin_canary_plan",
            ],
            vec![
                "bb",
                "margin",
                "--dry-run",
                "--asset",
                "USDT",
                "--borrow-amount",
                "10",
            ],
            vec![
                "bb",
                "margin-canary",
                "--plan-only",
                "--margin-symbol",
                "BNB/USDT",
                "--asset",
                "USDT",
                "--borrow-amount",
                "1",
                "--repay-amount",
                "1",
            ],
            vec![
                "bb",
                "futures",
                "--dry-run",
                "--symbol",
                "BTC/USDT",
                "--side",
                "BUY",
                "--quantity",
                "0.001",
            ],
            vec![
                "bb",
                "futures-canary",
                "--plan-only",
                "--symbol",
                "BTC/USDT",
                "--side",
                "BUY",
                "--quantity",
                "0.001",
                "--exit-side",
                "SELL",
                "--exit-reduce-only",
                "--exit-trigger",
                "operator approved close",
            ],
            vec![
                "bb",
                "futures-canary",
                "--dry-run",
                "--submit",
                "--quantity",
                "1",
            ],
            vec![
                "bb",
                "transfers",
                "--dry-run",
                "--source-wallet",
                "spot",
                "--destination-wallet",
                "funding",
                "--asset",
                "USDT",
                "--amount",
                "5",
            ],
            vec![
                "bb",
                "transfer-canary",
                "--plan-only",
                "--source-wallet",
                "spot",
                "--destination-wallet",
                "funding",
                "--asset",
                "USDT",
                "--amount",
                "1",
            ],
            vec![
                "bb",
                "withdrawals",
                "--dry-run",
                "--asset",
                "USDT",
                "--amount",
                "1",
                "--address",
                "TAllowlistedAddress",
                "--network",
                "TRX",
                "--allow-address",
                "TAllowlistedAddress",
                "--allow-network",
                "TRX",
                "--operator-id",
                "local-review",
                "--allowed-operator",
                "local-review",
                "--approval",
                "risk",
                "--approval",
                "ops",
            ],
            vec![
                "bb",
                "prediction",
                "--dry-run",
                "--market-data-file",
                "prediction-markets.json",
                "--symbol",
                "BTC/USDT",
                "--direction",
                "UP",
                "--stake-usdt",
                "1",
                "--bankroll-usdt",
                "100",
            ],
            vec!["bb", "spot-canary", "--plan-only"],
            vec!["bb", "live-spot-allocator", "--plan-only"],
            vec!["bb", "live-probe", "--confirm-real-order"],
        ] {
            Cli::try_parse_from(&argv).unwrap_or_else(|e| panic!("{argv:?}: {e}"));
        }
        assert!(Cli::try_parse_from(["bb", "api-permissions", "--feature", "nope"]).is_err());
        assert!(Cli::try_parse_from(["bb", "futures", "--side", "buy"]).is_err());
    }

    #[tokio::test]
    async fn gates_fail_before_loading_environment() {
        let parse = |argv: &[&str]| Cli::try_parse_from(argv).unwrap().command;
        for argv in [
            &["bb", "margin"][..],
            &["bb", "margin-canary"],
            &["bb", "futures"],
            &["bb", "futures-canary", "--plan-only", "--submit"],
            &["bb", "futures-canary", "--plan-only", "--quantity", "1"],
            &["bb", "transfers"],
            &["bb", "transfer-canary"],
            &["bb", "withdrawals"],
            &["bb", "prediction"],
            &["bb", "spot-canary"],
            &["bb", "live-spot-allocator"],
            &["bb", "live-probe"],
            &["bb", "product-modules", "--module", "not-real"],
        ] {
            assert_eq!(
                run(parse(argv)).await.unwrap(),
                ExitCode::FAILURE,
                "{argv:?}"
            );
        }
        assert_eq!(
            run(parse(&[
                "bb",
                "product-modules",
                "--module",
                "prediction_trading"
            ]))
            .await
            .unwrap(),
            ExitCode::SUCCESS
        );
        assert_eq!(
            run(parse(&[
                "bb",
                "advanced-readiness",
                "--requested-live-canary",
                "margin_canary_plan",
                "--requested-live-canary",
                "futures_canary_plan",
                "--fail-on-serial-conflict"
            ]))
            .await
            .unwrap(),
            ExitCode::FAILURE
        );
    }
}
