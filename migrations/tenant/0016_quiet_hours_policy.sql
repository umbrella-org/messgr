-- Quiet-hours policy (DESIGN.md §4.10, §6.1, T-043). scope_key defaults to
-- '' (NOT NULL, not nullable) so the institution-wide default row --
-- (scope, scope_key) = ('default', '') -- has a representable primary key;
-- a PRIMARY KEY column is implicitly NOT NULL, so a nullable scope_key
-- could never hold that row at all (see 03-data-model.md's own correction
-- note). scope='region'|'segment' rows are representable but currently
-- unreachable -- see 05-send-timing.md's §6.1 correction note; only
-- ('default', '') is ever read or written by this ticket's code.
CREATE TABLE quiet_hours_policy (
    scope       text NOT NULL,
    scope_key   text NOT NULL DEFAULT '',
    start_local time NOT NULL,
    end_local   time NOT NULL,
    PRIMARY KEY (scope, scope_key)
);
