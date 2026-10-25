-- Singleton-row cursor for the discovery pass's walk through the .ton DNS
-- collection contract's own transaction history (every domain's auction
-- start is a transaction the collection sends -- see discovery.rs). `lt`
-- (logical time) is TON's native per-account ordering key and supports
-- true keyset pagination (`start_lt`/`end_lt` range filters), unlike the
-- old /nft/items OFFSET-based approach this replaces, which got
-- unreliably slow at depth. Since this cursor only ever advances forward
-- (no daily full-rescan to catch a missed range), a page that fails after
-- retries must leave it where it is rather than skip past it. The `id`
-- CHECK enforces exactly one row.
CREATE TABLE discovery_progress (
    id          BOOLEAN PRIMARY KEY DEFAULT true CHECK (id),
    next_lt     BIGINT NOT NULL DEFAULT 0,
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

INSERT INTO discovery_progress (id, next_lt) VALUES (true, 0);
