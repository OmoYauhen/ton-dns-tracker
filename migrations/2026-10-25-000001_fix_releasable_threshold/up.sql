-- `interval '1 year'` was the wrong threshold twice over:
--
-- * nft-item.fc's dns_balance_release checks
--   `now() - last_fill_up_time > one_year`, where dns-utils.fc defines
--   one_year = 31622400 -- 366 days, not 365.
-- * Comparing `now() - last_fill_up_time` (a days+time interval) against
--   '1 year' (a months interval) makes Postgres normalize months to 30
--   days, so '1 year' compared as 360 days.
--
-- Net effect: domains showed as `releasable` 6 days before the contract
-- would actually accept dns_balance_release. Use the contract's constant
-- in plain seconds so no month/year normalization is involved.
--
-- Columns are listed explicitly rather than `d.*`: the original view
-- froze `d.*` at creation, before domains.events_synced_lt existed, and
-- CREATE OR REPLACE VIEW can't change a view's column list. Keep in sync
-- with domain_live_status in crates/core/src/schema.rs.
CREATE OR REPLACE VIEW domain_live_status AS
SELECT
    d.address,
    d.item_index,
    d.name,
    d.namespace,
    d.owner_address,
    d.last_fill_up_time,
    d.auction_max_bid_address,
    d.auction_max_bid_amount,
    d.auction_end_time,
    d.wallet_address,
    d.site_adnl,
    d.storage_bag_id,
    d.next_resolver,
    d.extra_records,
    d.code_hash,
    d.last_transaction_lt,
    d.updated_at,
    CASE
        WHEN d.auction_end_time IS NOT NULL AND now() <= d.auction_end_time
            THEN 'in_auction'
        WHEN d.auction_end_time IS NOT NULL AND now() > d.auction_end_time
            THEN 'auction_ended_unsettled'
        WHEN now() - d.last_fill_up_time > interval '31622400 seconds'
            THEN 'releasable'
        ELSE 'active'
    END AS status
FROM domains d;
