"""Airdrop farming infrastructure.

Phase 1 (this commit): multi-wallet manager (Solana + EVM), curated
opportunity catalog with eligibility criteria, and a scheduler that
produces a weekly per-wallet activity plan.

No on-chain automation in this phase — the scheduler emits a checklist
the operator follows. Automating individual protocol interactions is
per-protocol work and is layered on top in subsequent phases.
"""
