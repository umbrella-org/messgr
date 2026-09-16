-- Suppression list (DESIGN.md §5, §4.4, T-038): hard bounces, complaints, and
-- regulatory holds, keyed on the raw destination_hmac -- deliberately not
-- address_id or customer_id, so a recycled destination cannot inherit or
-- escape a suppression entry that belongs to whoever held it before (§5).
-- review_at is mandatory: an entry blocks only while review_at > now(), so
-- retiring one early is the same UPDATE as letting it expire naturally --
-- no separate "removed" state, no sweep job needed.
CREATE TABLE suppression (
    destination_hmac bytea       PRIMARY KEY,
    reason            text       NOT NULL,  -- hard_bounce | complaint | regulatory_hold
    added_at          timestamptz NOT NULL,
    review_at         timestamptz NOT NULL
);
