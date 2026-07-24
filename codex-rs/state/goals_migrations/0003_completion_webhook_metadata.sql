ALTER TABLE completion_bindings
ADD COLUMN callback_metadata_json TEXT NOT NULL DEFAULT '';

ALTER TABLE completion_outbox
ADD COLUMN callback_metadata_json TEXT NOT NULL DEFAULT '';

CREATE TABLE completion_webhook_outbox (
    event_id TEXT PRIMARY KEY NOT NULL
        REFERENCES completion_outbox(event_id),
    completion_work_id TEXT NOT NULL UNIQUE,
    thread_id TEXT NOT NULL,
    execution_kind TEXT NOT NULL CHECK(execution_kind IN ('normal', 'goal')),
    execution_id TEXT NOT NULL,
    callback_metadata_json TEXT NOT NULL CHECK(callback_metadata_json <> ''),
    terminal_status TEXT NOT NULL CHECK(terminal_status IN (
        'completed',
        'complete',
        'blocked',
        'usageLimited',
        'budgetLimited'
    )),
    final_text TEXT NOT NULL,
    terminal_at_ms INTEGER NOT NULL,
    state TEXT NOT NULL CHECK(state IN ('pending', 'sending', 'sent')),
    attempt_count INTEGER NOT NULL DEFAULT 0 CHECK(attempt_count >= 0),
    available_at_ms INTEGER NOT NULL,
    lease_id TEXT,
    lease_expires_at_ms INTEGER,
    last_error TEXT NOT NULL DEFAULT '',
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
);

CREATE INDEX completion_webhook_outbox_pending
ON completion_webhook_outbox(available_at_ms, created_at_ms, event_id)
WHERE state IN ('pending', 'sending');

CREATE TRIGGER completion_webhook_after_completion_insert
AFTER INSERT ON completion_outbox
WHEN NEW.callback_metadata_json <> ''
BEGIN
    INSERT OR IGNORE INTO completion_webhook_outbox (
        event_id,
        completion_work_id,
        thread_id,
        execution_kind,
        execution_id,
        callback_metadata_json,
        terminal_status,
        final_text,
        terminal_at_ms,
        state,
        available_at_ms,
        created_at_ms,
        updated_at_ms
    ) VALUES (
        NEW.event_id,
        NEW.completion_work_id,
        NEW.thread_id,
        NEW.execution_kind,
        NEW.execution_id,
        NEW.callback_metadata_json,
        NEW.terminal_status,
        NEW.final_text,
        NEW.terminal_at_ms,
        'pending',
        NEW.terminal_at_ms,
        NEW.created_at_ms,
        NEW.updated_at_ms
    );
END;

DROP TRIGGER completion_goal_terminal_after_insert;
DROP TRIGGER completion_goal_terminal_after_update;

CREATE TRIGGER completion_goal_terminal_after_insert
AFTER INSERT ON thread_goals
WHEN NEW.status IN ('complete', 'blocked', 'usage_limited', 'budget_limited')
BEGIN
    INSERT OR IGNORE INTO completion_outbox (
        event_id,
        completion_work_id,
        thread_id,
        execution_kind,
        execution_id,
        callback_metadata_json,
        terminal_status,
        final_text,
        terminal_at_ms,
        state,
        available_at_ms,
        created_at_ms,
        updated_at_ms
    )
    SELECT
        binding.completion_work_id,
        binding.completion_work_id,
        binding.thread_id,
        binding.execution_kind,
        binding.execution_id,
        binding.callback_metadata_json,
        CASE NEW.status
            WHEN 'usage_limited' THEN 'usageLimited'
            WHEN 'budget_limited' THEN 'budgetLimited'
            ELSE NEW.status
        END,
        '',
        NEW.updated_at_ms,
        'pending',
        NEW.updated_at_ms,
        NEW.updated_at_ms,
        NEW.updated_at_ms
    FROM completion_bindings AS binding
    WHERE binding.thread_id = NEW.thread_id
      AND binding.execution_kind = 'goal'
      AND binding.execution_id = NEW.goal_id
      AND binding.state = 'active';

    UPDATE completion_bindings
    SET state = 'terminal', updated_at_ms = NEW.updated_at_ms
    WHERE thread_id = NEW.thread_id
      AND execution_kind = 'goal'
      AND execution_id = NEW.goal_id
      AND state = 'active';
END;

CREATE TRIGGER completion_goal_terminal_after_update
AFTER UPDATE OF status ON thread_goals
WHEN NEW.status IN ('complete', 'blocked', 'usage_limited', 'budget_limited')
 AND NEW.status <> OLD.status
BEGIN
    INSERT OR IGNORE INTO completion_outbox (
        event_id,
        completion_work_id,
        thread_id,
        execution_kind,
        execution_id,
        callback_metadata_json,
        terminal_status,
        final_text,
        terminal_at_ms,
        state,
        available_at_ms,
        created_at_ms,
        updated_at_ms
    )
    SELECT
        binding.completion_work_id,
        binding.completion_work_id,
        binding.thread_id,
        binding.execution_kind,
        binding.execution_id,
        binding.callback_metadata_json,
        CASE NEW.status
            WHEN 'usage_limited' THEN 'usageLimited'
            WHEN 'budget_limited' THEN 'budgetLimited'
            ELSE NEW.status
        END,
        '',
        NEW.updated_at_ms,
        'pending',
        NEW.updated_at_ms,
        NEW.updated_at_ms,
        NEW.updated_at_ms
    FROM completion_bindings AS binding
    WHERE binding.thread_id = NEW.thread_id
      AND binding.execution_kind = 'goal'
      AND binding.execution_id = NEW.goal_id
      AND binding.state = 'active';

    UPDATE completion_bindings
    SET state = 'terminal', updated_at_ms = NEW.updated_at_ms
    WHERE thread_id = NEW.thread_id
      AND execution_kind = 'goal'
      AND execution_id = NEW.goal_id
      AND state = 'active';
END;
