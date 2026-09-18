-- comms_request(id) index (DESIGN.md §6.2, §10, T-041) — comms_request is
-- partitioned by created_at with no index on id alone; DELETE /comms/{id}
-- (and the future GET /comms/{id}, §10) both look up by id only. Declaring
-- the index on the partitioned parent propagates it to every existing
-- partition and every partition T-014's create-ahead job creates later.
CREATE INDEX ON comms_request (id);
