-- tdvmm corpus (svcchain): Postgres first-start schema.
--
-- Bind-mounted read-only into the db container at
-- /docker-entrypoint-initdb.d/10-schema.sql; the official postgres entrypoint
-- runs it once on first start against POSTGRES_DB (appdb). Fresh (tmpfs) data
-- dir every boot, so this runs every boot -- by design (closed world).
CREATE TABLE IF NOT EXISTS events (
    id    bigserial   PRIMARY KEY,
    ts    timestamptz NOT NULL DEFAULT now(),
    value text
);
