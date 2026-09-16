-- M-PG-REF-14: a logical-replication publication.
CREATE TABLE ref_base (id integer, payload text);
INSERT INTO ref_base SELECT g, 'r-' || g FROM generate_series(1, 3) g;
CREATE PUBLICATION ref_publication FOR TABLE ref_base;
