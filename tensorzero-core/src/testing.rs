#![cfg(test)]

use std::sync::Arc;

use crate::auth::AuthCache;
use crate::clickhouse::ClickHouseConnectionInfo;
use crate::config_parser::Config;
use crate::gateway_util::AppStateData;

pub fn get_unit_test_app_state_data(
    config: Arc<Config<'static>>,
    clickhouse_healthy: bool,
) -> AppStateData {
    let http_client = reqwest::Client::new();
    let clickhouse_connection_info = ClickHouseConnectionInfo::new_mock(clickhouse_healthy);
    let auth_cache = AuthCache::new();

    AppStateData {
        config,
        http_client,
        clickhouse_connection_info,
        auth_cache,
    }
}
