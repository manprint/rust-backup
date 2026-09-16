-- M-PG-REF-05: row-level security.
CREATE TABLE ref_base (id integer, payload text);
INSERT INTO ref_base SELECT g, 'r-' || g FROM generate_series(1, 3) g;
ALTER TABLE ref_base ENABLE ROW LEVEL SECURITY;
CREATE POLICY ref_visible ON ref_base USING (id > 0);
