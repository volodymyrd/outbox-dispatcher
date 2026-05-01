-- The outbox: append-only event log.
CREATE TABLE outbox_events (
    id              BIGSERIAL PRIMARY KEY,
    event_id        UUID        NOT NULL UNIQUE DEFAULT gen_random_uuid(),
    kind            TEXT        NOT NULL,
    aggregate_type  TEXT        NOT NULL,
    aggregate_id    UUID        NOT NULL,
    payload         JSONB       NOT NULL,
    metadata        JSONB       NOT NULL DEFAULT '{}'::jsonb,
    callbacks       JSONB       NOT NULL,
    actor_id        UUID            NULL,
    correlation_id  UUID            NULL,
    causation_id    UUID            NULL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT outbox_events_callbacks_nonempty
        CHECK (jsonb_typeof(callbacks) = 'array' AND jsonb_array_length(callbacks) > 0)
);

CREATE INDEX idx_outbox_events_aggregate
    ON outbox_events(aggregate_type, aggregate_id, id);
CREATE INDEX idx_outbox_events_kind_created
    ON outbox_events(kind, created_at);
CREATE INDEX idx_outbox_events_correlation
    ON outbox_events(correlation_id) WHERE correlation_id IS NOT NULL;

-- Per-callback delivery state.
CREATE TABLE outbox_deliveries (
    id                  BIGSERIAL PRIMARY KEY,
    event_id            UUID        NOT NULL REFERENCES outbox_events(event_id) ON DELETE CASCADE,
    callback_name       TEXT        NOT NULL,
    completion_mode     TEXT        NOT NULL,
    attempts            INT         NOT NULL DEFAULT 0,
    last_error          TEXT            NULL,
    last_attempt_at     TIMESTAMPTZ     NULL,
    available_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    locked_until        TIMESTAMPTZ     NULL,
    dispatched_at       TIMESTAMPTZ     NULL,
    processed_at        TIMESTAMPTZ     NULL,
    completion_cycles   INT         NOT NULL DEFAULT 0,
    dead_letter         BOOLEAN     NOT NULL DEFAULT FALSE,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (event_id, callback_name),
    CONSTRAINT outbox_deliveries_completion_mode_valid
        CHECK (completion_mode IN ('managed', 'external'))
);

-- The dispatcher's hot working set: rows it still owns.
CREATE INDEX idx_outbox_deliveries_pending
    ON outbox_deliveries (available_at, id)
    WHERE dispatched_at IS NULL
      AND processed_at IS NULL
      AND dead_letter = FALSE;

-- External-mode rows delivered but not yet completed by the receiver.
CREATE INDEX idx_outbox_deliveries_external_pending
    ON outbox_deliveries (dispatched_at)
    WHERE processed_at IS NULL
      AND dead_letter = FALSE
      AND dispatched_at IS NOT NULL
      AND completion_mode = 'external';

CREATE INDEX idx_outbox_deliveries_dead_letter
    ON outbox_deliveries (callback_name, last_attempt_at)
    WHERE dead_letter = TRUE;

-- LISTEN/NOTIFY trigger for low-latency wakeups.
CREATE OR REPLACE FUNCTION outbox_notify_new_event() RETURNS TRIGGER AS $$
BEGIN
    PERFORM pg_notify('outbox_events_new', NEW.event_id::text);
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER outbox_events_notify
    AFTER INSERT ON outbox_events
    FOR EACH ROW EXECUTE FUNCTION outbox_notify_new_event();
