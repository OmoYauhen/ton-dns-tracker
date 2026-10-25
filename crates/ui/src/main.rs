use std::env;

use askama::Template;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use axum::Router;
use chrono::{DateTime, Utc};
use diesel::prelude::*;
use diesel_async::pooled_connection::bb8::Pool;
use diesel_async::pooled_connection::AsyncDieselConnectionManager;
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use serde::Deserialize;
use ton_dns_core::models::{DomainEvent, DomainWithStatus};
use ton_dns_core::schema::{domain_events, domain_live_status, domains};

type DbPool = Pool<AsyncPgConnection>;

#[derive(Clone)]
struct AppState {
    pool: DbPool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt::init();

    let database_url = env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    let bind_addr = env::var("BIND_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".to_string());

    let manager = AsyncDieselConnectionManager::<AsyncPgConnection>::new(database_url);
    let pool: DbPool = Pool::builder().build(manager).await?;

    let app = Router::new()
        .route("/", get(list_domains))
        .route("/domain/{name}", get(show_domain))
        .route("/pending-events", get(show_pending_events))
        .with_state(AppState { pool });

    let listener = tokio::net::TcpListener::bind(&bind_addr).await?;
    tracing::info!(%bind_addr, "listening");
    axum::serve(listener, app).await?;

    Ok(())
}

#[derive(Debug, Deserialize)]
struct ListParams {
    status: Option<String>,
    #[serde(default = "default_limit")]
    limit: i64,
    #[serde(default)]
    offset: i64,
    /// "" (default) = shortest name first; "expiring" = soonest-to-expire
    /// first (ascending last_fill_up_time), for domains about to flip to
    /// `releasable`.
    #[serde(default)]
    sort: String,
}

fn default_limit() -> i64 {
    50
}

async fn list_domains(
    State(state): State<AppState>,
    Query(params): Query<ListParams>,
) -> impl IntoResponse {
    match render(&state, &params).await {
        Ok(html) => Html(html).into_response(),
        Err(err) => {
            tracing::error!(?err, "failed to render domain list");
            (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
        }
    }
}

async fn render(state: &AppState, params: &ListParams) -> anyhow::Result<String> {
    let mut conn = state.pool.get().await?;

    let limit = params.limit.clamp(1, 500);
    let offset = params.offset.max(0);

    // Built twice (once to count, once to page through) since a boxed
    // query can't be reused for two different selects.
    let mut count_query = domain_live_status::table.into_boxed();
    if let Some(status) = &params.status {
        if !status.is_empty() {
            count_query = count_query.filter(domain_live_status::status.eq(status.clone()));
        }
    }
    let total: i64 = count_query.count().get_result(&mut conn).await?;

    let mut query = domain_live_status::table.into_boxed();
    if let Some(status) = &params.status {
        if !status.is_empty() {
            query = query.filter(domain_live_status::status.eq(status.clone()));
        }
    }

    let query = if params.sort == "expiring" {
        // Soonest-to-expire first: oldest last_fill_up_time first, so
        // domains about to flip from `active` to `releasable` surface at
        // the top.
        query.order(domain_live_status::last_fill_up_time.asc())
    } else {
        // Shortest domain name first; alphabetical as a tiebreak for
        // determinism between same-length names.
        query
            .order(diesel::dsl::sql::<diesel::sql_types::Integer>(
                "length(name)",
            ))
            .then_order_by(domain_live_status::name.asc())
    };

    let rows: Vec<DomainWithStatus> = query.limit(limit).offset(offset).load(&mut conn).await?;

    let domains = rows
        .into_iter()
        .map(|d| DomainRow {
            name_only: d.name.clone(),
            name: format!("{}.{}", d.name, d.namespace),
            address_friendly: d.address.to_friendly(),
            status: d.status,
            owner: d.owner_address.map(|a| a.to_friendly()),
            last_fill_up_time: format_datetime(d.last_fill_up_time),
        })
        .collect();

    let template = DomainsTemplate {
        domains,
        status_filter: params.status.clone().unwrap_or_default(),
        sort: params.sort.clone(),
        total,
        limit,
        current_page: offset / limit + 1,
        total_pages: if total == 0 {
            1
        } else {
            (total + limit - 1) / limit
        },
        has_prev: offset > 0,
        has_next: offset + limit < total,
        prev_offset: (offset - limit).max(0),
        next_offset: offset + limit,
    };
    Ok(template.render()?)
}

struct DomainRow {
    name_only: String,
    name: String,
    address_friendly: String,
    status: String,
    owner: Option<String>,
    last_fill_up_time: String,
}

#[derive(Template)]
#[template(path = "domains.html")]
struct DomainsTemplate {
    domains: Vec<DomainRow>,
    status_filter: String,
    sort: String,
    total: i64,
    limit: i64,
    current_page: i64,
    total_pages: i64,
    has_prev: bool,
    has_next: bool,
    prev_offset: i64,
    next_offset: i64,
}

async fn show_domain(State(state): State<AppState>, Path(name): Path<String>) -> impl IntoResponse {
    match render_domain(&state, &name).await {
        Ok(Some(html)) => Html(html).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "domain not found").into_response(),
        Err(err) => {
            tracing::error!(?err, name, "failed to render domain detail");
            (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
        }
    }
}

async fn render_domain(state: &AppState, name: &str) -> anyhow::Result<Option<String>> {
    let mut conn = state.pool.get().await?;

    let domain: Option<DomainWithStatus> = domain_live_status::table
        .filter(domain_live_status::name.eq(name))
        .filter(domain_live_status::namespace.eq("ton"))
        .first(&mut conn)
        .await
        .optional()?;

    let Some(domain) = domain else {
        return Ok(None);
    };

    let events: Vec<DomainEvent> = domain_events::table
        .filter(domain_events::domain_address.eq(domain.address))
        .order(domain_events::event_time.asc())
        .load(&mut conn)
        .await?;

    let event_rows = events
        .into_iter()
        .map(|e| EventRow {
            kind: e.kind,
            event_time: format_datetime(e.event_time),
            actor: e.actor_address.map(|a| a.to_friendly()),
            amount: e
                .amount
                .map(format_nanoton)
                .unwrap_or_else(|| "—".to_string()),
            details: e.details.to_string(),
        })
        .collect();

    let template = DomainDetailTemplate {
        name_only: domain.name.clone(),
        name: format!("{}.{}", domain.name, domain.namespace),
        address_friendly: domain.address.to_friendly(),
        status: domain.status,
        owner: domain.owner_address.map(|a| a.to_friendly()),
        last_fill_up_time: format_datetime(domain.last_fill_up_time),
        auction_max_bid_address: domain.auction_max_bid_address.map(|a| a.to_friendly()),
        auction_max_bid_amount: domain.auction_max_bid_amount.map(format_nanoton),
        auction_end_time: domain.auction_end_time.map(format_datetime),
        wallet_address: domain.wallet_address.map(|a| a.to_friendly()),
        site_adnl: domain.site_adnl.map(hex::encode),
        storage_bag_id: domain.storage_bag_id.map(hex::encode),
        next_resolver: domain.next_resolver.map(|a| a.to_friendly()),
        events: event_rows,
    };
    Ok(Some(template.render()?))
}

/// Rounded to the nearest whole TON (nanoton precision isn't meaningful
/// at a glance for amounts that are almost always tens-to-hundreds of
/// TON, e.g. auction bids).
fn format_nanoton(amount: i64) -> String {
    format!("{:.0} TON", amount as f64 / 1_000_000_000.0)
}

/// "YYYY-MM-DD HH:MM (N days ago)" -- UTC, with a relative hint since
/// "N days ago" is what actually matters for judging freshness/expiry at
/// a glance.
fn format_datetime(dt: DateTime<Utc>) -> String {
    let days = Utc::now().signed_duration_since(dt).num_days();
    let relative = match days {
        0 => "today".to_string(),
        1 => "1 day ago".to_string(),
        d if d > 1 => format!("{d} days ago"),
        -1 => "in 1 day".to_string(),
        d => format!("in {} days", -d),
    };
    format!("{} ({relative})", dt.format("%Y-%m-%d %H:%M"))
}

struct EventRow {
    kind: String,
    event_time: String,
    actor: Option<String>,
    amount: String,
    details: String,
}

#[derive(Template)]
#[template(path = "domain.html")]
struct DomainDetailTemplate {
    name_only: String,
    name: String,
    address_friendly: String,
    status: String,
    owner: Option<String>,
    last_fill_up_time: String,
    auction_max_bid_address: Option<String>,
    auction_max_bid_amount: Option<String>,
    auction_end_time: Option<String>,
    wallet_address: Option<String>,
    site_adnl: Option<String>,
    storage_bag_id: Option<String>,
    next_resolver: Option<String>,
    events: Vec<EventRow>,
}

// Capped rather than paginated -- this is an operational/debug view, not
// something meant to be browsed page by page.
const PENDING_EVENTS_LIMIT: i64 = 500;

async fn show_pending_events(State(state): State<AppState>) -> impl IntoResponse {
    match render_pending_events(&state).await {
        Ok(html) => Html(html).into_response(),
        Err(err) => {
            tracing::error!(?err, "failed to render pending events list");
            (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
        }
    }
}

/// Domains where `events::sync_domain_events` (ton-dns-indexer) still has
/// transaction history left to fetch -- the exact same
/// `last_transaction_lt >= events_synced_lt` condition it uses to decide
/// what's worth a request, so this is a direct view of its backlog.
async fn render_pending_events(state: &AppState) -> anyhow::Result<String> {
    let mut conn = state.pool.get().await?;

    // Built twice (once to count, once to list) since a query can't be
    // reused for two different terminal operations -- same as render().
    let total: i64 = domains::table
        .filter(domains::last_transaction_lt.ge(domains::events_synced_lt))
        .count()
        .get_result(&mut conn)
        .await?;

    let rows: Vec<(String, String, i64, i64)> = domains::table
        .filter(domains::last_transaction_lt.ge(domains::events_synced_lt))
        .order(domains::last_transaction_lt.desc())
        .select((
            domains::name,
            domains::namespace,
            domains::last_transaction_lt,
            domains::events_synced_lt,
        ))
        .limit(PENDING_EVENTS_LIMIT)
        .load(&mut conn)
        .await?;

    let shown = rows.len() as i64;
    let rows = rows
        .into_iter()
        .map(
            |(name, namespace, last_transaction_lt, events_synced_lt)| PendingRow {
                name_only: name.clone(),
                name: format!("{name}.{namespace}"),
                last_transaction_lt,
                events_synced_lt,
            },
        )
        .collect();

    let template = PendingEventsTemplate { rows, total, shown };
    Ok(template.render()?)
}

struct PendingRow {
    name_only: String,
    name: String,
    last_transaction_lt: i64,
    events_synced_lt: i64,
}

#[derive(Template)]
#[template(path = "pending_events.html")]
struct PendingEventsTemplate {
    rows: Vec<PendingRow>,
    total: i64,
    shown: i64,
}
