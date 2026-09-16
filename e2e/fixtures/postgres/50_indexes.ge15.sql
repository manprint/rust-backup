-- M-PG-IDX-17. Loaded before 50_indexes.sql.
CREATE TABLE mx.t_idx_17 (id integer, tag text);
INSERT INTO mx.t_idx_17 VALUES (1, 'a'), (2, NULL), (3, 'b');
CREATE UNIQUE INDEX i_idx_17 ON mx.t_idx_17 (tag) NULLS NOT DISTINCT;
