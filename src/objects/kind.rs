/// The type of a git object, as `git cat-file -t` names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ObjectKind {
	Commit = 1,
	Tree = 2,
	Blob = 3,
	Tag = 4,
}

impl ObjectKind {
	/// The name git prints for this type.
	#[must_use]
	pub const fn name(self) -> &'static str {
		return match self {
			Self::Commit => "commit",
			Self::Tree => "tree",
			Self::Blob => "blob",
			Self::Tag => "tag",
		};
	}

	/// The type a loose object header or `cat-file` names.
	#[must_use]
	pub fn from_name(name: &[u8]) -> Option<Self> {
		return [Self::Commit, Self::Tree, Self::Blob, Self::Tag].into_iter().find(|kind| return kind.name().as_bytes() == name);
	}

	/// The type a pack entry header's 3-bit code names; 6 and 7 are deltas, not types.
	#[must_use]
	pub const fn from_pack_code(code: u8) -> Option<Self> {
		return match code {
			1 => Some(Self::Commit),
			2 => Some(Self::Tree),
			3 => Some(Self::Blob),
			4 => Some(Self::Tag),
			_ => None,
		};
	}
}

#[cfg(test)]
mod tests {
	use super::ObjectKind;

	#[test]
	fn names_and_pack_codes_round_trip() {
		[ObjectKind::Commit, ObjectKind::Tree, ObjectKind::Blob, ObjectKind::Tag].into_iter().zip(1..).for_each(|(kind, code)| {
			assert_eq!(ObjectKind::from_name(kind.name().as_bytes()), Some(kind));
			assert_eq!(ObjectKind::from_pack_code(code), Some(kind));
		});
		assert_eq!(ObjectKind::from_name(b"blobs"), None);
		assert_eq!([0, 5, 6, 7].map(ObjectKind::from_pack_code), [None; 4]);
	}
}
