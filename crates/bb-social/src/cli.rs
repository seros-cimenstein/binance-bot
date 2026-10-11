//! `bb social-signal`: collect read-only social evidence and/or print stored evidence.

use std::process::ExitCode;

use clap::{Args, Subcommand};

use crate::collect::SocialCollector;
use crate::evidence::load_evidence;
use crate::settings::SocialSettings;

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Collect read-only crypto social evidence.
    SocialSignal(SocialSignalArgs),
}

#[derive(Args, Debug)]
pub struct SocialSignalArgs {
    /// Collect enabled official API sources.
    #[arg(long)]
    pub collect: bool,
    /// Asset ticker to extract (repeatable).
    #[arg(long = "asset")]
    pub assets: Vec<String>,
    /// Print stored evidence only.
    #[arg(long)]
    pub evidence_only: bool,
}

pub async fn run(cmd: Command) -> anyhow::Result<ExitCode> {
    match cmd {
        Command::SocialSignal(args) => run_social_signal(args).await,
    }
}

async fn run_social_signal(args: SocialSignalArgs) -> anyhow::Result<ExitCode> {
    let settings = SocialSettings::from_env()?;
    if args.collect {
        let records = SocialCollector::new()?
            .collect(&settings, &args.assets)
            .await?;
        println!(
            "{{\"records_collected\": {}, \"audit_path\": {}}}",
            records.len(),
            serde_json::to_string(&settings.audit_path)?
        );
    }
    if args.evidence_only || !args.collect {
        let evidence = load_evidence(&settings, &args.assets)?;
        println!("{}", serde_json::to_string_pretty(&evidence)?);
    }
    Ok(ExitCode::SUCCESS)
}
