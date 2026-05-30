"""Solana memecoin sniper.

Phase 1 (this commit): detection only — subscribe to Solana log feeds for
the major launchpads (Raydium AMM v4, pump.fun) and emit
``PoolCreationEvent`` records as new tokens spawn. No wallet, no execution.

Subsequent phases add: token safety scoring, historical replay backtest,
paper executor (simulated Jupiter swap fills), and live executor.
"""
