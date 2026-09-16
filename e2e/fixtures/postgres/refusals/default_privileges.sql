-- M-PG-REF-13: ALTER DEFAULT PRIVILEGES, which changes objects not yet created.
CREATE TABLE ref_base (id integer, payload text);
INSERT INTO ref_base SELECT g, 'r-' || g FROM generate_series(1, 3) g;
ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT SELECT ON TABLES TO PUBLIC;
