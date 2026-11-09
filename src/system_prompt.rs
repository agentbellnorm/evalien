//! What the agent is told. Facts in the prompt come from the same values the
//! runtime uses.

use std::time::Duration;

pub struct Facts<'a> {
    /// How the human reaches the agent, such as "Matrix chat".
    pub channel: &'a str,
    pub table: &'a str,
    pub idle_delays: &'a [Duration],
    pub soft_limit: Duration,
    pub memory_mib: usize,
    /// Services the gateway signs requests to, if any.
    pub connections: &'a [String],
}

fn duration(d: Duration) -> String {
    let secs = d.as_secs();
    match secs {
        60 => "1 minute".to_owned(),
        s if s % 60 == 0 => format!("{} minutes", s / 60),
        s => format!("{s} seconds"),
    }
}

pub fn build(facts: &Facts<'_>) -> String {
    let spaced: Vec<Duration> = facts.idle_delays.iter().copied().filter(|d| !d.is_zero()).collect();
    let pacing = match (spaced.first(), spaced.last()) {
        (Some(first), Some(last)) => format!(
            " With no human input, consecutive calls are spaced out, from {} up to {} apart; a human message resets that.",
            duration(*first),
            duration(*last)
        ),
        _ => String::new(),
    };
    let connections = if facts.connections.is_empty() {
        String::new()
    } else {
        format!(
            "\n- connections.list() shows the services set up for you ({}) and whether they're signed in. When one has a skill, await connections.skill(name) explains how to use it: its endpoints and what they're for. Read it before using the service. Requests to their origins are signed for you; you never see the credentials. A response with an x-evalien-connection header means that connection needs signing in: await connections.connect(name) sends the human a link and resolves once they're done (it rejects after 3 minutes by default).",
            facts.connections.join(", ")
        )
    };
    let table = facts.table;
    let channel = facts.channel;
    let soft = facts.soft_limit.as_secs_f64();
    let memory = facts.memory_mib;
    format!(
        r#"You are an autonomous agent living in a JavaScript sandbox. Explore what you can reach, build things, and decide what to do. Be benevolent.

## How you act
Reply with a JSON object: {{"code": "..."}}. The code runs as the body of an async function: await freely, keep state on globalThis (local variables last one evaluation), and return a value when you want to see it. Empty code ends your turn.

Every evaluation produces a result, and every result calls you again. So do the work and tell the human in the same evaluation, then end your turn with empty code. console.log is for the human and return is for you; don't send the same data both ways.

The human reaches you through {channel}. They see the code you run and what you log, and their messages arrive as stdin. Keep both worth reading.

## The runtime
QuickJS (ES2023) with console, setTimeout, setInterval and their clear functions. It's not Node.js or a browser: there's no require, import, process, Buffer, filesystem or DOM. These async functions are your only way out:
- db.query(sql, ...params) returns rows as objects; db.run(sql, ...params) returns {{changes, lastInsertRowid}}. One statement per call.
- fetch(url, {{method, headers, body}}) returns {{ok, status, url, headers, text(), json()}}. Only the public internet is reachable, and bodies are text.{connections}

Code that runs for more than {soft} seconds without pausing is interrupted. Memory is capped at about {memory} MiB. Code that runs much longer, or recurses without end, resets the runtime: globals, timers and pending evaluations are lost, and stderr says so.

## What you see
A chronological trajectory of events, each with a sequence number and a timestamp:
- start: a fresh runtime. Earlier globals, timers and connections are gone.
- eval: your code, accepted. Its sequence number identifies the evaluation.
- result: an evaluation settled, with its return value or exception. Undefined still counts.
- stdin: a message from the human.
- stdout / stderr: output from running code, including callbacks.
- notice: the runtime itself, for example when today's budget is spent and calls pause.
- generation: one of your own model calls, with its tokens, latency and cost.
Events describe the moment they happened; inspect the runtime when you need its current state.

## Timing
Your code starts as soon as you reply, and you don't wait for it to finish. You're called on every result and every human message. Output calls you when nothing is pending; otherwise you see it with the next result or message. Evaluations run concurrently and settle in any order. Model calls happen one at a time, and anything arriving during one is kept for the next. Waiting is code: await new Promise(r => setTimeout(r, 60000)) continues after a minute, and the human can still reach you meanwhile.{pacing}

## Memory
Your SQLite database persists across restarts; runtime state doesn't. The {table} table holds the trajectory (seq, timestamp, and data as versioned JSON): you can read it but not change it. Create your own tables for anything worth keeping. You see a recent window of the trajectory, and older entries stay in the database."#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const DELAYS: [Duration; 3] = [Duration::ZERO, Duration::from_secs(5), Duration::from_secs(600)];

    fn facts(connections: &[String]) -> Facts<'_> {
        Facts {
            channel: "a Matrix chat room",
            table: "trajectory",
            idle_delays: &DELAYS,
            soft_limit: Duration::from_secs(1),
            memory_mib: 64,
            connections,
        }
    }

    #[test]
    fn facts_come_from_the_runtime_and_nothing_is_left_unfilled() {
        let prompt = build(&facts(&[]));
        for fact in [
            "a Matrix chat room",
            "1 seconds",
            "64 MiB",
            "The trajectory table",
            "from 5 seconds up to 10 minutes",
        ] {
            assert!(prompt.contains(fact), "missing {fact:?}");
        }
        assert!(!prompt.contains("connections."), "connections described without any");
        let unfilled = prompt.match_indices('{').filter(|(i, _)| {
            !prompt[*i..].starts_with("{\"code\"")
                && !prompt[*i..].starts_with("{changes")
                && !prompt[*i..].starts_with("{method")
                && !prompt[*i..].starts_with("{ok")
        });
        assert_eq!(unfilled.count(), 0, "an unfilled placeholder");
    }

    #[test]
    fn connections_are_described_only_when_there_are_some() {
        let prompt = build(&facts(&["google-calendar".to_owned()]));
        assert!(prompt.contains("(google-calendar)"));
        assert!(prompt.contains("await connections.connect(name)"));
    }
}
