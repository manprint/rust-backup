-- M-PG-REF-04: a user trigger.
CREATE TABLE ref_base (id integer, payload text);
INSERT INTO ref_base SELECT g, 'r-' || g FROM generate_series(1, 3) g;
CREATE FUNCTION ref_touch() RETURNS trigger LANGUAGE plpgsql
  AS $fn$ BEGIN RETURN NEW; END $fn$;
CREATE TRIGGER ref_touch_trigger BEFORE INSERT ON ref_base
  FOR EACH ROW EXECUTE PROCEDURE ref_touch();
