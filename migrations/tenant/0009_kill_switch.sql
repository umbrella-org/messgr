-- Kill switches (DESIGN.md §5.2, T-016): five scopes (global, channel,
-- producer, producer_channel, campaign), hold-by-default with an explicit
-- discard option, fully audited (engaged_by/at/reason, released_by/at).
CREATE TABLE kill_switch (
    id          uuid PRIMARY KEY,
    scope       text NOT NULL,           -- global | channel | producer | producer_channel | campaign
    scope_key   text,                    -- NULL for global
    on_queued   text NOT NULL,           -- hold | discard
    engaged_by  text NOT NULL,
    engaged_at  timestamptz NOT NULL,
    reason      text NOT NULL,
    released_by text,
    released_at timestamptz              -- NULL = currently active
);

-- Corrected form (this audit): Postgres treats every NULL as distinct, so a
-- bare (scope, scope_key) unique index never caught two simultaneous
-- `global`-scope switches (scope_key IS NULL on both). COALESCE gives every
-- scope a real, comparable value; '' is never a legitimate scope_key for any
-- non-global scope, so this cannot mask a genuine collision.
CREATE UNIQUE INDEX ON kill_switch (scope, COALESCE(scope_key, '')) WHERE released_at IS NULL;

-- Wakes the dispatcher's kill-switch cache (DESIGN.md §5.2): a dedicated
-- channel and 30-second poll fallback, separate from outbox_<channel>'s 1s
-- poll (§4.2) -- config re-read optimises nothing in steady state and only
-- needs to be fast during an incident. Fires on release too (UPDATE), not
-- only engage (INSERT), since a release is what the drain task must react
-- to promptly.
CREATE OR REPLACE FUNCTION kill_switch_notify() RETURNS trigger AS $$
BEGIN
    PERFORM pg_notify('kill_switch', NEW.id::text);
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER kill_switch_notify_trigger
    AFTER INSERT OR UPDATE ON kill_switch
    FOR EACH ROW
    EXECUTE FUNCTION kill_switch_notify();
