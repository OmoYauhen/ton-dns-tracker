//! Minimal client for the pieces of the toncenter v3 API this project
//! needs: paginated account transaction history (domain/event discovery),
//! and bulk account state fetches (used for client-side cell decoding
//! instead of per-domain `runGetMethod` calls -- see `crate::cell`).
//!
//! Every response shape here has been checked against a live toncenter
//! response, not just the v3 docs. Discovery used to page through
//! `/nft/items?collection_address=...` with plain OFFSET, which got
//! unreliably slow at depth (toncenter timeouts past roughly offset
//! 50000-100000 on the .ton collection); `transactions_page`'s
//! `start_lt`/`end_lt` is true keyset pagination over an account's own
//! transaction history and doesn't have that problem, so discovery now
//! walks the DNS collection contract's transactions instead -- see
//! ton-dns-indexer::discovery.

use std::sync::Mutex;
use std::time::Duration;

use serde::Deserialize;
use thiserror::Error;
use tokio::time::Instant;

#[derive(Debug, Error)]
pub enum TonCenterError {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("toncenter returned an error response: {0}")]
    Api(String),
    #[error("base64 decode error: {0}")]
    Base64(#[from] base64::DecodeError),
}

/// Leaky-bucket pacing that adapts to what toncenter actually tolerates,
/// rather than trusting `rps` (TONCENTER_RATE_LIMIT_RPS) as ground truth --
/// it's a configured guess (the project's free-tier assumption was never
/// verified against a live key), and a 429 response carries no
/// Retry-After header to size a backoff from (confirmed empirically: a
/// burst of requests against toncenter returns plain 429s with no rate
/// limit headers at all). So instead: on a 429, double the spacing
/// (capped at 30s/request) for every future caller sharing this limiter,
/// not just the one that got rate limited -- a 429 means the current pace
/// already exceeds this key's real limit.
///
/// Recovery back toward the originally configured rate is gated by
/// elapsed wall-clock time since the last 429 (`RECOVERY_COOLDOWN`), not
/// by a count of successful requests -- discovery/refresh/events all
/// share this one limiter and run concurrently, so a request *count* is
/// reached almost instantly (seen in practice: ~20 successes across three
/// concurrent loops within a couple seconds), undoing a backoff before it
/// had any real effect and leaving the interval oscillating between the
/// base rate and 2x it instead of ever escalating to what toncenter
/// actually needed.
struct RateLimiter {
    base_interval: Duration,
    state: Mutex<RateLimiterState>,
}

struct RateLimiterState {
    next_slot: Instant,
    interval: Duration,
    last_rate_limited_at: Option<Instant>,
}

const MAX_INTERVAL: Duration = Duration::from_secs(30);
const RECOVERY_COOLDOWN: Duration = Duration::from_secs(60);

impl RateLimiter {
    fn new(rps: f64) -> Self {
        let base_interval = Duration::from_secs_f64(1.0 / rps);
        Self {
            base_interval,
            state: Mutex::new(RateLimiterState {
                next_slot: Instant::now(),
                interval: base_interval,
                last_rate_limited_at: None,
            }),
        }
    }

    async fn acquire(&self) {
        let wait_until = {
            let mut state = self.state.lock().unwrap();
            let now = Instant::now();
            let start = state.next_slot.max(now);
            state.next_slot = start + state.interval;
            start
        };
        tokio::time::sleep_until(wait_until).await;
    }

    fn record_rate_limited(&self) {
        let mut state = self.state.lock().unwrap();
        state.last_rate_limited_at = Some(Instant::now());
        state.interval = (state.interval * 2).min(MAX_INTERVAL);
        tracing::warn!(new_interval = ?state.interval, "toncenter rate limited us, slowing down");
    }

    fn record_success(&self) {
        let mut state = self.state.lock().unwrap();
        if state.interval <= self.base_interval {
            return;
        }
        let Some(last_rate_limited_at) = state.last_rate_limited_at else {
            return;
        };
        if last_rate_limited_at.elapsed() < RECOVERY_COOLDOWN {
            return;
        }
        state.interval = (state.interval / 2).max(self.base_interval);
        state.last_rate_limited_at = Some(Instant::now());
        tracing::info!(new_interval = ?state.interval, "no rate limit errors for a while, speeding back up");
    }
}

pub struct TonCenterClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    limiter: RateLimiter,
}

impl TonCenterClient {
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>, rps: f64) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: base_url.into(),
            api_key: api_key.into(),
            limiter: RateLimiter::new(rps),
        }
    }

    async fn get<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<T, TonCenterError> {
        self.limiter.acquire().await;
        let url = format!("{}{}", self.base_url, path);
        let resp = self
            .http
            .get(&url)
            .header("X-API-Key", &self.api_key)
            .query(query)
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            if status.as_u16() == 429 {
                self.limiter.record_rate_limited();
            }
            let body = resp.text().await.unwrap_or_default();
            return Err(TonCenterError::Api(format!("{status}: {body}")));
        }
        self.limiter.record_success();
        Ok(resp.json::<T>().await?)
    }

    /// One page of an account's own transaction history, oldest-first,
    /// starting at `start_lt` (inclusive) -- confirmed against a live
    /// response that this is true keyset pagination: passing the lt of
    /// the oldest transaction already seen as the next call's `start_lt`
    /// resumes exactly where the previous page left off, with no
    /// OFFSET-style cost growth at depth. `limit` max 1000 per the docs,
    /// same as other list endpoints.
    pub async fn transactions_page(
        &self,
        account: &str,
        start_lt: i64,
        limit: u32,
    ) -> Result<TransactionsResponse, TonCenterError> {
        self.get(
            "/transactions",
            &[
                ("account", account.to_string()),
                ("start_lt", start_lt.to_string()),
                ("sort", "asc".to_string()),
                ("limit", limit.to_string()),
            ],
        )
        .await
    }

    /// Bulk account state fetch, including `data_boc` for client-side
    /// cell decoding. toncenter's docs say 1000 addresses/request max,
    /// but each is sent as a repeated `&address=...` query param, so the
    /// real ceiling is much lower in practice -- past ~850 addresses the
    /// built URL exceeds `http::Uri`'s hard 65534-byte limit and request
    /// building fails outright. Callers are responsible for chunking
    /// (see CHUNK_SIZE in ton-dns-indexer::refresh).
    pub async fn account_states(
        &self,
        addresses: &[String],
    ) -> Result<AccountStatesResponse, TonCenterError> {
        debug_assert!(
            addresses.len() <= 1000,
            "accountStates caps at 1000 addresses/request"
        );
        let query: Vec<(&str, String)> = addresses.iter().map(|a| ("address", a.clone())).collect();
        self.get("/accountStates", &query).await
    }
}

#[derive(Debug, Deserialize)]
pub struct TransactionsResponse {
    pub transactions: Vec<Transaction>,
}

#[derive(Debug, Deserialize)]
pub struct Transaction {
    /// Decimal string -- TON logical time, a monotonically increasing
    /// per-account sequence number; this is the pagination cursor (see
    /// `transactions_page`).
    pub lt: String,
    /// Unix timestamp.
    pub now: i64,
    pub in_msg: Option<Message>,
    #[serde(default)]
    pub out_msgs: Vec<Message>,
    #[serde(default)]
    pub description: TransactionDescription,
}

#[derive(Debug, Default, Deserialize)]
pub struct TransactionDescription {
    /// A failed/bounced message (e.g. a bid below the required minimum)
    /// produces no out_msgs -- callers classifying events from which
    /// out_msgs are present must skip aborted transactions first, or a
    /// failed attempt misreads as a different, successful event kind.
    #[serde(default)]
    pub aborted: bool,
}

#[derive(Debug, Deserialize)]
pub struct Message {
    pub source: Option<String>,
    pub destination: Option<String>,
    /// Nanoton, decimal string. Null for non-value (e.g. external)
    /// messages.
    pub value: Option<String>,
    /// Hex string like "0x5fcc3d14", null for a plain/comment message
    /// (TON's implicit "op 0" convention -- used for both starting a
    /// domain auction and placing a bid, see ton-dns-indexer::discovery
    /// and ::events).
    pub opcode: Option<String>,
    pub message_content: Option<MessageContent>,
    /// Present only on messages that deploy a new contract (carries its
    /// code+data) -- e.g. the .ton DNS collection initializing a
    /// freshly-auctioned domain's item contract. This is the structural
    /// signal discovery uses to find new domains, since op codes alone
    /// aren't a reliable filter (toncenter's generic opcode decoder
    /// mislabels messages it doesn't have contract-specific context for).
    pub init_state: Option<MessageContent>,
}

#[derive(Debug, Deserialize)]
pub struct MessageContent {
    /// Base64-encoded BOC.
    pub body: String,
}

#[derive(Debug, Deserialize)]
pub struct AccountStatesResponse {
    pub accounts: Vec<AccountState>,
}

#[derive(Debug, Deserialize)]
pub struct AccountState {
    pub address: String,
    pub status: String,
    pub code_hash: Option<String>,
    /// Logical time as a decimal string (matches how toncenter encodes
    /// other 64-bit fields in JSON to avoid precision loss).
    pub last_transaction_lt: Option<String>,
    /// Base64-encoded BOC of the account's data cell.
    pub data_boc: Option<String>,
}

impl AccountState {
    pub fn decode_data_boc(&self) -> Result<Option<Vec<u8>>, TonCenterError> {
        use base64::Engine;
        self.data_boc
            .as_deref()
            .map(|b64| base64::engine::general_purpose::STANDARD.decode(b64))
            .transpose()
            .map_err(TonCenterError::Base64)
    }
}
