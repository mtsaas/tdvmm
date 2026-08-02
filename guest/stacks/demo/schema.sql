-- deterministic-vmm demo stack: Postgres first-start schema.
--
-- Baked into the guest and bind-mounted read-only into the postgres container at
-- /docker-entrypoint-initdb.d/10-schema.sql, so the official postgres entrypoint
-- runs it once on first start against POSTGRES_DB (appdb).
--
-- Two tables model a tiny order pipeline:
--   orders     — one row per order the api ingests (via gRPC from the client).
--   summaries  — one row per virtual hour the worker rolls up (incremental).
CREATE TABLE IF NOT EXISTS orders (
    id     bigserial   PRIMARY KEY,
    ts     timestamptz NOT NULL DEFAULT now(),
    amount int         NOT NULL
);

CREATE TABLE IF NOT EXISTS summaries (
    id           bigserial   PRIMARY KEY,
    hour         int         NOT NULL,
    order_count  int         NOT NULL,
    total_amount bigint      NOT NULL,
    ts           timestamptz NOT NULL DEFAULT now()
);
