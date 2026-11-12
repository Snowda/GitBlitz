//! Batch answers in one shared buffer: each item is a header and a span, so a batch makes no allocation per item.

use std::fmt::{Debug, Formatter, Result as FmtResult};

use super::{Header, Object, ObjectKind, Oid};
use crate::span::Span;

/// One object of an [`Objects`] batch, its body borrowed from the batch. `body` is `None` over the caller's cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObjectRef<'o> {
	pub kind: ObjectKind,
	pub size: u64,
	pub body: Option<&'o [u8]>,
}

impl ObjectRef<'_> {
	/// This object with its own copy of the body.
	#[must_use]
	pub fn to_object(self) -> Object {
		return Object { kind: self.kind, size: self.size, body: self.body.map(<[u8]>::to_vec) };
	}
}

/// Objects as `git cat-file --batch` prints them, in the order asked. `None` for an object that git reports as absent.
#[derive(Clone, Default)]
pub struct Objects {
	buf: Vec<u8>,
	slots: Vec<Option<(Header, Option<Span>)>>,
}

impl Objects {
	/// An empty batch with room for `count` objects.
	pub(crate) fn with_capacity(count: usize) -> Self {
		return Self { buf: Vec::new(), slots: Vec::with_capacity(count) };
	}

	/// The batch over `buf`, git's output, with each slot's body a span of it.
	pub(crate) const fn from_parts(buf: Vec<u8>, slots: Vec<Option<(Header, Option<Span>)>>) -> Self {
		return Self { buf, slots };
	}

	/// An object whose body is not read: over the cap.
	pub(crate) fn push_unread(&mut self, header: Header) {
		self.slots.push(Some((header, None)));
	}

	/// An object whose body `read` puts onto the end of the shared buffer. On an error, the batch stays as it was.
	pub(crate) fn push_read<E>(&mut self, read: impl FnOnce(&mut Vec<u8>) -> Result<Header, E>) -> Result<(), E> {
		let start: usize = self.buf.len();
		let header: Header = read(&mut self.buf)?;
		self.slots.push(Some((header, Some(Span::new(start, self.buf.len())))));
		return Ok(());
	}

	#[must_use]
	pub const fn len(&self) -> usize {
		return self.slots.len();
	}

	/// The bytes the shared buffer holds.
	#[cfg(test)]
	pub(crate) const fn buffer_len(&self) -> usize {
		return self.buf.len();
	}

	#[must_use]
	pub const fn is_empty(&self) -> bool {
		return self.slots.is_empty();
	}

	/// The object at `at`. `None` past the end or where git reported the object as absent.
	#[must_use]
	pub fn get(&self, at: usize) -> Option<ObjectRef<'_>> {
		return self.slots.get(at).copied().flatten().map(|slot| return self.object(slot));
	}

	/// Each object in the order asked, `None` where git reported it as absent.
	pub fn iter(&self) -> impl ExactSizeIterator<Item = Option<ObjectRef<'_>>> + DoubleEndedIterator {
		return self.slots.iter().map(|slot| return slot.map(|slot| return self.object(slot)));
	}

	fn object(&self, (header, body): (Header, Option<Span>)) -> ObjectRef<'_> {
		return ObjectRef { kind: header.kind, size: header.size, body: body.map(|span| return span.bytes(&self.buf)) };
	}
}

// Two batches are equal when their objects are, whatever the layout of their buffers.
impl PartialEq for Objects {
	fn eq(&self, other: &Self) -> bool {
		return self.iter().eq(other.iter());
	}
}

impl Eq for Objects {}

impl Debug for Objects {
	fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
		return f.debug_list().entries(self.iter()).finish();
	}
}

/// One line of `git ls-tree --full-tree -l`: canonical mode, type, id, size (blobs only) and path from the top.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TreeEntry<'t> {
	pub mode: u32,
	pub kind: ObjectKind,
	pub oid: Oid,
	pub size: Option<u64>,
	pub path: &'t str,
}

/// A [`TreeEntry`] without its path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TreeHead {
	pub mode: u32,
	pub kind: ObjectKind,
	pub oid: Oid,
	pub size: Option<u64>,
}

impl TreeHead {
	pub(crate) const fn with_path(self, path: &str) -> TreeEntry<'_> {
		return TreeEntry { mode: self.mode, kind: self.kind, oid: self.oid, size: self.size, path };
	}
}

/// What `git ls-tree --full-tree -l` prints, in its order, the paths held in one shared buffer.
#[derive(Clone, Default)]
pub struct TreeEntries {
	buf: String,
	entries: Vec<(TreeHead, Span)>,
}

impl TreeEntries {
	/// The entries over `buf`, git's output, with each path a span of it.
	pub(crate) const fn from_parts(buf: String, entries: Vec<(TreeHead, Span)>) -> Self {
		return Self { buf, entries };
	}

	/// An entry with `path` copied onto the end of the shared buffer.
	pub(crate) fn push(&mut self, head: TreeHead, path: &str) {
		let start: usize = self.buf.len();
		self.buf.push_str(path);
		self.entries.push((head, Span::new(start, self.buf.len())));
	}

	/// Puts the entries in git's tree order, a subtree's name sorted as if it ended in `/`, and drops repeats.
	pub(crate) fn sort_and_dedup(&mut self) {
		let Self { buf, entries } = self;
		let key = |(head, path): &(TreeHead, Span)| {
			let slash: Option<u8> = (head.kind == ObjectKind::Tree).then_some(b'/');
			return path.text(buf).bytes().chain(slash);
		};
		entries.sort_by(|a, b| return key(a).cmp(key(b)));
		entries.dedup_by(|a, b| return a.0 == b.0 && a.1.text(buf) == b.1.text(buf));
	}

	#[must_use]
	pub const fn len(&self) -> usize {
		return self.entries.len();
	}

	#[must_use]
	pub const fn is_empty(&self) -> bool {
		return self.entries.is_empty();
	}

	#[must_use]
	pub fn get(&self, at: usize) -> Option<TreeEntry<'_>> {
		return self.entries.get(at).map(|&(head, path)| return head.with_path(path.text(&self.buf)));
	}

	pub fn iter(&self) -> impl ExactSizeIterator<Item = TreeEntry<'_>> + DoubleEndedIterator {
		return self.entries.iter().map(|&(head, path)| return head.with_path(path.text(&self.buf)));
	}
}

impl PartialEq for TreeEntries {
	fn eq(&self, other: &Self) -> bool {
		return self.iter().eq(other.iter());
	}
}

impl Eq for TreeEntries {}

impl Debug for TreeEntries {
	fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
		return f.debug_list().entries(self.iter()).finish();
	}
}

#[cfg(test)]
mod tests {
	use super::{ObjectRef, Objects, TreeEntries, TreeEntry, TreeHead};
	use crate::objects::{Header, Object, ObjectKind, Oid};
	use crate::span::Span;
	use crate::testrepo::TestResult;

	const BLOB: Header = Header { kind: ObjectKind::Blob, size: 2 };

	// A read slot, an unread slot and an empty slot, the reads laid out as `buf`.
	fn three(buf: &[u8]) -> Objects {
		return Objects::from_parts(buf.to_vec(), vec![Some((BLOB, Some(Span::new(buf.len() - 2, buf.len())))), Some((BLOB, None)), None]);
	}

	#[test]
	fn a_batch_answers_each_slot_and_none_past_the_end() {
		let batch: Objects = three(b"ab");
		assert_eq!((batch.len(), batch.is_empty(), Objects::default().is_empty()), (3, false, true));
		assert_eq!(batch.get(0), Some(ObjectRef { kind: ObjectKind::Blob, size: 2, body: Some(b"ab".as_slice()) }));
		assert_eq!(batch.get(1).map(ObjectRef::to_object), Some(Object { kind: ObjectKind::Blob, size: 2, body: None }));
		assert_eq!((batch.get(2), batch.get(3)), (None, None), "missing, then past the end");
		assert!(batch.iter().rev().map(|object| return object.is_some()).eq([false, true, true]), "back to front");
		assert_eq!(format!("{batch:?}"), format!("{:?}", batch.iter().collect::<Vec<Option<ObjectRef<'_>>>>()));
	}

	// Equality compares objects, not buffers: junk before a body changes nothing, a different body does.
	#[test]
	fn batches_are_equal_when_their_objects_are() {
		assert_eq!(three(b"ab"), three(b"junk ab"));
		assert_ne!(three(b"ab"), three(b"ac"));
	}

	// A read that fails leaves the batch as it was. One that works adds a slot over what it appended.
	#[test]
	fn push_read_adds_a_slot_only_when_the_read_works() -> TestResult {
		let mut batch: Objects = Objects::with_capacity(2);
		assert_eq!(batch.push_read(|_| return Err("no")), Err("no"));
		assert_eq!((batch.len(), batch.buffer_len()), (0, 0));
		batch.push_read(|buf| {
			buf.extend_from_slice(b"ab");
			return Ok::<Header, String>(BLOB);
		})?;
		batch.push_unread(BLOB);
		assert_eq!(batch, Objects::from_parts(b"ab".to_vec(), vec![Some((BLOB, Some(Span::new(0, 2)))), Some((BLOB, None))]));
		return Ok(());
	}

	// Entries sort as git orders a tree, a subtree as if its name ended in `/`, and exact repeats drop.
	#[test]
	fn tree_entries_sort_like_git_and_drop_repeats() -> TestResult {
		let oid: Oid = Oid::parse("0123456789abcdef0123456789abcdef01234567").map_err(|e| return e.to_string())?;
		let blob: TreeHead = TreeHead { mode: 0o100_644, kind: ObjectKind::Blob, oid, size: Some(1) };
		let tree: TreeHead = TreeHead { mode: 0o040_000, kind: ObjectKind::Tree, oid, size: None };
		let mut entries: TreeEntries = TreeEntries::default();
		[(tree, "a"), (blob, "a.c"), (blob, "a.c"), (blob, "a-b")].iter().for_each(|&(head, path)| entries.push(head, path));
		entries.sort_and_dedup();
		let paths: Vec<&str> = entries.iter().map(|entry: TreeEntry<'_>| return entry.path).collect();
		assert_eq!(paths, ["a-b", "a.c", "a"], "'-' and '.' sort before '/'");
		assert_eq!((entries.len(), entries.is_empty(), TreeEntries::default().is_empty()), (3, false, true));
		assert_eq!((entries.get(2), entries.get(3)), (Some(tree.with_path("a")), None));
		assert_eq!(format!("{entries:?}"), format!("{:?}", entries.iter().collect::<Vec<TreeEntry<'_>>>()));
		return Ok(());
	}
}
