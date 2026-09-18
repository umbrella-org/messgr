-- Producer send quotas and usage (DESIGN.md §5.1, T-042): two fixed windows
-- (per-minute burst, per-day total) per (producer, channel, class), enforced
-- at dispatch, never at ingest (AGENTS.md invariant 3). enforcement is
-- hard|soft; auth never gets a row here at all (exempt but counted
-- unconditionally in the dispatcher, never blocked -- AGENTS.md invariant 5).
CREATE TABLE producer_quota (
    producer_id uuid NOT NULL REFERENCES producer(id),
    channel     text NOT NULL,
    class       text NOT NULL,
    per_minute  int,                     -- burst ceiling; NULL = unlimited
    per_day     int,                     -- daily total; NULL = unlimited
    enforcement text NOT NULL,           -- hard | soft
    PRIMARY KEY (producer_id, channel, class)
);

-- Time-boxed uplift (e.g. a campaign day). Raises per_day only -- enforcement
-- is always inherited from the base producer_quota row, never overridden.
CREATE TABLE producer_quota_override (
    id          uuid PRIMARY KEY,
    producer_id uuid NOT NULL REFERENCES producer(id),
    channel     text NOT NULL,
    class       text NOT NULL,
    per_day     int  NOT NULL,
    valid_from  timestamptz NOT NULL,
    valid_to    timestamptz NOT NULL,    -- mandatory; overrides always expire
    approved_by text NOT NULL,
    reason      text NOT NULL
);

-- Flushed from the dispatcher's in-process QuotaTracker every few seconds
-- for reporting, and read back on dispatcher startup to rebuild the
-- in-process counters so a restart mid-window doesn't reset a producer's
-- allowance to zero. Retention sweep (minute rows after 7 days, day rows
-- after a year) is explicitly out of scope for T-042 -- a follow-up ticket.
CREATE TABLE producer_usage (
    producer_id  uuid NOT NULL,
    channel      text NOT NULL,
    class        text NOT NULL,
    granularity  text NOT NULL,          -- minute | day
    window_start timestamptz NOT NULL,
    sent         bigint NOT NULL DEFAULT 0,
    blocked      bigint NOT NULL DEFAULT 0,
    PRIMARY KEY (producer_id, channel, class, granularity, window_start)
);
CREATE INDEX ON producer_usage (granularity, window_start);
