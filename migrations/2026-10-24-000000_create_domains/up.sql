-- Addresses are stored as the 33 raw bytes (1 byte workchain + 32 byte
-- account hash) rather than text. See ton-dns-core::ton_address for the
-- raw/friendly string conversions done in the application layer.
--
-- `domains` holds only facts read directly off-chain-indexed-by-toncenter
-- or decoded from the NFT item's data cell (see ton-dns-core::cell).
-- Nothing here is a cached "is this expired" judgement -- that depends on
-- wall-clock time, not on the last time we synced, so it's computed in
-- the `domain_live_status` view below instead of stored.
CREATE TABLE domains (
    address                  BYTEA PRIMARY KEY,
    -- The NFT item's on-chain `uint256 index` (see ton-dns-core::cell) --
    -- for the .ton collection this is a hash of the domain name, not a
    -- small sequential counter, so it doesn't fit in BIGINT. NUMERIC(78,0)
    -- covers the full 2^256 range (up to 78 decimal digits, 0 fractional).
    item_index                NUMERIC(78, 0) NOT NULL,
    name                     TEXT NOT NULL CHECK (char_length(name) <= 126),
    namespace                TEXT NOT NULL DEFAULT 'ton',

    owner_address            BYTEA,

    last_fill_up_time        TIMESTAMPTZ NOT NULL,

    auction_max_bid_address  BYTEA,
    auction_max_bid_amount   BIGINT,
    auction_end_time         TIMESTAMPTZ,

    wallet_address           BYTEA,
    site_adnl                BYTEA,
    storage_bag_id           BYTEA,
    next_resolver            BYTEA,
    extra_records            JSONB,

    code_hash                BYTEA NOT NULL,
    last_transaction_lt      BIGINT NOT NULL,

    updated_at               TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX domains_item_index_namespace_key ON domains (namespace, item_index);
CREATE INDEX domains_name_idx ON domains (name);
CREATE INDEX domains_owner_address_idx ON domains (owner_address);
CREATE INDEX domains_last_fill_up_time_idx ON domains (last_fill_up_time);
CREATE INDEX domains_auction_end_time_idx ON domains (auction_end_time) WHERE auction_end_time IS NOT NULL;

CREATE FUNCTION set_updated_at() RETURNS trigger AS $$
BEGIN
    NEW.updated_at = now();
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER domains_set_updated_at
    BEFORE UPDATE ON domains
    FOR EACH ROW
    EXECUTE FUNCTION set_updated_at();

-- `status` is derived at read time from last_fill_up_time/auction_end_time
-- compared against now(), per the dns_balance_release logic in
-- nft-item.fc: a domain becomes `releasable` the instant one year passes
-- since its last fill-up, with no grace period -- that transition isn't
-- a blockchain event we can catch, so it can't live in a stored column.
CREATE VIEW domain_live_status AS
SELECT
    d.*,
    CASE
        WHEN d.auction_end_time IS NOT NULL AND now() <= d.auction_end_time
            THEN 'in_auction'
        WHEN d.auction_end_time IS NOT NULL AND now() > d.auction_end_time
            THEN 'auction_ended_unsettled'
        WHEN now() - d.last_fill_up_time > interval '1 year'
            THEN 'releasable'
        ELSE 'active'
    END AS status
FROM domains d;
