//! Postgres extension: executor hooks, shared memory, GUCs, SQL views and functions.

use std::ffi::CStr;
use std::sync::atomic::{AtomicU64, Ordering};

use pgrx::pg_sys;
use pgrx::prelude::*;

pgrx::pg_module_magic!(name, version);

/// How many times this backend has run our ExecutorStart hook. Only read by tests.
static HOOK_CALLS: AtomicU64 = AtomicU64::new(0);

/// The hook that was installed before ours (e.g. pg_stat_statements'), or None.
/// Written once in `_PG_init`, before any query runs.
static mut PREV_EXECUTOR_START: pg_sys::ExecutorStart_hook_type = None;

/// Runs once when Postgres loads the library. Hooks are global function pointers,
/// so the library has to be in `shared_preload_libraries`: every backend then
/// inherits them from the postmaster.
///
/// Several extensions can hook the same place: each one saves the hook it found
/// and calls it, so they form a chain.
#[pg_guard]
pub extern "C-unwind" fn _PG_init() {
    unsafe {
        PREV_EXECUTOR_START = pg_sys::ExecutorStart_hook;
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

    // Pass the query on down the chain. Only the last hook calls the standard
    // function, or the executor would start twice.
    unsafe {
        match PREV_EXECUTOR_START {
            Some(prev) => prev(query_desc, eflags),
            None => pg_sys::standard_ExecutorStart(query_desc, eflags),
        }
    }
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

    /// pg_stat_statements is loaded first, so its ExecutorStart hook is the one
    /// ours replaces. It only counts a query if that hook still runs.
    #[pg_test]
    fn pg_stat_statements_still_counts() {
        Spi::run("CREATE EXTENSION pg_stat_statements").unwrap();
        // pg_stat_statements groups queries by shape and ignores aliases, so
        // `SELECT 1 AS x` would share an entry with other tests' `SELECT 1`.
        // A table of its own makes this query unique.
        Spi::run("CREATE TABLE sluice_chain_test (x int)").unwrap();
        for _ in 0..3 {
            Spi::run("SELECT x FROM sluice_chain_test").unwrap();
        }
        let calls = Spi::get_one::<i64>(
            "SELECT coalesce(sum(calls), 0)::bigint FROM pg_stat_statements
             WHERE query = 'SELECT x FROM sluice_chain_test'",
        )
        .unwrap();
        assert_eq!(calls, Some(3));
    }
}

/// Required by `cargo pgrx test`.
#[cfg(test)]
pub mod pg_test {
    pub fn setup(_options: Vec<&str>) {}

    #[must_use]
    pub fn postgresql_conf_options() -> Vec<&'static str> {
        vec![
            "shared_preload_libraries = 'pg_stat_statements, pg_sluice'",
            // Tests run their queries from inside a function, so they are nested.
            "pg_stat_statements.track = 'all'",
        ]
    }
}
