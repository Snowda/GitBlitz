// Names written as a well-formed v2/v3/v4 index read back exactly, sorted and deduplicated.
// Without `--cfg fuzzing` this is an empty main, so the workspace builds and tests on stable.
#![cfg_attr(fuzzing, no_main)]

#[cfg(fuzzing)]
libfuzzer_sys::fuzz_target!(|data: &[u8]| {
	gitblitz_fuzz::index_roundtrip(data);
});

#[cfg(not(fuzzing))]
fn main() {}
