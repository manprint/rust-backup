-- M-PG-SEQ-01 .. 06. SEQ-07 is the identity sequence of mx.t_tab_08
-- (10_tables.sql); this file only depends on schema mx and mx.t_tab_12.

-- SEQ-01: every non-default option at once.
CREATE SEQUENCE mx.s_seq_01 INCREMENT 5 MINVALUE 10 MAXVALUE 1000 CACHE 20 CYCLE;
SELECT nextval('mx.s_seq_01');
SELECT nextval('mx.s_seq_01');

-- SEQ-02: a narrow sequence type.
CREATE SEQUENCE mx.s_seq_02 AS smallint INCREMENT 1 MINVALUE 1 MAXVALUE 32767;
SELECT nextval('mx.s_seq_02');

-- SEQ-03: owned by a column, so it is dropped with that column.
CREATE SEQUENCE mx.s_seq_03 OWNED BY mx.t_tab_12.id;
SELECT nextval('mx.s_seq_03');

-- SEQ-04: never called; last_value is the start value and is_called is false.
CREATE SEQUENCE mx.s_seq_04 START 42;

-- SEQ-05: parked on its maximum, one nextval away from either an error or a
-- wrap-around.
CREATE SEQUENCE mx.s_seq_05 MINVALUE 1 MAXVALUE 100;
SELECT setval('mx.s_seq_05', 100);

-- SEQ-06: descending.
CREATE SEQUENCE mx.s_seq_06 INCREMENT -3 MINVALUE -500 MAXVALUE -1 START -10;
SELECT nextval('mx.s_seq_06');
SELECT nextval('mx.s_seq_06');
