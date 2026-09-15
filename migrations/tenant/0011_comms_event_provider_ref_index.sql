-- Standalone index on comms_event.provider_ref (T-030): the reconciliation
-- job matches orphan_event rows against comms_event by provider_ref alone,
-- and the only existing index touching that column is the composite
-- UNIQUE (occurred_at, comms_request_id, event_type, provider_ref), which
-- can't serve a provider_ref-only lookup -- every reconcile match would
-- otherwise scan every partition.

CREATE INDEX ON comms_event (provider_ref);
