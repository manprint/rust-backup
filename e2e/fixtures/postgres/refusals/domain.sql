-- M-PG-REF-02: a domain.
CREATE TABLE ref_base (id integer, payload text);
INSERT INTO ref_base SELECT g, 'r-' || g FROM generate_series(1, 3) g;
CREATE DOMAIN ref_positive AS integer CHECK (VALUE > 0);
