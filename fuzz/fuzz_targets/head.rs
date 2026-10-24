// HEAD, loose ref, packed-refs and config texts: any answer is 40 lowercase hex from the file git reads.
// Without `--cfg fuzzing` this is an empty main, so the workspace builds and tests on stable.
#![cfg_attr(fuzzing, no_main)]

#[cfg(fuzzing)]
libfuzzer_sys::fuzz_target!(|data: &[u8]| {
	gitblitz_fuzz::head(data);
});

#[cfg(not(fuzzing))]
fn main() {}
