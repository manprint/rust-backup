-- M-PG-CON-17: on PostgreSQL 18 every NOT NULL is a pg_constraint row
-- (contype = 'n'). The auto-named, validated shape must round-trip; a named or
-- NOT VALID one is refused and lives in refusals/named_not_null.ge18.sql.
CREATE TABLE mx.t_con_17 (id integer NOT NULL, payload text NOT NULL);
INSERT INTO mx.t_con_17 SELECT g, 'nn-' || g FROM generate_series(1, 4) g;

-- The server keeps the `_not_null` label whole and truncates the table and
-- column names to fit 63 bytes. A probe that cut the label instead refused
-- these default-named constraints as "named": `id` fills exactly 63 bytes,
-- `idx` needs one byte of the table name cut, and the long column cuts both.
CREATE TABLE mx.t_con_17_a_table_name_well_past_half_of_namedatalen (
  id integer NOT NULL,
  idx integer NOT NULL,
  a_column_name_that_is_long_enough_too text NOT NULL
);
INSERT INTO mx.t_con_17_a_table_name_well_past_half_of_namedatalen
  SELECT g, g, 'long-' || g FROM generate_series(1, 3) g;
