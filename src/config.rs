//! The `.git/config` settings the file readers depend on, read the way git reads them.
//!
//! Only `[core]` and `[extensions]` keys count. A quoted or continued value, an unclosed section header, or a
//! boolean git refuses gives `Malformed`, and the caller then asks git.

use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use crate::error::{Miss, MissReason};
use crate::object_format::ObjectFormat;

pub const CONFIG_FILE: &str = "config";

/// The settings from one config file; an absent key keeps git's default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RepoConfig {
	/// `extensions.objectFormat`.
	pub object_format: ObjectFormat,
	/// `core.ignoreCase`.
	pub ignore_case: bool,
	/// Whether `refStorage` occurs anywhere in the file, in any case: deliberately broader than the key.
	pub ref_storage: bool,
}

impl Default for RepoConfig {
	fn default() -> Self {
		return Self { object_format: ObjectFormat::Sha1, ignore_case: false, ref_storage: false };
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
	Core,
	Extensions,
	Other,
}

impl Section {
	// A subsection (`[core "x"]` or `[core.x]`) is another section.
	const fn parse(name: &str) -> Self {
		return match name {
			n if n.eq_ignore_ascii_case("core") => Self::Core,
			n if n.eq_ignore_ascii_case("extensions") => Self::Extensions,
			_ => Self::Other,
		};
	}
}

/// The config at `path`, or `None` when that file does not exist.
///
/// # Errors
/// A [`Miss`] at `path` when it does not read or does not parse.
pub fn read(path: &Path) -> Result<Option<RepoConfig>, Miss> {
	return match fs::read(path) {
		Ok(bytes) => RepoConfig::parse(&bytes).map(Some).map_err(|reason: MissReason| return Miss::new(path, reason)),
		Err(err) if err.kind() == ErrorKind::NotFound => Ok(None),
		Err(err) => Err(Miss::io(path, &err)),
	};
}

impl RepoConfig {
	/// The settings in `bytes`; for a key set twice, the later value wins, as in git.
	///
	/// # Errors
	/// `Malformed` for a line this parser does not settle, `ObjectFormat` for an object format it does not know.
	pub fn parse(bytes: &[u8]) -> Result<Self, MissReason> {
		let mut config: Self = Self { ref_storage: mentions(bytes, "refstorage"), ..Self::default() };
		let mut section: Section = Section::Other;
		for raw in bytes.split(|b: &u8| return *b == b'\n') {
			let line = String::from_utf8_lossy(raw);
			let mut rest: &str = line.trim();
			if let Some(header) = rest.strip_prefix('[') {
				let (name, after) = header.split_once(']').ok_or(MissReason::Malformed)?;
				section = Section::parse(name);
				rest = after.trim();
			}
			if rest.is_empty() || rest.starts_with(['#', ';']) {
				continue;
			}
			// A value continued onto the next line could hide or fake any key there.
			if rest.bytes().rev().take_while(|b: &u8| return *b == b'\\').count() % 2 == 1 {
				return Err(MissReason::Malformed);
			}
			config.set(section, rest)?;
		}
		return Ok(config);
	}

	fn set(&mut self, section: Section, line: &str) -> Result<(), MissReason> {
		let (key, value) = match line.split_once('=') {
			Some((key, value)) => (key.trim(), Some(value)),
			None => (uncommented(line), None),
		};
		let is = |name: &str| -> bool { return key.eq_ignore_ascii_case(name) };
		match section {
			Section::Core if is("ignorecase") => self.ignore_case = boolean(value)?,
			Section::Extensions if is("objectformat") => self.object_format = object_format(value)?,
			_ => {}
		}
		return Ok(());
	}
}

// git's boolean: a bare key is true, an empty value false, then true/yes/on, false/no/off, or an integer.
fn boolean(value: Option<&str>) -> Result<bool, MissReason> {
	let Some(raw) = value else {
		return Ok(true);
	};
	return match plain(raw)?.to_ascii_lowercase().as_str() {
		"true" | "yes" | "on" => Ok(true),
		"false" | "no" | "off" | "" => Ok(false),
		number => {
			let digits: &str = number.strip_suffix(['k', 'm', 'g']).unwrap_or(number);
			digits.parse::<i64>().map(|n: i64| return n != 0).map_err(|_| return MissReason::Malformed)
		}
	};
}

fn object_format(value: Option<&str>) -> Result<ObjectFormat, MissReason> {
	let text: &str = plain(value.ok_or(MissReason::Malformed)?)?;
	return ObjectFormat::from_name(text).ok_or(MissReason::ObjectFormat);
}

// The value without its comment; a quote or escape is git's to unfold.
fn plain(raw: &str) -> Result<&str, MissReason> {
	let text: &str = uncommented(raw);
	if text.contains(['"', '\\']) {
		return Err(MissReason::Malformed);
	}
	return Ok(text);
}

fn uncommented(text: &str) -> &str {
	return text.split(['#', ';']).next().unwrap_or_default().trim();
}

// Whether `text` holds `key` in any ASCII case; `key` is lowercase.
fn mentions(text: &[u8], key: &str) -> bool {
	return text.windows(key.len()).any(|window: &[u8]| return window.eq_ignore_ascii_case(key.as_bytes()));
}

#[cfg(test)]
mod tests {
	use super::{RepoConfig, Section};
	use crate::error::MissReason;
	use crate::object_format::ObjectFormat;

	fn ignore_case(text: &str) -> Result<bool, MissReason> {
		return RepoConfig::parse(text.as_bytes()).map(|c: RepoConfig| return c.ignore_case);
	}

	fn object_format(text: &str) -> Result<ObjectFormat, MissReason> {
		return RepoConfig::parse(text.as_bytes()).map(|c: RepoConfig| return c.object_format);
	}

	#[test]
	fn an_empty_file_keeps_gits_defaults() {
		assert_eq!(RepoConfig::parse(b""), Ok(RepoConfig::default()));
		assert_eq!(RepoConfig::default().object_format, ObjectFormat::Sha1, "SHA-1 when unset");
	}

	// Each word or number that git reads as a boolean, not only `true`.
	#[test]
	fn ignorecase_reads_each_boolean_spelling_git_accepts() {
		let truthy: [&str; 7] = ["= true", "= yes", "= On", "= 1", "= -2", "= 1k", ""];
		let falsy: [&str; 6] = ["= false", "= no", "= OFF", "= 0", "=", "= 0 # off"];
		truthy.iter().for_each(|v: &&str| assert_eq!(ignore_case(&format!("[core]\n\tignorecase {v}\n")), Ok(true), "{v:?}"));
		falsy.iter().for_each(|v: &&str| assert_eq!(ignore_case(&format!("[core]\n\tignorecase {v}\n")), Ok(false), "{v:?}"));
	}

	#[test]
	fn the_later_value_wins_and_only_its_own_section_counts() {
		assert_eq!(ignore_case("[core]\nignorecase = true\nignorecase = false\n"), Ok(false));
		assert_eq!(ignore_case("[core]\nignorecase = false\n[core]\nIgnoreCase = yes\n"), Ok(true));
		assert_eq!(ignore_case("[other]\nignorecase = true\n"), Ok(false));
		assert_eq!(ignore_case("[core \"sub\"]\nignorecase = true\n[core.sub]\nignorecase = true\n"), Ok(false));
		assert_eq!(ignore_case("[Core] ignorecase = true\r\n"), Ok(true), "a key on the header line, CRLF endings");
	}

	#[test]
	fn lines_git_would_unfold_or_refuse_are_malformed() {
		let cases: [&str; 5] = [
			"[core]\nignorecase = maybe\n",
			"[core]\nignorecase = \"true\"\n",
			"[core\nignorecase = true\n",
			"[alias]\nx = a \\\n[core]\nignorecase = true\n",
			"[extensions]\nobjectformat\n",
		];
		cases.iter().for_each(|text: &&str| assert_eq!(RepoConfig::parse(text.as_bytes()), Err(MissReason::Malformed), "{text:?}"));
		assert_eq!(ignore_case("[alias]\nx = \"q\" \\\\\n[core]\nignorecase = 1\n"), Ok(true), "an escaped backslash ends the line");
		assert_eq!(ignore_case("[alias]\nx = \"quoted\" # \"q\"\n"), Ok(false), "quotes in another key are not ours to unfold");
	}

	#[test]
	fn objectformat_names_a_known_format_or_misses() {
		assert_eq!(object_format("[extensions]\n\tobjectFormat = sha256\n"), Ok(ObjectFormat::Sha256));
		assert_eq!(object_format("[Extensions]\nobjectformat = sha1\n"), Ok(ObjectFormat::Sha1));
		assert_eq!(object_format("[extensions]\nobjectformat = blake3\n"), Err(MissReason::ObjectFormat));
		assert_eq!(object_format("[core]\nobjectformat = sha256\n"), Ok(ObjectFormat::Sha1), "another section");
	}

	#[test]
	fn ref_storage_is_any_mention_in_any_case() {
		let mentioned = |text: &str| return RepoConfig::parse(text.as_bytes()).map(|c: RepoConfig| return c.ref_storage);
		assert_eq!(mentioned("[extensions]\n\trefStorage = files\n"), Ok(true));
		assert_eq!(mentioned("# REFSTORAGE in a comment\n"), Ok(true));
		assert_eq!(mentioned("[core]\nbare = false\n"), Ok(false));
	}

	#[test]
	fn section_names_ignore_case_and_subsections_are_other() {
		assert_eq!(["core", "CORE", "extensions", "core \"x\"", "core.x"].map(Section::parse), [
			Section::Core,
			Section::Core,
			Section::Extensions,
			Section::Other,
			Section::Other
		]);
	}
}
