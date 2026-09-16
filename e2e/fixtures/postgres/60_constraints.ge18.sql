-- M-PG-CON-17: on PostgreSQL 18 every NOT NULL is a pg_constraint row
-- (contype = 'n'). The auto-named, validated shape must round-trip; a named or
-- NOT VALID one is refused and lives in refusals/named_not_null.ge18.sql.
CREATE TABLE mx.t_con_17 (id integer NOT NULL, payload text NOT NULL);
INSERT INTO mx.t_con_17 SELECT g, 'nn-' || g FROM generate_series(1, 4) g;
