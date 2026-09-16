-- M-PG-CON-07: a foreign key that references a partitioned table (PostgreSQL 12+).
CREATE TABLE mx.t_con_07_parent (id integer, ts date NOT NULL, PRIMARY KEY (id, ts))
  PARTITION BY RANGE (ts);
CREATE TABLE mx.t_con_07_parent_a PARTITION OF mx.t_con_07_parent
  FOR VALUES FROM ('2026-01-01') TO ('2027-01-01');
CREATE TABLE mx.t_con_07_parent_b PARTITION OF mx.t_con_07_parent
  FOR VALUES FROM ('2027-01-01') TO ('2028-01-01');
INSERT INTO mx.t_con_07_parent SELECT g, DATE '2026-07-07' FROM generate_series(1, 5) g;
INSERT INTO mx.t_con_07_parent SELECT g, DATE '2027-07-07' FROM generate_series(6, 9) g;

CREATE TABLE mx.t_con_07_child (id integer PRIMARY KEY, ref integer, ref_ts date);
ALTER TABLE mx.t_con_07_child ADD CONSTRAINT c_con_07 FOREIGN KEY (ref, ref_ts)
  REFERENCES mx.t_con_07_parent (id, ts);
INSERT INTO mx.t_con_07_child SELECT g, g, DATE '2026-07-07' FROM generate_series(1, 5) g;
