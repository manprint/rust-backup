#!/usr/bin/env bash
# T-PG-INTROSPECT (plan Phase 2.2): spin a Docker PostgreSQL, seed a schema that
# exercises the introspection surface (roles, schema, table w/ columns/defaults/
# identity/constraints/indexes, sequence, view, function, extension, comments),
# then run the gated live introspection test against it.
#
# Usage:  bash e2e/postgres_introspect.sh [PG_MAJOR]   (default 16)
# Needs:  docker, cargo. No sudo. Exits non-zero on any failure.
set -euo pipefail
cd "$(dirname "$0")/.."

PG_MAJOR="${1:-16}"
CONTAINER="rb-pg-introspect-$$"
PORT=55432
PASSWORD="rbpg"

cleanup() { docker rm -f "$CONTAINER" >/dev/null 2>&1 || true; }
trap cleanup EXIT INT TERM

echo "==> starting postgres:${PG_MAJOR}-alpine on :${PORT}"
docker run -d --name "$CONTAINER" \
  -e POSTGRES_PASSWORD="$PASSWORD" \
  -p "${PORT}:5432" \
  "postgres:${PG_MAJOR}-alpine" >/dev/null

echo "==> waiting for readiness"
for _ in $(seq 1 30); do
  if docker exec "$CONTAINER" pg_isready -U postgres >/dev/null 2>&1; then break; fi
  sleep 1
done
docker exec "$CONTAINER" pg_isready -U postgres

echo "==> seeding fixture schema"
docker exec -i "$CONTAINER" psql -U postgres -v ON_ERROR_STOP=1 <<'SQL'
CREATE ROLE app_owner LOGIN PASSWORD 'x';
CREATE ROLE readers;
GRANT readers TO app_owner;
CREATE DATABASE appdb OWNER app_owner;
\connect appdb
CREATE EXTENSION IF NOT EXISTS pgcrypto;
CREATE SCHEMA app AUTHORIZATION app_owner;
COMMENT ON SCHEMA app IS 'application schema';
CREATE TABLE app.accounts (
  id     bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  email  text NOT NULL,
  status text NOT NULL DEFAULT 'active',
  UNIQUE (email)
) WITH (fillfactor = 80);
COMMENT ON TABLE app.accounts IS 'accounts';
COMMENT ON COLUMN app.accounts.id IS 'pk';
CREATE INDEX accounts_status_idx ON app.accounts (status);
CREATE TABLE app.orders (
  id       bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  acct     bigint NOT NULL REFERENCES app.accounts (id),
  total    numeric(12,2) NOT NULL
);
CREATE SEQUENCE app.ticket_seq START 100 INCREMENT 5;
CREATE VIEW app.active_accounts AS SELECT id, email FROM app.accounts WHERE status = 'active';
CREATE FUNCTION app.touch() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$;
INSERT INTO app.accounts (email) VALUES ('a@x'), ('b@x');
SQL

echo "==> running gated introspection test"
RUST_BACKUP_PG_HOST=127.0.0.1 \
RUST_BACKUP_PG_PORT="$PORT" \
RUST_BACKUP_PG_USER=postgres \
RUST_BACKUP_PG_PASSWORD="$PASSWORD" \
RUST_BACKUP_PG_DATABASE=appdb \
  cargo test -p rb-postgres --test introspect_live -- --nocapture

echo "==> PASS: postgres ${PG_MAJOR} introspection"
