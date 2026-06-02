-- M0007_decision_trace.sql
-- Persist the full structured decision trace (the skip-ladder provenance:
-- every rule the engine walked, what it saw, which one fired) alongside
-- each verdict-history row. Lets the Rule Lab show WHY the engine decided
-- the way it did on any past day, not just today's live decision.
-- Additive: existing rows default to '' (no stored trace).
ALTER TABLE verdict_history ADD COLUMN trace_json TEXT NOT NULL DEFAULT '';
