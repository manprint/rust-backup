-- M-PG-TAB-04, 05. Loaded before 10_tables.sql (a `.ge` file sorts first), so
-- it may only depend on 00_roles.sql.

-- TAB-04: RANGE partitioning with an explicit DEFAULT partition.
CREATE TABLE mx.t_tab_04 (id bigint, ts date NOT NULL, payload text)
  PARTITION BY RANGE (ts);
CREATE TABLE mx.t_tab_04_2026 PARTITION OF mx.t_tab_04
  FOR VALUES FROM ('2026-01-01') TO ('2027-01-01');
CREATE TABLE mx.t_tab_04_2027 PARTITION OF mx.t_tab_04
  FOR VALUES FROM ('2027-01-01') TO ('2028-01-01');
CREATE TABLE mx.t_tab_04_default PARTITION OF mx.t_tab_04 DEFAULT;
INSERT INTO mx.t_tab_04 SELECT g, DATE '2026-03-04', 'p26-' || g FROM generate_series(1, 20) g;
INSERT INTO mx.t_tab_04 SELECT g, DATE '2027-03-04', 'p27-' || g FROM generate_series(21, 45) g;
INSERT INTO mx.t_tab_04 SELECT g, DATE '2030-03-04', 'pdef-' || g FROM generate_series(46, 52) g;

-- TAB-05: LIST partitioning whose child is itself partitioned.
CREATE TABLE mx.t_tab_05 (id bigint, region text, bucket integer, payload text)
  PARTITION BY LIST (region);
CREATE TABLE mx.t_tab_05_north PARTITION OF mx.t_tab_05
  FOR VALUES IN ('north') PARTITION BY RANGE (bucket);
CREATE TABLE mx.t_tab_05_north_low PARTITION OF mx.t_tab_05_north
  FOR VALUES FROM (0) TO (100);
CREATE TABLE mx.t_tab_05_north_high PARTITION OF mx.t_tab_05_north
  FOR VALUES FROM (100) TO (1000);
CREATE TABLE mx.t_tab_05_south PARTITION OF mx.t_tab_05 FOR VALUES IN ('south');
INSERT INTO mx.t_tab_05 SELECT g, 'north', g, 'n-' || g FROM generate_series(1, 30) g;
INSERT INTO mx.t_tab_05 SELECT g, 'north', g + 100, 'nh-' || g FROM generate_series(31, 40) g;
INSERT INTO mx.t_tab_05 SELECT g, 'south', g, 's-' || g FROM generate_series(41, 55) g;

-- Legacy coverage: a partitioned parent whose primary key cascades to every
-- partition. The partitions' own catalog rows must not be re-emitted.
CREATE TABLE app.events (id bigint, ts date NOT NULL, PRIMARY KEY (id, ts))
  PARTITION BY RANGE (ts);
CREATE TABLE app.events_2026 PARTITION OF app.events
  FOR VALUES FROM ('2026-01-01') TO ('2027-01-01');
CREATE TABLE app.events_2027 PARTITION OF app.events
  FOR VALUES FROM ('2027-01-01') TO ('2028-01-01');
INSERT INTO app.events SELECT g, '2026-03-04'::date FROM generate_series(1, 50) g;
INSERT INTO app.events SELECT g, '2027-03-04'::date FROM generate_series(51, 120) g;
