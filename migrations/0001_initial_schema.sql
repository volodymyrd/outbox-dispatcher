-- ==============================================================================
-- TABLE: outbox_events
-- ==============================================================================
-- The outbox: append-only event log.
-- The publisher application writes rows to this table. The dispatcher reads
-- from it but never updates or deletes rows here (unless retention is enabled).
CREATE TABLE outbox_events
(
    -- A monotonically increasing internal ID. The dispatcher uses this as a high-water mark "cursor" for polling.
    id             BIGSERIAL PRIMARY KEY,

    -- The stable, external identifier for the event. Used as the idempotency key in outgoing webhooks.
    event_id       UUID        NOT NULL UNIQUE DEFAULT gen_random_uuid(),

    -- The versioned event identifier (e.g., "user.registered@v1"). Informational only; not used for routing.
    kind           TEXT        NOT NULL,

    -- Category, class, or domain model of the entity (e.g., "user", "order").
    aggregate_type TEXT        NOT NULL,

    -- Unique ID of the specific aggregate instance. Together with aggregate_type, enables strict ordering and querying.
    aggregate_id   UUID        NOT NULL,

    -- The actual business data of the event. The dispatcher treats this as an opaque blob and passes it through.
    payload        JSONB       NOT NULL,

    -- Non-semantic context (like the HTTP User-Agent, feature flags, or tracing info). Passed through to receiver.
    metadata       JSONB       NOT NULL        DEFAULT '{}'::jsonb,

    -- A JSON array of delivery targets (URLs, retry policies, signing keys). The dispatcher uses this for fan-out routing.
    callbacks      JSONB       NOT NULL,

    -- The ID of the user or system that triggered the event.
    actor_id       UUID NULL,

    -- Standard distributed-tracing identifier to track chains of events.
    correlation_id UUID NULL,

    -- The event that directly caused this event.
    causation_id   UUID NULL,

    -- The exact wall-clock time the event was inserted. Used for calculating dispatcher lag.
    created_at     TIMESTAMPTZ NOT NULL        DEFAULT now(),

    -- Ensures the publisher actually provided at least one destination for the event.
    CONSTRAINT outbox_events_callbacks_nonempty
        CHECK (jsonb_typeof(callbacks) = 'array' AND jsonb_array_length(callbacks) > 0)
);

-- Speeds up "Show me all events for this specific aggregate" queries.
CREATE INDEX idx_outbox_events_aggregate
    ON outbox_events (aggregate_type, aggregate_id, id);

-- Speeds up time-based event filtering by kind.
CREATE INDEX idx_outbox_events_kind_created
    ON outbox_events (kind, created_at);

-- Speeds up distributed tracing queries.
CREATE INDEX idx_outbox_events_correlation
    ON outbox_events (correlation_id) WHERE correlation_id IS NOT NULL;


-- ==============================================================================
-- TABLE: outbox_deliveries
-- ==============================================================================
-- Per-callback delivery state. When the dispatcher reads an outbox_event, it
-- generates one row in this table for *each* target inside the `callbacks` array.
CREATE TABLE outbox_deliveries
(
    -- Internal ID used by the Admin API to identify a specific delivery attempt.
    id                BIGSERIAL PRIMARY KEY,

    -- Links back to the source event. ON DELETE CASCADE ensures that deleting an old event cleans up its delivery history.
    event_id          UUID        NOT NULL REFERENCES outbox_events (event_id) ON DELETE CASCADE,

    -- The name of the target (e.g., "welcome_email"), extracted from the event's callbacks JSON array.
    callback_name     TEXT        NOT NULL,

    -- Either 'managed' (done on HTTP 2xx) or 'external' (requires an explicit callback/update from the receiver).
    completion_mode   TEXT        NOT NULL,

    -- How many times the dispatcher has tried to send this webhook.
    attempts          INT         NOT NULL DEFAULT 0,

    -- The error message from the most recent failure (e.g., "HTTP 503 Service Unavailable").
    last_error        TEXT NULL,

    -- When the last HTTP request was initiated.
    last_attempt_at   TIMESTAMPTZ NULL,

    -- The core of the retry backoff. The dispatcher will only pick up rows where available_at <= now().
    available_at      TIMESTAMPTZ NOT NULL DEFAULT now(),

    -- Concurrency control. When a dispatcher instance picks this up, it sets this to the future so parallel instances skip it.
    locked_until      TIMESTAMPTZ NULL,

    -- Set when the dispatcher successfully receives an HTTP 2xx response.
    dispatched_at     TIMESTAMPTZ NULL,

    -- Set when the business logic is fully complete. (By the dispatcher in managed mode, or downstream receiver in external mode).
    processed_at      TIMESTAMPTZ NULL,

    -- Used only in external mode. Counts how many times the row hung and had to be reset for redelivery by the timeout sweeper.
    completion_cycles INT         NOT NULL DEFAULT 0,

    -- Terminal failure flag. Set to TRUE if max attempts are exhausted, or if the callback configuration was structurally invalid.
    dead_letter       BOOLEAN     NOT NULL DEFAULT FALSE,

    -- When this delivery was initially scheduled.
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),

    -- Ensures the dispatcher is idempotent. It won't duplicate deliveries if it crashes and rescans.
    UNIQUE (event_id, callback_name),

    CONSTRAINT outbox_deliveries_completion_mode_valid
        CHECK (completion_mode IN ('managed', 'external'))
);

-- THE MOST CRITICAL INDEX: The "hot" working set. Keeps dispatcher polling extremely fast.
CREATE INDEX idx_outbox_deliveries_pending
    ON outbox_deliveries (available_at, id) WHERE dispatched_at IS NULL
      AND processed_at IS NULL
      AND dead_letter = FALSE;

-- Used by the sweeper to quickly find external-mode rows that were sent but never completed.
CREATE INDEX idx_outbox_deliveries_external_pending
    ON outbox_deliveries (dispatched_at) WHERE processed_at IS NULL
      AND dead_letter = FALSE
      AND dispatched_at IS NOT NULL
      AND completion_mode = 'external';

-- Used by the Admin API to quickly list dead-lettered messages.
CREATE INDEX idx_outbox_deliveries_dead_letter
    ON outbox_deliveries (callback_name, last_attempt_at) WHERE dead_letter = TRUE;


-- ==============================================================================
-- LISTEN/NOTIFY TRIGGER
-- ==============================================================================
-- This trigger fires a lightweight signal to wake up the Rust dispatcher
-- immediately when a new event is inserted, enabling sub-millisecond latency.
CREATE
OR REPLACE FUNCTION outbox_notify_new_event() RETURNS TRIGGER AS $$
BEGIN
    PERFORM
pg_notify('outbox_events_new', NEW.event_id::text);
RETURN NEW;
END;
$$
LANGUAGE plpgsql;

CREATE TRIGGER outbox_events_notify
    AFTER INSERT
    ON outbox_events
    FOR EACH ROW EXECUTE FUNCTION outbox_notify_new_event();
