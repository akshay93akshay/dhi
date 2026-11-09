-- Required swarm worker count, chosen with the topology at task creation.
-- Only meaningful for `topology = 'swarm'`; the API validates the 2..=8 range.
ALTER TABLE tasks
    ADD COLUMN IF NOT EXISTS swarm_workers INTEGER;
