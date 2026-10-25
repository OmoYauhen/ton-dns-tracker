use bigdecimal::BigDecimal;
use chrono::{DateTime, Utc};
use diesel::prelude::*;
use serde_json::Value;

use crate::cell::{self, DecodedDnsRecords, InitializedDomainItem};
use crate::schema::{domain_events, domain_live_status, domains};
use crate::ton_address::TonAddress;

/// One row per `.ton` NFT domain. Fields are exactly what's decoded off
/// the NFT item's data cell (see `crate::cell`) plus bookkeeping columns
/// -- no derived/time-dependent judgements live here, see
/// `domain_live_status` / `DomainStatus` for those.
#[derive(Debug, Clone, Queryable, Identifiable, Insertable, AsChangeset)]
#[diesel(table_name = domains, primary_key(address), check_for_backend(diesel::pg::Pg))]
pub struct Domain {
    pub address: TonAddress,
    /// The NFT item's on-chain `uint256 index` -- for the .ton collection
    /// this is a hash of the domain name, not a small counter, so it
    /// needs arbitrary precision (see item_index in the migration).
    pub item_index: BigDecimal,
    pub name: String,
    pub namespace: String,

    /// None while the domain sits owner-less between release and auction
    /// settlement (owner_address is the zero address on-chain in that
    /// window) -- see dns_balance_release in nft-item.fc.
    pub owner_address: Option<TonAddress>,

    pub last_fill_up_time: DateTime<Utc>,

    pub auction_max_bid_address: Option<TonAddress>,
    pub auction_max_bid_amount: Option<i64>,
    pub auction_end_time: Option<DateTime<Utc>>,

    /// dns_smc_address: a full contract address, hence TonAddress.
    pub wallet_address: Option<TonAddress>,

    /// dns_adnl_address: a bare 256-bit ADNL key, NOT a TonAddress --
    /// there is no workchain component, so this is stored as a raw
    /// 32-byte value rather than reusing the 33-byte TonAddress type.
    pub site_adnl: Option<Vec<u8>>,

    /// dns_storage_address bag_id: a bare 256-bit TON Storage bag id,
    /// same reasoning as site_adnl -- not a chain address.
    pub storage_bag_id: Option<Vec<u8>>,

    /// dns_next_resolver: a full contract address, hence TonAddress.
    pub next_resolver: Option<TonAddress>,

    /// Any dns record category we didn't recognize, keyed by the hex of
    /// its 256-bit category hash, value the hex of its raw value cell.
    /// Forward-compat catch-all so new categories don't get silently
    /// dropped.
    pub extra_records: Option<Value>,

    pub code_hash: Vec<u8>,
    pub last_transaction_lt: i64,
    pub updated_at: DateTime<Utc>,
}

impl Domain {
    /// Builds a row from decoded on-chain data. `updated_at` is left to
    /// the table's `DEFAULT now()` / the `set_updated_at` trigger, so it's
    /// not set here -- callers insert via `diesel::insert_into` and let
    /// Postgres fill it in, then re-set it on every subsequent upsert.
    pub fn from_chain(
        address: TonAddress,
        name: String,
        namespace: String,
        item: &InitializedDomainItem,
        records: DecodedDnsRecords,
        code_hash: Vec<u8>,
        last_transaction_lt: i64,
    ) -> Self {
        let owner_address = if cell::is_zero_address(&item.owner_address) {
            None
        } else {
            Some(item.owner_address)
        };

        Self {
            address,
            item_index: BigDecimal::from_biguint(item.index.clone(), 0),
            name,
            namespace,
            owner_address,
            last_fill_up_time: unix_to_datetime(item.last_fill_up_time_unix),
            auction_max_bid_address: item.auction.as_ref().map(|a| a.max_bid_address),
            auction_max_bid_amount: item
                .auction
                .as_ref()
                .map(|a| a.max_bid_amount_nanoton as i64),
            auction_end_time: item
                .auction
                .as_ref()
                .map(|a| unix_to_datetime(a.end_time_unix)),
            wallet_address: records.wallet_address,
            site_adnl: records.site_adnl.map(|b| b.to_vec()),
            storage_bag_id: records.storage_bag_id.map(|b| b.to_vec()),
            next_resolver: records.next_resolver,
            extra_records: if records.extra.is_empty() {
                None
            } else {
                Some(Value::Object(records.extra))
            },
            code_hash,
            last_transaction_lt,
            updated_at: Utc::now(),
        }
    }
}

fn unix_to_datetime(secs: u64) -> DateTime<Utc> {
    DateTime::from_timestamp(secs as i64, 0).expect("unix timestamp from chain is in range")
}

/// One row per on-chain action against a domain -- append-only, never
/// updated. Populated by walking transaction history: `Registered` comes
/// from the discovery pass (the collection contract's own history, the
/// only place a domain's auction start is visible); everything else comes
/// from `ton-dns-indexer::events` walking each item's own history (bids,
/// renewals, and transfers all go directly to the item, never through the
/// collection -- see nft-item.fc).
#[derive(Debug, Clone, Insertable)]
#[diesel(table_name = domain_events, check_for_backend(diesel::pg::Pg))]
pub struct NewDomainEvent {
    pub domain_address: TonAddress,
    pub lt: i64,
    pub event_time: DateTime<Utc>,
    pub kind: String,
    pub actor_address: Option<TonAddress>,
    pub amount: Option<i64>,
    pub details: Value,
}

/// Read-only projection of a `domain_events` row, for the UI's per-domain
/// history page.
#[derive(Debug, Clone, Queryable, Identifiable)]
#[diesel(table_name = domain_events, primary_key(id), check_for_backend(diesel::pg::Pg))]
pub struct DomainEvent {
    pub id: i64,
    pub domain_address: TonAddress,
    pub lt: i64,
    pub event_time: DateTime<Utc>,
    pub kind: String,
    pub actor_address: Option<TonAddress>,
    pub amount: Option<i64>,
    pub details: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DomainEventKind {
    Registered,
    Bid,
    TopUp,
    OwnershipChange,
    ReRegistered,
}

impl DomainEventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Registered => "registered",
            Self::Bid => "bid",
            Self::TopUp => "top_up",
            Self::OwnershipChange => "ownership_change",
            Self::ReRegistered => "re_registered",
        }
    }
}

/// Read-only projection of `domain_live_status`, the view that computes
/// `status` against `now()` at query time.
#[derive(Debug, Clone, Queryable, Identifiable)]
#[diesel(table_name = domain_live_status, primary_key(address), check_for_backend(diesel::pg::Pg))]
pub struct DomainWithStatus {
    pub address: TonAddress,
    pub item_index: BigDecimal,
    pub name: String,
    pub namespace: String,
    pub owner_address: Option<TonAddress>,
    pub last_fill_up_time: DateTime<Utc>,
    pub auction_max_bid_address: Option<TonAddress>,
    pub auction_max_bid_amount: Option<i64>,
    pub auction_end_time: Option<DateTime<Utc>>,
    pub wallet_address: Option<TonAddress>,
    pub site_adnl: Option<Vec<u8>>,
    pub storage_bag_id: Option<Vec<u8>>,
    pub next_resolver: Option<TonAddress>,
    pub extra_records: Option<Value>,
    pub code_hash: Vec<u8>,
    pub last_transaction_lt: i64,
    pub updated_at: DateTime<Utc>,
    pub status: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DomainStatus {
    Active,
    Releasable,
    InAuction,
    AuctionEndedUnsettled,
}

impl std::str::FromStr for DomainStatus {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "active" => Ok(Self::Active),
            "releasable" => Ok(Self::Releasable),
            "in_auction" => Ok(Self::InAuction),
            "auction_ended_unsettled" => Ok(Self::AuctionEndedUnsettled),
            other => Err(format!("unknown domain status: {other}")),
        }
    }
}

impl DomainWithStatus {
    pub fn status(&self) -> DomainStatus {
        self.status
            .parse()
            .expect("domain_live_status view only ever emits known status strings")
    }
}
