//! mbx's pool rule against its proven model.
//!
//! [`model`] is the Rust kernel bend-emit generates from SmoothBricks'
//! `specs/cowshed/laws/admission.bend`, the model whose laws are the pool's
//! contract (`specs/cowshed/05_gateway.md`, "The pool's laws" and "Held jobs",
//! Order). Each trace here runs through the model's `step(Spt, …)` and through
//! [`admission_pass`]; after every decision both must have granted the same
//! requests the same cores.
//!
//! The model abstracts the pool. Where they differ, the difference is excluded
//! by construction here, never tolerated:
//! - **Every request is moldable.** The model hands a waiting request
//!   min(want, free) cores once; it has no rigid request, which starts only on
//!   all of its cores. Every record here is [`Shape::Moldable`], the recorded
//!   window's rigid compiles and tests included.
//! - **Cores only.** The model has no memory: no record charges any and the
//!   budget's memory is unbounded, so neither the memory charge nor the
//!   available-memory gate takes part.
//! - **One pool.** One memory domain, no build ceiling, no pressure: the model
//!   has none of them.
//!
//! A tick is one second of the pool's clock, and the model's aging bound is
//! [`AGING_MS`] in ticks. The model compares its naturals in unary, so it is
//! handed each prediction's rank among the trace's, which orders requests as
//! the milliseconds mbx is handed do.

use super::scheduler_bend_model as model;
use super::*;
use std::collections::BTreeMap;
use std::rc::Rc;

const TICK_MS: u64 = 1000;
const BOUND_TICKS: u64 = AGING_MS / TICK_MS;
const TRACES: u64 = 2000;

#[derive(Clone, Copy, Debug)]
enum Op {
    /// A request at normal or low priority for `want` cores, its prediction
    /// as the model ranks it and as mbx records it.
    Arrive {
        normal: bool,
        want: u64,
        rank: u64,
        predicted_ms: Option<u64>,
    },
    Decide,
    Tick,
    /// The `n`th arrival ends: its grant is released, or it is withdrawn.
    Finish(u64),
}

/// The model and the pool, fed the same operations.
struct Sides {
    pool: u64,
    model: model::St,
    ticks: u64,
    arrivals: u64,
    waiting: Vec<Waiting>,
    running: Vec<(u64, Lease)>,
    /// Decisions taken while an aged request waited: the traces reach the
    /// aging rule.
    aged_decisions: usize,
}

fn empty_lane() -> model::Lane {
    model::Lane::Lane(Rc::new(model::List::Nil), Rc::new(model::List::Nil))
}

/// The cores each of the model's requests holds, by arrival.
fn holdings(state: &model::St) -> BTreeMap<u64, u64> {
    let model::St::St(_, _, gate, low) = state;
    let mut held = BTreeMap::new();
    for model::Lane::Lane(old, new) in [gate, low] {
        for list in [old, new] {
            let mut cursor = list;
            while let model::List::Con(model::E::E(id, _, k, _, _), tail) = &**cursor {
                held.insert(*id, *k);
                cursor = tail;
            }
        }
    }
    held
}

fn handoff(pool: u64) -> Handoff {
    Handoff {
        budget: Resources {
            cpu: pool,
            memory_bytes: 0,
        },
        gate: None,
    }
}

/// The `id`th arrival as its holder publishes it: moldable, charging no
/// memory, in the one domain.
fn record(
    id: u64,
    normal: bool,
    want: u64,
    predicted_ms: Option<u64>,
    now_ms: u64,
    pool: u64,
) -> Waiting {
    Waiting {
        pid: None,
        first_arrival_ms: Some(now_ms),
        name: None,
        mode_key: None,
        predicted_ms,
        ticket: id,
        weight: want,
        cpu: Some(want),
        memory_bytes: Some(0),
        priority: if normal { "normal" } else { "low" }.into(),
        domain: "host".into(),
        domain_capacity: pool,
        global_capacity: Some(pool),
        cpu_cap: Some(pool),
        mem_cap: Some(0),
        domain_memory_cap: Some(0),
        domain_cpu_cap: Some(pool),
        bytes_per_permit: 0,
        build: None,
        handoff: Some(handoff(pool)),
        shape: Some(Shape::Moldable),
    }
}

impl Sides {
    fn new(pool: u64) -> Self {
        Self {
            pool,
            model: model::St::St(pool, BOUND_TICKS, empty_lane(), empty_lane()),
            ticks: 0,
            arrivals: 0,
            waiting: Vec::new(),
            running: Vec::new(),
            aged_decisions: 0,
        }
    }

    fn now_ms(&self) -> u64 {
        self.ticks * TICK_MS
    }

    /// Apply `op` to both sides. A decision answers the grants both made,
    /// arrival to cores, or how they differ.
    fn apply(&mut self, op: Op) -> Result<BTreeMap<u64, u64>, String> {
        let step = match op {
            Op::Arrive {
                normal, want, rank, ..
            } => model::Op::Arrive(normal, self.arrivals + 1, want - 1, rank),
            Op::Decide => model::Op::Decide,
            Op::Tick => model::Op::Tick,
            Op::Finish(id) => model::Op::Finish(id),
        };
        let before = holdings(&self.model);
        self.model = model::step(model::Pol::Spt, step, self.model.clone());
        match op {
            Op::Arrive {
                normal,
                want,
                predicted_ms,
                ..
            } => {
                self.arrivals += 1;
                self.waiting.push(record(
                    self.arrivals,
                    normal,
                    want,
                    predicted_ms,
                    self.now_ms(),
                    self.pool,
                ));
            }
            Op::Tick => self.ticks += 1,
            Op::Finish(id) => {
                self.waiting.retain(|record| record.ticket != id);
                self.running.retain(|(running, _)| *running != id);
            }
            Op::Decide => {
                let modelled: BTreeMap<u64, u64> = holdings(&self.model)
                    .into_iter()
                    .filter(|&(id, k)| k > 0 && before.get(&id).copied().unwrap_or(0) == 0)
                    .collect();
                let granted = self.decide();
                if granted != modelled {
                    return Err(format!(
                        "at {} s the model granted {modelled:?} and admission_pass {granted:?}",
                        self.ticks
                    ));
                }
                return Ok(granted);
            }
        }
        Ok(BTreeMap::new())
    }

    /// One admission pass over everything waiting, its grants held from now.
    fn decide(&mut self) -> BTreeMap<u64, u64> {
        let Some(request) = self.waiting.first().cloned() else {
            return BTreeMap::new();
        };
        let now_ms = self.now_ms();
        if self.waiting.iter().any(|record| {
            now_ms.saturating_sub(record.first_arrival_ms.unwrap_or(now_ms)) >= AGING_MS
        }) {
            self.aged_decisions += 1;
        }
        let pass = admission_pass(
            self.running
                .iter()
                .map(|(_, lease)| lease.clone())
                .collect(),
            &self.waiting,
            request,
            handoff(self.pool),
            PassContext {
                now_ms,
                available_memory: None,
                single: false,
            },
            false,
        );
        let mut granted = BTreeMap::new();
        for grant in &pass.grants {
            let record = match grant.grantee {
                Grantee::Queued(index) => &pass.queue[index],
                Grantee::Request => &pass.request,
            };
            let lease = pass.live[grant.live_before].clone();
            granted.insert(record.ticket, lease.resources().cpu);
            self.running.push((record.ticket, lease));
        }
        self.waiting
            .retain(|record| !granted.contains_key(&record.ticket));
        granted
    }
}

fn run_trace(pool: u64, ops: &[Op]) -> Result<Sides, String> {
    let mut sides = Sides::new(pool);
    for (index, &op) in ops.iter().enumerate() {
        sides
            .apply(op)
            .map_err(|error| format!("operation {index}: {error}"))?;
    }
    Ok(sides)
}

/// The shortest trace left by dropping operations one at a time while the
/// two sides still disagree.
fn shrink(pool: u64, mut ops: Vec<Op>) -> Vec<Op> {
    loop {
        let before = ops.len();
        let mut index = 0;
        while index < ops.len() {
            let mut fewer = ops.clone();
            fewer.remove(index);
            if run_trace(pool, &fewer).is_err() {
                ops = fewer;
            } else {
                index += 1;
            }
        }
        if ops.len() == before {
            return ops;
        }
    }
}

/// SplitMix64: seeded, so a failing trace is its seed.
struct Random(u64);

impl Random {
    fn below(&mut self, bound: u64) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        (z ^ (z >> 31)) % bound
    }
}

/// A pool of 1 to 18 cores and up to 40 operations. Each trace weighs its
/// ticks differently, so some reach the aging bound within their length.
fn random_trace(seed: u64) -> (u64, Vec<Op>) {
    let mut random = Random(seed);
    let pool = 1 + random.below(18);
    let ticks = 1 + random.below(40);
    let mut arrivals = 0;
    let length = 1 + random.below(40);
    let ops = (0..length)
        .map(|_| match random.below(6 + ticks) {
            0..=2 => {
                arrivals += 1;
                let seconds = 1 + random.below(600);
                Op::Arrive {
                    normal: random.below(2) == 0,
                    want: 1 + random.below(4),
                    rank: seconds,
                    predicted_ms: Some(seconds * TICK_MS),
                }
            }
            3 | 4 => Op::Decide,
            5 => Op::Finish(1 + random.below(arrivals.max(1))),
            _ => Op::Tick,
        })
        .collect();
    (pool, ops)
}

/// The model recurses once per list entry and once per unit of a natural.
fn on_a_deep_stack(test: impl FnOnce() + Send + 'static) {
    let outcome = std::thread::Builder::new()
        .stack_size(512 << 20)
        .spawn(test)
        .unwrap()
        .join();
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

#[test]
fn admission_pass_grants_what_the_proven_model_grants_on_random_traces() {
    on_a_deep_stack(|| {
        let mut aged = 0;
        for seed in 0..TRACES {
            let (pool, ops) = random_trace(seed);
            match run_trace(pool, &ops) {
                Ok(sides) => aged += sides.aged_decisions,
                Err(error) => {
                    let shrunk = shrink(pool, ops);
                    let shrunk_error = run_trace(pool, &shrunk).err().unwrap_or_default();
                    panic!(
                        "seed {seed}, pool {pool}: {error}\nshrunk trace: {shrunk:?}\nwhich fails at {shrunk_error}"
                    );
                }
            }
        }
        assert!(aged > 0, "no trace reached the aging bound");
    });
}

const RECORDED: &str = include_str!("scheduler_bend_replay_20261109.txt");

struct Recorded {
    arrival_ms: u64,
    normal: bool,
    cpu: u64,
    predicted_ms: Option<u64>,
    run_ms: u64,
}

/// The recorded window's capacity and requests, in arrival order.
fn recorded() -> (u64, Vec<Recorded>) {
    let mut capacity = None;
    let mut requests = Vec::new();
    for line in RECORDED.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        match fields.as_slice() {
            [] => {}
            [comment, ..] if comment.starts_with('#') => {}
            ["capacity", cpu] => capacity = Some(cpu.parse().unwrap()),
            ["request", arrival, priority, cpu, predicted, run] => requests.push(Recorded {
                arrival_ms: arrival.parse().unwrap(),
                normal: *priority == "normal",
                cpu: cpu.parse().unwrap(),
                predicted_ms: (*predicted != "-").then(|| predicted.parse().unwrap()),
                run_ms: run.parse().unwrap(),
            }),
            _ => panic!("not a recorded line: {line}"),
        }
    }
    (capacity.expect("the window names its capacity"), requests)
}

#[test]
fn admission_pass_grants_what_the_proven_model_grants_on_the_recorded_gate_window() {
    on_a_deep_stack(|| {
        let (pool, requests) = recorded();
        let mut predictions: Vec<u64> = requests.iter().filter_map(|r| r.predicted_ms).collect();
        predictions.sort_unstable();
        predictions.dedup();
        // An unpredicted request ranks after every predicted one, as mbx
        // orders `None` after every prediction.
        let rank = |predicted: Option<u64>| {
            predicted.map_or(predictions.len() as u64 + 1, |ms| {
                predictions.binary_search(&ms).unwrap() as u64 + 1
            })
        };
        let mut sides = Sides::new(pool);
        let mut ends: BTreeMap<u64, Vec<u64>> = BTreeMap::new();
        let mut next = 0;
        let mut granted = 0;
        loop {
            let tick = sides.ticks;
            for id in ends.remove(&tick).unwrap_or_default() {
                sides.apply(Op::Finish(id)).unwrap();
            }
            while let Some(request) = requests
                .get(next)
                .filter(|request| request.arrival_ms / TICK_MS <= tick)
            {
                sides
                    .apply(Op::Arrive {
                        normal: request.normal,
                        want: request.cpu,
                        rank: rank(request.predicted_ms),
                        predicted_ms: request.predicted_ms,
                    })
                    .unwrap();
                next += 1;
            }
            let grants = sides
                .apply(Op::Decide)
                .unwrap_or_else(|error| panic!("after {next} arrivals: {error}"));
            for id in grants.into_keys() {
                let run_ms = requests[usize::try_from(id).unwrap() - 1].run_ms;
                ends.entry(tick + run_ms.div_ceil(TICK_MS).max(1))
                    .or_default()
                    .push(id);
                granted += 1;
            }
            if next == requests.len() && ends.is_empty() && sides.waiting.is_empty() {
                break;
            }
            sides.apply(Op::Tick).unwrap();
        }
        assert_eq!(granted, requests.len(), "every recorded request ran");
        assert!(
            sides.aged_decisions > 0,
            "the window reached the aging bound"
        );
    });
}
