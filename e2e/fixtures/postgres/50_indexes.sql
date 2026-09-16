-- M-PG-IDX-01 .. 05, 07 .. 12, 14, 15, 16, 18 on mx.t_idx_base (10_tables.sql)
-- and on mx.m_mv_01 (40_matviews.sql).

-- IDX-01: plain btree.
CREATE INDEX i_idx_01 ON mx.t_idx_base (name);

-- IDX-02: unique.
CREATE UNIQUE INDEX i_idx_02 ON mx.t_idx_base (id);

-- IDX-03: partial.
CREATE INDEX i_idx_03 ON mx.t_idx_base (amount) WHERE flag;

-- IDX-04: on an expression.
CREATE INDEX i_idx_04 ON mx.t_idx_base (lower(name));

-- IDX-05: multi-column with an explicit order and null placement.
CREATE INDEX i_idx_05 ON mx.t_idx_base (amount DESC NULLS FIRST, name);

-- IDX-07: GIN over jsonb.
CREATE INDEX i_idx_07 ON mx.t_idx_base USING gin (doc);

-- IDX-08: GiST over a range column.
CREATE INDEX i_idx_08 ON mx.t_idx_base USING gist (span);

-- IDX-09: hash.
CREATE INDEX i_idx_09 ON mx.t_idx_base USING hash (id);

-- IDX-10: BRIN.
CREATE INDEX i_idx_10 ON mx.t_idx_base USING brin (created);

-- IDX-11: a non-default operator class.
CREATE INDEX i_idx_11 ON mx.t_idx_base (name text_pattern_ops);

-- IDX-12: index storage parameters.
CREATE INDEX i_idx_12 ON mx.t_idx_base (created) WITH (fillfactor = 50);

-- IDX-14: an index on a materialized view.
CREATE INDEX i_idx_14 ON mx.m_mv_01 (label);

-- IDX-15: an explicit collation.
CREATE INDEX i_idx_15 ON mx.t_idx_base (name COLLATE "C");

-- IDX-16: a quoted, mixed-case index name.
CREATE INDEX "I Idx 16" ON mx.t_idx_base (id, name);

-- IDX-18: commented.
CREATE INDEX i_idx_18 ON mx.t_idx_base (flag);
COMMENT ON INDEX mx.i_idx_18 IS 'index comment for M-PG-IDX-18';

-- Legacy coverage: a plain index on the legacy orders table.
CREATE INDEX orders_acct_idx ON app.orders (acct);
