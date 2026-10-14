//! Postgres extension: executor hooks, shared memory, GUCs, SQL views and functions.

use std::cell::RefCell;
use std::ffi::{CStr, CString};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use pgrx::pg_sys;
use pgrx::prelude::*;
use pgrx::{GucContext, GucFlags, GucRegistry, GucSetting, PgAtomic, pg_shmem_init};
use sluice_core::{RuleIndex, Rules};

pgrx::pg_module_magic!(name, version);

/// `sluice.enabled`: off makes the extension let every query through.
static ENABLED: GucSetting<bool> = GucSetting::<bool>::new(true);

/// `sluice.rules`: the rule set as JSON. Read through [`refresh_rules`].
static RULES: GucSetting<Option<CString>> =
    GucSetting::<Option<CString>>::new(Some(c"{\"budgets\":[]}"));

/// This backend's parsed copy of `sluice.rules`.
#[derive(Default)]
struct LoadedRules {
    /// The `sluice.rules` text last looked at, valid or not.
    text: String,
    /// Parsed from the last valid text. No budgets until there is one.
    rules: Rules,
    /// Which budgets a query's tags fall under, built from `rules`.
    index: RuleIndex,
}

thread_local! {
    // Every backend is a process of its own, so each one keeps its own copy.
    static LOADED_RULES: RefCell<LoadedRules> = RefCell::new(LoadedRules::default());
}

/// How many client queries the ExecutorStart hook has handled in this backend
/// (nested queries are skipped). Only read by tests.
///
/// Every backend is a process of its own, with its own copy of each `static`:
/// this counts this backend's queries only.
static HOOK_CALLS: AtomicU64 = AtomicU64::new(0);

/// The same count for all backends together. It lives in shared memory, which
/// the postmaster sets up at startup and every backend then maps.
static QUERIES_CHECKED: PgAtomic<AtomicU64> = unsafe { PgAtomic::new(c"sluice_queries_checked") };

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
    // SIGHUP settings live in postgresql.conf (or ALTER SYSTEM) and apply to
    // the whole server. pg_reload_conf() makes every backend read them again,
    // between two queries, so they change without a restart.
    GucRegistry::define_bool_guc(
        c"sluice.enabled",
        c"Whether sluice checks queries at all.",
        c"Off lets every query through.",
        &ENABLED,
        GucContext::Sighup,
        GucFlags::default(),
    );
    GucRegistry::define_string_guc(
        c"sluice.rules",
        c"The sluice rule set, as JSON.",
        c"The budgets and what they match.",
        &RULES,
        GucContext::Sighup,
        GucFlags::default(),
    );
    // Any other sluice.* name is now a typo, and setting it is an error
    // instead of being silently ignored.
    unsafe { pg_sys::MarkGUCPrefixReserved(c"sluice".as_ptr()) };

    // Asks the postmaster for room in shared memory. That is only possible
    // while it starts, so another reason to be in shared_preload_libraries.
    pg_shmem_init!(QUERIES_CHECKED);

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
    if should_check(eflags) && !source_text.is_null() {
        HOOK_CALLS.fetch_add(1, Ordering::Relaxed);
        QUERIES_CHECKED.get().fetch_add(1, Ordering::Relaxed);
        refresh_rules();
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

/// Parses `sluice.rules` again if it changed since the last query.
///
/// pg_reload_conf() changes the setting in every backend, but only between two
/// queries: so checking at the start of each query is enough. Invalid rules are
/// reported once and otherwise ignored: the backend keeps its previous rules.
fn refresh_rules() {
    let text = match RULES.get() {
        Some(text) => text.to_string_lossy().into_owned(),
        None => String::new(),
    };
    LOADED_RULES.with_borrow_mut(|loaded| {
        if loaded.text == text {
            return;
        }
        match sluice_core::validate(&text, sluice_core::rules::DEFAULT_MAX_BUDGETS) {
            Ok(rules) => {
                loaded.index = RuleIndex::new(&rules);
                loaded.rules = rules;
            }
            Err(errors) => {
                let errors: Vec<String> = errors.iter().map(ToString::to_string).collect();
                warning!(
                    "sluice: sluice.rules is invalid, keeping the previous rules: {}",
                    errors.join("; ")
                );
            }
        }
        loaded.text = text;
    });
}

/// SQL functions any user can call, to check rules and queries by hand.
#[pg_schema]
mod sluice {
    use pgrx::JsonB;
    use pgrx::prelude::*;
    use serde_json::{Value, json};

    use crate::{LOADED_RULES, refresh_rules};

    // A new schema is only usable by its owner.
    pgrx::extension_sql!(
        "GRANT USAGE ON SCHEMA sluice TO PUBLIC;",
        name = "grant_sluice_schema",
        finalize,
    );

    /// Checks a rule set without applying it. Returns its errors, e.g.
    /// `[{"path": "budgets[0].mode", "message": "..."}]`, or `[]` if it is valid.
    #[pg_extern]
    fn validate(rules: &str) -> JsonB {
        let errors = match sluice_core::validate(rules, sluice_core::rules::DEFAULT_MAX_BUDGETS) {
            Ok(_) => Vec::new(),
            Err(errors) => errors
                .iter()
                .map(|e| json!({ "path": e.path, "message": e.message }))
                .collect(),
        };
        JsonB(Value::Array(errors))
    }

    /// What sluice sees in a query, without running it: its tags and the
    /// budgets they match, e.g. `{"tags": {"app": "x"}, "budgets": ["reports"]}`.
    #[pg_extern]
    fn explain(sql: &str) -> JsonB {
        refresh_rules();
        let tags = sluice_core::parse_tags(sql);
        let budgets: Vec<String> = LOADED_RULES.with_borrow(|loaded| {
            loaded
                .index
                .matching_budgets(&tags)
                .into_iter()
                .map(|i| loaded.rules.budgets[usize::from(i)].name.clone())
                .collect()
        });
        let tags: serde_json::Map<String, Value> = tags
            .iter()
            .map(|(key, value)| (key.to_owned(), Value::from(value)))
            .collect();
        JsonB(json!({ "tags": tags, "budgets": budgets }))
    }
}

/// Whether sluice should look at the query that is starting.
fn should_check(eflags: i32) -> bool {
    if !ENABLED.get() {
        return false;
    }
    // Queries run by a function or trigger belong to the query that started them.
    if NESTING_LEVEL.load(Ordering::Relaxed) > 0 {
        return false;
    }
    // Plain EXPLAIN only plans the query. EXPLAIN ANALYZE runs it, so it is checked.
    if eflags as u32 & pg_sys::EXEC_FLAG_EXPLAIN_ONLY != 0 {
        return false;
    }
    // Autovacuum, parallel workers and other background processes.
    if unsafe { pg_sys::MyBackendType } != pg_sys::BackendType::B_BACKEND {
        return false;
    }
    // Superusers must always get in, e.g. to fix a bad rule.
    // TODO(M3): make this the sluice.exempt_superuser setting.
    if unsafe { pg_sys::superuser() } {
        return false;
    }
    true
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

    use crate::{HOOK_CALLS, LOADED_RULES, QUERIES_CHECKED};

    /// Does nothing: `client_tests` run it to get the test server started.
    #[pg_test]
    fn start_server() {}

    /// Lets `client_tests` read this backend's hook counter.
    #[pg_extern]
    fn hook_calls() -> i64 {
        HOOK_CALLS.load(Ordering::Relaxed) as i64
    }

    /// Lets `client_tests` read the count shared by all backends.
    #[pg_extern]
    fn queries_checked() -> i64 {
        QUERIES_CHECKED.get().load(Ordering::Relaxed) as i64
    }

    /// Lets `client_tests` see the budgets this backend has loaded.
    #[pg_extern]
    fn loaded_budgets() -> Vec<String> {
        LOADED_RULES.with_borrow(|loaded| {
            loaded
                .rules
                .budgets
                .iter()
                .map(|b| b.name.clone())
                .collect()
        })
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
    use std::sync::{Once, RwLock, RwLockReadGuard};
    use std::thread;
    use std::time::{Duration, Instant};

    use postgres::error::SqlState;

    /// The tests share one server and run in parallel. A test that changes a
    /// server-wide setting holds this for writing; every other test holds it
    /// for reading, so it never runs while a setting is changed.
    static SERVER_SETTINGS: RwLock<()> = RwLock::new(());

    /// Held for the whole test by tests that leave server settings alone.
    fn default_settings() -> RwLockReadGuard<'static, ()> {
        // A test that failed while holding the lock for writing poisons it.
        // Its `SystemSetting` has put the setting back already.
        SERVER_SETTINGS.read().unwrap_or_else(|e| e.into_inner())
    }

    /// Starts the test server if it isn't running yet, and connects to it as
    /// the user that created it, a superuser.
    fn connect() -> postgres::Client {
        // pgrx only starts the server from run_test, which runs a #[pg_test].
        pgrx_tests::run_test(
            "start_server",
            None,
            crate::pg_test::postgresql_conf_options(),
        )
        .unwrap();
        let mut client = pgrx_tests::client().unwrap().0;
        static SETUP: Once = Once::new();
        SETUP.call_once(|| setup(&mut client));
        client
    }

    /// Connects as an ordinary role, like an app. Superusers aren't checked.
    fn connect_as_app() -> postgres::Client {
        let mut client = connect();
        client.batch_execute("SET ROLE sluice_app").unwrap();
        client
    }

    /// What the tests need, created once per test run.
    fn setup(client: &mut postgres::Client) {
        client
            .batch_execute(
                "CREATE ROLE sluice_app;
                 GRANT USAGE ON SCHEMA tests TO sluice_app;

                 CREATE FUNCTION sluice_three_queries() RETURNS void
                 LANGUAGE plpgsql AS $$
                 BEGIN
                     -- Not `PERFORM 1`: PL/pgSQL works that out without the executor.
                     PERFORM count(*) FROM pg_class;
                     PERFORM count(*) FROM pg_class;
                     PERFORM count(*) FROM pg_class;
                 END $$;

                 CREATE FUNCTION sluice_fails() RETURNS void
                 LANGUAGE plpgsql AS $$
                 BEGIN
                     PERFORM count(*) / 0 FROM pg_class;
                 END $$;",
            )
            .unwrap();
    }

    fn assert_rejected(client: &mut postgres::Client, sql: &str) {
        let err = client.simple_query(sql).unwrap_err();
        assert_eq!(*err.code().unwrap(), SqlState::INSUFFICIENT_RESOURCES);
    }

    #[test]
    fn blocked_app_is_rejected() {
        let _settings = default_settings();
        let mut client = connect_as_app();
        let err = client
            .simple_query("SELECT 1 /* app='blocked' */")
            .unwrap_err();
        let err = err.as_db_error().unwrap();
        assert_eq!(*err.code(), SqlState::INSUFFICIENT_RESOURCES); // 53000
        assert_eq!(err.message(), "sluice: query rejected");
        assert_eq!(err.detail(), Some("reason=blocked app=blocked"));
    }

    #[test]
    fn other_apps_run() {
        let _settings = default_settings();
        let mut client = connect_as_app();
        client
            .simple_query("SELECT 1 /* app='dashboard' */")
            .unwrap();
        client.simple_query("SELECT 1").unwrap();
    }

    /// Superusers must always get in, e.g. to fix a bad rule.
    #[test]
    fn superuser_is_not_checked() {
        let _settings = default_settings();
        let mut client = connect();
        client.simple_query("SELECT 1 /* app='blocked' */").unwrap();
    }

    /// Plain EXPLAIN only plans the query. EXPLAIN ANALYZE runs it.
    #[test]
    fn explain_is_not_checked_but_explain_analyze_is() {
        let _settings = default_settings();
        let mut client = connect_as_app();
        client
            .simple_query("EXPLAIN SELECT 1 /* app='blocked' */")
            .unwrap();
        assert_rejected(&mut client, "EXPLAIN ANALYZE SELECT 1 /* app='blocked' */");
    }

    /// Turning sluice off takes ALTER SYSTEM and a reload, no restart: even a
    /// session that is already open stops being checked.
    #[test]
    fn disabling_lets_queries_through() {
        let _settings = SERVER_SETTINGS.write().unwrap_or_else(|e| e.into_inner());
        let mut client = connect_as_app();
        assert_rejected(&mut client, "SELECT 1 /* app='blocked' */");

        let disabled = SystemSetting::set("sluice.enabled", "off");
        wait_for_setting(&mut client, "sluice.enabled", "off");
        client.simple_query("SELECT 1 /* app='blocked' */").unwrap();

        drop(disabled);
        wait_for_setting(&mut client, "sluice.enabled", "on");
        assert_rejected(&mut client, "SELECT 1 /* app='blocked' */");
    }

    const REPORTS_RULES: &str =
        r#"{"budgets":[{"name":"reports","mode":"warn","match":[{"app":"dashboard"}]}]}"#;

    /// The budgets the backend behind `client` has loaded. The query goes
    /// through the hook first, which loads the rules again if they changed.
    fn loaded_budgets(client: &mut postgres::Client) -> Vec<String> {
        client
            .query_one("SELECT tests.loaded_budgets()", &[])
            .unwrap()
            .get(0)
    }

    /// Rules written from one session reach a session that is already open,
    /// after pg_reload_conf().
    #[test]
    fn new_rules_reach_open_sessions() {
        let _settings = SERVER_SETTINGS.write().unwrap_or_else(|e| e.into_inner());
        let mut session_b = connect_as_app();
        assert_eq!(loaded_budgets(&mut session_b), Vec::<String>::new());

        // SystemSetting writes the rules from a session of its own (session A).
        let _rules = SystemSetting::set("sluice.rules", REPORTS_RULES);
        wait_for_setting(&mut session_b, "sluice.rules", REPORTS_RULES);
        assert_eq!(loaded_budgets(&mut session_b), ["reports"]);
    }

    /// A typo in the rules must not leave the server without any: each
    /// backend keeps the last rules that were valid (and logs a WARNING).
    #[test]
    fn invalid_rules_keep_the_previous_ones() {
        let _settings = SERVER_SETTINGS.write().unwrap_or_else(|e| e.into_inner());
        let mut client = connect_as_app();
        let _rules = SystemSetting::set("sluice.rules", REPORTS_RULES);
        wait_for_setting(&mut client, "sluice.rules", REPORTS_RULES);
        assert_eq!(loaded_budgets(&mut client), ["reports"]);

        let _invalid = SystemSetting::set("sluice.rules", "{not json");
        wait_for_setting(&mut client, "sluice.rules", "{not json");
        assert_eq!(loaded_budgets(&mut client), ["reports"]);
    }

    /// Any user can check a rule set before applying it.
    #[test]
    fn validate_lists_errors() {
        let _settings = default_settings();
        let mut client = connect_as_app();
        let error_count: i32 = client
            .query_one(
                "SELECT jsonb_array_length(sluice.validate($1))",
                &[&REPORTS_RULES],
            )
            .unwrap()
            .get(0);
        assert_eq!(error_count, 0);

        let bad = r#"{"budgets":[{"name":"x","mode":"bogus","match":[{"app":"a"}]}]}"#;
        let path: String = client
            .query_one("SELECT sluice.validate($1) -> 0 ->> 'path'", &[&bad])
            .unwrap()
            .get(0);
        assert_eq!(path, "budgets[0].mode");
    }

    /// `sluice.explain` shows the tags sluice reads and the budgets they match.
    #[test]
    fn explain_shows_tags_and_budgets() {
        let _settings = SERVER_SETTINGS.write().unwrap_or_else(|e| e.into_inner());
        let mut client = connect_as_app();
        let _rules = SystemSetting::set("sluice.rules", REPORTS_RULES);
        wait_for_setting(&mut client, "sluice.rules", REPORTS_RULES);

        let mut assert_explains = |sql: &str, expected: &str| {
            // Both sides go through jsonb, so spacing and key order don't matter.
            let row = client
                .query_one(
                    "SELECT sluice.explain($1)::text, $2::text::jsonb::text",
                    &[&sql, &expected],
                )
                .unwrap();
            assert_eq!(row.get::<_, String>(0), row.get::<_, String>(1));
        };
        assert_explains(
            "SELECT 1 /* app='dashboard',user='an' */",
            r#"{"tags": {"app": "dashboard", "user": "an"}, "budgets": ["reports"]}"#,
        );
        assert_explains(
            "SELECT 1 /* app='api' */",
            r#"{"tags": {"app": "api"}, "budgets": []}"#,
        );
        assert_explains("SELECT 1", r#"{"tags": {}, "budgets": []}"#);
    }

    /// A server-wide setting changed with ALTER SYSTEM. Dropping it resets the
    /// setting, also when the test fails, since the other tests share the server.
    struct SystemSetting {
        name: &'static str,
    }

    impl SystemSetting {
        fn set(name: &'static str, value: &str) -> Self {
            let quoted = value.replace('\'', "''");
            alter_system(
                &format!("ALTER SYSTEM SET {name} = '{quoted}'"),
                name,
                value,
            );
            Self { name }
        }
    }

    impl Drop for SystemSetting {
        fn drop(&mut self) {
            let default: String = connect()
                .query_one(
                    "SELECT boot_val FROM pg_settings WHERE name = $1",
                    &[&self.name],
                )
                .unwrap()
                .get(0);
            let sql = format!("ALTER SYSTEM RESET {}", self.name);
            alter_system(&sql, self.name, &default);
        }
    }

    /// Runs an ALTER SYSTEM command that sets `name` to `value`, and reloads the
    /// config. Returns once new connections get the value; open ones may lag.
    fn alter_system(sql: &str, name: &str, value: &str) {
        let mut admin = connect();
        // ALTER SYSTEM can't run in a transaction, so not in one batch with the reload.
        admin.batch_execute(sql).unwrap();
        admin.batch_execute("SELECT pg_reload_conf()").unwrap();
        // The postmaster reloads first, then signals its backends. So once
        // `admin` has the value, every backend forked from now on has it too.
        wait_for_setting(&mut admin, name, value);
    }

    /// Waits until the backend behind `client` uses `value` for `name`.
    /// pg_reload_conf() only sends a signal: each backend reloads its config
    /// on its own, before its next query.
    fn wait_for_setting(client: &mut postgres::Client, name: &str, value: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let current: String = client
                .query_one("SELECT current_setting($1)", &[&name])
                .unwrap()
                .get(0);
            if current == value {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{name} is {current}, not {value}"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// Every backend is a process of its own, so a plain `static` is a
    /// separate counter in each one. The shared one counts every session's queries.
    #[test]
    fn sessions_share_the_query_count() {
        let _settings = default_settings();
        let mut session_a = connect_as_app();
        let mut session_b = connect_as_app();
        let read = |client: &mut postgres::Client| -> i64 {
            client
                .query_one("SELECT tests.queries_checked()", &[])
                .unwrap()
                .get(0)
        };
        let before = read(&mut session_b);
        for _ in 0..3 {
            session_a.simple_query("SELECT 1").unwrap();
        }
        let after = read(&mut session_b);
        // A's 3 queries and B's second read. Other tests run meanwhile, so
        // the count can only be higher.
        assert!(after - before >= 4, "B saw {} queries", after - before);
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
        let _settings = default_settings();
        let mut client = connect_as_app();
        let calls = hook_calls_during(&mut client, |c| {
            c.simple_query("SELECT 1").unwrap();
            c.simple_query("SELECT 2").unwrap();
        });
        assert_eq!(calls, 2);
    }

    /// A function's queries belong to the query that called it: count it once.
    #[test]
    fn nested_queries_are_handled_once() {
        let _settings = default_settings();
        let mut client = connect_as_app();
        let calls = hook_calls_during(&mut client, |c| {
            c.simple_query("SELECT sluice_three_queries()").unwrap();
        });
        assert_eq!(calls, 1);
    }

    /// The nesting level must come back down even when a nested query fails,
    /// or every later query in the session would look nested and be skipped.
    #[test]
    fn failed_nested_query_does_not_leave_session_nested() {
        let _settings = default_settings();
        let mut client = connect_as_app();
        let err = client.simple_query("SELECT sluice_fails()").unwrap_err();
        assert_eq!(*err.code().unwrap(), SqlState::DIVISION_BY_ZERO);
        assert_rejected(&mut client, "SELECT 1 /* app='blocked' */");
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
