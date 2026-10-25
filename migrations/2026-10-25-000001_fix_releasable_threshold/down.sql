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
        WHEN now() - d.last_fill_up_time > interval '1 year'
            THEN 'releasable'
        ELSE 'active'
    END AS status
FROM domains d;
