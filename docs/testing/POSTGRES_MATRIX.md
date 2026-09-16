# PostgreSQL fidelity matrix

Catalogue of the PostgreSQL cases the module must reproduce 1:1, or refuse before
any byte is transferred. The runner is `e2e/postgres_matrix.sh`: it prints one
`PASS <ID>`, `FAIL <ID>` or `SKIP <ID>` line per row and a final `MATRIX PG <major>:
p pass, f fail, s skip` summary. This document is the catalogue only — it never
records execution status, so a row here is never "green" or "red"; the runner
output is the single source of truth for that.

Row IDs are `M-PG-<KIND>-<nn>`. Fixture objects live in schema `mx` (plus
`"Mixed Schema"` for TAB-14 and `ext` for EXT-06) and embed kind and row number,
for example `mx.t_tab_03`, `mx.s_seq_01`, `mx.v_view_02`, `mx.m_mv_04`,
`i_idx_07`, `c_con_11`. Fixture files live in `e2e/fixtures/postgres/` and are
loaded in lexical order; a `.ge<major>` suffix means the file is loaded only when
the server major is at least `<major>` (see `e2e/fixtures/postgres/README.md`).

`Min major` is the lowest PostgreSQL major on which the row runs; below it the
runner prints `SKIP`. `Expected` is `round-trip` (the object and its data must
exist on the destination, identical to the source) or `refused before transfer`
(the run must fail during Analyze or Validate, with no destination database
left behind). `Oracle` names the external check that decides the row:

| Oracle | Meaning |
|--------|---------|
| `schema diff` | `pg_dump --schema-only` taken inside the destination container against source and destination, normalised, `diff` empty |
| `row digest` | `count(*)` and the order-independent md5 digest of every row of the relation, taken with `FROM ONLY`, equal on both sides |
| `seq state` | `last_value` and `is_called` equal on both sides |
| `con state` | `convalidated` and `pg_get_constraintdef` equal on both sides |
| `idx def` | `pg_get_indexdef` equal on both sides |
| `catalog` | a targeted catalog query named in the row |
| `refusal` | the source exits non-zero before transfer, the message names the object kind, and the destination database does not exist |

## TAB — tables

| ID | Case | Min major | Fixture file | Expected | Oracle |
|----|------|-----------|--------------|----------|--------|
| M-PG-TAB-01 | plain heap table with all common column types (int, bigint, numeric(12,4), text, varchar(40), bool, date, timestamptz, interval, jsonb, bytea, uuid, int[], text[]) | 10 | `10_tables.sql` | round-trip | schema diff + row digest |
| M-PG-TAB-02 | UNLOGGED table | 10 | `10_tables.sql` | round-trip | schema diff + row digest |
| M-PG-TAB-03 | table with `reloptions` (`fillfactor=70`) | 10 | `10_tables.sql` | round-trip | schema diff |
| M-PG-TAB-04 | RANGE-partitioned parent with two partitions and a DEFAULT partition | 11 | `10_tables.ge11.sql` | round-trip | schema diff + row digest |
| M-PG-TAB-05 | LIST-partitioned parent with a sub-partitioned child | 11 | `10_tables.ge11.sql` | round-trip | schema diff + row digest |
| M-PG-TAB-06 | classic INHERITS parent and child, rows in both | 10 | `10_tables.sql` | round-trip | schema diff + row digest |
| M-PG-TAB-07 | table with a dropped column (attnum gap) and rows | 10 | `10_tables.sql` | round-trip | schema diff + row digest |
| M-PG-TAB-08 | identity columns `GENERATED ALWAYS` and `BY DEFAULT`, rows beyond the seed | 10 | `10_tables.sql` | round-trip | schema diff + row digest + seq state |
| M-PG-TAB-09 | stored generated column | 12 | `10_tables.ge12.sql` | round-trip | schema diff + row digest |
| M-PG-TAB-10 | column with explicit `COLLATE "C"` and a column with the database default collation | 10 | `10_tables.sql` | round-trip | schema diff |
| M-PG-TAB-11 | column default calling `nextval` of a standalone sequence | 10 | `10_tables.sql` | round-trip | schema diff + seq state |
| M-PG-TAB-12 | zero-row table | 10 | `10_tables.sql` | round-trip | schema diff + row digest |
| M-PG-TAB-13 | 100 000-row table (bulk) | 10 | `10_tables.sql` | round-trip | row digest |
| M-PG-TAB-14 | quoted mixed-case schema and table (`"Mixed Schema"."Weird Table"`) with reserved-word column `"select"` | 10 | `10_tables.sql` | round-trip | schema diff + row digest |
| M-PG-TAB-15 | fifty-column table | 10 | `10_tables.sql` | round-trip | schema diff + row digest |
| M-PG-TAB-16 | TOAST-heavy table, 1 MiB text values, 200 rows | 10 | `10_tables.sql` | round-trip | row digest |
| M-PG-TAB-17 | table-level and column-level `COMMENT` | 10 | `10_tables.sql` | round-trip | schema diff |
| M-PG-TAB-18 | table owned by a non-superuser role with GRANTs to two roles | 10 | `00_roles.sql`, `10_tables.sql` | round-trip | schema diff |

## SEQ — sequences

| ID | Case | Min major | Fixture file | Expected | Oracle |
|----|------|-----------|--------------|----------|--------|
| M-PG-SEQ-01 | standalone sequence `INCREMENT 5 MINVALUE 10 MAXVALUE 1000 CACHE 20 CYCLE` | 10 | `20_sequences.sql` | round-trip | schema diff + seq state |
| M-PG-SEQ-02 | `AS smallint` sequence | 10 | `20_sequences.sql` | round-trip | schema diff + seq state |
| M-PG-SEQ-03 | sequence `OWNED BY` a column | 10 | `20_sequences.sql` | round-trip | schema diff |
| M-PG-SEQ-04 | sequence never called (`is_called = false`) | 10 | `20_sequences.sql` | round-trip | seq state |
| M-PG-SEQ-05 | sequence advanced to its `MAXVALUE` | 10 | `20_sequences.sql` | round-trip | seq state |
| M-PG-SEQ-06 | negative-increment sequence | 10 | `20_sequences.sql` | round-trip | schema diff + seq state |
| M-PG-SEQ-07 | identity-backed sequence of TAB-08, current value identical after restore | 10 | `10_tables.sql` | round-trip | seq state |

## VIEW — views

| ID | Case | Min major | Fixture file | Expected | Oracle |
|----|------|-----------|--------------|----------|--------|
| M-PG-VIEW-01 | simple view | 10 | `30_views.sql` | round-trip | schema diff |
| M-PG-VIEW-02 | three-level view chain | 10 | `30_views.sql` | round-trip | schema diff |
| M-PG-VIEW-03 | view with `GROUP BY` on the primary key | 10 | `30_views.sql` | round-trip | schema diff |
| M-PG-VIEW-04 | updatable view `WITH CHECK OPTION` | 10 | `30_views.sql` | round-trip | schema diff |
| M-PG-VIEW-05 | view with `security_barrier` | 10 | `30_views.sql` | round-trip | schema diff |
| M-PG-VIEW-06 | view calling a user function | 10 | `30_views.sql` | round-trip | schema diff |
| M-PG-VIEW-07 | view over a materialized view | 10 | `40_matviews.sql` | round-trip | schema diff |
| M-PG-VIEW-08 | recursive CTE view | 10 | `30_views.sql` | round-trip | schema diff |
| M-PG-VIEW-09 | view with a `COMMENT` | 10 | `30_views.sql` | round-trip | schema diff |

## MV — materialized views

| ID | Case | Min major | Fixture file | Expected | Oracle |
|----|------|-----------|--------------|----------|--------|
| M-PG-MV-01 | populated matview | 10 | `40_matviews.sql` | round-trip | schema diff + row digest |
| M-PG-MV-02 | matview `WITH NO DATA`, must stay unpopulated | 10 | `40_matviews.sql` | round-trip | catalog (`pg_class.relispopulated = false`) |
| M-PG-MV-03 | matview over a matview (refresh order) | 10 | `40_matviews.sql` | round-trip | schema diff + row digest |
| M-PG-MV-04 | matview with a UNIQUE index and a non-unique index | 10 | `40_matviews.sql` | round-trip | schema diff + idx def |
| M-PG-MV-05 | matview with `reloptions` | 10 | `40_matviews.sql` | round-trip | schema diff |
| M-PG-MV-06 | matview over a view | 10 | `40_matviews.sql` | round-trip | schema diff + row digest |
| M-PG-MV-07 | matview with a `COMMENT` (regression of commit `3723036`) | 10 | `40_matviews.sql` | round-trip | schema diff |

## IDX — indexes

| ID | Case | Min major | Fixture file | Expected | Oracle |
|----|------|-----------|--------------|----------|--------|
| M-PG-IDX-01 | btree | 10 | `50_indexes.sql` | round-trip | idx def |
| M-PG-IDX-02 | unique | 10 | `50_indexes.sql` | round-trip | idx def |
| M-PG-IDX-03 | partial (`WHERE`) | 10 | `50_indexes.sql` | round-trip | idx def |
| M-PG-IDX-04 | expression (`lower(col)`) | 10 | `50_indexes.sql` | round-trip | idx def |
| M-PG-IDX-05 | multi-column `DESC NULLS FIRST` | 10 | `50_indexes.sql` | round-trip | idx def |
| M-PG-IDX-06 | `INCLUDE` covering index | 11 | `50_indexes.ge11.sql` | round-trip | idx def |
| M-PG-IDX-07 | GIN on jsonb | 10 | `50_indexes.sql` | round-trip | idx def |
| M-PG-IDX-08 | GiST on a range column | 10 | `50_indexes.sql` | round-trip | idx def |
| M-PG-IDX-09 | hash | 10 | `50_indexes.sql` | round-trip | idx def |
| M-PG-IDX-10 | BRIN | 10 | `50_indexes.sql` | round-trip | idx def |
| M-PG-IDX-11 | opclass `text_pattern_ops` | 10 | `50_indexes.sql` | round-trip | idx def |
| M-PG-IDX-12 | storage parameter `fillfactor=50` | 10 | `50_indexes.sql` | round-trip | idx def |
| M-PG-IDX-13 | partitioned index attached to its partitions | 11 | `50_indexes.ge11.sql` | round-trip | idx def + catalog (`pg_inherits` over `pg_index`) |
| M-PG-IDX-14 | index on a matview | 10 | `50_indexes.sql` | round-trip | idx def |
| M-PG-IDX-15 | index with `COLLATE "C"` | 10 | `50_indexes.sql` | round-trip | idx def |
| M-PG-IDX-16 | quoted index name | 10 | `50_indexes.sql` | round-trip | idx def |
| M-PG-IDX-17 | `NULLS NOT DISTINCT` unique index | 15 | `50_indexes.ge15.sql` | round-trip | idx def |
| M-PG-IDX-18 | index with a `COMMENT` | 10 | `50_indexes.sql` | round-trip | schema diff |

## CON — constraints

| ID | Case | Min major | Fixture file | Expected | Oracle |
|----|------|-----------|--------------|----------|--------|
| M-PG-CON-01 | single-column PRIMARY KEY | 10 | `60_constraints.sql` | round-trip | con state |
| M-PG-CON-02 | composite PRIMARY KEY | 10 | `60_constraints.sql` | round-trip | con state |
| M-PG-CON-03 | UNIQUE | 10 | `60_constraints.sql` | round-trip | con state |
| M-PG-CON-04 | FK `ON DELETE CASCADE ON UPDATE SET NULL` | 10 | `60_constraints.sql` | round-trip | con state |
| M-PG-CON-05 | FK `DEFERRABLE INITIALLY DEFERRED` | 10 | `60_constraints.sql` | round-trip | con state |
| M-PG-CON-06 | FK `NOT VALID` with a violating row present, must stay NOT VALID and the row must round-trip | 10 | `60_constraints.sql` | round-trip | con state + row digest |
| M-PG-CON-07 | FK referencing a partitioned table | 12 | `60_constraints.ge12.sql` | round-trip | con state |
| M-PG-CON-08 | FK from a partitioned table | 11 | `60_constraints.ge11.sql` | round-trip | con state |
| M-PG-CON-09 | self-referencing FK | 10 | `60_constraints.sql` | round-trip | con state |
| M-PG-CON-10 | composite FK | 10 | `60_constraints.sql` | round-trip | con state |
| M-PG-CON-11 | CHECK | 10 | `60_constraints.sql` | round-trip | con state |
| M-PG-CON-12 | CHECK `NOT VALID` | 10 | `60_constraints.sql` | round-trip | con state |
| M-PG-CON-13 | CHECK `NO INHERIT` | 10 | `60_constraints.sql` | round-trip | con state |
| M-PG-CON-14 | EXCLUDE USING gist (requires `btree_gist`, created by this fixture) | 10 | `60_constraints.sql` | round-trip | con state |
| M-PG-CON-15 | constraint `COMMENT` | 10 | `60_constraints.sql` | round-trip | schema diff |
| M-PG-CON-16 | constraint inherited on a partition (`conislocal = false`), must not be re-added | 11 | `60_constraints.ge11.sql` | round-trip | catalog (`pg_constraint.conislocal`, `coninhcount`) |
| M-PG-CON-17 | plain NOT NULL as a catalog row (`contype = 'n'`) | 18 | `60_constraints.ge18.sql` | round-trip | catalog (`pg_constraint.contype = 'n'`) |
| M-PG-CON-18 | UNIQUE constraint `NULLS NOT DISTINCT` | 15 | `60_constraints.ge15.sql` | round-trip | con state |

## EXT — extensions

| ID | Case | Min major | Fixture file | Expected | Oracle |
|----|------|-----------|--------------|----------|--------|
| M-PG-EXT-01 | `pg_trgm` with a GIN trigram index | 10 | `70_extensions.sql` | round-trip | schema diff + idx def |
| M-PG-EXT-02 | `btree_gist` (used by CON-14) | 10 | `60_constraints.sql` | round-trip | catalog (`pg_extension`) |
| M-PG-EXT-03 | `hstore` with an hstore column and a GIN index | 10 | `70_extensions.sql` | round-trip | schema diff + row digest |
| M-PG-EXT-04 | `citext` column | 10 | `70_extensions.sql` | round-trip | schema diff + row digest |
| M-PG-EXT-05 | `uuid-ossp` with a `uuid_generate_v4()` column default | 10 | `70_extensions.sql` | round-trip | schema diff |
| M-PG-EXT-06 | extension installed in a non-public schema (`CREATE EXTENSION tablefunc SCHEMA ext`; `hstore` already occupies `public` for EXT-03) | 10 | `70_extensions.sql` | round-trip | catalog (`pg_extension.extnamespace`) |
| M-PG-EXT-07 | custom extension `rbtest` with a config table registered by `pg_extension_config_dump`, custom row matching the condition | 10 | `71_rbtest_extension.sh`, `70_extensions.sql` | round-trip | row digest of `rbtest_cfg` (k = 1000 present, seeded rows not duplicated) |
| M-PG-EXT-08 | extension whose exact version is missing on the destination | 10 | `71_rbtest_extension.sh 1.1` on the destination | refused before transfer without `--extension-version default`; round-trip with it | refusal, then catalog (`pg_extension.extversion`) plus a logged deviation line |

## GIS — PostGIS

Only run under `RB_PG_IMAGE_REPO=postgis/postgis`; otherwise every row prints
`SKIP`. The image tag per major is resolved by the runner (`postgis_tag`) and
checked with `docker manifest inspect`; a major with no resolvable PostGIS tag
prints `SKIP` for the whole group. Tags resolved on 2026-09-16 (R5):

| major | 10 | 11 | 12 | 13 | 14 | 15 | 16 | 17 | 18 |
|-------|----|----|----|----|----|----|----|----|----|
| tag | `10-2.5` | `11-3.3` | `12-3.4` | `13-3.5` | `14-3.5` | `15-3.5` | `16-3.5` | `17-3.5` | `18-3.6` |

A cross-major PostGIS pair ships two different PostGIS versions, so the whole
case runs with `--extension-version default`; M-PG-GIS-06 asserts both halves of
that policy (refused without the flag, restored with it and the substitution
reported as a `deviation:` line).

| ID | Case | Min major | Fixture file | Expected | Oracle |
|----|------|-----------|--------------|----------|--------|
| M-PG-GIS-01 | `CREATE EXTENSION postgis` | 10 | `postgis/80_postgis.sql` | round-trip | catalog (`pg_extension`) |
| M-PG-GIS-02 | table with `geometry(Point,4326)` and `geography` columns and rows | 10 | `postgis/80_postgis.sql` | round-trip | schema diff + row digest |
| M-PG-GIS-03 | GiST index on the geometry column | 10 | `postgis/80_postgis.sql` | round-trip | idx def |
| M-PG-GIS-04 | custom `spatial_ref_sys` row (srid 990001) | 10 | `postgis/80_postgis.sql` | round-trip | row digest of `spatial_ref_sys` restricted to the extension config condition |
| M-PG-GIS-05 | view using `ST_AsText` | 10 | `postgis/80_postgis.sql` | round-trip | schema diff |
| M-PG-GIS-06 | cross-major pair `12:16`, extension version differs | 12 | `postgis/80_postgis.sql` | refused before transfer without `--extension-version default`; round-trip with it | refusal, then schema diff |

## REF — refusals

Each row is loaded alone into its own scratch database on top of one plain
table, and the run must fail before any byte is transferred.

| ID | Case | Min major | Fixture file | Expected | Oracle |
|----|------|-----------|--------------|----------|--------|
| M-PG-REF-01 | enum type | 10 | `refusals/enum.sql` | refused before transfer | refusal |
| M-PG-REF-02 | domain | 10 | `refusals/domain.sql` | refused before transfer | refusal |
| M-PG-REF-03 | composite type | 10 | `refusals/composite_type.sql` | refused before transfer | refusal |
| M-PG-REF-04 | trigger | 10 | `refusals/trigger.sql` | refused before transfer | refusal |
| M-PG-REF-05 | RLS policy | 10 | `refusals/rls_policy.sql` | refused before transfer | refusal |
| M-PG-REF-06 | rule on a table (non-view) | 10 | `refusals/rule.sql` | refused before transfer | refusal |
| M-PG-REF-07 | large object | 10 | `refusals/large_object.sql` | refused before transfer | refusal |
| M-PG-REF-08 | user-defined collation | 10 | `refusals/collation.sql` | refused before transfer | refusal |
| M-PG-REF-09 | event trigger | 10 | `refusals/event_trigger.sql` | refused before transfer | refusal |
| M-PG-REF-10 | foreign table (`postgres_fdw`) | 10 | `refusals/foreign_table.sql` | refused before transfer | refusal |
| M-PG-REF-11 | user-defined aggregate | 10 | `refusals/aggregate.sql` | refused before transfer | refusal |
| M-PG-REF-12 | column-level GRANT | 10 | `refusals/column_grant.sql` | refused before transfer | refusal |
| M-PG-REF-13 | `ALTER DEFAULT PRIVILEGES` | 10 | `refusals/default_privileges.sql` | refused before transfer | refusal |
| M-PG-REF-14 | publication | 10 | `refusals/publication.sql` | refused before transfer | refusal |
| M-PG-REF-15 | named NOT NULL constraint | 18 | `refusals/named_not_null.ge18.sql` | refused before transfer | refusal |
