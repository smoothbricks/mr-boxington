use super::*;
use crate::scheduler::WAITERS_DIR;
use serde_json::{Value, json};
use std::path::Path;
use tokio::io::{DuplexStream, Lines, ReadHalf, WriteHalf};

/// Pools of `cpus` cores under `dir` that read nothing of the host, so the
/// grants below depend only on the requests each test makes.
fn pools(dir: &Path, cpus: u64) -> Pools {
    Pools::new(
        Pool::isolated(dir, cpus, 0, SchedulerPriority::Interactive),
        Pool::isolated(dir, cpus, 0, SchedulerPriority::Normal),
        Pool::isolated(dir, cpus, 0, SchedulerPriority::Low),
    )
}

/// One client connection, served in this test's runtime as the socket's
/// connections are.
struct Client {
    lines: Lines<BufReader<ReadHalf<DuplexStream>>>,
    writer: WriteHalf<DuplexStream>,
    served: tokio::task::JoinHandle<Result<()>>,
}

impl Client {
    fn connect(pools: &Pools) -> Self {
        let (client, server) = tokio::io::duplex(4096);
        let served = tokio::spawn(serve_connection(server, pools.clone()));
        let (reader, writer) = tokio::io::split(client);
        Self {
            lines: BufReader::new(reader).lines(),
            writer,
            served,
        }
    }

    async fn send_line(&mut self, line: &str) {
        self.writer
            .write_all(format!("{line}\n").as_bytes())
            .await
            .unwrap();
    }

    async fn send(&mut self, request: Value) {
        self.send_line(&request.to_string()).await;
    }

    /// The next reply, or `None` once the server has closed the connection.
    async fn reply(&mut self) -> Option<Value> {
        self.lines
            .next_line()
            .await
            .unwrap()
            .map(|line| serde_json::from_str(&line).unwrap())
    }

    /// Close the connection, as a client that dies does, and wait for the
    /// server to be done with it.
    async fn close(self) {
        let Self {
            lines,
            writer,
            served,
        } = self;
        drop((lines, writer));
        served.await.unwrap().unwrap();
    }
}

fn reserve(id: u64, cpus: u64, priority: &str) -> Value {
    json!({"type": "reserve", "id": id, "cpus": cpus, "priority": priority})
}

fn release(id: u64) -> Value {
    json!({"type": "release", "id": id})
}

fn granted(id: u64, cpus: u64) -> Option<Value> {
    Some(json!({"type": "granted", "id": id, "cpus": cpus}))
}

/// Wait until exactly `requests` requests wait in the pool under `dir`: the
/// pool publishes a waiting request's record before it sleeps on it, and
/// removes it when the request is granted or withdrawn.
async fn queued(dir: &Path, requests: usize) {
    let waiters = dir.join(WAITERS_DIR);
    while std::fs::read_dir(&waiters).map_or(0, Iterator::count) != requests {
        tokio::task::yield_now().await;
    }
}

#[test]
fn a_request_is_exactly_one_of_the_protocol_lines() {
    for (line, request) in [
        (
            r#"{"type":"reserve","id":3,"cpus":4,"priority":"low"}"#,
            Some(Request::Reserve {
                id: 3,
                cpus: 4,
                priority: SchedulerPriority::Low,
                memory: None,
                name: None,
                job_started_ms: None,
            }),
        ),
        (
            r#"{"type":"reserve","id":3,"cpus":4,"priority":"normal","memory":1024}"#,
            Some(Request::Reserve {
                id: 3,
                cpus: 4,
                priority: SchedulerPriority::Normal,
                memory: Some(1024),
                name: None,
                job_started_ms: None,
            }),
        ),
        (
            r#"{"type":"reserve","id":3,"cpus":4,"priority":"low","name":"nx:ws:build"}"#,
            Some(Request::Reserve {
                id: 3,
                cpus: 4,
                priority: SchedulerPriority::Low,
                memory: None,
                name: Some("nx:ws:build".into()),
                job_started_ms: None,
            }),
        ),
        (
            r#"{"type":"release","id":3}"#,
            Some(Request::Release { id: 3 }),
        ),
        (
            r#"{"type":"reserve","id":3,"cpus":4,"priority":"interactive","job_started_ms":1700000000000}"#,
            Some(Request::Reserve {
                id: 3,
                cpus: 4,
                priority: SchedulerPriority::Interactive,
                memory: None,
                name: None,
                job_started_ms: Some(1_700_000_000_000),
            }),
        ),
        // Absent or null is no job start: the request does not decay.
        (
            r#"{"type":"reserve","id":3,"cpus":4,"priority":"interactive","job_started_ms":null}"#,
            Some(Request::Reserve {
                id: 3,
                cpus: 4,
                priority: SchedulerPriority::Interactive,
                memory: None,
                name: None,
                job_started_ms: None,
            }),
        ),
        // A priority is named, never defaulted: the pool's order depends on it.
        (r#"{"type":"reserve","id":3,"cpus":4}"#, None),
        (
            r#"{"type":"reserve","id":3,"cpus":4,"priority":"urgent"}"#,
            None,
        ),
        (
            r#"{"type":"reserve","id":3,"cpus":4,"priority":"low","colour":"red"}"#,
            None,
        ),
        (r#"{"type":"release","id":-1}"#, None),
        (r#"{"type":"hold","id":3}"#, None),
        ("reserve 3 4 low", None),
    ] {
        assert_eq!(
            serde_json::from_str::<Request>(line).ok(),
            request,
            "{line}"
        );
    }
}

#[tokio::test]
async fn one_connection_holds_many_grants_and_releases_each() {
    let dir = tempfile::tempdir().unwrap();
    let pools = pools(dir.path(), 2);
    let mut client = Client::connect(&pools);

    client.send(reserve(1, 1, "normal")).await;
    assert_eq!(client.reply().await, granted(1, 1));
    client.send(reserve(2, 1, "normal")).await;
    assert_eq!(client.reply().await, granted(2, 1));
    // The pool is full: the third waits in it beside the two the connection holds.
    client.send(reserve(3, 1, "normal")).await;
    queued(dir.path(), 1).await;
    client.send(release(1)).await;
    assert_eq!(client.reply().await, granted(3, 1));
    client.send(release(2)).await;
    client.send(release(3)).await;
    // Both gone, and id 1 free again: the whole pool is this request's.
    client.send(reserve(1, 2, "normal")).await;
    assert_eq!(client.reply().await, granted(1, 2));
    client.close().await;
}

#[tokio::test]
async fn a_closed_connection_releases_its_grants_and_withdraws_its_requests() {
    let dir = tempfile::tempdir().unwrap();
    let pools = pools(dir.path(), 1);
    let mut dying = Client::connect(&pools);
    dying.send(reserve(1, 1, "normal")).await;
    assert_eq!(dying.reply().await, granted(1, 1));
    dying.send(reserve(2, 1, "normal")).await;
    queued(dir.path(), 1).await;
    let mut survivor = Client::connect(&pools);
    survivor.send(reserve(1, 1, "normal")).await;
    queued(dir.path(), 2).await;

    dying.close().await;
    // Neither its grant nor its older request holds the survivor back.
    assert_eq!(survivor.reply().await, granted(1, 1));
    // Its request left the queue with nothing granted to hold.
    queued(dir.path(), 0).await;
    survivor.close().await;
}

#[tokio::test]
async fn requests_are_granted_in_mbx_reserve_order_normal_priority_first() {
    let dir = tempfile::tempdir().unwrap();
    let pools = pools(dir.path(), 1);
    let mut holder = Client::connect(&pools);
    holder.send(reserve(1, 1, "normal")).await;
    assert_eq!(holder.reply().await, granted(1, 1));
    // One connection, so its replies come in the order the pool grants.
    let mut client = Client::connect(&pools);
    client.send(reserve(1, 1, "low")).await;
    queued(dir.path(), 1).await;
    client.send(reserve(2, 1, "low")).await;
    queued(dir.path(), 2).await;
    client.send(reserve(3, 1, "normal")).await;
    queued(dir.path(), 3).await;

    holder.send(release(1)).await;
    // The youngest goes first, by its priority; then the low ones by age.
    assert_eq!(client.reply().await, granted(3, 1));
    client.send(release(3)).await;
    assert_eq!(client.reply().await, granted(1, 1));
    client.send(release(1)).await;
    assert_eq!(client.reply().await, granted(2, 1));
    client.close().await;
    holder.close().await;
}

#[tokio::test]
async fn a_refused_request_leaves_its_connection_serving() {
    let dir = tempfile::tempdir().unwrap();
    // No memory budget, so a memory charge cannot be held.
    let pools = pools(dir.path(), 2);
    let mut client = Client::connect(&pools);
    client
        .send(json!({"type": "reserve", "id": 1, "cpus": 1, "priority": "low", "memory": 1024}))
        .await;
    let refused = client.reply().await.unwrap();
    assert_eq!(refused["type"], "refused", "{refused}");
    assert_eq!(refused["id"], 1, "{refused}");
    assert!(
        refused["error"]
            .as_str()
            .is_some_and(|error| error.contains("memory"))
    );
    client.send(reserve(1, 2, "low")).await;
    assert_eq!(client.reply().await, granted(1, 2));
    client.close().await;
}

#[tokio::test]
async fn a_client_that_breaks_the_protocol_loses_its_connection_and_its_grants() {
    for broken in [
        // A second live request under one id.
        r#"{"type":"reserve","id":1,"cpus":1,"priority":"low"}"#,
        // A release of an id it does not hold.
        r#"{"type":"release","id":7}"#,
        r#"{"type":"reserve","id":2,"cpus":1}"#,
        "nonsense",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let pools = pools(dir.path(), 2);
        let mut client = Client::connect(&pools);
        client.send(reserve(1, 2, "normal")).await;
        assert_eq!(client.reply().await, granted(1, 2));

        client.send_line(broken).await;
        let said = client.reply().await.unwrap();
        assert_eq!(said["type"], "error", "{broken}: {said}");
        assert!(
            said["error"]
                .as_str()
                .is_some_and(|error| !error.is_empty())
        );
        assert_eq!(
            client.reply().await,
            None,
            "{broken}: the connection closes"
        );
        // What it held went with it.
        let mut next = Client::connect(&pools);
        next.send(reserve(1, 2, "normal")).await;
        assert_eq!(next.reply().await, granted(1, 2), "{broken}");
        next.close().await;
        client.close().await;
    }
}

#[tokio::test]
async fn a_named_reservation_names_its_class_and_prediction_in_its_decisions() {
    let dir = tempfile::tempdir().unwrap();
    let pools = pools(dir.path(), 1);
    let mut client = Client::connect(&pools);
    let named = |id: u64| json!({"type": "reserve", "id": id, "cpus": 1, "priority": "low", "name": "nx:ws:build"});
    client.send(named(1)).await;
    assert_eq!(client.reply().await, granted(1, 1));
    client.send(release(1)).await;
    client.send(named(2)).await;
    assert_eq!(client.reply().await, granted(2, 1));
    client.close().await;
    let admitted: Vec<Value> = std::fs::read_to_string(dir.path().join("decisions.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|row| row["event"] == "admitted")
        .collect();
    assert_eq!(admitted.len(), 2);
    for row in &admitted {
        assert_eq!(row["name"], "nx:ws:build");
        assert_eq!(row["mode_key"], "nx:ws:build");
    }
    assert_eq!(
        admitted[0].get("predicted_ms"),
        None,
        "none of its class has run"
    );
    assert!(
        admitted[1]["predicted_ms"].is_u64(),
        "the first one's release recorded its run: {}",
        admitted[1]
    );
}
