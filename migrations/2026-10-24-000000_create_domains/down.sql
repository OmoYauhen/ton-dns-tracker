DROP VIEW IF EXISTS domain_live_status;
DROP TRIGGER IF EXISTS domains_set_updated_at ON domains;
DROP FUNCTION IF EXISTS set_updated_at();
DROP TABLE IF EXISTS domains;
