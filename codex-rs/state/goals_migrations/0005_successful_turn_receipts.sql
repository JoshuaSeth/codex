-- Local execution evidence, independent of optional central/webhook publication.
-- No historical backfill: absence is unknown, never proof of successful completion.
CREATE TABLE successful_turn_receipts (
    thread_id TEXT NOT NULL,
    turn_id TEXT NOT NULL,
    completed_at_ms INTEGER NOT NULL,
    PRIMARY KEY (thread_id, turn_id)
) WITHOUT ROWID;
