#![no_main]

use libfuzzer_sys::fuzz_target;

// parse_tags runs on every query, so it must never panic, whatever the client sends.
fuzz_target!(|sql: &str| {
    let tags = sluice_core::parse_tags(sql);
    assert!(tags.len() <= sluice_core::tags::MAX_TAGS);
});
