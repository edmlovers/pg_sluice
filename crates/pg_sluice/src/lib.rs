//! Postgres extension: executor hooks, shared memory, GUCs, SQL views and functions.

use std::ffi::CStr;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use pgrx::pg_sys;
use pgrx::prelude::*;

pgrx::pg_module_magic!(name, version);

/// How many client queries the ExecutorStart hook has handled in this backend
/// (nested queries are skipped). Only read by tests.
static HOOK_CALLS: AtomicU64 = AtomicU64::new(0);

/// How many queries are running in this backend right now, counting the ones
/// started by other queries (a function's queries run inside the query that
/// called it). So in ExecutorStart, 0 means the query came from the client.
static NESTING_LEVEL: AtomicU32 = AtomicU32::new(0);

// The hooks that were installed before ours (e.g. pg_stat_statements'), or None.
// Written once in `_PG_init`, before any query runs.
static mut PREV_EXECUTOR_START: pg_sys::ExecutorStart_hook_type = None;
static mut PREV_EXECUTOR_RUN: pg_sys::ExecutorRun_hook_type = None;
static mut PREV_EXECUTOR_FINISH: pg_sys::ExecutorFinish_hook_type = None;

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
        PREV_EXECUTOR_RUN = pg_sys::ExecutorRun_hook;
        pg_sys::ExecutorRun_hook = Some(executor_run);
        PREV_EXECUTOR_FINISH = pg_sys::ExecutorFinish_hook;
        pg_sys::ExecutorFinish_hook = Some(executor_finish);
    }
}

/// Called by Postgres before every query starts executing.
#[pg_guard]
unsafe extern "C-unwind" fn executor_start(query_desc: *mut pg_sys::QueryDesc, eflags: i32) {
    let source_text = unsafe { (*query_desc).sourceText };
    // Nested queries are covered by the client query that started them.
    if NESTING_LEVEL.load(Ordering::Relaxed) == 0 && !source_text.is_null() {
        HOOK_CALLS.fetch_add(1, Ordering::Relaxed);
        let sql = unsafe { CStr::from_ptr(source_text) }.to_string_lossy();
        let tags = sluice_core::parse_tags(&sql);
        // A hard-coded rule until budgets arrive.
        if tags.get("app") == Some("blocked") {
            // Raises a Postgres ERROR: the query never starts, and the client
            // gets SQLSTATE 53000 with this message and detail.
            ereport!(
                ERROR,
                PgSqlErrorCode::ERRCODE_INSUFFICIENT_RESOURCES,
                "sluice: query rejected",
                "reason=blocked app=blocked",
            );
        }
    }

    // Pass the query on down the chain. Only the last hook calls the standard
    // function, or the executor would start twice.
    //
    // `prev` is a raw C function pointer. Unlike the `pg_sys::` functions, pgrx
    // doesn't wrap it, so an ERROR inside it would longjmp straight across our
    // Rust frames. pg_guard_ffi_boundary turns that ERROR into a Rust panic,
    // which unwinds properly (and runs `finally` in the hooks below).
    unsafe {
        match PREV_EXECUTOR_START {
            Some(prev) => pg_sys::ffi::pg_guard_ffi_boundary(|| prev(query_desc, eflags)),
            None => pg_sys::standard_ExecutorStart(query_desc, eflags),
        }
    }
}

/// Runs the query. Any query started meanwhile (e.g. by a function the query
/// calls) is nested inside this one.
#[pg_guard]
unsafe extern "C-unwind" fn executor_run(
    query_desc: *mut pg_sys::QueryDesc,
    direction: pg_sys::ScanDirection::Type,
    count: u64,
) {
    NESTING_LEVEL.fetch_add(1, Ordering::Relaxed);
    // `finally` runs even when the query fails: a Postgres ERROR jumps straight
    // out of this function, past any line after the call.
    PgTryBuilder::new(|| unsafe {
        match PREV_EXECUTOR_RUN {
            Some(prev) => pg_sys::ffi::pg_guard_ffi_boundary(|| prev(query_desc, direction, count)),
            None => pg_sys::standard_ExecutorRun(query_desc, direction, count),
        }
    })
    .finally(|| {
        NESTING_LEVEL.fetch_sub(1, Ordering::Relaxed);
    })
    .execute();
}

/// Runs AFTER triggers and the like, which can start queries too.
#[pg_guard]
unsafe extern "C-unwind" fn executor_finish(query_desc: *mut pg_sys::QueryDesc) {
    NESTING_LEVEL.fetch_add(1, Ordering::Relaxed);
    PgTryBuilder::new(|| unsafe {
        match PREV_EXECUTOR_FINISH {
            Some(prev) => pg_sys::ffi::pg_guard_ffi_boundary(|| prev(query_desc)),
            None => pg_sys::standard_ExecutorFinish(query_desc),
        }
    })
    .finally(|| {
        NESTING_LEVEL.fetch_sub(1, Ordering::Relaxed);
    })
    .execute();
}

#[cfg(any(test, feature = "pg_test"))]
#[pg_schema]
mod tests {
    use std::sync::atomic::Ordering;

    use pgrx::prelude::*;

    use crate::HOOK_CALLS;

    /// Does nothing: `client_tests` run it to get the test server started.
    #[pg_test]
    fn start_server() {}

    /// Lets `client_tests` read this backend's hook counter.
    #[pg_extern]
    fn hook_calls() -> i64 {
        HOOK_CALLS.load(Ordering::Relaxed) as i64
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

/// Tests that connect as a normal client. Their queries are top-level, like an
/// app's, and errors come back with their SQLSTATE.
#[cfg(test)]
mod client_tests {
    use postgres::error::SqlState;

    /// Starts the test server if it isn't running yet, and connects to it.
    fn connect() -> postgres::Client {
        // pgrx only starts the server from run_test, which runs a #[pg_test].
        pgrx_tests::run_test(
            "start_server",
            None,
            crate::pg_test::postgresql_conf_options(),
        )
        .unwrap();
        pgrx_tests::client().unwrap().0
    }

    #[test]
    fn blocked_app_is_rejected() {
        let mut client = connect();
        let err = client
            .simple_query("SELECT 1 /* app='blocked' */")
            .unwrap_err();
        let err = err.as_db_error().unwrap();
        assert_eq!(*err.code(), SqlState::INSUFFICIENT_RESOURCES); // 53000
        assert_eq!(err.message(), "sluice: query rejected");
        assert_eq!(err.detail(), Some("reason=blocked app=blocked"));
    }

    /// Calls `f` and returns how many queries the hook handled meanwhile.
    fn hook_calls_during(
        client: &mut postgres::Client,
        f: impl FnOnce(&mut postgres::Client),
    ) -> i64 {
        let read = |client: &mut postgres::Client| -> i64 {
            client
                .query_one("SELECT tests.hook_calls()", &[])
                .unwrap()
                .get(0)
        };
        let before = read(client);
        f(client);
        // The second read is a query too.
        read(client) - before - 1
    }

    #[test]
    fn hook_runs_for_each_query() {
        let mut client = connect();
        let calls = hook_calls_during(&mut client, |c| {
            c.simple_query("SELECT 1").unwrap();
            c.simple_query("SELECT 2").unwrap();
        });
        assert_eq!(calls, 2);
    }

    /// A function's queries belong to the query that called it: count it once.
    #[test]
    fn nested_queries_are_handled_once() {
        let mut client = connect();
        client
            .batch_execute(
                "CREATE OR REPLACE FUNCTION sluice_three_queries() RETURNS void
                 LANGUAGE plpgsql AS $$
                 BEGIN
                     PERFORM count(*) FROM pg_class;
                     PERFORM count(*) FROM pg_class;
                     PERFORM count(*) FROM pg_class;
                 END $$",
            )
            .unwrap();
        let calls = hook_calls_during(&mut client, |c| {
            c.simple_query("SELECT sluice_three_queries()").unwrap();
        });
        assert_eq!(calls, 1);
    }

    /// The nesting level must come back down even when a nested query fails,
    /// or every later query in the session would look nested and be skipped.
    #[test]
    fn failed_nested_query_does_not_leave_session_nested() {
        let mut client = connect();
        client
            .batch_execute(
                "CREATE OR REPLACE FUNCTION sluice_fails() RETURNS void
                 LANGUAGE plpgsql AS $$
                 BEGIN
                     PERFORM count(*) / 0 FROM pg_class;
                 END $$",
            )
            .unwrap();
        let err = client.simple_query("SELECT sluice_fails()").unwrap_err();
        assert_eq!(*err.code().unwrap(), SqlState::DIVISION_BY_ZERO);

        let err = client
            .simple_query("SELECT 1 /* app='blocked' */")
            .unwrap_err();
        assert_eq!(*err.code().unwrap(), SqlState::INSUFFICIENT_RESOURCES);
    }

    #[test]
    fn other_apps_run() {
        let mut client = connect();
        client
            .simple_query("SELECT 1 /* app='dashboard' */")
            .unwrap();
        client.simple_query("SELECT 1").unwrap();
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
            // #[pg_test] queries run inside a function, so they are nested.
            "pg_stat_statements.track = 'all'",
        ]
    }
}
