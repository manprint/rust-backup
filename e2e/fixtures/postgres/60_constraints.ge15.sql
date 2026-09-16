-- M-PG-CON-18: a unique constraint that treats NULLs as equal (PostgreSQL 15+).
CREATE TABLE mx.t_con_18 (id integer, tag text);
ALTER TABLE mx.t_con_18 ADD CONSTRAINT c_con_18 UNIQUE NULLS NOT DISTINCT (tag);
INSERT INTO mx.t_con_18 VALUES (1, 'x'), (2, NULL), (3, 'y');
