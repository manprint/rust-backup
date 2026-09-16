-- M-PG-REF-15: on PostgreSQL 18 a NOT NULL constraint can carry an operator
-- chosen name, which this build cannot reproduce (it rebuilds the server's
-- default name).
CREATE TABLE ref_base (id integer, payload text);
INSERT INTO ref_base SELECT g, 'r-' || g FROM generate_series(1, 3) g;
ALTER TABLE ref_base ADD CONSTRAINT ref_named_not_null NOT NULL id;
