-- M-PG-REF-09: an event trigger, which is cluster-wide behaviour.
CREATE TABLE ref_base (id integer, payload text);
INSERT INTO ref_base SELECT g, 'r-' || g FROM generate_series(1, 3) g;
CREATE FUNCTION ref_event() RETURNS event_trigger LANGUAGE plpgsql
  AS $fn$ BEGIN END $fn$;
CREATE EVENT TRIGGER ref_event_trigger ON ddl_command_end EXECUTE PROCEDURE ref_event();
