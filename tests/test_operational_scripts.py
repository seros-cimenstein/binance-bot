from __future__ import annotations

import os
import sqlite3
import subprocess
from pathlib import Path


ROOT = Path(__file__).parents[1]
BACKUP_SCRIPT = ROOT / "scripts" / "backup_db.sh"
RESTORE_SCRIPT = ROOT / "scripts" / "restore_db.sh"


def _database(path: Path, value: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with sqlite3.connect(path) as connection:
        connection.execute("CREATE TABLE state (value TEXT NOT NULL)")
        connection.execute("INSERT INTO state (value) VALUES (?)", (value,))


def _value(path: Path) -> str:
    with sqlite3.connect(path) as connection:
        return str(connection.execute("SELECT value FROM state").fetchone()[0])


def _env(project_dir: Path, **overrides: str) -> dict[str, str]:
    env = dict(os.environ)
    env.update(
        {
            "BINANCE_BOT_PROJECT_DIR": str(project_dir),
            "BINANCE_BOT_ENV_FILE": str(project_dir / ".env"),
            "BINANCE_BOT_BACKUP_DIR": str(project_dir / "backups"),
            "BINANCE_BOT_SERVICE_NAME": "not-a-running-test-service.service",
        }
    )
    env.update(overrides)
    (project_dir / ".env").write_text("DB_PATH=data/live.db\n", encoding="ascii")
    return env


def test_backup_script_validates_snapshots_and_prunes_retention(tmp_path: Path) -> None:
    database = tmp_path / "data" / "live.db"
    _database(database, "running")
    env = _env(tmp_path, BINANCE_BOT_BACKUP_RETENTION="2")

    for _ in range(3):
        subprocess.run([str(BACKUP_SCRIPT)], env=env, check=True, capture_output=True)

    backups = sorted((tmp_path / "backups").glob("live-*.db"))
    assert len(backups) == 2
    assert all(_value(backup) == "running" for backup in backups)


def test_restore_script_creates_safety_backup_and_restores_valid_snapshot(
    tmp_path: Path,
) -> None:
    database = tmp_path / "data" / "live.db"
    restore_source = tmp_path / "restore-source.db"
    _database(database, "current")
    _database(restore_source, "restored")
    env = _env(tmp_path)

    subprocess.run(
        [str(RESTORE_SCRIPT), str(restore_source)],
        env=env,
        check=True,
        capture_output=True,
    )

    assert _value(database) == "restored"
    safety_backups = list((tmp_path / "backups").glob("live-*.db"))
    assert len(safety_backups) == 1
    assert _value(safety_backups[0]) == "current"


def test_restore_script_refuses_while_service_is_active(tmp_path: Path) -> None:
    database = tmp_path / "data" / "live.db"
    restore_source = tmp_path / "restore-source.db"
    _database(database, "current")
    _database(restore_source, "restored")
    fake_bin = tmp_path / "bin"
    fake_bin.mkdir()
    systemctl = fake_bin / "systemctl"
    systemctl.write_text("#!/usr/bin/env bash\nexit 0\n", encoding="ascii")
    systemctl.chmod(0o755)
    env = _env(tmp_path, PATH=f"{fake_bin}:{os.environ['PATH']}")

    result = subprocess.run(
        [str(RESTORE_SCRIPT), str(restore_source)],
        env=env,
        check=False,
        capture_output=True,
        text=True,
    )

    assert result.returncode == 1
    assert "Refusing restore" in result.stderr
    assert _value(database) == "current"


def test_backup_timer_uses_service_user_and_daily_retention() -> None:
    service = (ROOT / "deploy" / "binance-bot-backup.service").read_text()
    timer = (ROOT / "deploy" / "binance-bot-backup.timer").read_text()

    assert "User=bot" in service
    assert "BINANCE_BOT_BACKUP_RETENTION=14" in service
    assert "ExecStart=/opt/binance-bot/scripts/backup_db.sh" in service
    assert "OnCalendar=*-*-* 02:15:00" in timer
    assert "Persistent=true" in timer


def test_user_units_run_from_user_owned_deployment_without_privileged_user() -> None:
    service = (ROOT / "deploy" / "user" / "binance-bot.service").read_text()
    pool = (ROOT / "deploy" / "user" / "binance-pool-paper.service").read_text()
    backup = (ROOT / "deploy" / "user" / "binance-bot-backup.service").read_text()
    timer = (ROOT / "deploy" / "user" / "binance-bot-backup.timer").read_text()
    pool_backup = (ROOT / "deploy" / "user" / "binance-pool-paper-backup.service").read_text()
    pool_timer = (ROOT / "deploy" / "user" / "binance-pool-paper-backup.timer").read_text()
    claude_timer = (ROOT / "deploy" / "user" / "binance-claude-spot.timer").read_text()

    assert "WorkingDirectory=%h/.local/share/binance-bot" in service
    assert "ExecStart=%h/.local/share/binance-bot/venv/bin/python -m src.main" in service
    assert "User=" not in service
    assert "ExecStart=%h/.local/share/binance-bot/venv/bin/python -m src.pool" in pool
    assert "Environment=POOL_MODE=paper" in pool
    assert "User=" not in pool
    assert "ExecStart=%h/.local/share/binance-bot/scripts/backup_db.sh" in backup
    assert "Persistent=true" in timer
    assert "BINANCE_BOT_DB_SETTING_KEY=POOL_DB_PATH" in pool_backup
    assert "BINANCE_BOT_BACKUP_RETENTION=35" in pool_backup
    assert "Unit=binance-pool-paper-backup.service" in pool_timer
    assert "OnActiveSec=10min" in claude_timer
    assert "OnUnitActiveSec=10min" in claude_timer


def test_user_deployment_preserves_existing_database_after_migration() -> None:
    setup_script = (ROOT / "scripts" / "setup_user_deploy.sh").read_text()
    cutover_script = (ROOT / "scripts" / "cutover_to_user_service.sh").read_text()

    assert 'if [ ! -f "$PROJECT_DIR/data/live.db" ] && [ -r "$SOURCE_DB" ]; then' in setup_script
    assert "Preserving existing user database" in setup_script
    assert "Start paper evaluation with: systemctl --user enable --now binance-pool-paper.service" in setup_script
    assert "User bot service is already active; the one-time handoff is complete." in cutover_script


def test_pool_status_reports_paper_ledger_only() -> None:
    status_script = (ROOT / "scripts" / "pool_status.sh").read_text()

    assert "binance-pool-paper.service" in status_script
    assert "POOL_DB_PATH" in status_script
    assert "Paper decisions" in status_script


def test_pool_replay_script_uses_isolated_replay_databases() -> None:
    replay_script = (ROOT / "scripts" / "pool_replay.sh").read_text()

    assert "src.pool.replay" in replay_script
    assert "POOL_REPLAY_DB_DIR" in replay_script
    assert "pool_market_replay.json" in replay_script
    assert "pool_market_replay_stress.json" in replay_script
    assert "pool-paper.db" not in replay_script
