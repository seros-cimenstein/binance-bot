#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! CLI path precedence and state-file side effects of `airdrop-scheduler`.

use std::process::ExitCode;

use bb_airdrop::catalog::{Chain, Status, filter_by};
use bb_airdrop::cli::{Action, SchedulerArgs, run_scheduler};
use bb_airdrop::scheduler::{CEX_WALLET_NAME, TaskKey, current_iso_week, load_state};

fn args(state_path: Option<std::path::PathBuf>, action: Action) -> SchedulerArgs {
    SchedulerArgs {
        store_path: None,
        state_path,
        action,
    }
}

#[tokio::test]
async fn cli_can_mark_cex_task_done_under_data_dir() {
    let dir = tempfile::tempdir().unwrap();
    let cex_name = filter_by(Some(Chain::Cex), Some(Status::Active), None)[0].name;
    let rc = run_scheduler(
        args(
            None,
            Action::Done {
                wallet_name: CEX_WALLET_NAME.into(),
                opportunity_name: cex_name.into(),
            },
        ),
        dir.path(),
    )
    .await
    .unwrap();
    assert_eq!(rc, ExitCode::SUCCESS);
    let state = load_state(&dir.path().join("airdrop-scheduler.json")).unwrap();
    assert!(state.is_completed_this_week(
        &TaskKey::new(CEX_WALLET_NAME, cex_name),
        &current_iso_week()
    ));
}

#[tokio::test]
async fn cli_undone_writes_state_under_data_dir() {
    let dir = tempfile::tempdir().unwrap();
    let action = Action::Undone {
        wallet_name: "w1".into(),
        opportunity_name: "Kamino Finance".into(),
    };
    run_scheduler(args(None, action), dir.path()).await.unwrap();
    assert!(dir.path().join("airdrop-scheduler.json").exists());
}

#[tokio::test]
async fn cli_flag_beats_data_dir() {
    let dir = tempfile::tempdir().unwrap();
    let env_dir = dir.path().join("env");
    let flag_path = dir.path().join("flag").join("state.json");
    let action = Action::Undone {
        wallet_name: "w1".into(),
        opportunity_name: "Kamino Finance".into(),
    };
    run_scheduler(args(Some(flag_path.clone()), action), &env_dir)
        .await
        .unwrap();
    assert!(flag_path.exists());
    assert!(!env_dir.join("airdrop-scheduler.json").exists());
}

#[tokio::test]
async fn cli_corrupt_state_exits_1() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("airdrop-scheduler.json"), "not json").unwrap();
    let rc = run_scheduler(args(None, Action::List), dir.path())
        .await
        .unwrap();
    assert_eq!(rc, ExitCode::from(1));
}
