-- M-PG-EXT-01, 03, 04, 05, 06, 07. Loaded last of the ungated files, so every
-- relation it references already exists. EXT-02 (btree_gist) is created by
-- 60_constraints.sql, which needs it for the exclusion constraint.

-- EXT-01: trigram search.
CREATE EXTENSION pg_trgm;
CREATE TABLE mx.t_ext_01 (id integer, title text);
INSERT INTO mx.t_ext_01 SELECT g, 'title number ' || g FROM generate_series(1, 20) g;
CREATE INDEX i_ext_01_trgm ON mx.t_ext_01 USING gin (title gin_trgm_ops);

-- EXT-03: an extension-provided type in a column, with its own index.
CREATE EXTENSION hstore;
CREATE TABLE mx.t_ext_03 (id integer, attributes hstore);
INSERT INTO mx.t_ext_03
SELECT g, hstore(ARRAY['k', 'n'], ARRAY['v' || g, g::text]) FROM generate_series(1, 10) g;
CREATE INDEX i_ext_03_gin ON mx.t_ext_03 USING gin (attributes);

-- EXT-04: a case-insensitive text column.
CREATE EXTENSION citext;
CREATE TABLE mx.t_ext_04 (id integer, label citext);
INSERT INTO mx.t_ext_04 SELECT g, ('MiXeD-' || g)::citext FROM generate_series(1, 6) g;

-- EXT-05: a column default calling an extension function.
CREATE EXTENSION "uuid-ossp";
CREATE TABLE mx.t_ext_05 (id integer, ident uuid DEFAULT uuid_generate_v4());
INSERT INTO mx.t_ext_05 (id) SELECT g FROM generate_series(1, 5) g;

-- EXT-06: an extension installed outside public. hstore is already installed in
-- public for EXT-03 and an extension exists at most once per database, so this
-- row uses tablefunc, which has the same property under test (extnamespace).
CREATE EXTENSION tablefunc SCHEMA ext;

-- EXT-07: a custom extension with a configuration table. Its files are
-- installed by 71_rbtest_extension.sh before this file is loaded. The rows
-- CREATE EXTENSION inserts (k = 1 .. 3) are extension data and are not copied;
-- the row matching the registered condition (k >= 1000) is user data and must
-- round-trip.
CREATE EXTENSION rbtest;
INSERT INTO rbtest_cfg VALUES (1000, 'custom');
