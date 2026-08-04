-- dvmm Step 2b: Postgres first-start schema.
--
-- Baked into the guest and bind-mounted read-only into the postgres container
-- at /docker-entrypoint-initdb.d/10-schema.sql, so the official postgres
-- entrypoint runs it ONCE, on first start, against POSTGRES_DB (appdb). The
-- data dir is fresh (tmpfs) every boot, so this runs every boot -- by design.
--
-- Table shape is exactly the locked 2b spec: bigserial primary key, a
-- default-now timestamp, plus a value column.
CREATE TABLE IF NOT EXISTS events (
    id    bigserial   PRIMARY KEY,
    ts    timestamptz NOT NULL DEFAULT now(),
    value text
);
