// The optional JSON payload on stdin; the caller names each field it wants with an RFC 6901 pointer.

use std::io::{self, Read};

use serde_json::Value;

const BOM: char = '\u{feff}';

/// The whole payload; empty stdin is `Null`, so every field in it is absent.
pub fn read(stdin: &mut impl Read) -> Result<Value, io::Error> {
	let mut text: String = String::new();
	stdin.read_to_string(&mut text)?;
	// PowerShell 5.1 can put a UTF-8 BOM before piped text, and serde_json rejects a BOM.
	let json: &str = text.trim_start_matches(BOM);
	if json.trim().is_empty() {
		return Ok(Value::Null);
	}
	return serde_json::from_str(json).map_err(io::Error::from);
}

/// Whether `pointer` is a JSON pointer into the payload, not the whole document.
#[must_use]
pub fn is_pointer(pointer: &str) -> bool {
	return pointer.starts_with('/');
}

/// The string at `pointer`; absent or `null` is `None`, any other type is an error.
pub fn text<'v>(payload: &'v Value, pointer: &str) -> Result<Option<&'v str>, String> {
	return match payload.pointer(pointer) {
		None | Some(Value::Null) => Ok(None),
		Some(Value::String(text)) => Ok(Some(text)),
		Some(_) => Err(format!("payload field {pointer} is not a string")),
	};
}

/// The bool at `pointer`; absent or `null` is `false`, any other type is an error.
pub fn truth(payload: &Value, pointer: &str) -> Result<bool, String> {
	return match payload.pointer(pointer) {
		None | Some(Value::Null) => Ok(false),
		Some(Value::Bool(truth)) => Ok(*truth),
		Some(_) => Err(format!("payload field {pointer} is not a bool")),
	};
}

#[cfg(test)]
mod tests {
	use serde_json::{json, Value};

	use super::{is_pointer, read, text, truth};

	#[test]
	fn empty_or_bom_only_stdin_is_null_and_a_bom_before_json_is_dropped() -> Result<(), String> {
		let read_str = |input: &str| return read(&mut input.as_bytes()).map_err(|err| format!("read {input:?}: {err:?}"));
		assert_eq!(read_str("")?, Value::Null);
		assert_eq!(read_str("\u{feff} \n")?, Value::Null);
		assert_eq!(read_str("\u{feff}{\"a\":1}")?, json!({"a": 1}));
		assert!(read(&mut "{not json".as_bytes()).is_err());
		return Ok(());
	}

	#[test]
	fn fields_are_found_by_pointer_and_absent_or_null_means_unset() -> Result<(), String> {
		let payload: Value = json!({"cwd": "C:/r", "tool": {"path": "a.rs"}, "again": true, "gone": null});
		assert_eq!(text(&payload, "/cwd")?, Some("C:/r"));
		assert_eq!(text(&payload, "/tool/path")?, Some("a.rs"));
		assert_eq!(text(&payload, "/missing")?, None);
		assert_eq!(text(&payload, "/gone")?, None);
		assert_eq!(text(&Value::Null, "/cwd")?, None);
		assert!(truth(&payload, "/again")?);
		assert!(!truth(&payload, "/missing")?);
		assert!(!truth(&payload, "/gone")?);
		return Ok(());
	}

	#[test]
	fn a_field_of_the_wrong_type_is_an_error_not_a_default() {
		let payload: Value = json!({"cwd": 5, "again": "yes"});
		assert_eq!(text(&payload, "/cwd"), Err(String::from("payload field /cwd is not a string")));
		assert_eq!(truth(&payload, "/again"), Err(String::from("payload field /again is not a bool")));
	}

	#[test]
	fn only_a_leading_slash_makes_a_pointer() {
		assert!(is_pointer("/cwd") && is_pointer("/a/b"));
		assert!(!is_pointer("") && !is_pointer("cwd") && !is_pointer("a/b"));
	}
}
