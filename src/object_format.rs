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

	/// The `extensions.objectFormat` value that selects this format.
	#[cfg(feature = "list")]
	#[must_use]
	pub const fn name(self) -> &'static str {
		return match self {
			Self::Sha1 => "sha1",
			Self::Sha256 => "sha256",
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

	#[cfg(feature = "list")]
	#[test]
	fn each_format_is_named_as_extensions_object_format_spells_it() {
		assert_eq!([ObjectFormat::Sha1, ObjectFormat::Sha256].map(ObjectFormat::name), ["sha1", "sha256"]);
	}
}
