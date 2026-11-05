use std::fmt::{Display, Formatter, Result as FmtResult};
use std::str::FromStr;

use arrayvec::ArrayString;
use thiserror::Error as ThisError;

use crate::object_format::ObjectFormat;

/// Bytes in a SHA-1 object id, the only width the object reader reads.
pub const OID_LEN: usize = ObjectFormat::Sha1.raw_len();
/// Hex digits in a SHA-1 object id.
pub const OID_HEX_LEN: usize = ObjectFormat::Sha1.hex_len();
const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";
const NIBBLE_BITS: u32 = 4;
const LOW_NIBBLE: u8 = 0x0f;

/// A SHA-1 object id in its 20 raw bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Oid([u8; OID_LEN]);

/// Text that is not 40 hex digits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ThisError)]
#[error("not a 40-hex-digit object id ({len} bytes)")]
pub struct OidError {
	pub len: usize,
}

impl Oid {
	#[must_use]
	pub const fn from_raw(raw: [u8; OID_LEN]) -> Self {
		return Self(raw);
	}

	#[must_use]
	pub const fn as_raw(&self) -> &[u8; OID_LEN] {
		return &self.0;
	}

	/// The id from 40 hex digits, in either case, as git accepts a full id.
	///
	/// # Errors
	/// [`OidError`] for any other length or a non-hex digit.
	pub fn parse(text: &str) -> Result<Self, OidError> {
		let err: OidError = OidError { len: text.len() };
		let bytes: &[u8] = text.as_bytes();
		if bytes.len() != OID_LEN * 2 {
			return Err(err);
		}
		let mut raw: [u8; OID_LEN] = [0; OID_LEN];
		for (out, &[high, low]) in raw.iter_mut().zip(bytes.as_chunks::<2>().0) {
			let (Some(high), Some(low)) = (nibble(high), nibble(low)) else {
				return Err(err);
			};
			*out = (high << NIBBLE_BITS) | low;
		}
		return Ok(Self(raw));
	}

	/// The id from the start of `bytes`, or `None` when fewer than 20 bytes remain.
	#[must_use]
	pub fn from_prefix(bytes: &[u8]) -> Option<Self> {
		let raw: &[u8; OID_LEN] = bytes.first_chunk::<OID_LEN>()?;
		return Some(Self(*raw));
	}

	/// The id in hex: 40 lowercase digits, kept on the stack.
	#[must_use]
	pub fn hex(&self) -> ArrayString<OID_HEX_LEN> {
		let mut out: ArrayString<OID_HEX_LEN> = ArrayString::new();
		self.0.iter().for_each(|byte: &u8| {
			out.push(char::from(HEX_DIGITS[usize::from(byte >> NIBBLE_BITS)]));
			out.push(char::from(HEX_DIGITS[usize::from(byte & LOW_NIBBLE)]));
		});
		return out;
	}
}

impl FromStr for Oid {
	type Err = OidError;

	fn from_str(text: &str) -> Result<Self, OidError> {
		return Self::parse(text);
	}
}

impl Display for Oid {
	fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
		return f.write_str(&self.hex());
	}
}

const fn nibble(digit: u8) -> Option<u8> {
	const TEN: u8 = 10;
	return match digit {
		b'0'..=b'9' => Some(digit - b'0'),
		b'a'..=b'f' => Some(digit - b'a' + TEN),
		b'A'..=b'F' => Some(digit - b'A' + TEN),
		_ => None,
	};
}

#[cfg(test)]
mod tests {
	use arrayvec::ArrayString;

	use super::{Oid, OidError, OID_HEX_LEN, OID_LEN};

	const HEX: &str = "0123456789abcdef0123456789abcdef01234567";

	#[test]
	fn hex_round_trips_and_uppercase_reads_the_same() -> Result<(), OidError> {
		let oid: Oid = HEX.parse()?;
		assert_eq!(oid.to_string(), HEX);
		assert_eq!(Oid::parse(&HEX.to_uppercase())?, oid);
		assert_eq!(oid.as_raw()[0], 0x01, "the first byte holds the first two digits");
		return Ok(());
	}

	#[test]
	fn hex_is_two_lowercase_digits_per_byte() -> Result<(), OidError> {
		assert_eq!(HEX.parse::<Oid>()?.hex().as_str(), HEX);
		// Oracle: std's `{:02x}` for every byte value, at the first and last position.
		for byte in 0..=u8::MAX {
			let mut raw: [u8; OID_LEN] = [0; OID_LEN];
			(raw[0], raw[OID_LEN - 1]) = (byte, byte);
			let hex: ArrayString<OID_HEX_LEN> = Oid::from_raw(raw).hex();
			let pair: &str = &format!("{byte:02x}");
			let zeros: bool = hex[2..OID_HEX_LEN - 2].bytes().all(|b| b == b'0');
			assert!(hex.starts_with(pair) && hex.ends_with(pair) && zeros, "byte {byte:#04x}: {hex}");
		}
		return Ok(());
	}

	#[test]
	fn other_lengths_and_non_hex_are_refused() {
		assert_eq!(Oid::parse(&HEX[1..]), Err(OidError { len: 39 }));
		assert_eq!(Oid::parse(&format!("{HEX}0")), Err(OidError { len: 41 }));
		assert_eq!(Oid::parse(&format!("g{}", &HEX[1..])), Err(OidError { len: 40 }));
		assert_eq!(OidError { len: 3 }.to_string(), "not a 40-hex-digit object id (3 bytes)");
	}

	#[test]
	fn from_prefix_needs_twenty_bytes() {
		let bytes: [u8; OID_LEN + 1] = [7; OID_LEN + 1];
		assert_eq!(Oid::from_prefix(&bytes), Some(Oid::from_raw([7; OID_LEN])));
		assert_eq!(Oid::from_prefix(&bytes[..OID_LEN - 1]), None);
	}
}
