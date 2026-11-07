//! Where one item sits in a shared buffer, so a list of items makes no allocation per item.

/// The byte range `start..end` of a shared buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
	pub start: usize,
	pub end: usize,
}

impl Span {
	pub const fn new(start: usize, end: usize) -> Self {
		return Self { start, end };
	}

	pub fn text(self, text: &str) -> &str {
		return text.get(self.start..self.end).unwrap_or_default();
	}

	pub fn bytes(self, bytes: &[u8]) -> &[u8] {
		return bytes.get(self.start..self.end).unwrap_or_default();
	}
}
