//! One repository's object store: its packs, then its loose objects. Deltas resolve across both.

use std::fs::{self, DirEntry};
use std::io::ErrorKind;
use std::mem;
use std::path::{Path, PathBuf};

use arrayvec::ArrayVec;

use super::delta::{self, Sizes, HEADER_MAX};
use super::idx::Idx;
use super::kind::ObjectKind;
use super::loose::{self, Loose};
use super::oid::Oid;
use super::pack::{EntryKind, Pack};
use super::zlib::{inflate, inflate_exact_append, Fill, InflateError, Input};
use super::{Header, Object};
use crate::error::{Miss, MissReason};

const PACK_DIR: &str = "pack";
const IDX_EXTENSION: &str = "idx";
const PACK_EXTENSION: &str = "pack";
const ALTERNATES: &str = "info/alternates";
/// Longest delta chain followed: git caps `pack.depth` at 4095, so a longer chain is a cycle or corruption.
pub const MAX_CHAIN: usize = 4096;
/// Most deltas a read applies, held on the stack; `gc --aggressive` packs to depth 50, `--depth 250` still fits.
pub const READ_DEPTH_MAX: usize = 256;

/// The (pack, offset) of each delta above a chain's base, top first.
type Chain = ArrayVec<(usize, u64), READ_DEPTH_MAX>;

/// Where an object lives.
enum Place {
	Packed { pack: usize, offset: u64 },
	Loose(Loose),
}

/// Where a delta chain's base object is: a whole pack entry or a loose object.
enum Base {
	Packed { pack: usize, offset: u64 },
	Loose(Loose),
}

/// The packs and loose objects under one `objects` directory.
pub struct Store {
	objects: PathBuf,
	packs: Vec<Pack>,
}

impl Store {
	/// Opens every `.idx` under `objects/pack` that has its `.pack`, as git's `prepare_packed_git` does.
	///
	/// # Errors
	/// A miss when a pack or its index does not open and check out.
	pub fn open(objects: &Path) -> Result<Self, Miss> {
		let dir: PathBuf = objects.join(PACK_DIR);
		let mut packs: Vec<Pack> = match fs::read_dir(&dir) {
			Ok(entries) => entries
				.filter_map(Result::ok)
				.map(|entry: DirEntry| return entry.path())
				.filter(|p| return is_pack_index(p))
				.map(|path: PathBuf| return Pack::open(Idx::open(&path)?))
				.collect::<Result<_, Miss>>()?,
			Err(err) if err.kind() == ErrorKind::NotFound => Vec::new(),
			Err(err) => return Err(Miss::io(&dir, &err)),
		};
		packs.sort_by(|a: &Pack, b: &Pack| return a.idx.path().cmp(b.idx.path()));
		return Ok(Self { objects: objects.to_path_buf(), packs });
	}

	/// `oid`'s type and size, without inflating its body: a delta's size comes from its own header.
	///
	/// # Errors
	/// `MissingObject` (or `Alternates`) when no copy is here; `Malformed` or `DeltaChain` for a bad store.
	pub fn header(&self, oid: &Oid) -> Result<Header, Miss> {
		let (pack, offset) = match self.place(oid)? {
			Place::Loose(loose) => return Ok(loose.header),
			Place::Packed { pack, offset } => (pack, offset),
		};
		let (entry, mut input) = self.pack(pack)?.entry(offset)?;
		let (size, first) = match entry.kind {
			EntryKind::Whole(kind) => return Ok(Header { kind, size: entry.size }),
			delta_kind => (self.delta_sizes(pack, &mut input)?.result, delta_kind),
		};
		return Ok(Header { kind: self.base_kind(pack, first)?, size });
	}

	/// `oid` whole.
	///
	/// # Errors
	/// As [`Self::header`], plus `Malformed` for a stream or delta that does not hold what its header says.
	pub fn read(&self, oid: &Oid) -> Result<Object, Miss> {
		let mut body: Vec<u8> = Vec::new();
		let header: Header = self.read_into(oid, &mut body)?;
		return Ok(Object { kind: header.kind, size: header.size, body: Some(body) });
	}

	/// [`Self::read`] with the body put onto the end of `out`; on an error, `out` keeps only what it held.
	pub fn read_into(&self, oid: &Oid, out: &mut Vec<u8>) -> Result<Header, Miss> {
		let start: usize = out.len();
		let kind: ObjectKind = match self.place(oid)? {
			Place::Loose(loose) => {
				loose.read_append(out)?;
				loose.header.kind
			}
			Place::Packed { pack, offset } => self.read_packed(pack, offset, out)?,
		};
		let size: u64 = u64::try_from(out.len() - start).map_err(|_| return Miss::new(&self.objects, MissReason::Malformed))?;
		return Ok(Header { kind, size });
	}

	fn place(&self, oid: &Oid) -> Result<Place, Miss> {
		for (pack, entry) in self.packs.iter().enumerate() {
			if let Some(offset) = entry.idx.find(oid)? {
				return Ok(Place::Packed { pack, offset });
			}
		}
		if let Some(found) = Loose::open(&self.objects, oid)? {
			return Ok(Place::Loose(found));
		}
		let alternates: PathBuf = self.objects.join(ALTERNATES);
		return match alternates.try_exists() {
			Ok(true) => Err(Miss::new(&alternates, MissReason::Alternates)),
			Ok(false) => Err(Miss::new(&loose::path(&self.objects, oid), MissReason::MissingObject)),
			Err(err) => Err(Miss::io(&alternates, &err)),
		};
	}

	fn pack(&self, pack: usize) -> Result<&Pack, Miss> {
		return self.packs.get(pack).ok_or_else(|| return Miss::new(&self.objects, MissReason::Malformed));
	}

	fn delta_sizes(&self, pack: usize, input: &mut Input<'_>) -> Result<Sizes, Miss> {
		let mut head: [u8; HEADER_MAX] = [0; HEADER_MAX];
		let filled: usize = match inflate(input, &mut head).map_err(|err| return self.inflate_miss(pack, err))? {
			Fill::Ended(len) => len,
			Fill::Full => HEADER_MAX,
		};
		return delta::sizes(head.get(..filled).unwrap_or_default()).map_err(|_| return self.malformed(pack));
	}

	// The type at the bottom of a delta chain, from entry headers alone.
	fn base_kind(&self, pack: usize, first: EntryKind) -> Result<ObjectKind, Miss> {
		let (mut pack, mut kind) = (pack, first);
		for _ in 0..MAX_CHAIN {
			let offset: u64 = match kind {
				EntryKind::Whole(found) => return Ok(found),
				EntryKind::OfsDelta(base) => base,
				EntryKind::RefDelta(base) => match self.place(&base)? {
					Place::Loose(loose) => return Ok(loose.header.kind),
					Place::Packed { pack: next, offset } => {
						pack = next;
						offset
					}
				},
			};
			kind = self.pack(pack)?.entry(offset)?.0.kind;
		}
		return Err(Miss::new(self.pack(pack)?.path(), MissReason::DeltaChain));
	}

	// Applies the chain's deltas from the bottom up, swapping two body buffers and reusing one for the ops.
	// The top delta, or a base with none above it, writes straight onto the end of `out`.
	fn read_packed(&self, pack: usize, offset: u64, out: &mut Vec<u8>) -> Result<ObjectKind, Miss> {
		let (base, deltas) = self.chain_base(pack, offset)?;
		let Some((&top, below)) = deltas.split_first() else {
			return self.read_base(&base, out);
		};
		let mut body: Vec<u8> = Vec::new();
		let kind: ObjectKind = self.read_base(&base, &mut body)?;
		let mut next: Vec<u8> = Vec::new();
		let mut ops: Vec<u8> = Vec::new();
		for &(at_pack, at_offset) in below.iter().rev() {
			self.delta_ops(at_pack, at_offset, &mut ops)?;
			delta::apply_into(&body, &ops, &mut next).map_err(|_| return self.malformed(at_pack))?;
			mem::swap(&mut body, &mut next);
		}
		self.delta_ops(top.0, top.1, &mut ops)?;
		delta::apply_append(&body, &ops, out).map_err(|_| return self.malformed(top.0))?;
		return Ok(kind);
	}

	// The instructions of the delta at `offset`, in place of what `ops` held.
	fn delta_ops(&self, pack: usize, offset: u64, ops: &mut Vec<u8>) -> Result<(), Miss> {
		let (entry, mut input) = self.pack(pack)?.entry(offset)?;
		ops.clear();
		return self.inflate_sized(pack, &mut input, entry.size, ops);
	}

	// The base's body onto the end of `out`, and its type.
	fn read_base(&self, base: &Base, out: &mut Vec<u8>) -> Result<ObjectKind, Miss> {
		return match base {
			Base::Loose(loose) => loose.read_append(out).map(|()| return loose.header.kind),
			&Base::Packed { pack, offset } => {
				let (entry, mut input) = self.pack(pack)?.entry(offset)?;
				let EntryKind::Whole(kind) = entry.kind else {
					return Err(self.malformed(pack));
				};
				self.inflate_sized(pack, &mut input, entry.size, out)?;
				Ok(kind)
			}
		};
	}

	// Where the chain's base object is, and the (pack, offset) of each delta above it, top first.
	fn chain_base(&self, pack: usize, offset: u64) -> Result<(Base, Chain), Miss> {
		let mut deltas: Chain = ArrayVec::new();
		let (mut pack_at, mut offset_at) = (pack, offset);
		let mut push = |at: (usize, u64)| -> Result<(), Miss> {
			if deltas.try_push(at).is_ok() {
				return Ok(());
			}
			return Err(Miss::new(self.pack(pack)?.path(), MissReason::DeltaChain));
		};
		let base: Base = loop {
			let (entry, _) = self.pack(pack_at)?.entry(offset_at)?;
			match entry.kind {
				EntryKind::Whole(_) => break Base::Packed { pack: pack_at, offset: offset_at },
				EntryKind::OfsDelta(base) => {
					push((pack_at, offset_at))?;
					offset_at = base;
				}
				EntryKind::RefDelta(base) => {
					push((pack_at, offset_at))?;
					match self.place(&base)? {
						Place::Loose(loose) => break Base::Loose(loose),
						Place::Packed { pack: next, offset } => (pack_at, offset_at) = (next, offset),
					}
				}
			}
		};
		return Ok((base, deltas));
	}

	// The `size` bytes the stream at `input` inflates to, onto the end of `out`.
	fn inflate_sized(&self, pack: usize, input: &mut Input<'_>, size: u64, out: &mut Vec<u8>) -> Result<(), Miss> {
		let len: usize = usize::try_from(size).map_err(|_| return self.malformed(pack))?;
		return inflate_exact_append(input, len, out).map_err(|err| return self.inflate_miss(pack, err));
	}

	fn inflate_miss(&self, pack: usize, err: InflateError) -> Miss {
		let path: &Path = self.packs.get(pack).map_or(self.objects.as_path(), Pack::path);
		return match err {
			InflateError::Io(kind) => Miss::new(path, MissReason::Io(kind)),
			InflateError::Corrupt => Miss::new(path, MissReason::Malformed),
		};
	}

	fn malformed(&self, pack: usize) -> Miss {
		return self.inflate_miss(pack, InflateError::Corrupt);
	}
}

fn is_pack_index(path: &Path) -> bool {
	let is_idx: bool = path.extension().is_some_and(|ext| return ext == IDX_EXTENSION);
	return is_idx && path.with_extension(PACK_EXTENSION).is_file();
}

#[cfg(test)]
mod tests {
	use std::fs;
	use std::io::ErrorKind;
	use std::path::{Path, PathBuf};

	use tempfile::{tempdir, TempDir};

	use super::{Store, ALTERNATES, PACK_DIR, READ_DEPTH_MAX};
	use crate::error::{Miss, MissReason};
	use crate::objects::fixture::{append_delta, entry_head, put_loose, ref_head, zlib, PackBuilder, BLOB};
	use crate::objects::kind::ObjectKind;
	use crate::objects::loose;
	use crate::objects::oid::{Oid, OID_LEN};
	use crate::objects::{Header, Object};
	use crate::testrepo::TestResult;

	fn id(byte: u8) -> [u8; OID_LEN] {
		return [byte; OID_LEN];
	}

	fn objects_dir() -> TestResult<(TempDir, PathBuf)> {
		let dir: TempDir = tempdir().map_err(|e| e.to_string())?;
		let objects: PathBuf = dir.path().join("objects");
		fs::create_dir_all(&objects).map_err(|e| e.to_string())?;
		return Ok((dir, objects));
	}

	fn open(objects: &Path) -> TestResult<Store> {
		return Store::open(objects).map_err(|m| format!("open: {m}"));
	}

	fn blob(body: &[u8]) -> Object {
		return Object { kind: ObjectKind::Blob, size: u64::try_from(body.len()).unwrap_or(u64::MAX), body: Some(body.to_vec()) };
	}

	fn reasons(store: &Store, raw: [u8; OID_LEN]) -> (Option<MissReason>, Option<MissReason>) {
		let oid: Oid = Oid::from_raw(raw);
		return (store.header(&oid).err().map(|m| m.reason), store.read(&oid).err().map(|m| m.reason));
	}

	// Each delta copies its base and appends, so a wrong order or a skipped link changes the bytes.
	#[test]
	fn ofs_delta_chains_apply_from_the_bottom_up() -> TestResult {
		let (_dir, objects) = objects_dir()?;
		let mut pack: PackBuilder = PackBuilder::default();
		let base: u64 = pack.whole(id(1), BLOB, b"hello");
		let middle: u64 = pack.ofs(id(2), base, &append_delta(5, b" world"));
		pack.ofs(id(3), middle, &append_delta(11, b"!"));
		pack.write(&objects.join(PACK_DIR), "pack-a")?;
		let store: Store = open(&objects)?;
		let read = |raw: [u8; OID_LEN]| return store.read(&Oid::from_raw(raw)).map_err(|m| m.to_string());
		assert_eq!(read(id(1))?, blob(b"hello"));
		assert_eq!(read(id(2))?, blob(b"hello world"));
		assert_eq!(read(id(3))?, blob(b"hello world!"));
		let header: Header = store.header(&Oid::from_raw(id(3))).map_err(|m| m.to_string())?;
		assert_eq!(header, Header { kind: ObjectKind::Blob, size: 12 }, "the size comes from the top delta's header");
		return Ok(());
	}

	#[test]
	fn ref_deltas_resolve_through_a_loose_base_and_through_another_pack() -> TestResult {
		let (_dir, objects) = objects_dir()?;
		put_loose(&objects, id(0x10), b"loose")?;
		let mut first: PackBuilder = PackBuilder::default();
		first.ref_delta(id(0x20), id(0x10), &append_delta(5, b" base"));
		first.ref_delta(id(0x21), id(0x30), &append_delta(6, b"+"));
		first.write(&objects.join(PACK_DIR), "pack-a")?;
		let mut second: PackBuilder = PackBuilder::default();
		second.whole(id(0x30), BLOB, b"packed");
		second.write(&objects.join(PACK_DIR), "pack-b")?;
		let store: Store = open(&objects)?;
		for (raw, body) in [(id(0x20), b"loose base".as_slice()), (id(0x21), b"packed+".as_slice()), (id(0x10), b"loose".as_slice())] {
			let oid: Oid = Oid::from_raw(raw);
			assert_eq!(store.read(&oid).map_err(|m| m.to_string())?, blob(body), "{oid}");
			let want: Header = Header { kind: ObjectKind::Blob, size: u64::try_from(body.len()).map_err(|e| e.to_string())? };
			assert_eq!(store.header(&oid).map_err(|m| m.to_string())?, want, "{oid}");
		}
		return Ok(());
	}

	// Each link's result is shorter or longer than the last, so bytes left in a reused buffer would show.
	#[test]
	fn a_chain_that_shrinks_and_grows_reads_each_level_exactly() -> TestResult {
		let (_dir, objects) = objects_dir()?;
		let mut pack: PackBuilder = PackBuilder::default();
		let mut at: u64 = pack.whole(id(1), BLOB, b"hello world");
		let links: [(&[u8], &[u8]); 4] =
			[(&[11, 3, 0x90, 3], b"hel"), (&append_delta(3, b"p me"), b"help me"), (&[7, 2, 0x90, 2], b"he"), (&append_delta(2, b"y!"), b"hey!")];
		let ids: [u8; 4] = [2, 3, 4, 5];
		ids.iter().zip(&links).for_each(|(&n, &(ops, _))| at = pack.ofs(id(n), at, ops));
		pack.write(&objects.join(PACK_DIR), "pack-a")?;
		let store: Store = open(&objects)?;
		ids.iter().zip(&links).try_for_each(|(&n, &(_, want))| -> TestResult {
			assert_eq!(store.read(&Oid::from_raw(id(n))).map_err(|m| m.to_string())?, blob(want), "level {n}");
			return Ok(());
		})?;
		return Ok(());
	}

	// A read holds at most READ_DEPTH_MAX deltas; a deeper chain misses for git, though its header still answers.
	#[test]
	fn a_chain_deeper_than_the_read_buffer_misses_only_the_read() -> TestResult {
		let nth = |n: usize| -> [u8; OID_LEN] {
			let mut raw: [u8; OID_LEN] = [0xaa; OID_LEN];
			raw[..2].copy_from_slice(&u16::try_from(n).unwrap_or(u16::MAX).to_be_bytes());
			return raw;
		};
		let (_dir, objects) = objects_dir()?;
		let mut pack: PackBuilder = PackBuilder::default();
		let mut at: u64 = pack.whole(nth(0), BLOB, b"x");
		(1..=READ_DEPTH_MAX + 1).for_each(|n: usize| at = pack.ofs(nth(n), at, &[1, 1, 0x90, 1]));
		pack.write(&objects.join(PACK_DIR), "pack-a")?;
		let store: Store = open(&objects)?;
		let deepest: Oid = Oid::from_raw(nth(READ_DEPTH_MAX));
		assert_eq!(store.read(&deepest).map_err(|m| m.to_string())?, blob(b"x"), "exactly READ_DEPTH_MAX deltas resolve");
		assert_eq!(reasons(&store, nth(READ_DEPTH_MAX + 1)), (None, Some(MissReason::DeltaChain)));
		return Ok(());
	}

	#[test]
	fn a_delta_on_itself_is_a_delta_chain_miss_not_a_hang() -> TestResult {
		let (_dir, objects) = objects_dir()?;
		let mut pack: PackBuilder = PackBuilder::default();
		pack.ref_delta(id(7), id(7), &append_delta(1, b"x"));
		pack.write(&objects.join(PACK_DIR), "pack-a")?;
		let store: Store = open(&objects)?;
		assert_eq!(reasons(&store, id(7)), (Some(MissReason::DeltaChain), Some(MissReason::DeltaChain)));
		return Ok(());
	}

	#[test]
	fn deltas_and_streams_that_do_not_hold_what_they_say_are_malformed() -> TestResult {
		let (_dir, objects) = objects_dir()?;
		let mut pack: PackBuilder = PackBuilder::default();
		let base: u64 = pack.whole(id(1), BLOB, b"hello");
		pack.ofs(id(2), base, &append_delta(9, b"!"));
		pack.ofs(id(3), base, &[0x80]);
		pack.entry(id(4), &ref_head(4, id(1)), b"not zlib");
		pack.entry(id(5), &entry_head(BLOB, 10), &zlib(b"abc"));
		pack.write(&objects.join(PACK_DIR), "pack-a")?;
		let store: Store = open(&objects)?;
		let malformed: Option<MissReason> = Some(MissReason::Malformed);
		assert_eq!(reasons(&store, id(2)), (None, malformed), "a delta for a 9-byte base on a 5-byte one");
		assert_eq!(reasons(&store, id(3)), (malformed, malformed), "a delta header cut short");
		assert_eq!(reasons(&store, id(4)), (malformed, malformed), "delta data that is not zlib");
		assert_eq!(reasons(&store, id(5)), (None, malformed), "a stream shorter than its entry header");
		let miss: Miss = store.read(&Oid::from_raw(id(4))).err().ok_or("id 4 read")?;
		assert_eq!(miss.path, objects.join(PACK_DIR).join("pack-a.pack"), "the miss names the pack");
		return Ok(());
	}

	#[test]
	fn a_missing_object_names_its_loose_path_or_the_alternates_file() -> TestResult {
		let (_dir, objects) = objects_dir()?;
		let store: Store = open(&objects)?;
		let oid: Oid = Oid::from_raw(id(9));
		let want = || return Some(Miss::new(&loose::path(&objects, &oid), MissReason::MissingObject));
		assert_eq!(store.header(&oid).err(), want());
		assert_eq!(store.read(&oid).err(), want());
		let alternates: PathBuf = objects.join(ALTERNATES);
		fs::create_dir_all(alternates.parent().ok_or("no parent")?).map_err(|e| e.to_string())?;
		fs::write(&alternates, "/elsewhere/objects\n").map_err(|e| e.to_string())?;
		assert_eq!(store.read(&oid).err(), Some(Miss::new(&alternates, MissReason::Alternates)));
		return Ok(());
	}

	#[test]
	fn only_an_index_with_its_pack_opens_and_a_bad_one_fails_the_store() -> TestResult {
		let (_dir, objects) = objects_dir()?;
		let pack_dir: PathBuf = objects.join(PACK_DIR);
		fs::create_dir_all(&pack_dir).map_err(|e| e.to_string())?;
		fs::write(pack_dir.join("pack-orphan.idx"), b"no pack beside me").map_err(|e| e.to_string())?;
		fs::write(pack_dir.join("notes.txt"), b"not an index").map_err(|e| e.to_string())?;
		let store: Store = open(&objects)?;
		assert_eq!(reasons(&store, id(1)).0, Some(MissReason::MissingObject), "the orphan index was skipped");
		fs::write(pack_dir.join("pack-orphan.pack"), b"now it has one").map_err(|e| e.to_string())?;
		let miss: Option<Miss> = Store::open(&objects).err();
		assert_eq!(miss, Some(Miss::new(&pack_dir.join("pack-orphan.idx"), MissReason::Malformed)));
		return Ok(());
	}

	#[test]
	fn a_pack_dir_that_is_a_file_is_an_io_miss_and_no_pack_dir_is_empty() -> TestResult {
		let (_dir, objects) = objects_dir()?;
		assert!(Store::open(&objects.join("absent")).is_ok(), "no pack directory means no packs");
		fs::write(objects.join(PACK_DIR), b"a file").map_err(|e| e.to_string())?;
		let reason: Option<MissReason> = Store::open(&objects).err().map(|m| m.reason);
		assert!(matches!(reason, Some(MissReason::Io(kind)) if kind != ErrorKind::NotFound), "{reason:?}");
		return Ok(());
	}
}
