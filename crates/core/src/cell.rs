//! Decodes a `.ton` domain NFT item's raw account data cell (the
//! `data_boc` returned by toncenter's `/accountStates`) without running
//! any TVM get-method. This mirrors `load_data()` in
//! `ton-blockchain/dns-contract`'s `func/nft-item.fc` field-for-field:
//!
//! ```text
//! uint256 index
//! MsgAddressInt collection_address
//! MsgAddressInt owner_address        \
//! cell content (ref)                  \  only present once the item
//! cell domain (ref)                   /  has been initialized by the
//! Maybe^Cell auction                 /   collection (see `init?` below)
//! uint64 last_fill_up_time          /
//! ```
//!
//! The cell layout and tonlib-core call conventions here (`load_coins`,
//! `load_dict`'s key/value reader signatures) were checked against
//! tonlib-core 0.24.6's actual source once it compiled, not guessed --
//! what's still unverified is the *semantic* mapping (does the decoded
//! `wallet`/`site`/`storage`/`dns_next_resolver` data actually match what
//! real domains show in wallets/explorers). Worth cross-checking a
//! handful of real domains against toncenter's
//! `/api/v3/dns/get-dns-records?domain=...` for the same names before
//! trusting this at scale.

use std::collections::HashMap;
use std::sync::OnceLock;

use num_bigint::BigUint;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tonlib_core::cell::{ArcCell, BagOfCells, Cell, TonCellError};

use crate::ton_address::TonAddress;

/// Converts a tonlib-core address (returned by `CellParser::load_address`)
/// into our own `TonAddress`, by round-tripping through its string form
/// rather than reading its internal fields directly -- tonlib-core's
/// exact `TonAddress` field names weren't verifiable from this sandbox
/// (docs.rs 404'd on that specific page), but every TON address type
/// implements `Display` in one of the two standard forms, and our own
/// `FromStr` already accepts both. Confirm once compiling.
fn to_ton_address(addr: impl std::fmt::Display) -> Result<TonAddress, CellError> {
    addr.to_string()
        .parse::<TonAddress>()
        .map_err(|_| CellError::UnexpectedLayout("could not convert tonlib-core address"))
}

#[derive(Debug, Error)]
pub enum CellError {
    #[error("ton cell error: {0}")]
    Cell(#[from] TonCellError),
    #[error("unexpected data layout: {0}")]
    UnexpectedLayout(&'static str),
    #[error("value out of expected range: {0}")]
    OutOfRange(&'static str),
    #[error("domain label is not valid utf-8")]
    InvalidDomainLabel,
    /// The item's `content` cell isn't the onchain-tagged records
    /// dictionary at all, so the domain has no resolvable DNS records --
    /// `dnsresolve` in nft-item.fc parses it the same way and would throw
    /// too. Real mainnet data, not a decoder bug: seen both as a bare
    /// `dns_adnl_address` record stored directly as the content cell, and
    /// as a 0x00 tag followed by a ref with no HashmapE Maybe-bit.
    #[error("content cell is not a dns records dictionary: {0}")]
    NotDnsRecords(&'static str),
}

#[derive(Debug, Clone)]
pub struct AuctionInfo {
    pub max_bid_address: TonAddress,
    pub max_bid_amount_nanoton: u64,
    pub end_time_unix: u64,
}

#[derive(Debug, Clone)]
pub struct InitializedDomainItem {
    pub index: BigUint,
    pub collection_address: TonAddress,
    pub owner_address: TonAddress,
    pub content: ArcCell,
    pub domain: ArcCell,
    pub auction: Option<AuctionInfo>,
    pub last_fill_up_time_unix: u64,
}

#[derive(Debug, Clone)]
pub enum NftItemState {
    /// The collection has reserved this index/address but hasn't sent
    /// the message that starts its first auction yet -- see the `~init?`
    /// branch in `recv_internal`. In practice, by the time a domain shows
    /// up in `/nft/items` for the collection it should already be past
    /// this state, but the contract models it explicitly so we do too.
    Uninitialized {
        index: BigUint,
        collection_address: TonAddress,
    },
    Initialized(InitializedDomainItem),
}

/// `zero_address()` in the FunC source: `addr_std` on the basechain with
/// an all-zero hash. `dns_balance_release` sets `owner_address` to this
/// while a domain is released-but-not-yet-settled; callers should treat
/// it as "no owner" rather than a real address.
pub fn is_zero_address(addr: &TonAddress) -> bool {
    addr.workchain == 0 && addr.hash == [0u8; 32]
}

pub fn decode_nft_item(data_boc: &[u8]) -> Result<NftItemState, CellError> {
    let boc = BagOfCells::parse(data_boc)?;
    let root = boc.single_root()?;
    let mut parser = root.parser();

    // Not run through `biguint_to_u64` like the other fields below -- the
    // .ton collection derives this from a hash of the domain name, so it
    // routinely exceeds u64 and needs the full 256 bits (see item_index
    // in the `domains` migration).
    let index = parser.load_uint(256)?;
    let collection_address = to_ton_address(parser.load_address()?)?;

    if parser.remaining_bits() == 0 {
        return Ok(NftItemState::Uninitialized {
            index,
            collection_address,
        });
    }

    let owner_address = to_ton_address(parser.load_address()?)?;
    let content = parser.next_reference()?;
    let domain = parser.next_reference()?;

    let has_auction = parser.load_uint(1)? == BigUint::from(1u8);
    let auction = if has_auction {
        let auction_cell = parser.next_reference()?;
        Some(decode_auction(&auction_cell)?)
    } else {
        None
    };

    let last_fill_up_time_unix = biguint_to_u64(&parser.load_uint(64)?, "last_fill_up_time")?;

    Ok(NftItemState::Initialized(InitializedDomainItem {
        index,
        collection_address,
        owner_address,
        content,
        domain,
        auction,
        last_fill_up_time_unix,
    }))
}

fn decode_auction(cell: &Cell) -> Result<AuctionInfo, CellError> {
    let mut parser = cell.parser();
    let max_bid_address = to_ton_address(parser.load_address()?)?;

    // `unpack_auction` in nft-item.fc reads this with `load_coins()`
    // (TON's VarUInteger(16) amount encoding: 4-bit length prefix + that
    // many bytes) rather than a fixed-width load_uint -- confirmed this
    // matches tonlib-core's own `load_coins` impl (same 4-bit-len-prefix
    // scheme) once the crate source was available to check.
    let max_bid_amount_nanoton = biguint_to_u64(&parser.load_coins()?, "auction max bid amount")?;

    let end_time_unix = biguint_to_u64(&parser.load_uint(64)?, "auction end time")?;

    Ok(AuctionInfo {
        max_bid_address,
        max_bid_amount_nanoton,
        end_time_unix,
    })
}

/// Decodes the `domain` ref cell into its label, e.g. `"alice"` for
/// `alice.ton`. Per the FunC comment this is raw bytes with no trailing
/// NUL, built from a single cell with no snake-format continuation, so
/// it's just "read all remaining bytes as utf-8".
pub fn decode_domain_label(domain_cell: &Cell) -> Result<String, CellError> {
    let mut parser = domain_cell.parser();
    let remaining_bits = parser.remaining_bits();
    if !remaining_bits.is_multiple_of(8) {
        return Err(CellError::UnexpectedLayout(
            "domain label cell is not byte-aligned",
        ));
    }
    let bytes = parser.load_bytes(remaining_bits / 8)?;
    String::from_utf8(bytes).map_err(|_| CellError::InvalidDomainLabel)
}

/// Decodes a plain-text TON "comment" message body: a leading 32-bit zero
/// tag (the standard convention marking a text comment rather than an
/// opcode-prefixed message) followed by raw UTF-8 bytes. Domain names are
/// short enough to always fit in one cell, so -- like `decode_domain_label`
/// above -- this doesn't handle the multi-cell "snake format" a long
/// comment would need.
///
/// Used by discovery to recover a domain's name from the message that
/// triggered its auction start: `nft-collection.fc` deploys a new item in
/// response to a plain comment (the domain name) sent directly to the
/// collection -- see ton-dns-indexer::discovery.
pub fn decode_text_comment(body_cell: &Cell) -> Result<String, CellError> {
    let mut parser = body_cell.parser();
    let tag = parser.load_uint(32)?;
    if tag != BigUint::from(0u8) {
        return Err(CellError::UnexpectedLayout("expected text comment tag (0)"));
    }
    let remaining_bits = parser.remaining_bits();
    if !remaining_bits.is_multiple_of(8) {
        return Err(CellError::UnexpectedLayout(
            "text comment body is not byte-aligned",
        ));
    }
    let bytes = parser.load_bytes(remaining_bits / 8)?;
    String::from_utf8(bytes).map_err(|_| CellError::InvalidDomainLabel)
}

/// `decode_text_comment`, given the message body's raw BOC bytes
/// (already base64-decoded) instead of an already-parsed `Cell` -- mirrors
/// `decode_nft_item`'s `data_boc: &[u8]` shape, so callers outside this
/// crate never need to touch `tonlib_core` directly.
pub fn decode_text_comment_boc(data_boc: &[u8]) -> Result<String, CellError> {
    let boc = BagOfCells::parse(data_boc)?;
    let root = boc.single_root()?;
    decode_text_comment(&root)
}

#[derive(Debug, Clone, Default)]
pub struct DecodedDnsRecords {
    pub wallet_address: Option<TonAddress>,
    /// Raw 256-bit ADNL key -- not a TonAddress, there's no workchain here.
    pub site_adnl: Option<[u8; 32]>,
    /// Raw 256-bit TON Storage bag id -- also not a TonAddress.
    pub storage_bag_id: Option<[u8; 32]>,
    pub next_resolver: Option<TonAddress>,
    /// category_hash(hex) -> raw value cell BOC(hex), for any category we
    /// don't special-case above, so nothing is silently dropped.
    pub extra: Map<String, Value>,
}

/// Decodes the `content` ref cell: an 8-bit onchain-data tag followed by
/// a `Maybe ^Cell` holding the actual category -> value dictionary
/// (`change_dns_record` builds this with `udict_set_ref`/`new_dict()`,
/// i.e. a real HashmapE(256, ^Cell), unlike the `auction` Maybe^Cell
/// above which just wraps a plain cell).
pub fn decode_dns_records(content_cell: &Cell) -> Result<DecodedDnsRecords, CellError> {
    let mut parser = content_cell.parser();
    let tag = parser.load_uint(8)?;
    if tag != BigUint::from(0u8) {
        return Err(CellError::NotDnsRecords("expected onchain content tag (0)"));
    }
    if parser.remaining_bits() == 0 {
        return Err(CellError::NotDnsRecords(
            "no records dictionary after onchain content tag",
        ));
    }

    let mut out = DecodedDnsRecords::default();

    // `load_dict` reads the Maybe-bit itself (and the ref behind it if
    // set), confirmed against tonlib-core 0.24's source -- no manual
    // peeling needed here, unlike `auction` above which really is a bare
    // Maybe^Cell rather than a HashmapE.
    let entries: HashMap<[u8; 32], ArcCell> =
        parser.load_dict(256, key_reader_256, |p| p.next_reference())?;

    for (key32, value_cell) in entries {
        // A known category's value not matching its expected TEP-81
        // layout is real, observed mainnet behavior, not just a
        // theoretical edge case: a domain's `storage` entry was seen with
        // a value cell only 24 bits long, far short of the 272-bit
        // dns_storage_address#7473 record the tag implies. One malformed
        // record shouldn't fail every *other* record in the dict, let
        // alone the whole domain's refresh (see the `Err(_)` fallthrough
        // below) -- so a decode failure here falls back to the same
        // best-effort hex-dump as a genuinely unrecognized category,
        // rather than aborting the function.
        if key32 == wallet_category_hash() {
            // dns_smc_address#9fd3 smc_addr:MsgAddressInt ... -- confirmed
            // against a real domain's on-chain data that this 16-bit tag
            // precedes the address and has to be skipped first, or
            // MsgAddressInt parsing desyncs (see load_dns_record_address).
            if let Ok(address) = load_dns_record_address(&value_cell) {
                out.wallet_address = Some(address);
                continue;
            }
        } else if key32 == site_category_hash() {
            // dns_adnl_address#ad01 adnl_addr:bits256 ...
            if let Ok(bytes) = load_fixed_32_after_tag(&value_cell) {
                out.site_adnl = Some(bytes);
                continue;
            }
        } else if key32 == storage_category_hash() {
            // dns_storage_address#7473 bag_id:bits256
            if let Ok(bytes) = load_fixed_32_after_tag(&value_cell) {
                out.storage_bag_id = Some(bytes);
                continue;
            }
        } else if key32 == next_resolver_category_hash() {
            // dns_next_resolver#ba93 resolver:MsgAddressInt
            if let Ok(address) = load_dns_record_address(&value_cell) {
                out.next_resolver = Some(address);
                continue;
            }
        }

        // Unrecognized category, or a known one whose value didn't
        // decode as expected (see above) -- best-effort hex-dump of this
        // cell's own bits. Doesn't capture nested ref cells
        // (re-serializing a full subtree back to a standalone BOC needs a
        // `BagOfCells`/`Cell` builder API we didn't verify the shape of),
        // which is fine for now since every known TEP-81 category is a
        // flat value -- revisit if an unrecognized category turns out to
        // carry references.
        let mut vp = value_cell.parser();
        let bits = vp.remaining_bits();
        let value_hex = if bits % 8 == 0 {
            hex::encode(vp.load_bytes(bits / 8)?)
        } else {
            String::new()
        };
        out.extra
            .insert(hex::encode(key32), Value::String(value_hex));
    }

    Ok(out)
}

/// Skips the 16-bit TEP-81 record tag then reads a fixed 256-bit payload
/// (`dns_adnl_address#ad01 adnl_addr:bits256 ...` /
/// `dns_storage_address#7473 bag_id:bits256`).
fn load_fixed_32_after_tag(cell: &Cell) -> Result<[u8; 32], CellError> {
    let mut parser = cell.parser();
    parser.load_uint(16)?;
    let bytes = parser.load_bytes(32)?;
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    Ok(out)
}

/// Skips the 16-bit TEP-81 record tag then reads a `MsgAddressInt`
/// (`dns_smc_address#9fd3 smc_addr:MsgAddressInt ...` /
/// `dns_next_resolver#ba93 resolver:MsgAddressInt`).
fn load_dns_record_address(cell: &Cell) -> Result<TonAddress, CellError> {
    let mut parser = cell.parser();
    parser.load_uint(16)?;
    to_ton_address(parser.load_address()?)
}

/// `sha256(category_name)` interpreted as the 256-bit dict key, matching
/// FunC's `"name"H` string-hash literal syntax (see TEP-81).
pub fn category_hash(name: &str) -> [u8; 32] {
    Sha256::digest(name.as_bytes()).into()
}

/// `KeyReader<[u8; 32]>` for `CellParser::load_dict`: tonlib-core hands
/// dict keys back as an already-parsed `BigUint`, so this just
/// re-encodes it as a fixed-width big-endian 32-byte array.
fn key_reader_256(key: &BigUint) -> Result<[u8; 32], TonCellError> {
    let bytes = key.to_bytes_be();
    let mut out = [0u8; 32];
    out[32 - bytes.len()..].copy_from_slice(&bytes);
    Ok(out)
}

macro_rules! cached_category_hash {
    ($fn_name:ident, $category:literal) => {
        pub fn $fn_name() -> [u8; 32] {
            static CACHE: OnceLock<[u8; 32]> = OnceLock::new();
            *CACHE.get_or_init(|| category_hash($category))
        }
    };
}

cached_category_hash!(wallet_category_hash, "wallet");
cached_category_hash!(site_category_hash, "site");
cached_category_hash!(storage_category_hash, "storage");
cached_category_hash!(next_resolver_category_hash, "dns_next_resolver");

fn biguint_to_u64(v: &BigUint, field: &'static str) -> Result<u64, CellError> {
    let bytes = v.to_bytes_be();
    if bytes.len() > 8 {
        return Err(CellError::OutOfRange(field));
    }
    let mut buf = [0u8; 8];
    buf[8 - bytes.len()..].copy_from_slice(&bytes);
    Ok(u64::from_be_bytes(buf))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    fn content_of(data_boc_b64: &str) -> ArcCell {
        let boc = base64::engine::general_purpose::STANDARD
            .decode(data_boc_b64)
            .unwrap();
        match decode_nft_item(&boc).unwrap() {
            NftItemState::Initialized(item) => item.content,
            NftItemState::Uninitialized { .. } => panic!("expected an initialized item"),
        }
    }

    // hostpredator.ton (mainnet): content is a bare dns_adnl_address
    // record (#ad01), not the onchain-tagged dictionary.
    #[test]
    fn bare_adnl_record_content_is_not_dns_records() {
        let content = content_of("te6ccgEBAwEAogAC1ZVOgPOs7wY8HkzH5J9Ot6obzk5RqzVV0TVDYLcKSDRrgBbumyvWQKh+MNgNZuNXEVrgnvxKwmGV8sMSbRT6GW2Z8AFvYNPq0emKEcYI7XoOjJZdnFPwNbnZNRZxbYled7epOAAAAADUSI2xAQIARq0BsmqyLZQyOlVD6fkCVpnXbJq/6c4hl0FkDshnCz2cSSIAABhob3N0cHJlZGF0b3I=");
        assert!(matches!(
            decode_dns_records(&content),
            Err(CellError::NotDnsRecords(_))
        ));
    }

    // chmonya.ton (mainnet): 0x00 tag, then a ref but no Maybe-bit.
    #[test]
    fn tag_without_dictionary_is_not_dns_records() {
        let content = content_of("te6ccgEBBQEAoAAC1SRcGo5mbwjOazoBgWGyOrUmV8fHf0UMuuYOI89j7MXDgBbumyvWQKh+MNgNZuNXEVrgnvxKwmGV8sMSbRT6GW2Z8AALTM6ryJrrPROnVcqiW4hKqc0z7tgEWzvFkpJsShaLNAAAAADVddZfAQIBAgADAA5jaG1vbnlhAQAEAD4AHWh0dHBzOi8vbmFtZXNwYWNlLm15L2xvZ28ucG5n");
        assert!(matches!(
            decode_dns_records(&content),
            Err(CellError::NotDnsRecords(_))
        ));
    }
}
