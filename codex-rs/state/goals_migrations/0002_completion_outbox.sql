CREATE TABLE completion_bindings (
    completion_work_id TEXT PRIMARY KEY NOT NULL,
    thread_id TEXT NOT NULL,
    execution_kind TEXT NOT NULL CHECK(execution_kind IN ('normal', 'goal')),
    execution_id TEXT NOT NULL,
    state TEXT NOT NULL CHECK(state IN ('registered', 'active', 'terminal')),
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
);

CREATE INDEX completion_bindings_execution
ON completion_bindings(thread_id, execution_kind, execution_id, state);

CREATE TABLE completion_outbox (
    event_id TEXT PRIMARY KEY NOT NULL,
    completion_work_id TEXT NOT NULL UNIQUE
        REFERENCES completion_bindings(completion_work_id),
    thread_id TEXT NOT NULL,
    execution_kind TEXT NOT NULL CHECK(execution_kind IN ('normal', 'goal')),
    execution_id TEXT NOT NULL,
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

CREATE INDEX completion_outbox_pending
ON completion_outbox(available_at_ms, created_at_ms, event_id)
WHERE state IN ('pending', 'sending');

CREATE TABLE completion_callback_inbox (
    delivery_id TEXT PRIMARY KEY NOT NULL,
    event_id TEXT NOT NULL,
    completion_work_id TEXT NOT NULL,
    target_thread_id TEXT NOT NULL,
    source_agent_display_id TEXT NOT NULL,
    execution_kind TEXT NOT NULL CHECK(execution_kind IN ('normal', 'goal')),
    execution_id TEXT NOT NULL,
    terminal_status TEXT NOT NULL CHECK(terminal_status IN (
        'completed',
        'complete',
        'blocked',
        'usageLimited',
        'budgetLimited'
    )),
    callback_text TEXT NOT NULL,
    final_text TEXT NOT NULL,
    terminal_at_ms INTEGER NOT NULL,
    payload_digest TEXT NOT NULL,
    call_id TEXT NOT NULL UNIQUE,
    state TEXT NOT NULL CHECK(state IN ('pending', 'injected', 'delivered')),
    injected_boot_id TEXT,
    attempt_count INTEGER NOT NULL DEFAULT 0 CHECK(attempt_count >= 0),
    last_error TEXT NOT NULL DEFAULT '',
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
);

CREATE INDEX completion_callback_inbox_pending
ON completion_callback_inbox(created_at_ms, delivery_id)
WHERE state IN ('pending', 'injected');

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
