-- M-PG-VIEW-01 .. 06, 08, 09. VIEW-07 (view over a materialized view) lives in
-- 40_matviews.sql because it must be created after that matview.

-- VIEW-01: the plain case.
CREATE VIEW mx.v_view_01 AS SELECT c_int, c_text FROM mx.t_tab_01 WHERE c_bool;

-- VIEW-02: a three-level chain, named so that alphabetical order is the wrong
-- creation order.
CREATE VIEW mx.v_view_02_c AS SELECT c_int, c_text FROM mx.t_tab_01;
CREATE VIEW mx.v_view_02_b AS SELECT c_int, c_text FROM mx.v_view_02_c WHERE c_int > 2;
CREATE VIEW mx.v_view_02_a AS SELECT c_int FROM mx.v_view_02_b WHERE c_int < 20;

-- VIEW-03: only creatable once the primary key exists — `label` is neither
-- grouped nor aggregated and is legal solely because it is functionally
-- dependent on the grouped primary key.
CREATE VIEW mx.v_view_03 AS
  SELECT t.id_always, t.label, count(*) AS rows
    FROM mx.t_tab_08 t
   GROUP BY t.id_always;

-- VIEW-04: updatable, with a check option.
CREATE VIEW mx.v_view_04 AS
  SELECT id, payload FROM mx.t_tab_03 WHERE id < 100 WITH CASCADED CHECK OPTION;

-- VIEW-05: a security barrier view.
CREATE VIEW mx.v_view_05 WITH (security_barrier = true) AS
  SELECT id, payload FROM mx.t_tab_02;

-- VIEW-06: a view calling a user function.
CREATE FUNCTION mx.f_double(value integer) RETURNS integer LANGUAGE sql IMMUTABLE
  AS $fn$ SELECT value * 2 $fn$;
CREATE VIEW mx.v_view_06 AS SELECT c_int, mx.f_double(c_int) AS doubled FROM mx.t_tab_01;

-- VIEW-08: a recursive CTE.
CREATE VIEW mx.v_view_08 AS
  WITH RECURSIVE counter(n) AS (
    SELECT 1
    UNION ALL
    SELECT n + 1 FROM counter WHERE n < 10
  )
  SELECT n FROM counter;

-- VIEW-10: an IN list on a varchar column, the shape of Odoo's report views.
-- PostgreSQL stores it as `c_varchar::text = ANY (ARRAY['…'::character
-- varying, …]::text[])` and renders the restored copy as `ARRAY['…'::character
-- varying::text, …]`: the text does not survive its own round-trip, on any
-- major, so only a comparison against the destination's rendering can pass.
CREATE VIEW mx.v_view_10 AS
  SELECT c_int, c_varchar FROM mx.t_tab_01
   WHERE c_varchar IN ('varchar-1', 'varchar-2') AND c_varchar NOT IN ('x', 'y');

-- VIEW-09: commented.
CREATE VIEW mx.v_view_09 AS SELECT c_int FROM mx.t_tab_01;
COMMENT ON VIEW mx.v_view_09 IS 'view comment for M-PG-VIEW-09';

-- Legacy coverage: a view reading a view, named so that catalog order is the
-- wrong creation order, and a view that is only creatable once the primary key
-- exists (`status` is functionally dependent on the grouped key).
CREATE VIEW app.zombies AS SELECT id, email FROM app.accounts WHERE status <> 'active';
CREATE VIEW app.active AS SELECT * FROM app.zombies;
CREATE VIEW app.account_status AS
  SELECT a.id, a.status, count(o.id) AS orders
    FROM app.accounts a LEFT JOIN app.orders o ON o.acct = a.id
   GROUP BY a.id;
COMMENT ON VIEW app.account_status IS 'status per account';
