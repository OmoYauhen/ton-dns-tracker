-- One-time backfill for domains discovered before discovery.rs started
-- seeding events_synced_lt from the deploy transaction's own lt (they're
-- stuck at the column default of 0). Without this, events::run's trigger
-- condition (last_transaction_lt >= events_synced_lt) matches every one
-- of them on its very first check regardless of whether anything has
-- actually happened since registration -- at ~200k domains, a needless
-- history-fetch call for the (likely overwhelming majority) that only
-- ever had their one deploy transaction doesn't scale.
--
-- Each domain's `registered` domain_events row already has the deploy
-- transaction's lt recorded (see discovery.rs), so this is exactly the
-- same value discovery now seeds directly -- just applied retroactively.
-- Safe regardless of whether a domain has had real activity since: if
-- last_transaction_lt is already past this value, the events::run
-- condition still correctly matches and picks it up for a real sync.
UPDATE domains
SET events_synced_lt = de.lt + 1
FROM domain_events de
WHERE de.domain_address = domains.address
  AND de.kind = 'registered'
  AND domains.events_synced_lt = 0;
