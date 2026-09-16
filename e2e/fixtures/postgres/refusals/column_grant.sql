-- M-PG-REF-12: a column-level privilege.
CREATE TABLE ref_base (id integer, payload text);
INSERT INTO ref_base SELECT g, 'r-' || g FROM generate_series(1, 3) g;
GRANT SELECT (payload) ON ref_base TO PUBLIC;
