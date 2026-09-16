-- M-PG-REF-07: a large object, which lives outside any table.
CREATE TABLE ref_base (id integer, payload text);
INSERT INTO ref_base SELECT g, 'r-' || g FROM generate_series(1, 3) g;
SELECT lo_from_bytea(0, '\x6c61726765206f626a656374'::bytea);
