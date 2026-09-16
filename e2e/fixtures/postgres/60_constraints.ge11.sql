-- M-PG-CON-08, 16. Loaded before 60_constraints.sql, so its tables are local.

-- CON-08: a foreign key declared on a partitioned table.
CREATE TABLE mx.t_con_08_parent (id integer PRIMARY KEY);
INSERT INTO mx.t_con_08_parent SELECT g FROM generate_series(1, 10) g;
CREATE TABLE mx.t_con_08 (id integer, ref integer, ts date NOT NULL)
  PARTITION BY RANGE (ts);
CREATE TABLE mx.t_con_08_a PARTITION OF mx.t_con_08
  FOR VALUES FROM ('2026-01-01') TO ('2027-01-01');
CREATE TABLE mx.t_con_08_b PARTITION OF mx.t_con_08
  FOR VALUES FROM ('2027-01-01') TO ('2028-01-01');
ALTER TABLE mx.t_con_08 ADD CONSTRAINT c_con_08 FOREIGN KEY (ref)
  REFERENCES mx.t_con_08_parent (id);
INSERT INTO mx.t_con_08 SELECT g, g, DATE '2026-05-05' FROM generate_series(1, 6) g;
INSERT INTO mx.t_con_08 SELECT g, g, DATE '2027-05-05' FROM generate_series(7, 10) g;

-- CON-16: a check declared on the parent, inherited by every partition
-- (conislocal = false there) and therefore not re-added on the partitions.
CREATE TABLE mx.t_con_16 (id integer, amount numeric(8,2), ts date NOT NULL)
  PARTITION BY RANGE (ts);
ALTER TABLE mx.t_con_16 ADD CONSTRAINT c_con_16 CHECK (amount >= 0);
CREATE TABLE mx.t_con_16_a PARTITION OF mx.t_con_16
  FOR VALUES FROM ('2026-01-01') TO ('2027-01-01');
CREATE TABLE mx.t_con_16_b PARTITION OF mx.t_con_16
  FOR VALUES FROM ('2027-01-01') TO ('2028-01-01');
INSERT INTO mx.t_con_16 SELECT g, g, DATE '2026-06-06' FROM generate_series(1, 5) g;
INSERT INTO mx.t_con_16 SELECT g, g, DATE '2027-06-06' FROM generate_series(6, 9) g;
