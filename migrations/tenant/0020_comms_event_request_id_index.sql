-- Message-detail event history (DESIGN.md §11, T-048): comms_event has no
-- existing index on comms_request_id alone (only (customer_id,
-- occurred_at)); this lookup runs far more often than the campaign-reach
-- query the existing (campaign_id, created_at) index on comms_request
-- already serves.
CREATE INDEX ON comms_event (comms_request_id, occurred_at);
