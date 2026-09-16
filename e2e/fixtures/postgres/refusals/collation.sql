-- M-PG-REF-08: a user-defined collation.
CREATE TABLE ref_base (id integer, payload text);
INSERT INTO ref_base SELECT g, 'r-' || g FROM generate_series(1, 3) g;
CREATE COLLATION ref_collation (locale = 'C');
