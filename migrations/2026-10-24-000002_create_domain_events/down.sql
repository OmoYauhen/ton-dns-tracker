ALTER TABLE domains DROP COLUMN IF EXISTS events_synced_lt;
DROP TABLE IF EXISTS domain_events;
