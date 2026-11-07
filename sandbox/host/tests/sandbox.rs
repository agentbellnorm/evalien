use std::{sync::Arc, time::Duration};

use evalien_sandbox::{
    EvalId, Event, Limits, Sandbox,
    capabilities::{Demo, Nothing},
};

fn sandbox() -> Sandbox {
    Sandbox::new(Arc::new(Demo), Limits::default()).unwrap()
}

/// Drain events until the sandbox is idle (or the timeout hits).
async fn drain(sandbox: &mut Sandbox) -> Vec<Event> {
    let mut events = Vec::new();
    let run = async {
        while let Some(event) = sandbox.next_event().await {
            events.push(event);
        }
    };
    tokio::time::timeout(Duration::from_secs(20), run)
        .await
        .expect("sandbox never went idle");
    events
}

async fn run(sandbox: &mut Sandbox, code: &str) -> (EvalId, Vec<Event>) {
    let id = sandbox.eval(code).unwrap();
    (id, drain(sandbox).await)
}

fn settled(events: &[Event], id: EvalId) -> (bool, String) {
    events
        .iter()
        .find_map(|e| match e {
            Event::Settled { id: i, ok, value } if *i == id => Some((*ok, value.clone())),
            _ => None,
        })
        .unwrap_or_else(|| panic!("eval {id} never settled: {events:?}"))
}

async fn value(sandbox: &mut Sandbox, code: &str) -> String {
    let (id, events) = run(sandbox, code).await;
    let (ok, value) = settled(&events, id);
    assert!(ok, "eval failed: {value}");
    value
}

#[tokio::test]
async fn returns_values_and_keeps_globals() {
    let mut sb = sandbox();
    assert_eq!(value(&mut sb, "globalThis.n = 41; return n + 1").await, "42");
    assert_eq!(value(&mut sb, "return n").await, "41");
    assert_eq!(
        value(&mut sb, "return { a: [1, 'b'], m: new Map([[1, 2]]) }").await,
        r#"{a: [1, "b"], m: Map(1) {1 => 2}}"#
    );
}

#[tokio::test]
async fn nothing_ambient_exists() {
    let mut sb = sandbox();
    let probe = "return ['process', 'require', 'Deno', 'Bun', 'std', 'os', 'fetch', 'WebAssembly', 'XMLHttpRequest']
        .filter(name => typeof globalThis[name] !== 'undefined')";
    assert_eq!(value(&mut sb, probe).await, "[]");
    let (id, events) = run(&mut sb, "return await import('fs')").await;
    assert!(!settled(&events, id).0, "dynamic import must fail");
}

#[tokio::test]
async fn capabilities_are_only_what_is_granted() {
    let mut sb = Sandbox::new(Arc::new(Nothing), Limits::default()).unwrap();
    assert_eq!(value(&mut sb, "return typeof echo").await, r#""undefined""#);
}

#[tokio::test]
async fn host_calls_resolve_and_run_concurrently() {
    let mut sb = sandbox();
    let slow = sb.eval("return await delay({ ms: 200, value: 'slow' })").unwrap();
    let fast = sb
        .eval("return await Promise.all([echo(1), delay({ ms: 10, value: 'fast' })])")
        .unwrap();
    let events = drain(&mut sb).await;
    let order: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            Event::Settled { id, .. } => Some(*id),
            _ => None,
        })
        .collect();
    assert_eq!(order, vec![fast, slow]);
    assert_eq!(settled(&events, fast).1, r#"[1, "fast"]"#);
    assert_eq!(settled(&events, slow).1, r#""slow""#);
}

#[tokio::test]
async fn host_call_errors_reject() {
    let mut sb = sandbox();
    assert_eq!(
        value(
            &mut sb,
            "try { await delay({ ms: -1 }) } catch (e) { return e.message }"
        )
        .await,
        r#""delay: ms must be a non-negative integer""#
    );
}

#[tokio::test]
async fn timers_and_console() {
    let mut sb = sandbox();
    let code = "
        let ticks = 0;
        const cancelled = setTimeout(() => console.log('never'), 10);
        clearTimeout(cancelled);
        await new Promise(done => {
            const t = setInterval(() => { console.log('tick', ++ticks); if (ticks === 3) { clearInterval(t); done(); } }, 5);
        });
        console.error('done');
        return ticks";
    let (id, events) = run(&mut sb, code).await;
    assert_eq!(settled(&events, id), (true, "3".into()));
    let out: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            Event::Stdout(s) | Event::Stderr(s) => Some(s.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(out, vec!["tick 1", "tick 2", "tick 3", "done"]);
}

#[tokio::test]
async fn errors_settle_with_agent_line_numbers() {
    let mut sb = sandbox();
    let (id, events) = run(&mut sb, "const x = 1;\nthrow new TypeError('boom')").await;
    let (ok, value) = settled(&events, id);
    assert!(!ok);
    assert!(value.starts_with("TypeError: boom"), "{value}");
    assert!(value.contains("<input>:2:"), "{value}");
    assert!(!value.contains("evaluate"), "prelude frames leaked: {value}");

    let (id, events) = run(&mut sb, "return (").await;
    assert!(settled(&events, id).1.starts_with("SyntaxError"));
}

#[tokio::test]
async fn soft_limit_interrupts_and_keeps_globals() {
    let mut sb = Sandbox::new(
        Arc::new(Demo),
        Limits {
            soft_slice: Duration::from_millis(100),
            ..Limits::default()
        },
    )
    .unwrap();
    value(&mut sb, "globalThis.kept = 'yes'").await;
    for spin in [
        "while (true) {}",
        "try { while (true) {} } catch { return 'caught' } finally { return 'finally' }",
    ] {
        let (id, events) = run(&mut sb, spin).await;
        let (ok, value) = settled(&events, id);
        assert!(!ok && value.contains("interrupted"), "{spin}: {value}");
        assert!(!events.iter().any(|e| matches!(e, Event::Reset { .. })));
    }
    assert_eq!(value(&mut sb, "return kept").await, r#""yes""#);
}

#[tokio::test]
async fn interrupt_after_await_is_reported() {
    // Known gap: a spin after an await escapes as an uncatchable error from a
    // job, and nothing ties it back to its eval. The interrupt is reported but
    // the eval stays pending.
    let mut sb = Sandbox::new(
        Arc::new(Demo),
        Limits {
            soft_slice: Duration::from_millis(100),
            ..Limits::default()
        },
    )
    .unwrap();
    let (id, events) = run(&mut sb, "await echo(1); while (true) {}").await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::Stderr(s) if s.contains("interrupted"))),
        "{events:?}"
    );
    assert!(sb.pending().any(|p| p == id));
}

#[tokio::test]
async fn hard_limit_kills_and_replaces_the_instance() {
    // Soft limit out of reach, so only the epoch deadline can stop the spin.
    let limits = Limits {
        soft_slice: Duration::from_secs(60),
        hard_slice: Duration::from_millis(200),
        ..Limits::default()
    };
    let mut sb = Sandbox::new(Arc::new(Demo), limits).unwrap();
    value(&mut sb, "globalThis.lost = true").await;
    let sleeper = sb
        .eval("await new Promise(r => setTimeout(r, 5000)); return 'woke'")
        .unwrap();
    let spinner = sb.eval("while (true) {}").unwrap();
    let events = drain(&mut sb).await;
    assert!(
        matches!(&events[0], Event::Reset { reason } if reason.contains("killed")),
        "{events:?}"
    );
    assert!(!settled(&events, sleeper).0);
    assert!(!settled(&events, spinner).0);
    assert_eq!(sb.pending().count(), 0);
    assert_eq!(value(&mut sb, "return typeof lost").await, r#""undefined""#);
}

#[tokio::test]
async fn memory_is_capped() {
    let mut sb = sandbox();
    value(&mut sb, "globalThis.kept = 1").await;
    let (id, events) = run(&mut sb, "const a = []; while (true) a.push('x'.repeat(1e6))").await;
    let (ok, value) = settled(&events, id);
    assert!(!ok && value.contains("out of memory"), "{value}");
    assert_eq!(self::value(&mut sb, "return kept").await, "1");
    // Freed again: the same allocation pattern fits once the array is gone.
    assert_eq!(self::value(&mut sb, "return 'y'.repeat(1e7).length").await, "10000000");
}

#[tokio::test]
async fn deep_recursion_is_a_catchable_error() {
    let mut sb = sandbox();
    let v = value(
        &mut sb,
        "const f = n => f(n + 1) + 1; try { f(0) } catch (e) { return e.name + ': ' + e.message }",
    )
    .await;
    assert!(v.contains("RangeError") || v.contains("InternalError"), "{v}");
}

#[tokio::test]
async fn output_is_bounded() {
    let mut sb = sandbox();
    let (id, events) = run(
        &mut sb,
        "for (let i = 0; i < 1e5; i++) console.log('x'.repeat(100)); return 'done'",
    )
    .await;
    assert_eq!(settled(&events, id), (true, r#""done""#.into()));
    let bytes: usize = events
        .iter()
        .map(|e| match e {
            Event::Stdout(s) => s.len(),
            _ => 0,
        })
        .sum();
    assert!(bytes <= Limits::default().output_per_slice + 200, "{bytes}");
    let (_, events) = run(&mut sb, "console.log('z'.repeat(1e6))").await;
    assert!(matches!(&events[0], Event::Stdout(s) if s.ends_with("bytes truncated]") && s.len() < 70_000));
}

#[tokio::test]
async fn bridge_internals_are_unreachable() {
    let mut sb = sandbox();
    // The raw imports live in a closure; the agent can only clobber its own globals.
    let v = value(
        &mut sb,
        "return Object.getOwnPropertyNames(globalThis).filter(n => /raw|settled|bridge|timerSet/.test(n))",
    )
    .await;
    assert_eq!(v, "[]");
    value(&mut sb, "globalThis.setTimeout = null; globalThis.Promise = null").await;
    assert_eq!(value(&mut sb, "return 1 + 1").await, "2");
}
