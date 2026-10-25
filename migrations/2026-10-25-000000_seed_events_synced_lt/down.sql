-- Best-effort revert -- resets any domain whose events_synced_lt matches
-- what this migration would have set it to, back to 0. Not perfectly
-- reversible if events::run has since advanced some of them further, but
-- migrations are rarely rolled back in practice and that's a harmless
-- direction to be wrong in (just means a few domains get re-checked for
-- events once).
UPDATE domains
SET events_synced_lt = 0
FROM domain_events de
WHERE de.domain_address = domains.address
  AND de.kind = 'registered'
  AND domains.events_synced_lt = de.lt + 1;
