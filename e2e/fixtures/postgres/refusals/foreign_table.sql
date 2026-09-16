-- M-PG-REF-10: a foreign table, whose data lives in another server.
CREATE TABLE ref_base (id integer, payload text);
INSERT INTO ref_base SELECT g, 'r-' || g FROM generate_series(1, 3) g;
CREATE EXTENSION postgres_fdw;
CREATE SERVER ref_server FOREIGN DATA WRAPPER postgres_fdw
  OPTIONS (host 'localhost', port '5432', dbname 'postgres');
CREATE USER MAPPING FOR CURRENT_USER SERVER ref_server OPTIONS (user 'postgres');
CREATE FOREIGN TABLE ref_foreign (id integer, payload text)
  SERVER ref_server OPTIONS (schema_name 'public', table_name 'ref_base');
