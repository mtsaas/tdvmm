-- dvmm corpus (webstack): Postgres first-start schema.
--
-- Baked into the guest and bind-mounted read-only into the postgres container at
-- /docker-entrypoint-initdb.d/10-schema.sql, so the official postgres entrypoint
-- runs it once on first start against POSTGRES_DB (appdb). The data dir is fresh
-- (tmpfs) every boot, so this runs every boot -- by design (closed world).
CREATE TABLE IF NOT EXISTS events (
    id    bigserial   PRIMARY KEY,
    ts    timestamptz NOT NULL DEFAULT now(),
    value text
);
