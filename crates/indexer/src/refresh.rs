use std::collections::HashMap;

use anyhow::Result;
use diesel::dsl::sql;
use diesel::prelude::*;
use diesel::sql_types::Bool;
use diesel_async::RunQueryDsl;
use ton_dns_core::cell::{self, CellError, NftItemState};
use ton_dns_core::models::Domain;
use ton_dns_core::schema::domains;
use ton_dns_core::ton_address::TonAddress;
use ton_dns_core::toncenter::TonCenterClient;

use crate::{events, DbPool};

// toncenter's docs say /accountStates accepts up to 1000 addresses/request,
// but building that as repeated `&address=...` query params produces a URL
// past `http::Uri`'s hard 65534-byte length limit (it packs offsets into
// u16) -- reqwest's "Parsed Url is not a valid Uri" is that limit, not a
// toncenter error. 500 addresses is comfortably under it.
const CHUNK_SIZE: usize = 500;

/// Re-fetches raw account state for every domain worth re-checking and
/// re-decodes it client-side (see `ton_dns_core::cell`), upserting the
/// result. This is the path that keeps `last_fill_up_time`/auction state
/// current -- `domain_live_status` then computes `releasable`/`in_auction`
/// against wall-clock time on every read, so this loop only needs to run
/// often enough that *that* input data doesn't go stale, not fast enough
/// to catch every state transition live.
///
/// "Worth re-checking" throttles by *time since this row was last
/// refreshed* (`updated_at`, stamped to `now()` on every refresh by both
/// `Domain::from_chain` and the `set_updated_at` DB trigger -- no extra
/// bookkeeping needed), not by expiry distance. An earlier version of
/// this skipped domains "comfortably far from expiry", but that heuristic
/// turned out not to shrink the real set much at all: most real mainnet
/// `.ton` domains are already at or past their 1-year mark (registered
/// years ago, never renewed), so "far from expiry" barely excluded
/// anyone -- a refresh pass was still re-fetching essentially the whole
/// table every 5 minutes. Time-since-last-check works regardless of
/// *why* a domain is boring (far from expiry, already releasable,
/// unsettled auction nobody's touched in months -- all of them), and
/// directly bounds the per-pass workload: once the backlog is cleared,
/// steady-state work is roughly total/REFRESH_THROTTLE-worth of domains
/// per pass, not the whole table.
///
/// Two things always get checked regardless of the throttle: domains
/// with `last_transaction_lt = 0` (discovery's marker for "never actually
/// refreshed yet" -- these need their first real decode promptly, not
/// whenever the throttle next allows), and domains with a *currently
/// open* auction (`auction_end_time` in the future) -- bids can land any
/// moment and are worth catching promptly, unlike a long-settled or
/// long-unsettled domain that only changes when a human takes a rare
/// action.
const REFRESH_THROTTLE: &str = "1 hour";

pub async fn run(client: &TonCenterClient, pool: &DbPool, collection_address: &str) -> Result<()> {
    tracing::info!("starting refresh pass");
    let mut conn = pool.get().await?;
    let known: Vec<(TonAddress, i64)> = domains::table
        .filter(sql::<Bool>(&format!(
            "last_transaction_lt = 0 \
             OR (auction_end_time IS NOT NULL AND now() <= auction_end_time) \
             OR updated_at < now() - interval '{REFRESH_THROTTLE}'",
        )))
        .select((domains::address, domains::last_transaction_lt))
        .load(&mut conn)
        .await?;

    if known.is_empty() {
        tracing::info!("no domains worth re-checking yet, skipping refresh");
        return Ok(());
    }

    // Used after each upsert to detect whether this domain's on-chain
    // state actually changed since last time -- if so, its event history
    // (bids/top-ups/ownership changes) likely has something new too, so
    // sync it right away instead of waiting for the next periodic
    // events::run sweep (which still exists as a catch-up/safety net for
    // anything missed here, e.g. this call failing).
    let old_last_transaction_lt: HashMap<TonAddress, i64> = known.iter().copied().collect();
    let addresses: Vec<TonAddress> = known.into_iter().map(|(address, _)| address).collect();

    let total_chunks = addresses.len().div_ceil(CHUNK_SIZE);
    let mut updated = 0;
    for (chunk_index, chunk) in addresses.chunks(CHUNK_SIZE).enumerate() {
        tracing::info!(
            chunk = chunk_index + 1,
            total_chunks,
            "fetching account states"
        );
        let addr_strings: Vec<String> = chunk.iter().map(|a| a.to_raw()).collect();
        let response = client.account_states(&addr_strings).await?;

        for account in response.accounts {
            let Some(data_boc) = account.decode_data_boc()? else {
                continue;
            };
            let address: TonAddress = account.address.parse()?;

            // code_hash comes back base64-encoded, not hex (confirmed
            // against a real response: hex::decode failed immediately on
            // a character outside hex's alphabet) -- same encoding as
            // data_boc above.
            let code_hash = account
                .code_hash
                .as_deref()
                .map(|b64| {
                    use base64::Engine;
                    base64::engine::general_purpose::STANDARD.decode(b64)
                })
                .transpose()?
                .unwrap_or_default();
            let last_transaction_lt: i64 = account
                .last_transaction_lt
                .as_deref()
                .unwrap_or("0")
                .parse()?;

            let state = match cell::decode_nft_item(&data_boc) {
                Ok(state) => state,
                Err(err) => {
                    tracing::warn!(address = %account.address, ?err, "failed to decode domain cell, skipping");
                    continue;
                }
            };

            let NftItemState::Initialized(item) = state else {
                continue; // reserved index, collection hasn't started its auction yet
            };

            let name = match cell::decode_domain_label(&item.domain) {
                Ok(name) => name,
                Err(err) => {
                    tracing::warn!(address = %account.address, ?err, "failed to decode domain label, skipping");
                    continue;
                }
            };
            // Unlike a failed domain-label/cell decode above, this does
            // NOT `continue` (skip the whole domain) on failure -- owner,
            // auction state, and last_fill_up_time were already
            // successfully decoded above via decode_nft_item, and
            // throwing all of that away over a DNS-records problem (e.g.
            // genuinely offchain content, tag != 0) left real domains
            // permanently stuck showing stale/placeholder owner data,
            // since the same decode failure recurs every single pass.
            // Saving with empty DNS records and no auction/owner data
            // lost is strictly better than saving nothing.
            let records = match cell::decode_dns_records(&item.content) {
                Ok(records) => records,
                // Expected for a handful of domains whose owners stored
                // something else as content -- recurs every pass, so not
                // worth a warning (see CellError::NotDnsRecords).
                Err(CellError::NotDnsRecords(reason)) => {
                    tracing::debug!(
                        address = %account.address,
                        reason,
                        "domain has no dns records dictionary, saving domain without them"
                    );
                    cell::DecodedDnsRecords::default()
                }
                Err(err) => {
                    tracing::warn!(
                        address = %account.address,
                        ?err,
                        "failed to decode dns records, saving domain without them"
                    );
                    cell::DecodedDnsRecords::default()
                }
            };

            let domain = Domain::from_chain(
                address,
                name,
                "ton".to_string(),
                &item,
                records,
                code_hash,
                last_transaction_lt,
            );

            diesel::insert_into(domains::table)
                .values(&domain)
                .on_conflict(domains::address)
                .do_update()
                .set(&domain)
                .execute(&mut conn)
                .await?;
            updated += 1;

            // `old == 0` means this is the domain's first-ever successful
            // refresh (0 is discovery's placeholder, see discovery.rs) --
            // that 0 -> real_lt transition isn't a genuine on-chain event,
            // just the first observation, so it shouldn't trigger a sync
            // call here. events_synced_lt is now seeded from the deploy
            // transaction's own lt (see discovery.rs), so the periodic
            // events::run sweep already correctly decides on its own
            // whether this domain has any real history worth fetching --
            // no need to force it inline and risk a wasted call for the
            // likely-common case of "nothing happened since registration".
            let old = old_last_transaction_lt.get(&address).copied().unwrap_or(0);
            let changed = old != 0 && old != last_transaction_lt;
            if changed {
                if let Err(err) =
                    events::sync_domain_events(client, pool, address, collection_address).await
                {
                    tracing::warn!(
                        address = %account.address,
                        ?err,
                        "failed to sync domain events after detecting a change, \
                         will retry on the next events sync pass"
                    );
                }
            }
        }
    }

    tracing::info!(updated, "refresh pass complete");
    Ok(())
}
