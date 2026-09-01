-- Wakes an idle dispatcher's per-channel claim loop (DESIGN.md §4.2:
-- "LISTEN/NOTIFY on insert wakes an idle dispatcher immediately; a
-- 1-second poll is the fallback"). A trigger, not an app-level pg_notify
-- call in ingest::repo::insert_transactional, so every future outbox
-- writer wakes the dispatcher for free (T-013 decision 5). One Postgres
-- NOTIFY channel per outbox `channel` value (outbox_sms, outbox_email,
-- outbox_whatsapp) so each per-channel claim loop only ever wakes for its
-- own channel -- no payload filtering needed.
CREATE OR REPLACE FUNCTION outbox_notify() RETURNS trigger AS $$
BEGIN
    PERFORM pg_notify('outbox_' || NEW.channel, NEW.comms_request_id::text);
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER outbox_notify_trigger
    AFTER INSERT ON outbox
    FOR EACH ROW
    EXECUTE FUNCTION outbox_notify();
