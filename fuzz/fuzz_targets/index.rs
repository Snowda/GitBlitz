// Raw bytes as `.git/index`: any listing has a valid header and is sorted, deduplicated and repeatable.
// Without `--cfg fuzzing` this is an empty main, so the workspace builds and tests on stable.
#![cfg_attr(fuzzing, no_main)]

#[cfg(fuzzing)]
libfuzzer_sys::fuzz_target!(|data: &[u8]| {
	gitblitz_fuzz::index(data);
});

#[cfg(not(fuzzing))]
fn main() {}
