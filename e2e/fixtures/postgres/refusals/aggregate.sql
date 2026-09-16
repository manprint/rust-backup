-- M-PG-REF-11: a user-defined aggregate.
CREATE TABLE ref_base (id integer, payload text);
INSERT INTO ref_base SELECT g, 'r-' || g FROM generate_series(1, 3) g;
CREATE FUNCTION ref_add(integer, integer) RETURNS integer LANGUAGE sql IMMUTABLE
  AS $fn$ SELECT $1 + $2 $fn$;
CREATE AGGREGATE ref_total (integer) (SFUNC = ref_add, STYPE = integer, INITCOND = '0');
