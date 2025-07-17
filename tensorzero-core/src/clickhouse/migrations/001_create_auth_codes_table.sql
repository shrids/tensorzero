-- 001_create_auth_codes_table.sql
CREATE TABLE IF NOT EXISTS tupleap_auth_codes (
    auth_code String,
    tenant_id String,
    username String,
    created_at DateTime64(3),
    is_active UInt8 DEFAULT 1,
    usage_count UInt64 DEFAULT 0,
    created_by String,
    expires_at Nullable(DateTime64(3))
) ENGINE = MergeTree()
ORDER BY (tenant_id, auth_code)
SETTINGS index_granularity = 8192;

-- Index for fast lookups
ALTER TABLE tupleap_auth_codes ADD INDEX idx_auth_code auth_code TYPE bloom_filter(0.01) GRANULARITY 1;
