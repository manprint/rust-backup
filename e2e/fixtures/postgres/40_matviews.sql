-- M-PG-MV-01 .. 07 and M-PG-VIEW-07.

-- MV-01: populated.
CREATE MATERIALIZED VIEW mx.m_mv_01 AS
  SELECT c_int AS id, c_text AS label FROM mx.t_tab_01;

-- MV-02: created WITH NO DATA and it must stay unpopulated after a restore.
CREATE MATERIALIZED VIEW mx.m_mv_02 AS
  SELECT c_int AS id, c_varchar AS label FROM mx.t_tab_01 WITH NO DATA;

-- MV-03: a matview over a matview; the refresh order is the property under test.
CREATE MATERIALIZED VIEW mx.m_mv_03 AS
  SELECT id, count(*) AS rows FROM mx.m_mv_01 GROUP BY id;

-- MV-04: a unique index (which makes REFRESH CONCURRENTLY possible) and a
-- second, non-unique one.
CREATE MATERIALIZED VIEW mx.m_mv_04 AS
  SELECT c_int AS id, c_text AS label, c_bool AS flag FROM mx.t_tab_01;
CREATE UNIQUE INDEX i_mv_04_unique ON mx.m_mv_04 (id);
CREATE INDEX i_mv_04_label ON mx.m_mv_04 (label);

-- MV-05: storage parameters on a materialized view.
CREATE MATERIALIZED VIEW mx.m_mv_05 WITH (fillfactor = 70) AS
  SELECT c_int AS id FROM mx.t_tab_01;

-- MV-06: a matview whose source is a view.
CREATE MATERIALIZED VIEW mx.m_mv_06 AS
  SELECT c_int, c_text FROM mx.v_view_01;

-- MV-07: COMMENT ON MATERIALIZED VIEW, not COMMENT ON VIEW (regression 3723036).
CREATE MATERIALIZED VIEW mx.m_mv_07 AS SELECT c_int AS id FROM mx.t_tab_01;
COMMENT ON MATERIALIZED VIEW mx.m_mv_07 IS 'matview comment for M-PG-MV-07';

-- VIEW-07: a plain view reading a materialized view.
CREATE VIEW mx.v_view_07 AS SELECT id, label FROM mx.m_mv_01;

-- Legacy coverage: a matview whose query is parsed at creation time even
-- WITH NO DATA, a populated one whose query must run after the data load, and
-- COMMENT ON MATERIALIZED VIEW for the second relkind.
CREATE MATERIALIZED VIEW app.account_status_mv AS
  SELECT a.id, a.email, count(o.id) AS orders
    FROM app.accounts a LEFT JOIN app.orders o ON o.acct = a.id
   GROUP BY a.id;
CREATE MATERIALIZED VIEW app.account_totals AS
  SELECT a.id, count(o.id) AS orders
    FROM app.accounts a LEFT JOIN app.orders o ON o.acct = a.id
   GROUP BY a.id;
CREATE UNIQUE INDEX account_totals_id_idx ON app.account_totals (id);
COMMENT ON MATERIALIZED VIEW app.account_totals IS 'totals per account';
