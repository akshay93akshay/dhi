-- Fact-based proof: a met goal names the confirmed fact that proved it.
-- One new column, no new table. The 14-table schema is unchanged.
ALTER TABLE work_goals
    ADD COLUMN IF NOT EXISTS proof_fact_id UUID REFERENCES facts(id) ON DELETE SET NULL;
