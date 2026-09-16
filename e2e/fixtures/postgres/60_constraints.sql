-- M-PG-CON-01 .. 06, 09 .. 15, and M-PG-EXT-02 (btree_gist, which CON-14
-- needs). The tables are local to this file.

-- CON-14 needs an equality operator class for a scalar column in a GiST
-- exclusion constraint; that comes from btree_gist (M-PG-EXT-02).
CREATE EXTENSION IF NOT EXISTS btree_gist;

-- CON-01: single-column primary key.
CREATE TABLE mx.t_con_01 (id integer, payload text);
ALTER TABLE mx.t_con_01 ADD CONSTRAINT c_con_01 PRIMARY KEY (id);
INSERT INTO mx.t_con_01 SELECT g, 'pk-' || g FROM generate_series(1, 5) g;

-- CON-02: composite primary key.
CREATE TABLE mx.t_con_02 (a integer, b integer, payload text);
ALTER TABLE mx.t_con_02 ADD CONSTRAINT c_con_02 PRIMARY KEY (a, b);
INSERT INTO mx.t_con_02 SELECT g, g * 2, 'cpk-' || g FROM generate_series(1, 4) g;

-- CON-03: unique.
CREATE TABLE mx.t_con_03 (id integer, code text);
ALTER TABLE mx.t_con_03 ADD CONSTRAINT c_con_03 UNIQUE (code);
INSERT INTO mx.t_con_03 SELECT g, 'u-' || g FROM generate_series(1, 4) g;

-- CON-04: referential actions on both sides.
CREATE TABLE mx.t_con_04_child (id integer PRIMARY KEY, parent integer);
ALTER TABLE mx.t_con_04_child ADD CONSTRAINT c_con_04 FOREIGN KEY (parent)
  REFERENCES mx.t_con_01 (id) ON DELETE CASCADE ON UPDATE SET NULL;
INSERT INTO mx.t_con_04_child SELECT g, g FROM generate_series(1, 5) g;

-- CON-05: deferrable, deferred by default.
CREATE TABLE mx.t_con_05_child (id integer PRIMARY KEY, parent integer);
ALTER TABLE mx.t_con_05_child ADD CONSTRAINT c_con_05 FOREIGN KEY (parent)
  REFERENCES mx.t_con_01 (id) DEFERRABLE INITIALLY DEFERRED;
INSERT INTO mx.t_con_05_child SELECT g, g FROM generate_series(1, 5) g;

-- CON-06: NOT VALID over a row that violates it. The row must round-trip and
-- the constraint must stay unvalidated.
CREATE TABLE mx.t_con_06_child (id integer PRIMARY KEY, parent integer);
INSERT INTO mx.t_con_06_child VALUES (1, 1), (2, 999);
ALTER TABLE mx.t_con_06_child ADD CONSTRAINT c_con_06 FOREIGN KEY (parent)
  REFERENCES mx.t_con_01 (id) NOT VALID;

-- CON-09: self-referencing.
CREATE TABLE mx.t_con_09 (id integer PRIMARY KEY, parent integer);
ALTER TABLE mx.t_con_09 ADD CONSTRAINT c_con_09 FOREIGN KEY (parent)
  REFERENCES mx.t_con_09 (id);
INSERT INTO mx.t_con_09 VALUES (1, NULL), (2, 1), (3, 2);

-- CON-10: composite foreign key.
CREATE TABLE mx.t_con_10 (a integer, b integer, payload text);
ALTER TABLE mx.t_con_10 ADD CONSTRAINT c_con_10 FOREIGN KEY (a, b)
  REFERENCES mx.t_con_02 (a, b);
INSERT INTO mx.t_con_10 SELECT g, g * 2, 'cfk-' || g FROM generate_series(1, 4) g;

-- CON-11: check.
CREATE TABLE mx.t_con_11 (id integer, amount numeric(8,2));
ALTER TABLE mx.t_con_11 ADD CONSTRAINT c_con_11 CHECK (amount >= 0);
INSERT INTO mx.t_con_11 SELECT g, g * 1.5 FROM generate_series(1, 4) g;

-- CON-12: check NOT VALID over a violating row.
CREATE TABLE mx.t_con_12 (id integer, amount numeric(8,2));
INSERT INTO mx.t_con_12 VALUES (1, 5.00), (2, -3.00);
ALTER TABLE mx.t_con_12 ADD CONSTRAINT c_con_12 CHECK (amount >= 0) NOT VALID;

-- CON-13: a check that does not descend to children.
CREATE TABLE mx.t_con_13 (id integer, amount numeric(8,2));
ALTER TABLE mx.t_con_13 ADD CONSTRAINT c_con_13 CHECK (amount < 1000) NO INHERIT;
INSERT INTO mx.t_con_13 SELECT g, g FROM generate_series(1, 3) g;

-- CON-14: exclusion constraint mixing an equality column with a range overlap.
CREATE TABLE mx.t_con_14 (id integer PRIMARY KEY, room integer, during tsrange);
ALTER TABLE mx.t_con_14 ADD CONSTRAINT c_con_14
  EXCLUDE USING gist (room WITH =, during WITH &&);
INSERT INTO mx.t_con_14 VALUES
  (1, 1, tsrange('2026-01-01', '2026-01-02')),
  (2, 1, tsrange('2026-02-01', '2026-02-02')),
  (3, 2, tsrange('2026-01-01', '2026-01-02'));

-- CON-15: a commented constraint.
CREATE TABLE mx.t_con_15 (id integer, code text);
ALTER TABLE mx.t_con_15 ADD CONSTRAINT c_con_15 UNIQUE (code);
COMMENT ON CONSTRAINT c_con_15 ON mx.t_con_15 IS 'constraint comment for M-PG-CON-15';
INSERT INTO mx.t_con_15 SELECT g, 'c-' || g FROM generate_series(1, 3) g;
