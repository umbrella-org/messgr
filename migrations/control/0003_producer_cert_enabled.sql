-- Denormalizes producer.enabled (tenant DB) onto producer_cert (control DB), so mTLS
-- resolution (DESIGN.md §4.9, §11.1, T-006) can distinguish "unknown cert" from "known but
-- disabled" with a single control-database query, honouring §4.11's stated ordering that
-- resolution happens before any tenant database is opened. Kept in sync by
-- src/producer/register.rs's disable_producer (control-first write — T-006 decision 2).
ALTER TABLE producer_cert ADD COLUMN enabled bool NOT NULL DEFAULT true;
