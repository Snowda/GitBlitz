//! A list of paths kept in one buffer: each path is a span of one `String`, so the list makes no allocation per path.

use std::fmt::{Debug, Formatter, Result as FmtResult};
use std::path::Path;

pub(crate) use crate::span::Span;

/// Paths relative to the listed directory, with `/` separators, sorted as [`Path`]s.
#[derive(Default)]
pub struct FileList {
	text: String,
	spans: Vec<Span>,
}

impl FileList {
	/// The list whose paths are the `spans` of `text`, in that order.
	pub(crate) const fn from_parts(text: String, spans: Vec<Span>) -> Self {
		return Self { text, spans };
	}

	/// The non-empty NUL-separated paths of `text`, as `git ls-files -z` prints them, in that order.
	pub(crate) fn from_nul_separated(text: String) -> Self {
		let mut spans: Vec<Span> = Vec::new();
		let mut start: usize = 0;
		for part in text.split('\0') {
			let end: usize = start + part.len();
			if !part.is_empty() {
				spans.push(Span::new(start, end));
			}
			start = end + 1;
		}
		return Self { text, spans };
	}

	#[must_use]
	pub const fn len(&self) -> usize {
		return self.spans.len();
	}

	#[must_use]
	pub const fn is_empty(&self) -> bool {
		return self.spans.is_empty();
	}

	/// Each path, borrowed from the listing's buffer.
	pub fn iter(&self) -> impl ExactSizeIterator<Item = &Path> + DoubleEndedIterator {
		return self.texts().map(Path::new);
	}

	pub(crate) fn texts(&self) -> impl ExactSizeIterator<Item = &str> + DoubleEndedIterator {
		return self.spans.iter().map(|span: &Span| return span.text(&self.text));
	}

	/// Appends `path` with each `\` made a `/`, as git prints a path on Windows.
	pub(crate) fn push_slashed(&mut self, path: &str) {
		let start: usize = self.text.len();
		self.text.extend(path.chars().map(|c: char| if c == '\\' { '/' } else { c }));
		self.spans.push(Span::new(start, self.text.len()));
	}

	pub(crate) fn last(&self) -> Option<&str> {
		return self.spans.last().map(|span: &Span| return span.text(&self.text));
	}

	pub(crate) fn pop(&mut self) {
		if let Some(span) = self.spans.pop() {
			self.text.truncate(span.start);
		}
	}

	/// Keeps only the paths under `prefix`, each made relative to it; the buffer is not copied.
	pub(crate) fn retain_under(&mut self, prefix: &str) {
		let text: &str = &self.text;
		self.spans.retain_mut(|span: &mut Span| {
			let is_under: bool = span.text(text).starts_with(prefix);
			span.start += if is_under { prefix.len() } else { 0 };
			return is_under;
		});
	}

	/// Sorts the paths as [`Path`]s compare (by component), stably.
	pub(crate) fn sort(&mut self) {
		let text: &str = &self.text;
		self.spans.sort_by(|a: &Span, b: &Span| return Path::new(a.text(text)).cmp(Path::new(b.text(text))));
	}

	/// Drops each path equal to the one before it.
	pub(crate) fn dedup(&mut self) {
		let text: &str = &self.text;
		self.spans.dedup_by(|a: &mut Span, b: &mut Span| return Path::new(a.text(text)) == Path::new(b.text(text)));
	}
}

impl PartialEq for FileList {
	fn eq(&self, other: &Self) -> bool {
		return self.iter().eq(other.iter());
	}
}

impl Eq for FileList {}

impl<P: AsRef<Path>> PartialEq<[P]> for FileList {
	fn eq(&self, other: &[P]) -> bool {
		return self.iter().eq(other.iter().map(AsRef::as_ref));
	}
}

impl<P: AsRef<Path>, const N: usize> PartialEq<[P; N]> for FileList {
	fn eq(&self, other: &[P; N]) -> bool {
		return *self == other[..];
	}
}

impl Debug for FileList {
	fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
		return f.debug_list().entries(self.iter()).finish();
	}
}

#[cfg(test)]
mod tests {
	use std::path::{Path, PathBuf};

	use super::{FileList, Span};

	#[test]
	fn nul_separated_text_skips_empty_parts_and_keeps_its_order() {
		let files: FileList = FileList::from_nul_separated(String::from("b.rs\0\0a.rs\0"));
		assert_eq!(files, ["b.rs", "a.rs"]);
		assert_eq!((files.len(), files.is_empty()), (2, false));
		assert!(FileList::from_nul_separated(String::new()).is_empty(), "no text lists nothing");
	}

	// Path order compares components, so `a/b` sorts before `a.b`; plain string order puts `a.b` first.
	#[test]
	fn sort_uses_path_order_not_string_order() {
		let mut files: FileList = FileList::from_nul_separated(String::from("a.b\0a/b\0a\0"));
		files.sort();
		assert_eq!(files, ["a", "a/b", "a.b"]);
		let mut strings: [&str; 3] = ["a.b", "a/b", "a"];
		strings.sort_unstable();
		assert_ne!(files, strings, "the two orders differ for this input");
	}

	#[test]
	fn dedup_drops_only_adjacent_equal_paths() {
		let mut files: FileList = FileList::from_nul_separated(String::from("a\0a\0b\0a\0"));
		files.dedup();
		assert_eq!(files, ["a", "b", "a"]);
	}

	#[test]
	fn retain_under_keeps_the_subtree_relative_to_the_prefix() {
		let mut files: FileList = FileList::from_nul_separated(String::from("sub/a.rs\0top.rs\0sub/deep/b.rs\0subway.rs\0"));
		files.retain_under("sub/");
		assert_eq!(files, ["a.rs", "deep/b.rs"]);
		files.retain_under("");
		assert_eq!(files, ["a.rs", "deep/b.rs"], "an empty prefix keeps everything unchanged");
	}

	#[test]
	fn push_slashed_turns_backslashes_into_slashes_and_pop_removes_the_last_path() {
		let mut files: FileList = FileList::default();
		files.push_slashed("kept.rs");
		files.push_slashed("dir\\sub\\x.rs");
		assert_eq!(files.last(), Some("dir/sub/x.rs"));
		files.pop();
		assert_eq!(files, ["kept.rs"]);
		assert_eq!(files.last(), Some("kept.rs"));
		files.push_slashed("next.rs");
		assert_eq!(files, ["kept.rs", "next.rs"], "pop also gave back the popped path's text");
		files.pop();
		files.pop();
		files.pop();
		assert_eq!((files.last(), files.is_empty()), (None, true));
	}

	// Equality is over the paths, not the buffer: different layouts of the same paths are equal.
	#[test]
	fn equality_compares_paths_not_layout() {
		let parts: FileList = FileList::from_parts(String::from("xa.rsb.rs"), vec![Span::new(1, 5), Span::new(5, 9)]);
		let text: FileList = FileList::from_nul_separated(String::from("a.rs\0b.rs"));
		assert_eq!(parts, text);
		assert_eq!(parts, [PathBuf::from("a.rs"), PathBuf::from("b.rs")]);
		assert_eq!(parts, *[Path::new("a.rs"), Path::new("b.rs")].as_slice(), "a slice compares like an array");
		assert_ne!(parts, [Path::new("a.rs")], "a shorter list differs");
		assert_ne!(parts, ["a.rs", "c.rs"], "a different path differs");
		assert_ne!(parts, FileList::from_nul_separated(String::from("b.rs\0a.rs")), "order matters");
		assert_eq!(parts.iter().next_back(), Some(Path::new("b.rs")));
		assert_eq!(format!("{parts:?}"), r#"["a.rs", "b.rs"]"#);
	}
}
