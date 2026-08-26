ALTER TABLE completion_outbox
ADD COLUMN terminal_turn_id TEXT;

ALTER TABLE completion_webhook_outbox
ADD COLUMN terminal_turn_id TEXT;

DROP TRIGGER completion_webhook_after_completion_insert;

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
        terminal_turn_id,
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
        NEW.available_at_ms,
        CASE WHEN NEW.execution_kind = 'normal' THEN NEW.execution_id ELSE NULL END,
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
        terminal_turn_id,
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
        NEW.updated_at_ms + 60000,
        NULL,
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
        terminal_turn_id,
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
        NEW.updated_at_ms + 60000,
        NULL,
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
