//! The harness with every dependency in memory. Model calls wait for the test
//! to reply. Evaluations settle immediately with "ran <code>" unless the code
//! is "hold", which waits for `release`.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use evalien::db::MemoryStore;
use evalien::domain::{
    Evaluator, EvaluatorStopped, Event, FinishReason, GenerateError, Generation, Model, Outcome, Prompt, Reply, Seq,
    Trajectory, Usage, Usd,
};
use evalien::harness::{Harness, Stopped, context::render_entry};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

fn generation() -> Generation {
    Generation {
        model: "test/model".into(),
        latency: Duration::from_millis(5),
        finish_reason: FinishReason::Stop,
        usage: Usage {
            uncached_input_tokens: 1,
            cache_read_tokens: 2,
            cache_write_tokens: 3,
            output_tokens: 4,
            reasoning_tokens: 0,
            cost: Usd::new(0.001).unwrap(),
        },
    }
}

struct Call {
    prompt: Prompt,
    reply: oneshot::Sender<Result<Reply, GenerateError>>,
}

struct FakeModel {
    calls: mpsc::UnboundedSender<Call>,
}

impl Model for FakeModel {
    async fn generate(&self, prompt: &Prompt) -> Result<Reply, GenerateError> {
        let (reply, answer) = oneshot::channel();
        let _ = self.calls.send(Call {
            prompt: prompt.clone(),
            reply,
        });
        answer
            .await
            .unwrap_or_else(|_| Err(GenerateError::Failed("test finished".into())))
    }
}

struct FakeEvaluator {
    log: Arc<Trajectory>,
    held: Arc<Mutex<VecDeque<Seq>>>,
}

impl Evaluator for FakeEvaluator {
    async fn start(&self, eval: Seq, code: String) -> Result<(), EvaluatorStopped> {
        if code == "hold" {
            self.held.lock().unwrap().push_back(eval);
        } else {
            let text = format!("ran {code}");
            self.log
                .append(Event::Result {
                    eval,
                    outcome: Outcome::Return,
                    text,
                })
                .unwrap();
        }
        Ok(())
    }
}

struct Rig {
    log: Arc<Trajectory>,
    calls: mpsc::UnboundedReceiver<Call>,
    held: Arc<Mutex<VecDeque<Seq>>>,
    done: JoinHandle<Stopped>,
}

impl Rig {
    fn start(idle_delays: Vec<Duration>) -> Rig {
        let log = Arc::new(Trajectory::new(MemoryStore::new()).unwrap());
        let (tx, calls) = mpsc::unbounded_channel();
        let held = Arc::new(Mutex::new(VecDeque::new()));
        let harness = Harness {
            log: log.clone(),
            model: FakeModel { calls: tx },
            evaluator: FakeEvaluator {
                log: log.clone(),
                held: held.clone(),
            },
            instructions: "test instructions".into(),
            idle_delays,
            debug: |_| {},
        };
        Rig {
            log,
            calls,
            held,
            done: tokio::spawn(harness.run()),
        }
    }

    async fn next_call(&mut self) -> Call {
        tokio::time::timeout(Duration::from_secs(2), self.calls.recv())
            .await
            .expect("timed out waiting for a model call")
            .expect("the harness stopped")
    }

    async fn no_call_within(&mut self, wait: Duration) {
        assert!(
            tokio::time::timeout(wait, self.calls.recv()).await.is_err(),
            "unexpected model call"
        );
    }

    fn reply(call: Call, code: &str) {
        let _ = call.reply.send(Ok(Reply {
            generation: generation(),
            code: code.into(),
        }));
    }

    fn types(&self) -> Vec<&'static str> {
        self.log.read_all().unwrap().iter().map(|e| kind(&e.event)).collect()
    }

    fn type_line(&self, text: &str) {
        self.log.append(Event::Stdin { text: text.into() }).unwrap();
    }

    fn print(&self, text: &str) {
        self.log.append(Event::Stdout { text: text.into() }).unwrap();
    }

    fn release(&self, text: &str) {
        let eval = self.held.lock().unwrap().pop_front().expect("nothing is held");
        self.log
            .append(Event::Result {
                eval,
                outcome: Outcome::Return,
                text: text.into(),
            })
            .unwrap();
    }

    /// What a prompt through this seq contains.
    fn through(&self, seq: Seq) -> Vec<String> {
        self.log
            .read(Seq::ZERO, seq)
            .unwrap()
            .iter()
            .map(render_entry)
            .collect()
    }

    fn latest(&self) -> Vec<String> {
        self.through(self.log.last_seq())
    }

    async fn until(&self, condition: impl Fn(&Rig) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !condition(self) {
            assert!(Instant::now() < deadline, "timed out waiting for the harness");
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }
}

fn kind(event: &Event) -> &'static str {
    match event {
        Event::Start => "start",
        Event::Eval { .. } => "eval",
        Event::Stdin { .. } => "stdin",
        Event::Stdout { .. } => "stdout",
        Event::Stderr { .. } => "stderr",
        Event::Result { .. } => "result",
        Event::Generation { .. } => "generation",
    }
}

#[tokio::test]
async fn every_result_wakes_the_model_with_the_trajectory_through_that_point() {
    let mut rig = Rig::start(vec![]);
    let call = rig.next_call().await;
    assert_eq!(
        call.prompt,
        Prompt {
            instructions: "test instructions".into(),
            blocks: rig.through(Seq::new(1))
        }
    );

    Rig::reply(call, "first");
    let call = rig.next_call().await;
    assert_eq!(rig.types(), ["start", "generation", "eval", "result"]);
    let entries = rig.log.read_all().unwrap();
    assert_eq!(
        entries[1].event,
        Event::Generation {
            through: Seq::new(1),
            generation: generation()
        }
    );
    assert_eq!(entries[2].event, Event::Eval { code: "first".into() });
    assert_eq!(
        entries[3].event,
        Event::Result {
            eval: entries[2].seq,
            outcome: Outcome::Return,
            text: "ran first".into()
        }
    );
    assert_eq!(call.prompt.blocks, rig.through(Seq::new(4)));
}

#[tokio::test]
async fn input_during_a_model_call_is_kept_for_the_next_one() {
    let mut rig = Rig::start(vec![]);
    let call = rig.next_call().await;
    rig.type_line("first");
    rig.type_line("second");
    Rig::reply(call, "work");
    let call = rig.next_call().await;
    // The result settled before the next decision, so one call sees both.
    assert_eq!(rig.types(), ["start", "stdin", "stdin", "generation", "eval", "result"]);
    assert_eq!(call.prompt.blocks, rig.through(Seq::new(6)));
}

#[tokio::test]
async fn input_reaches_the_model_while_an_evaluation_is_pending_and_results_settle_in_any_order() {
    let mut rig = Rig::start(vec![]);
    let call = rig.next_call().await;
    Rig::reply(call, "hold");
    rig.until(|r| r.types().contains(&"eval")).await;
    // Output from a pending evaluation doesn't wake the model by itself.
    rig.print("progress\n");
    rig.no_call_within(Duration::from_millis(20)).await;

    rig.type_line("hello?");
    let call = rig.next_call().await;
    assert_eq!(call.prompt.blocks, rig.latest());
    Rig::reply(call, "second");
    let third = rig.next_call().await;
    rig.release("'resumed'");
    rig.until(|r| r.types().iter().filter(|t| **t == "result").count() == 2)
        .await;
    let entries = rig.log.read_all().unwrap();
    let evals: Vec<Seq> = entries
        .iter()
        .filter(|e| matches!(e.event, Event::Eval { .. }))
        .map(|e| e.seq)
        .collect();
    let results: Vec<Seq> = entries
        .iter()
        .filter_map(|e| match e.event {
            Event::Result { eval, .. } => Some(eval),
            _ => None,
        })
        .collect();
    assert_eq!(results, [evals[1], evals[0]]);
    // The third call was already running when the first evaluation settled.
    Rig::reply(third, "");
    let fourth = rig.next_call().await;
    assert_eq!(fourth.prompt.blocks, rig.latest());
}

#[tokio::test]
async fn empty_code_ends_the_turn_and_the_next_input_or_idle_output_starts_another() {
    let mut rig = Rig::start(vec![]);
    let call = rig.next_call().await;
    Rig::reply(call, "  \n");
    rig.no_call_within(Duration::from_millis(20)).await;
    assert_eq!(rig.types(), ["start", "generation"]);

    rig.type_line("wake up");
    let call = rig.next_call().await;
    Rig::reply(call, "");
    rig.no_call_within(Duration::from_millis(20)).await;
    // With nothing pending, output from background code wakes the model.
    rig.print("alert\n");
    rig.next_call().await;
    assert_eq!(rig.types(), ["start", "generation", "stdin", "generation", "stdout"]);
}

#[tokio::test]
async fn quiet_calls_are_spaced_out_and_human_input_ends_the_wait() {
    let mut rig = Rig::start(vec![Duration::ZERO, Duration::from_millis(300)]);
    let call = rig.next_call().await;
    let mut at = vec![Instant::now()];
    Rig::reply(call, "one");
    let call = rig.next_call().await;
    at.push(Instant::now());
    Rig::reply(call, "two");
    let call = rig.next_call().await;
    at.push(Instant::now());
    assert!(
        at[1] - at[0] < Duration::from_millis(200),
        "first quiet call took {:?}",
        at[1] - at[0]
    );
    assert!(
        at[2] - at[1] >= Duration::from_millis(250),
        "second quiet call came after {:?}",
        at[2] - at[1]
    );

    Rig::reply(call, "three");
    rig.no_call_within(Duration::from_millis(30)).await;
    let sent = Instant::now();
    rig.type_line("hurry");
    let call = rig.next_call().await;
    assert!(
        sent.elapsed() < Duration::from_millis(200),
        "input waited {:?}",
        sent.elapsed()
    );
    assert_eq!(rig.types().last(), Some(&"stdin"));
    assert_eq!(call.prompt.blocks, rig.latest());
}

#[tokio::test]
async fn a_billed_call_without_source_is_recorded_then_stops_the_harness() {
    let mut rig = Rig::start(vec![]);
    let call = rig.next_call().await;
    let unusable = Generation {
        finish_reason: FinishReason::Length,
        ..generation()
    };
    let error = GenerateError::Unusable {
        generation: unusable.clone(),
        reason: "did not complete: length".into(),
    };
    let _ = call.reply.send(Err(error.clone()));
    let stopped = (&mut rig.done).await.unwrap();
    assert!(matches!(stopped, Stopped::Generation(e) if e == error));
    let last = rig.log.read_all().unwrap().pop().unwrap();
    assert_eq!(
        last.event,
        Event::Generation {
            through: Seq::new(1),
            generation: unusable
        }
    );
}

#[tokio::test]
async fn other_errors_stop_the_harness_without_a_recorded_call() {
    let mut rig = Rig::start(vec![]);
    let call = rig.next_call().await;
    let budget = GenerateError::BudgetSpent {
        spent: Usd::new(1.5).unwrap(),
        budget: Usd::new(1.0).unwrap(),
    };
    let _ = call.reply.send(Err(budget.clone()));
    let stopped = (&mut rig.done).await.unwrap();
    assert!(matches!(stopped, Stopped::Generation(e) if e == budget));
    assert_eq!(rig.types(), ["start"]);
}
