-- PostgreSQL 16 grant options. A plain GRANT inherits, so a grant made WITH
-- INHERIT FALSE, SET FALSE must keep both options through the restore: lost,
-- the copy would hold wider privileges than the source.
--
-- Re-loaded into every scratch database of the same container, like
-- 00_roles.sql: re-granting an existing membership only restates its options.
DO $$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_catalog.pg_roles WHERE rolname = 'mx_auditor') THEN
    CREATE ROLE mx_auditor NOLOGIN;
  END IF;
END
$$;
GRANT mx_writer TO mx_auditor WITH INHERIT FALSE, SET FALSE;
