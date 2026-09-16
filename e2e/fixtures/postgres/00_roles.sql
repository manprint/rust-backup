-- Roles and schemas every later fixture file depends on.
--
-- Loaded first, and re-loaded into every scratch database of the same
-- container: roles are cluster-wide, so each CREATE ROLE is guarded.
DO $$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_catalog.pg_roles WHERE rolname = 'mx_owner') THEN
    CREATE ROLE mx_owner NOLOGIN;
  END IF;
  IF NOT EXISTS (SELECT 1 FROM pg_catalog.pg_roles WHERE rolname = 'mx_reader') THEN
    CREATE ROLE mx_reader NOLOGIN;
  END IF;
  IF NOT EXISTS (SELECT 1 FROM pg_catalog.pg_roles WHERE rolname = 'mx_writer') THEN
    CREATE ROLE mx_writer NOLOGIN;
  END IF;
END
$$;

CREATE SCHEMA mx;
-- M-PG-TAB-14 lives in a quoted mixed-case schema.
CREATE SCHEMA "Mixed Schema";
-- M-PG-EXT-06 installs an extension outside public.
CREATE SCHEMA ext;

GRANT USAGE ON SCHEMA mx TO mx_reader, mx_writer;

-- Legacy coverage kept from the inline fixture the runner used before the
-- matrix: a login role whose custom GUC value is SQL-shaped (it must travel as
-- data, never as a second statement), a role membership, and the schema the
-- legacy objects live in.
DO $$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_catalog.pg_roles WHERE rolname = 'app_owner') THEN
    CREATE ROLE app_owner LOGIN PASSWORD 'x';
  END IF;
  IF NOT EXISTS (SELECT 1 FROM pg_catalog.pg_roles WHERE rolname = 'readers') THEN
    CREATE ROLE readers;
  END IF;
END
$$;
GRANT readers TO app_owner;
ALTER ROLE app_owner SET "myapp.k" = '1; ALTER ROLE app_owner SUPERUSER';
ALTER ROLE app_owner SET statement_timeout = '5min';

CREATE SCHEMA app AUTHORIZATION app_owner;
