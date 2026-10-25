-- Append-only log of on-chain actions for each domain: bids, top-ups
-- (renewals), ownership changes, and expired-domain re-registrations.
-- Populated by walking each item's OWN transaction history (see
-- ton-dns-indexer::events) -- NOT the collection's, since bids/renewals/
-- transfers are all sent directly to the item contract once it exists
-- (see nft-item.fc). `registered` events (exactly one per domain) come
-- from the discovery pass instead, which reads the collection contract's
-- history, the only place a domain's initial auction-start is visible.
--
-- `lt` is the triggering transaction's logical time -- TON's native
-- per-account ordering key. Unique per (domain, lt, kind) rather than
-- just (domain, lt) so re-running a sync pass is idempotent while still
-- allowing the rare case where one transaction legitimately produces two
-- distinct events -- e.g. an auction settling and, in that same message,
-- the newly-settled owner immediately sending a transfer.
CREATE TABLE domain_events (
    id              BIGSERIAL PRIMARY KEY,
    domain_address  BYTEA NOT NULL REFERENCES domains (address) ON DELETE CASCADE,
    lt              BIGINT NOT NULL,
    event_time      TIMESTAMPTZ NOT NULL,
    -- 'registered' | 'bid' | 'top_up' | 'ownership_change' | 're_registered'
    kind            TEXT NOT NULL,
    actor_address   BYTEA,
    amount          BIGINT,
    details         JSONB NOT NULL DEFAULT '{}',

    UNIQUE (domain_address, lt, kind)
);

CREATE INDEX domain_events_domain_address_lt_idx ON domain_events (domain_address, lt);

-- Per-domain cursor for the event sync pass -- same lt-based keyset
-- pagination reasoning as discovery_progress, just one per domain instead
-- of a single global row.
ALTER TABLE domains ADD COLUMN events_synced_lt BIGINT NOT NULL DEFAULT 0;
