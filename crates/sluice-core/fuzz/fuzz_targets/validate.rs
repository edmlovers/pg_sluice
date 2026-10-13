#![no_main]

use libfuzzer_sys::fuzz_target;
use sluice_core::rules::DEFAULT_MAX_BUDGETS;

// validate reads sluice.rules, which an admin edits by hand: bad JSON must come
// back as errors, never as a panic.
fuzz_target!(|json: &str| {
    let _ = sluice_core::validate(json, DEFAULT_MAX_BUDGETS);
});
