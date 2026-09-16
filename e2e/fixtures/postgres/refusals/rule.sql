-- M-PG-REF-06: a rule on a table (a view's own _RETURN rule is supported).
CREATE TABLE ref_base (id integer, payload text);
INSERT INTO ref_base SELECT g, 'r-' || g FROM generate_series(1, 3) g;
CREATE RULE ref_no_insert AS ON INSERT TO ref_base DO INSTEAD NOTHING;
