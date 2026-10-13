//! Postgres extension: executor hooks, shared memory, GUCs, SQL views and functions.

use std::ffi::CStr;
use std::sync::atomic::{AtomicU64, Ordering};

use pgrx::pg_sys;
use pgrx::prelude::*;

pgrx::pg_module_magic!(name, version);

/// How many times this backend has run our ExecutorStart hook. Only read by tests.
static HOOK_CALLS: AtomicU64 = AtomicU64::new(0);

/// Runs once when Postgres loads the library. Hooks are global function pointers,
/// so the library has to be in `shared_preload_libraries`: every backend then
/// inherits them from the postmaster.
#[pg_guard]
pub extern "C-unwind" fn _PG_init() {
    unsafe {
        pg_sys::ExecutorStart_hook = Some(executor_start);
    }
}

/// Called by Postgres before every query starts executing.
#[pg_guard]
unsafe extern "C-unwind" fn executor_start(query_desc: *mut pg_sys::QueryDesc, eflags: i32) {
    HOOK_CALLS.fetch_add(1, Ordering::Relaxed);

    let source_text = unsafe { (*query_desc).sourceText };
    if !source_text.is_null() {
        let sql = unsafe { CStr::from_ptr(source_text) }.to_string_lossy();
        notice!("sluice: {sql}");
    }

    unsafe { pg_sys::standard_ExecutorStart(query_desc, eflags) };
}

#[cfg(any(test, feature = "pg_test"))]
#[pg_schema]
mod tests {
    use std::sync::atomic::Ordering;

    use pgrx::prelude::*;

    use crate::HOOK_CALLS;

    #[pg_test]
    fn hook_runs_for_each_query() {
        let before = HOOK_CALLS.load(Ordering::Relaxed);
        Spi::run("SELECT 1").unwrap();
        Spi::run("SELECT 2").unwrap();
        assert_eq!(HOOK_CALLS.load(Ordering::Relaxed) - before, 2);
    }
}

/// Required by `cargo pgrx test`.
#[cfg(test)]
pub mod pg_test {
    pub fn setup(_options: Vec<&str>) {}

    #[must_use]
    pub fn postgresql_conf_options() -> Vec<&'static str> {
        vec!["shared_preload_libraries = 'pg_sluice'"]
    }
}
