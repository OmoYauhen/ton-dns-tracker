use std::time::Duration;

use anyhow::{bail, Result};
use bigdecimal::BigDecimal;
use diesel::prelude::*;
use diesel_async::RunQueryDsl;
use ton_dns_core::models::{DomainEventKind, NewDomainEvent};
use ton_dns_core::schema::{discovery_progress, domain_events, domains};
use ton_dns_core::ton_address::TonAddress;
use ton_dns_core::toncenter::{TonCenterClient, Transaction, TransactionsResponse};

use crate::DbPool;

// toncenter's /nft/items?collection_address=... (plain OFFSET pagination)
// got unreliably slow at depth (timeouts past roughly offset 50000-100000
// on this collection, even after retries). Walking the collection
// contract's OWN transaction history instead uses true keyset pagination
// (start_lt), which doesn't have that problem -- see
// ton_dns_core::toncenter module docs.
const PAGE_SIZE: u32 = 100;
const MAX_PAGE_RETRIES: u32 = 5;

/// Discovers new domains by walking the .ton DNS collection contract's own
/// transaction history rather than /nft/items. Starting a domain auction
/// is a plain text-comment message (the domain name) with value attached,
/// sent directly to the collection; `nft-collection.fc`'s only handler for
/// that (`op == 0`) deploys the new item -- visible here as an outgoing
/// message carrying `init_state` (the new contract's code+data), the only
/// kind of message this contract ever sends with one. Everything needed
/// (new item address, first bidder, first bid, registration time, domain
/// name) comes from this one transaction, with no need to decode the
/// deploy message's own `init_state` cell.
///
/// Only `address`/`name` are meaningful on the inserted row -- everything
/// else (owner, last_fill_up_time, dns records, the *real* item_index...)
/// gets filled in by the next `refresh` pass, which is why newly
/// discovered rows use `now()` as a placeholder `last_fill_up_time` (reads
/// as "active" rather than "releasable", the safer direction to be wrong
/// in for a brand new domain) and a placeholder `item_index` derived from
/// the item's own address (see below) rather than the real value -- a
/// 256-bit hash of the domain name that has to exactly match the chain's
/// own `slice_hash`, not worth risking getting subtly wrong here when
/// refresh reads the authoritative value directly off the item's own
/// account state moments later.
///
/// The cursor (`discovery_progress.next_lt`) only ever advances forward --
/// unlike the old /nft/items-offset approach, there's no daily full-rescan
/// to catch a missed range, so a page that fails after retries stops the
/// whole run rather than skipping past it, leaving the cursor exactly
/// where it was for next time.
pub async fn run(client: &TonCenterClient, pool: &DbPool, collection_address: &str) -> Result<()> {
    tracing::info!(collection_address, "starting discovery pass");
    let mut conn = pool.get().await?;

    let mut cursor: i64 = discovery_progress::table
        .select(discovery_progress::next_lt)
        .first::<i64>(&mut conn)
        .await?;
    if cursor > 0 {
        tracing::info!(cursor, "resuming discovery pass from saved lt");
    }

    let mut registered = 0;
    loop {
        tracing::info!(cursor, registered, "fetching collection transactions page");
        let page = fetch_page_with_retry(client, collection_address, cursor, PAGE_SIZE).await?;
        let got = page.transactions.len() as u32;

        for tx in &page.transactions {
            for out_msg in &tx.out_msgs {
                if out_msg.init_state.is_none() {
                    continue;
                }
                let Some(destination) = out_msg.destination.as_deref() else {
                    tracing::warn!(lt = %tx.lt, "deploy message has no destination, skipping");
                    continue;
                };
                let address: TonAddress = match destination.parse() {
                    Ok(address) => address,
                    Err(err) => {
                        tracing::warn!(lt = %tx.lt, ?err, "could not parse deploy destination address, skipping");
                        continue;
                    }
                };

                let name = match decode_triggering_comment(tx) {
                    Ok(name) => name,
                    Err(err) => {
                        tracing::warn!(
                            lt = %tx.lt,
                            ?err,
                            "could not decode domain name from triggering comment, using placeholder"
                        );
                        String::new()
                    }
                };

                let Some(event_time) = chrono::DateTime::from_timestamp(tx.now, 0) else {
                    tracing::warn!(lt = %tx.lt, now = tx.now, "transaction timestamp out of range, skipping");
                    continue;
                };

                let deploy_lt: i64 = tx.lt.parse()?;

                // Placeholder -- see the fn doc comment for why we don't
                // try to compute the real value here. Still has to be
                // unique within the (namespace, item_index) constraint
                // though, so derive it from the item's own (already
                // address-unique) hash rather than a shared constant --
                // a fixed placeholder like 0 collides the moment a
                // second domain is discovered before refresh corrects
                // the first one.
                let item_index_placeholder = BigDecimal::from(u64::from_be_bytes(
                    address.hash[0..8].try_into().expect("8 bytes"),
                ));

                let rows = diesel::insert_into(domains::table)
                    .values((
                        domains::address.eq(address),
                        domains::item_index.eq(item_index_placeholder),
                        domains::name.eq(&name),
                        domains::namespace.eq("ton"),
                        domains::last_fill_up_time.eq(diesel::dsl::now),
                        domains::code_hash.eq(Vec::<u8>::new()),
                        domains::last_transaction_lt.eq(0i64),
                        // Seeded from the deploy transaction's own lt,
                        // not left at the column default of 0: a domain
                        // that never has any activity beyond its deploy
                        // keeps reporting that exact lt as its
                        // last_transaction_lt forever, so
                        // `last_transaction_lt >= events_synced_lt`
                        // (events::run's trigger condition) correctly
                        // never matches it -- no wasted history-fetch
                        // call for the ~200k domains that likely have
                        // nothing beyond their one registration event.
                        // Domains with real post-deploy activity (bids,
                        // settlement, renewal, transfer) still match as
                        // soon as their last_transaction_lt moves past
                        // this.
                        domains::events_synced_lt.eq(deploy_lt + 1),
                    ))
                    .on_conflict(domains::address)
                    .do_nothing()
                    .execute(&mut conn)
                    .await?;

                let actor_address: Option<TonAddress> = tx
                    .in_msg
                    .as_ref()
                    .and_then(|m| m.source.as_deref())
                    .and_then(|s| s.parse().ok());
                let amount: Option<i64> = tx
                    .in_msg
                    .as_ref()
                    .and_then(|m| m.value.as_deref())
                    .and_then(|v| v.parse().ok());

                diesel::insert_into(domain_events::table)
                    .values(NewDomainEvent {
                        domain_address: address,
                        lt: deploy_lt,
                        event_time,
                        kind: DomainEventKind::Registered.as_str().to_string(),
                        actor_address,
                        amount,
                        details: serde_json::json!({ "name": name }),
                    })
                    .on_conflict((
                        domain_events::domain_address,
                        domain_events::lt,
                        domain_events::kind,
                    ))
                    .do_nothing()
                    .execute(&mut conn)
                    .await?;

                if rows > 0 {
                    registered += 1;
                }
            }
        }

        if let Some(last) = page.transactions.last() {
            cursor = last.lt.parse::<i64>()? + 1;
        }
        diesel::update(discovery_progress::table)
            .set((
                discovery_progress::next_lt.eq(cursor),
                discovery_progress::updated_at.eq(diesel::dsl::now),
            ))
            .execute(&mut conn)
            .await?;

        if got < PAGE_SIZE {
            break;
        }
    }

    tracing::info!(registered, cursor, "discovery pass complete");
    Ok(())
}

/// The domain name comes from the comment the first bidder sent to the
/// collection to trigger this deploy -- that's `tx.in_msg`, not anything
/// on the deploy message itself.
fn decode_triggering_comment(tx: &Transaction) -> Result<String> {
    use base64::Engine;
    let body = tx
        .in_msg
        .as_ref()
        .and_then(|m| m.message_content.as_ref())
        .ok_or_else(|| anyhow::anyhow!("deploy transaction has no in_msg body"))?;
    let bytes = base64::engine::general_purpose::STANDARD.decode(&body.body)?;
    Ok(ton_dns_core::cell::decode_text_comment_boc(&bytes)?)
}

/// toncenter has been observed returning a transient 5xx -- retry a
/// handful of times with backoff before giving up on the whole pass.
async fn fetch_page_with_retry(
    client: &TonCenterClient,
    collection_address: &str,
    start_lt: i64,
    limit: u32,
) -> Result<TransactionsResponse> {
    let mut attempt = 0;
    loop {
        match client
            .transactions_page(collection_address, start_lt, limit)
            .await
        {
            Ok(page) => return Ok(page),
            Err(err) if attempt < MAX_PAGE_RETRIES => {
                attempt += 1;
                let backoff = Duration::from_secs(1 << attempt);
                tracing::warn!(
                    start_lt,
                    attempt,
                    ?err,
                    ?backoff,
                    "transactions page request failed, retrying"
                );
                tokio::time::sleep(backoff).await;
            }
            Err(err) => bail!("transactions page request failed after all retries: {err}"),
        }
    }
}
