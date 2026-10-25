use std::time::Duration;

use anyhow::{bail, Result};
use diesel::prelude::*;
use diesel_async::RunQueryDsl;
use ton_dns_core::models::{DomainEventKind, NewDomainEvent};
use ton_dns_core::schema::{domain_events, domains};
use ton_dns_core::ton_address::TonAddress;
use ton_dns_core::toncenter::{TonCenterClient, Transaction, TransactionsResponse};

use crate::DbPool;

const PAGE_SIZE: u32 = 100;
const MAX_PAGE_RETRIES: u32 = 5;
// Bounds how much a single pass takes on, even once the events_synced_lt
// seeding (see discovery.rs) has cut the backlog down to domains with
// genuine post-deploy activity -- without a cap, a large-enough backlog
// would make one pass run far longer than the 5-minute interval between
// them, leaving every later pass queued up behind it instead of making
// steady, bounded progress each tick.
const MAX_DOMAINS_PER_PASS: i64 = 500;

// TEP-81/nft-item.fc op codes, confirmed against the contract source
// (op-codes.fc, dns-utils.fc) -- not guessed.
const OP_TRANSFER: &str = "0x5fcc3d14";
const OP_OWNERSHIP_ASSIGNED: &str = "0x05138d91";
const OP_OUTBID_NOTIFICATION: &str = "0x557cea20";
const OP_FILL_UP: &str = "0x370fec51";
const OP_DNS_BALANCE_RELEASE: &str = "0x4ed14b65";

/// Loops over domains with unsynced activity and brings their event
/// history up to date. Each domain is independent and keeps its own
/// cursor (`domains.events_synced_lt`), so a failure on one domain is
/// logged and skipped rather than aborting the whole pass -- unlike
/// discovery's single shared cursor, there's no single run-wide position
/// to protect here.
pub async fn run(client: &TonCenterClient, pool: &DbPool, collection_address: &str) -> Result<()> {
    tracing::info!("starting domain events sync pass");
    let mut conn = pool.get().await?;
    // Only domains with a transaction since our last check need a call --
    // without this, every pass would need one HTTP request per known
    // domain regardless of activity, which doesn't scale (~200k domains,
    // most dormant most 5-minute windows). last_transaction_lt is already
    // refreshed by ::refresh every pass, so this filter is free. Oldest
    // last_transaction_lt first so a large backlog is worked through in a
    // stable order across passes instead of always re-examining whichever
    // rows a plain unordered scan happens to return first.
    let addresses: Vec<TonAddress> = domains::table
        .filter(domains::last_transaction_lt.ge(domains::events_synced_lt))
        .order(domains::last_transaction_lt.asc())
        .select(domains::address)
        .limit(MAX_DOMAINS_PER_PASS)
        .load(&mut conn)
        .await?;
    drop(conn);

    let mut synced = 0;
    let mut failed = 0;
    for address in addresses {
        match sync_domain_events(client, pool, address, collection_address).await {
            Ok(()) => synced += 1,
            Err(err) => {
                failed += 1;
                tracing::warn!(
                    address = %address.to_raw(),
                    ?err,
                    "failed to sync domain events, will retry next pass"
                );
            }
        }
    }

    tracing::info!(synced, failed, "domain events sync pass complete");
    Ok(())
}

/// Walks ONE domain's own transaction history from its persisted cursor
/// (`domains.events_synced_lt`), classifying bids/top-ups/ownership
/// changes/re-registrations into `domain_events`. Reusable standalone
/// (e.g. to backfill a single domain's history on demand) or looped over
/// every known domain by `run` above.
///
/// This is deliberately a *different* history than discovery walks: bids,
/// renewals, and transfers are all sent directly to the item contract
/// once it exists, never through the collection (see nft-item.fc) -- the
/// collection's own history (which discovery reads) only ever shows a
/// domain's initial auction-start.
pub async fn sync_domain_events(
    client: &TonCenterClient,
    pool: &DbPool,
    address: TonAddress,
    collection_address: &str,
) -> Result<()> {
    let mut conn = pool.get().await?;
    let address_str = address.to_raw();

    let mut cursor: i64 = domains::table
        .find(address)
        .select(domains::events_synced_lt)
        .first::<i64>(&mut conn)
        .await?;

    loop {
        let page = fetch_page_with_retry(client, &address_str, cursor, PAGE_SIZE).await?;
        let got = page.transactions.len() as u32;

        for tx in &page.transactions {
            let classified = classify_transaction(tx, collection_address);
            if classified.is_empty() {
                continue;
            }
            let Some(event_time) = chrono::DateTime::from_timestamp(tx.now, 0) else {
                tracing::warn!(lt = %tx.lt, now = tx.now, "transaction timestamp out of range, skipping");
                continue;
            };
            for event in classified {
                diesel::insert_into(domain_events::table)
                    .values(NewDomainEvent {
                        domain_address: address,
                        lt: tx.lt.parse()?,
                        event_time,
                        kind: event.kind.as_str().to_string(),
                        actor_address: event.actor,
                        amount: event.amount,
                        details: event.details,
                    })
                    .on_conflict((
                        domain_events::domain_address,
                        domain_events::lt,
                        domain_events::kind,
                    ))
                    .do_nothing()
                    .execute(&mut conn)
                    .await?;
            }
        }

        if let Some(last) = page.transactions.last() {
            cursor = last.lt.parse::<i64>()? + 1;
        }
        diesel::update(domains::table.find(address))
            .set(domains::events_synced_lt.eq(cursor))
            .execute(&mut conn)
            .await?;

        if got < PAGE_SIZE {
            break;
        }
    }

    Ok(())
}

struct ClassifiedEvent {
    kind: DomainEventKind,
    actor: Option<TonAddress>,
    amount: Option<i64>,
    details: serde_json::Value,
}

/// A transaction can produce zero, one, or (rarely) two events -- e.g. an
/// auction settling and, in that same message, the newly-settled owner
/// immediately sending a transfer.
fn classify_transaction(tx: &Transaction, collection_address: &str) -> Vec<ClassifiedEvent> {
    let mut events = Vec::new();

    // A failed/bounced message (e.g. a bid below the required minimum)
    // produces no out_msgs, which would otherwise misclassify as a
    // different, successful event kind below.
    if tx.description.aborted {
        return events;
    }

    let Some(in_msg) = tx.in_msg.as_ref() else {
        return events;
    };

    // The deploy/init transaction, sent by the collection itself -- not a
    // user action. Already captured as a `registered` event by discovery,
    // which reads the collection's own history instead.
    if in_msg.source.as_deref() == Some(collection_address) {
        return events;
    }

    let actor: Option<TonAddress> = in_msg.source.as_deref().and_then(|s| s.parse().ok());
    let amount: Option<i64> = in_msg.value.as_deref().and_then(|v| v.parse().ok());

    match in_msg.opcode.as_deref() {
        None => {
            // Plain comment/value message: a bid during an open auction,
            // or the owner topping up (renewing) an already-settled one --
            // both use the same implicit "op 0" convention. Only a real
            // bid gets an outbid_notification sent back to the previous
            // bidder, so that's the reliable signal between the two.
            let is_bid = tx
                .out_msgs
                .iter()
                .any(|m| m.opcode.as_deref() == Some(OP_OUTBID_NOTIFICATION));
            events.push(ClassifiedEvent {
                kind: if is_bid {
                    DomainEventKind::Bid
                } else {
                    DomainEventKind::TopUp
                },
                actor,
                amount,
                details: serde_json::json!({}),
            });
        }
        Some(op) if op == OP_TRANSFER => {
            let new_owner: Option<TonAddress> = tx
                .out_msgs
                .iter()
                .find(|m| m.opcode.as_deref() == Some(OP_OWNERSHIP_ASSIGNED))
                .and_then(|m| m.destination.as_deref())
                .and_then(|d| d.parse().ok());
            events.push(ClassifiedEvent {
                kind: DomainEventKind::OwnershipChange,
                actor: new_owner.or(actor),
                amount: None,
                details: serde_json::json!({
                    "mechanism": "transfer",
                    "from": actor.map(|a| a.to_raw()),
                }),
            });
        }
        Some(op) if op == OP_DNS_BALANCE_RELEASE => {
            events.push(ClassifiedEvent {
                kind: DomainEventKind::ReRegistered,
                actor,
                amount,
                details: serde_json::json!({}),
            });
        }
        _ => {}
    }

    // Independent of the op dispatch above: auction settlement is a side
    // effect bolted onto whatever message happens to arrive once an open
    // auction's end_time has passed, not a dedicated op -- detect it via
    // the outgoing fill_up-to-collection message it always sends instead.
    if let Some(fill_up) = tx.out_msgs.iter().find(|m| {
        m.opcode.as_deref() == Some(OP_FILL_UP)
            && m.destination.as_deref() == Some(collection_address)
    }) {
        events.push(ClassifiedEvent {
            kind: DomainEventKind::OwnershipChange,
            // The new owner is the auction's already-recorded
            // max_bid_address (domains.auction_max_bid_address), not
            // derivable from this message alone.
            actor: None,
            amount: fill_up.value.as_deref().and_then(|v| v.parse().ok()),
            details: serde_json::json!({ "mechanism": "auction_settled" }),
        });
    }

    events
}

/// toncenter has been observed returning a transient 5xx -- retry a
/// handful of times with backoff before giving up on this domain for now
/// (its cursor stays where it was, so the next pass retries).
async fn fetch_page_with_retry(
    client: &TonCenterClient,
    account: &str,
    start_lt: i64,
    limit: u32,
) -> Result<TransactionsResponse> {
    let mut attempt = 0;
    loop {
        match client.transactions_page(account, start_lt, limit).await {
            Ok(page) => return Ok(page),
            Err(err) if attempt < MAX_PAGE_RETRIES => {
                attempt += 1;
                let backoff = Duration::from_secs(1 << attempt);
                tracing::warn!(
                    account,
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
