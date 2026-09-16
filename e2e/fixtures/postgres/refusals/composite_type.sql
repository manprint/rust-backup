-- M-PG-REF-03: a standalone composite type.
CREATE TABLE ref_base (id integer, payload text);
INSERT INTO ref_base SELECT g, 'r-' || g FROM generate_series(1, 3) g;
CREATE TYPE ref_pair AS (a integer, b text);
