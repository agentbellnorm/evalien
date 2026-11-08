use std::{
    net::IpAddr,
    sync::{Arc, OnceLock},
    time::Duration,
};

use evalien::db::{self, Access, SqliteStore};
use evalien::domain::{Event, Outcome, Seq, TRAJECTORY_TABLE, Trajectory};
use evalien::evaluation::capabilities::{Fetch, Sql, is_public};
use evalien::evaluation::{Capabilities, Limits, Sandbox, SandboxHost};

/// Compiling the guest takes a moment, so tests share one host.
fn host() -> Arc<SandboxHost> {
    static HOST: OnceLock<Arc<SandboxHost>> = OnceLock::new();
    HOST.get_or_init(|| SandboxHost::new().unwrap()).clone()
}

struct Rig {
    sandbox: Sandbox,
    next: u64,
}

impl Rig {
    async fn new(capabilities: Capabilities, limits: Limits) -> Rig {
        Rig {
            sandbox: Sandbox::new(host(), capabilities, limits).await.unwrap(),
            next: 1,
        }
    }

    async fn plain() -> Rig {
        Rig::new(Capabilities::none(), Limits::default()).await
    }

    fn start(&mut self) -> Seq {
        let eval = Seq::new(self.next);
        self.next += 1;
        eval
    }

    /// Evaluate and drive the sandbox until nothing more can happen.
    async fn run(&mut self, code: &str) -> (Seq, Vec<Event>) {
        let eval = self.start();
        self.sandbox.evaluate(eval, code).await;
        (eval, self.drain().await)
    }

    async fn drain(&mut self) -> Vec<Event> {
        let mut events = self.sandbox.take_events();
        while self.sandbox.is_waiting() {
            let wakeup = tokio::time::timeout(Duration::from_secs(20), self.sandbox.next_wakeup())
                .await
                .expect("the sandbox never went idle")
                .expect("wakeups stopped");
            self.sandbox.wake(wakeup).await;
            events.extend(self.sandbox.take_events());
        }
        events
    }

    async fn value(&mut self, code: &str) -> String {
        let (eval, events) = self.run(code).await;
        let (outcome, text) = result(&events, eval);
        assert_eq!(outcome, Outcome::Return, "eval failed: {text}");
        text
    }

    async fn error(&mut self, code: &str) -> String {
        let (eval, events) = self.run(code).await;
        let (outcome, text) = result(&events, eval);
        assert_eq!(outcome, Outcome::Throw, "eval should have failed, returned {text}");
        text
    }
}

fn result(events: &[Event], eval: Seq) -> (Outcome, String) {
    events
        .iter()
        .find_map(|event| match event {
            Event::Result { eval: e, outcome, text } if *e == eval => Some((*outcome, text.clone())),
            _ => None,
        })
        .unwrap_or_else(|| panic!("eval {eval} never settled: {events:?}"))
}

fn output(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::Stdout { text } | Event::Stderr { text } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn returns_values_and_keeps_globals() {
    let mut rig = Rig::plain().await;
    assert_eq!(rig.value("globalThis.n = 41; return n + 1").await, "42");
    assert_eq!(rig.value("return n").await, "41");
    assert_eq!(
        rig.value("return { a: [1, 'b'], m: new Map([[1, 2]]) }").await,
        r#"{a: [1, "b"], m: Map(1) {1 => 2}}"#
    );
    assert_eq!(rig.value("return undefined").await, "undefined");
}

#[tokio::test]
async fn nothing_ambient_exists() {
    let mut rig = Rig::plain().await;
    let probe =
        "return ['process', 'require', 'Deno', 'Bun', 'std', 'os', 'fetch', 'db', 'WebAssembly', 'XMLHttpRequest']
        .filter(name => typeof globalThis[name] !== 'undefined')";
    assert_eq!(rig.value(probe).await, "[]");
    rig.error("return await import('fs')").await;
}

#[tokio::test]
async fn timers_and_console() {
    let mut rig = Rig::plain().await;
    let code = "
        let ticks = 0;
        const cancelled = setTimeout(() => console.log('never'), 10);
        clearTimeout(cancelled);
        await new Promise(done => {
            const t = setInterval(() => { console.log('tick', ++ticks); if (ticks === 3) { clearInterval(t); done(); } }, 5);
        });
        console.error('done');
        return ticks";
    let (eval, events) = rig.run(code).await;
    assert_eq!(result(&events, eval), (Outcome::Return, "3".into()));
    assert_eq!(output(&events), ["tick 1", "tick 2", "tick 3", "done"]);
}

#[tokio::test]
async fn evaluations_run_concurrently() {
    let mut rig = Rig::plain().await;
    let slow = rig.start();
    rig.sandbox
        .evaluate(slow, "await new Promise(r => setTimeout(r, 100)); return 'slow'")
        .await;
    let fast = rig.start();
    rig.sandbox
        .evaluate(fast, "await new Promise(r => setTimeout(r, 10)); return 'fast'")
        .await;
    let events = rig.drain().await;
    let order: Vec<Seq> = events
        .iter()
        .filter_map(|e| match e {
            Event::Result { eval, .. } => Some(*eval),
            _ => None,
        })
        .collect();
    assert_eq!(order, [fast, slow]);
}

#[tokio::test]
async fn errors_settle_with_the_agents_line_numbers() {
    let mut rig = Rig::plain().await;
    let text = rig.error("const x = 1;\nthrow new TypeError('boom')").await;
    assert!(text.starts_with("TypeError: boom"), "{text}");
    assert!(text.contains("<input>:2:"), "{text}");
    assert!(!text.contains("evaluate"), "prelude frames leaked: {text}");
    assert!(rig.error("return (").await.starts_with("SyntaxError"));
}

#[tokio::test]
async fn the_soft_limit_interrupts_and_keeps_globals() {
    let limits = Limits {
        soft_slice: Duration::from_millis(100),
        ..Limits::default()
    };
    let mut rig = Rig::new(Capabilities::none(), limits).await;
    rig.value("globalThis.kept = 'yes'").await;
    for spin in [
        "while (true) {}",
        "try { while (true) {} } catch { return 'caught' } finally { return 'finally' }",
    ] {
        let (eval, events) = rig.run(spin).await;
        let (outcome, text) = result(&events, eval);
        assert!(
            outcome == Outcome::Throw && text.contains("interrupted"),
            "{spin}: {text}"
        );
        assert!(!output(&events).iter().any(|line| line.contains("runtime reset")));
    }
    assert_eq!(rig.value("return kept").await, r#""yes""#);
}

#[tokio::test]
async fn an_interrupt_after_an_await_is_reported() {
    // Known gap: a spin after an await escapes from a promise job, and nothing
    // ties it back to its evaluation. It's reported, but the eval stays pending.
    let limits = Limits {
        soft_slice: Duration::from_millis(100),
        ..Limits::default()
    };
    let mut rig = Rig::new(Capabilities::none(), limits).await;
    let (_, events) = rig
        .run("await null; await new Promise(r => setTimeout(r, 1)); while (true) {}")
        .await;
    assert!(
        output(&events).iter().any(|line| line.contains("interrupted")),
        "{events:?}"
    );
    assert!(!events.iter().any(|e| matches!(e, Event::Result { .. })));
}

#[tokio::test]
async fn the_hard_limit_kills_and_replaces_the_instance() {
    // The soft limit is out of reach, so only the epoch deadline can stop the spin.
    let limits = Limits {
        soft_slice: Duration::from_secs(60),
        hard_slice: Duration::from_millis(200),
        ..Limits::default()
    };
    let mut rig = Rig::new(Capabilities::none(), limits).await;
    rig.value("globalThis.lost = true").await;
    let sleeper = rig.start();
    rig.sandbox
        .evaluate(sleeper, "await new Promise(r => setTimeout(r, 5000)); return 'woke'")
        .await;
    let (spinner, events) = rig.run("while (true) {}").await;
    assert!(output(&events)[0].contains("runtime reset"), "{events:?}");
    assert_eq!(result(&events, sleeper).0, Outcome::Throw);
    assert_eq!(result(&events, spinner).0, Outcome::Throw);
    assert!(!rig.sandbox.is_waiting());
    assert_eq!(rig.value("return typeof lost").await, r#""undefined""#);
}

#[tokio::test]
async fn memory_is_capped_and_running_out_is_catchable() {
    let mut rig = Rig::plain().await;
    rig.value("globalThis.kept = 1").await;
    let text = rig.error("const a = []; while (true) a.push('x'.repeat(1e6))").await;
    assert!(text.contains("out of memory"), "{text}");
    assert_eq!(rig.value("return kept").await, "1");
    assert_eq!(rig.value("return 'y'.repeat(1e7).length").await, "10000000");
}

#[tokio::test]
async fn runaway_recursion_replaces_the_instance() {
    let mut rig = Rig::plain().await;
    rig.value("globalThis.lost = true").await;
    let (eval, events) = rig
        .run("const f = n => f(n + 1) + 1; try { f(0) } catch (e) { return e.name }")
        .await;
    let (outcome, text) = result(&events, eval);
    assert!(outcome == Outcome::Throw && text.contains("runtime reset"), "{text}");
    assert_eq!(rig.value("return typeof lost").await, r#""undefined""#);
}

#[tokio::test]
async fn output_is_bounded_but_results_get_through() {
    let mut rig = Rig::plain().await;
    let (eval, events) = rig
        .run("for (let i = 0; i < 1e5; i++) console.log('x'.repeat(100)); return 'done'")
        .await;
    assert_eq!(result(&events, eval), (Outcome::Return, r#""done""#.into()));
    let bytes: usize = output(&events).iter().map(String::len).sum();
    assert!(bytes <= Limits::default().output_per_slice + 200, "{bytes}");
    let (_, events) = rig.run("console.log('z'.repeat(1e6))").await;
    assert!(output(&events)[0].ends_with("bytes truncated]"));
}

#[tokio::test]
async fn the_bridge_is_out_of_reach() {
    let mut rig = Rig::plain().await;
    let names = rig
        .value("return Object.getOwnPropertyNames(globalThis).filter(n => /raw|settled|bridge|setTimer/.test(n))")
        .await;
    assert_eq!(names, "[]");
    rig.value("globalThis.setTimeout = null; globalThis.Promise = null")
        .await;
    assert_eq!(rig.value("return 1 + 1").await, "2");
}

#[tokio::test]
async fn the_checker_parses_without_running() {
    let mut checker = host().checker().await.unwrap();
    assert!(checker.parses("await x; return 1").await.unwrap());
    assert!(checker.parses("while (true) {}").await.unwrap());
    assert!(!checker.parses("return (").await.unwrap());
    assert!(!checker.parses("<invoke name=\"eval\">").await.unwrap());
}

/// A database holding a trajectory with one eval, and the agent's access to it.
async fn agent_database() -> (tempfile::TempDir, Rig) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("agent.db");
    let store = SqliteStore::new(db::open(&path, Access::ReadWrite).unwrap(), TRAJECTORY_TABLE).unwrap();
    let log = Trajectory::new(store).unwrap();
    log.append(Event::Eval { code: "1".into() }).unwrap();
    let sql = Sql::open(&path, TRAJECTORY_TABLE).unwrap();
    (
        dir,
        Rig::new(Capabilities::none().with_sql(sql), Limits::default()).await,
    )
}

#[tokio::test]
async fn the_agent_keeps_its_own_tables_and_reads_its_history() {
    let (_dir, mut rig) = agent_database().await;
    rig.value("await db.run('CREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT)')")
        .await;
    assert_eq!(
        rig.value("return await db.run('INSERT INTO notes (body) VALUES (?)', 'hi')")
            .await,
        "{changes: 1, lastInsertRowid: 1}"
    );
    assert_eq!(
        rig.value("return await db.query('SELECT body FROM notes')").await,
        r#"[{body: "hi"}]"#
    );
    let history = rig
        .value("return (await db.query('SELECT seq, data FROM trajectory')).length")
        .await;
    assert_eq!(history, "1");
    assert!(
        rig.value("return await db.query('PRAGMA table_info(notes)')")
            .await
            .contains("body")
    );
}

#[tokio::test]
async fn the_agent_cannot_change_its_history_or_reach_other_files() {
    let (dir, mut rig) = agent_database().await;
    let outside = dir.path().join("copy.db");
    for attack in [
        "INSERT INTO trajectory (timestamp, data) VALUES ('t', '{}')".to_owned(),
        "UPDATE trajectory SET data = '{}'".to_owned(),
        "DELETE FROM trajectory".to_owned(),
        "DROP TABLE trajectory".to_owned(),
        "ALTER TABLE trajectory RENAME TO gone".to_owned(),
        "CREATE TRIGGER sneaky AFTER INSERT ON trajectory BEGIN DELETE FROM trajectory; END".to_owned(),
        "CREATE INDEX idx ON trajectory (data)".to_owned(),
        format!("ATTACH DATABASE '{}' AS other", outside.display()),
        format!("VACUUM INTO '{}'", outside.display()),
        "PRAGMA writable_schema = ON".to_owned(),
        "PRAGMA journal_mode = DELETE".to_owned(),
        "SELECT load_extension('x')".to_owned(),
    ] {
        let text = rig
            .error(&format!("await db.run({})", serde_json::to_string(&attack).unwrap()))
            .await;
        assert!(
            text.contains("not authorized") || text.contains("authorization denied"),
            "{attack}: {text}"
        );
    }
    assert!(!outside.exists());
    assert_eq!(
        rig.value("return (await db.query('SELECT * FROM trajectory')).length")
            .await,
        "1"
    );
}

#[tokio::test]
async fn fetch_refuses_anything_but_the_public_internet() {
    let mut rig = Rig::new(
        Capabilities::none().with_fetch(Fetch::new(None).unwrap()),
        Limits::default(),
    )
    .await;
    for url in [
        "http://127.0.0.1:9/",
        "http://169.254.169.254/latest/meta-data/",
        "http://localhost:9/",
        "http://[::1]:9/",
        "http://[::ffff:10.0.0.1]/",
        "http://10.1.2.3/",
        "file:///etc/passwd",
    ] {
        let text = rig.error(&format!("await fetch('{url}')")).await;
        assert!(
            text.contains("not a public address") || text.contains("public address") || text.contains("only http"),
            "{url}: {text}"
        );
    }
}

#[test]
fn public_addresses_exclude_private_reserved_and_metadata_ranges() {
    let public = ["1.1.1.1", "8.8.8.8", "2606:4700:4700::1111"];
    let private = [
        "127.0.0.1",
        "10.0.0.1",
        "172.16.0.1",
        "192.168.1.1",
        "169.254.169.254",
        "100.64.0.1",
        "0.0.0.0",
        "255.255.255.255",
        "224.0.0.1",
        "240.0.0.1",
        "198.18.0.1",
        "::1",
        "::",
        "fc00::1",
        "fe80::1",
        "::ffff:127.0.0.1",
        "64:ff9b::a00:1",
        "2001:db8::1",
    ];
    for ip in public {
        assert!(is_public(ip.parse::<IpAddr>().unwrap()), "{ip} should be public");
    }
    for ip in private {
        assert!(!is_public(ip.parse::<IpAddr>().unwrap()), "{ip} should not be public");
    }
}
