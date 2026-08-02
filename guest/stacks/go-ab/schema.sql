-- deterministic-vmm Phase-2b Go A/B stack: Postgres first-start schema.
--
-- IDENTICAL to the dogfood stack's schema (guest/stacks/dogfood/schema.sql).
-- Bind-mounted read-only into the postgres container at
-- /docker-entrypoint-initdb.d/10-schema.sql, so the official postgres entrypoint
-- runs it once on first start against POSTGRES_DB (appdb). The data dir is fresh
-- (tmpfs) every boot, so this runs every boot -- by design.
--
-- Table shape is exactly the locked 2b spec: bigserial primary key, a
-- default-now timestamp, plus a value column. The Go service and the shell
-- service write to the same schema, so the ONLY variable between the two stacks
-- is the language runtime.
CREATE TABLE IF NOT EXISTS events (
    id    bigserial   PRIMARY KEY,
    ts    timestamptz NOT NULL DEFAULT now(),
    value text
);
