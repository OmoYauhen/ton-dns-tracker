//! TON addresses, stored as 33 raw bytes: a 1-byte workchain plus the
//! 32-byte account hash. That's the actual information content of an
//! address -- the "friendly" base64 form wallets show you just adds a
//! flag byte and a CRC16 checksum on top, both derivable, not data.
//!
//! Parsing/formatting is hand-rolled here rather than delegated to
//! tonlib-core's own `TonAddress`, because its exact field/method names
//! weren't verifiable from this sandbox (no crates.io/docs.rs access to
//! pin against). The raw `wc:hex` form and the CRC16/XMODEM friendly
//! form are both stable, documented parts of the TON address spec, so
//! hand-rolling them is low-risk; double check this against a real
//! address once you can run `cargo test` for real.

use diesel::backend::Backend;
use diesel::deserialize::{self, FromSql};
use diesel::pg::Pg;
use diesel::serialize::{self, Output, ToSql};
use diesel::sql_types::Binary;
use diesel::{AsExpression, FromSqlRow};
use std::fmt;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum TonAddressError {
    #[error("expected 33 raw bytes (1 workchain + 32 hash), got {0}")]
    WrongLength(usize),
    #[error("invalid raw address string: {0}")]
    InvalidRaw(String),
    #[error("invalid friendly address string: {0}")]
    InvalidFriendly(String),
    #[error("friendly address checksum mismatch")]
    BadChecksum,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, AsExpression, FromSqlRow)]
#[diesel(sql_type = Binary)]
pub struct TonAddress {
    pub workchain: i8,
    pub hash: [u8; 32],
}

impl TonAddress {
    pub const MASTERCHAIN: i8 = -1;
    pub const BASECHAIN: i8 = 0;

    pub fn new(workchain: i8, hash: [u8; 32]) -> Self {
        Self { workchain, hash }
    }

    /// The 33-byte binary form stored in Postgres: workchain byte + hash.
    pub fn to_bytes(&self) -> [u8; 33] {
        let mut out = [0u8; 33];
        out[0] = self.workchain as u8;
        out[1..].copy_from_slice(&self.hash);
        out
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, TonAddressError> {
        if bytes.len() != 33 {
            return Err(TonAddressError::WrongLength(bytes.len()));
        }
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&bytes[1..]);
        Ok(Self {
            workchain: bytes[0] as i8,
            hash,
        })
    }

    /// Parses the raw form, e.g.
    /// "0:83dfd552e63729b472fcbcc8c45ebcc6691702558b68ec7527e1ba403a0f31a".
    pub fn from_raw(s: &str) -> Result<Self, TonAddressError> {
        let (wc, hex) = s
            .split_once(':')
            .ok_or_else(|| TonAddressError::InvalidRaw(s.to_string()))?;
        let workchain: i8 = wc
            .parse()
            .map_err(|_| TonAddressError::InvalidRaw(s.to_string()))?;
        if hex.len() != 64 {
            return Err(TonAddressError::InvalidRaw(s.to_string()));
        }
        let mut hash = [0u8; 32];
        for (i, slot) in hash.iter_mut().enumerate() {
            *slot = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16)
                .map_err(|_| TonAddressError::InvalidRaw(s.to_string()))?;
        }
        Ok(Self { workchain, hash })
    }

    pub fn to_raw(&self) -> String {
        let mut hex = String::with_capacity(64);
        for b in self.hash {
            hex.push_str(&format!("{:02x}", b));
        }
        format!("{}:{}", self.workchain, hex)
    }

    /// Parses the "friendly" base64 form (1 flag byte + 1 workchain byte +
    /// 32-byte hash + 2-byte CRC16/XMODEM) used by wallets and most TON
    /// tooling. The bounceable/testnet flag bits are accepted but not
    /// surfaced -- we only need workchain + hash for identity here.
    pub fn from_friendly(s: &str) -> Result<Self, TonAddressError> {
        use base64::Engine;
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(s)
            .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(s))
            .map_err(|_| TonAddressError::InvalidFriendly(s.to_string()))?;
        if bytes.len() != 36 {
            return Err(TonAddressError::InvalidFriendly(s.to_string()));
        }
        let (body, checksum) = bytes.split_at(34);
        if checksum != crc16_xmodem(body) {
            return Err(TonAddressError::BadChecksum);
        }
        let workchain = body[1] as i8;
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&body[2..34]);
        Ok(Self { workchain, hash })
    }

    /// Renders the non-bounceable, mainnet friendly form ("UQ..."/"0Q...").
    /// Wallets (Tonkeeper etc.) and explorers default to showing this form
    /// rather than the bounceable "EQ..." one these days, so that's what
    /// gets displayed -- `from_friendly` accepts either on the way in
    /// regardless (it doesn't surface the bounceable/testnet flag bits).
    pub fn to_friendly(&self) -> String {
        use base64::Engine;
        let mut body = [0u8; 34];
        body[0] = 0x51; // non-bounceable, mainnet
        body[1] = self.workchain as u8;
        body[2..].copy_from_slice(&self.hash);
        let checksum = crc16_xmodem(&body);
        let mut full = Vec::with_capacity(36);
        full.extend_from_slice(&body);
        full.extend_from_slice(&checksum);
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(full)
    }
}

impl std::str::FromStr for TonAddress {
    type Err = TonAddressError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.contains(':') {
            Self::from_raw(s)
        } else {
            Self::from_friendly(s)
        }
    }
}

impl fmt::Display for TonAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_friendly())
    }
}

fn crc16_xmodem(data: &[u8]) -> [u8; 2] {
    const CRC: crc::Crc<u16> = crc::Crc::<u16>::new(&crc::CRC_16_XMODEM);
    CRC.checksum(data).to_be_bytes()
}

impl ToSql<Binary, Pg> for TonAddress {
    fn to_sql<'b>(&'b self, out: &mut Output<'b, '_, Pg>) -> serialize::Result {
        // Not delegating to `<[u8] as ToSql>::to_sql` here: that needs a
        // reference living as long as `'b`, but `self.to_bytes()` returns
        // an owned array, so `&self.to_bytes()` is a temporary that's
        // dropped too soon. Writing straight into `out` sidesteps that.
        use std::io::Write;
        out.write_all(&self.to_bytes())?;
        Ok(serialize::IsNull::No)
    }
}

impl FromSql<Binary, Pg> for TonAddress {
    fn from_sql(bytes: <Pg as Backend>::RawValue<'_>) -> deserialize::Result<Self> {
        let raw = <Vec<u8> as FromSql<Binary, Pg>>::from_sql(bytes)?;
        Ok(TonAddress::from_bytes(&raw)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_roundtrip() {
        // Built from a known-good 32-byte pattern (not hand-typed hex)
        // after a hand-typed fixture here turned out to be one character
        // short and silently tested nothing.
        let hex: String = (0..32).map(|i| format!("{:02x}", i as u8)).collect();
        let raw = format!("0:{hex}");

        let addr = TonAddress::from_raw(&raw).unwrap();
        assert_eq!(addr.workchain, 0);
        assert_eq!(addr.to_raw(), raw);
    }

    #[test]
    fn bytes_roundtrip() {
        let addr = TonAddress::new(0, [0xAB; 32]);
        let bytes = addr.to_bytes();
        assert_eq!(bytes.len(), 33);
        assert_eq!(TonAddress::from_bytes(&bytes).unwrap(), addr);
    }

    #[test]
    fn friendly_roundtrip() {
        let addr = TonAddress::new(0, [0xCD; 32]);
        let friendly = addr.to_friendly();
        assert_eq!(TonAddress::from_friendly(&friendly).unwrap(), addr);
        // Also exercised through the dispatching FromStr/Display impls.
        assert_eq!(friendly.parse::<TonAddress>().unwrap(), addr);
        assert_eq!(addr.to_string(), friendly);
    }

    #[test]
    fn friendly_rejects_bad_checksum() {
        let addr = TonAddress::new(0, [0xCD; 32]);
        let mut friendly = addr.to_friendly();
        friendly.pop();
        friendly.push(if friendly.ends_with('A') { 'B' } else { 'A' });
        assert!(TonAddress::from_friendly(&friendly).is_err());
    }
}
