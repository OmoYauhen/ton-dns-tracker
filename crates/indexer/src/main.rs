mod discovery;
mod events;
mod refresh;

use std::env;
use std::sync::Arc;
use std::time::Duration;

use diesel_async::pooled_connection::bb8::Pool;
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::AsyncPgConnection;
use ton_dns_core::ton_address::TonAddress;
use ton_dns_core::toncenter::TonCenterClient;
use tracing_subscriber::EnvFilter;

pub type DbPool = Pool<AsyncPgConnection>;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    // EnvFilter::from_default_env() defaults to ERROR-only when RUST_LOG
    // isn't set, which would silence every progress log below -- default
    // to INFO instead, still overridable via RUST_LOG.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let database_url = env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    let api_key = env::var("TONCENTER_API_KEY").expect("TONCENTER_API_KEY must be set");
    let base_url = env::var("TONCENTER_BASE_URL")
        .unwrap_or_else(|_| "https://toncenter.com/api/v3".to_string());
    let rps: f64 = env::var("TONCENTER_RATE_LIMIT_RPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10.0);

    // TODO: resolve from blockchain config param #4 instead (see
    // ton_dns_core::toncenter module docs) -- using an env override for
    // now so the rest of the pipeline is testable without that piece.
    //
    // Normalized to raw "wc:hex" form here, once: toncenter's
    // /transactions responses always give message source/destination
    // addresses in raw form (confirmed repeatedly against live
    // responses), and events::classify_transaction string-compares
    // against this value to recognize the collection -- a friendly-form
    // "EQ..." address (what a user would naturally put in .env) would
    // silently never match.
    let dns_collection_address = env::var("DNS_COLLECTION_ADDRESS")
        .expect("DNS_COLLECTION_ADDRESS must be set until config-param lookup is implemented")
        .parse::<TonAddress>()
        .expect("DNS_COLLECTION_ADDRESS must be a valid TON address")
        .to_raw();

    let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(database_url);
    let pool: DbPool = Pool::builder().build(manager).await?;

    let client = Arc::new(TonCenterClient::new(base_url, api_key, rps));

    tracing::info!(
        collection_address = %dns_collection_address,
        rate_limit_rps = rps,
        "starting ton-dns-indexer"
    );

    // Discovery walks the collection contract's own transaction history
    // via an lt cursor that only ever advances forward (see
    // discovery::run), so a periodic catch-up is normally cheap -- just
    // however many new domains were registered since last time, not a
    // full rescan. 5 minutes, same as refresh.
    let discovery_task = {
        let client = client.clone();
        let pool = pool.clone();
        let collection = dns_collection_address.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(5 * 60));
            loop {
                interval.tick().await;
                if let Err(err) = discovery::run(&client, &pool, &collection).await {
                    tracing::error!(?err, "discovery pass failed");
                }
            }
        })
    };

    // Refresh re-decodes domains worth re-checking (skips comfortably
    // far-from-expiry active ones -- see refresh::run) and, for any whose
    // state actually changed, syncs its event history inline. At 1000
    // addresses/request and 10 req/s this is well within the free-tier
    // rate limit -- see TONCENTER_RATE_LIMIT_RPS to raise it once on a
    // paid plan.
    let refresh_task = {
        let client = client.clone();
        let pool = pool.clone();
        let collection = dns_collection_address.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(5 * 60));
            loop {
                interval.tick().await;
                if let Err(err) = refresh::run(&client, &pool, &collection).await {
                    tracing::error!(?err, "refresh pass failed");
                }
            }
        })
    };

    // Event sync walks each recently-active domain's own transaction
    // history (bids/top-ups/ownership changes/re-registrations -- see
    // events::run). Only domains where refresh just saw a new
    // last_transaction_lt need a request, so this stays cheap at scale
    // despite running every 5 minutes -- see the filter in events::run.
    let events_task = {
        let client = client.clone();
        let pool = pool.clone();
        let collection = dns_collection_address.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(5 * 60));
            loop {
                interval.tick().await;
                if let Err(err) = events::run(&client, &pool, &collection).await {
                    tracing::error!(?err, "events sync pass failed");
                }
            }
        })
    };

    // All three tasks loop forever internally (errors are logged and the
    // loop keeps going), so `?` here only ever fires if a task panicked.
    tokio::select! {
        result = discovery_task => { result?; }
        result = refresh_task => { result?; }
        result = events_task => { result?; }
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("received ctrl-c, shutting down");
        }
    }

    Ok(())
}
