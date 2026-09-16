-- M-PG-TAB-09. Loaded before 10_tables.sql, so it depends only on 00_roles.sql.
CREATE TABLE mx.t_tab_09 (
  id       integer,
  base     numeric(10,2),
  doubled  numeric(12,2) GENERATED ALWAYS AS (base * 2) STORED
);
INSERT INTO mx.t_tab_09 (id, base)
SELECT g, (g * 1.5)::numeric(10,2) FROM generate_series(1, 12) g;
