-- M-PG-IDX-06, 13. Loaded before 50_indexes.sql, so it depends only on the
-- table fixtures (10_*).

-- IDX-06: a covering index.
CREATE UNIQUE INDEX i_idx_06 ON mx.t_idx_base (id) INCLUDE (name, amount);

-- IDX-13: an index on a partitioned parent, which PostgreSQL creates on every
-- partition and attaches to the parent.
CREATE INDEX i_idx_13 ON mx.t_tab_04 (ts, id);

-- Legacy coverage: an index on a partitioned parent, which stays invalid until
-- every partition index is attached to it.
CREATE INDEX events_ts_idx ON app.events (ts);
