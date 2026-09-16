# PostgreSQL matrix fixtures

One file per object group of `docs/testing/POSTGRES_MATRIX.md`. The runner
`e2e/postgres_matrix.sh` loads them with `rb_pg_load_fixtures` (`e2e/lib.sh`).

## Naming rule

```
NN_<kind>[.ge<major>].sql
```

- `NN` is a two-digit load order. Files are loaded in lexical order, so a group
  that depends on another must sort after it: roles (`00`) before tables (`10`),
  tables before sequences (`20`), views (`30`) and materialized views (`40`),
  indexes (`50`) after the relations they index, constraints (`60`) after the
  indexes, extensions (`70`) last, PostGIS (`80`) last of all.
- `<kind>` names the object group: `roles`, `tables`, `sequences`, `views`,
  `matviews`, `indexes`, `constraints`, `extensions`, `postgis`.
- `.ge<major>` is optional and gates the file: it is loaded only when the server
  major is greater than or equal to `<major>`. Below it the runner prints
  `SKIP <file> (needs >= <major>)` and every row that lives in that file prints
  `SKIP` too. Example: `10_tables.ge11.sql` holds the partitioning rows.

Every file must load with `psql -v ON_ERROR_STOP=1` on every major it claims to
support, and must be idempotent across a fresh database only — the runner always
loads into a newly created database, never into a dirty one.

Objects are named after their matrix row so a failure names the case:
`mx.t_tab_03`, `mx.s_seq_01`, `mx.v_view_02`, `mx.m_mv_04`, `i_idx_07`,
`c_con_11`. Schema `mx` holds everything except the quoted-identifier row
(`"Mixed Schema"."Weird Table"`, TAB-14) and the non-public extension schema
(`ext`, EXT-06).

## `refusals/`

One file per `M-PG-REF-*` row, each loaded **alone** into its own scratch
database on top of one plain table, because each of them makes the whole run
fail before transfer. The same `.ge<major>` gate applies
(`named_not_null.ge18.sql`).

## `oracle_ignore.txt`

Extended regular expressions, one per line, `#` comments allowed. Every line of
the `pg_dump --schema-only` oracle output matching one of them is dropped before
the source and destination dumps are compared. Use it only for rendering
differences that are not the property under test, and state the reason in a
comment above the pattern.

## `71_rbtest_extension.sh`

Not loaded by `rb_pg_load_fixtures`: it installs the files of the custom
extension `rbtest` into a running container (`pg_config --sharedir`) before
`70_extensions.sql` runs `CREATE EXTENSION rbtest`.
