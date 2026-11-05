/// The hash git names objects with, as `extensions.objectFormat` sets it (SHA-1 when unset).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectFormat {
	Sha1,
	Sha256,
}

impl ObjectFormat {
	/// Bytes in one binary object id, as the index stores it.
	#[must_use]
	pub const fn raw_len(self) -> usize {
		return match self {
			Self::Sha1 => 20,
			Self::Sha256 => 32,
		};
	}

	/// Hex digits in one object id, as refs and `git rev-parse` print it.
	#[must_use]
	pub const fn hex_len(self) -> usize {
		return self.raw_len() * 2;
	}

	/// The format an `extensions.objectFormat` value selects; `None` for any other text, which git must judge.
	#[must_use]
	pub fn from_name(name: &str) -> Option<Self> {
		return match name {
			"sha1" => Some(Self::Sha1),
			"sha256" => Some(Self::Sha256),
			_ => None,
		};
	}
}

#[cfg(test)]
mod tests {
	use super::ObjectFormat;

	#[test]
	fn each_format_has_gits_id_widths() {
		assert_eq!([ObjectFormat::Sha1, ObjectFormat::Sha256].map(ObjectFormat::hex_len), [40, 64]);
		assert_eq!([ObjectFormat::Sha1, ObjectFormat::Sha256].map(ObjectFormat::raw_len), [20, 32]);
	}

	#[test]
	fn each_format_is_found_by_its_extensions_object_format_name() {
		assert_eq!(["sha1", "sha256"].map(ObjectFormat::from_name), [Some(ObjectFormat::Sha1), Some(ObjectFormat::Sha256)]);
		assert_eq!(["SHA256", "sha-1", ""].map(ObjectFormat::from_name), [None, None, None]);
	}
}
