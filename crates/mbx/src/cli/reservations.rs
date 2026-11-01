//! Serve reservations of the machine-wide pool over a Unix socket.
//!
//! `mbx reserve` holds capacity for one command, and spends a process, plus the
//! worker that holds its lease, on each one. A runner that starts many short
//! external commands -- an Nx run's tasks -- starts `mbx reservations` once and
//! asks it instead: each request is one line on a connection, admitted exactly
//! as `mbx reserve` is admitted, and its grant lasts until its client releases
//! it or the connection ends.
//!
//! The protocol is newline-delimited JSON, one object per line:
//!
//! - `{"type":"reserve","id":N,"cpus":C,"priority":"normal"|"low"}`, with an
//!   optional `"memory":BYTES`, asks for at most `C` cores. The reply is
//!   `{"type":"granted","id":N,"cpus":K}` once the pool admits it, `K` being
//!   the cores it holds: as many of `C` as were free, at least one. A request
//!   the pool refuses is answered `{"type":"refused","id":N,"error":"..."}`.
//! - `{"type":"release","id":N}` gives grant `N` back, or withdraws it while it
//!   still waits. It has no reply.
//!
//! A connection holds any number of grants at once, each under the id its
//! client chose; an id is free again once released. Replies come in the order
//! the pool grants, not the order of the requests. Closing the connection
//! releases every grant it holds and withdraws every request still waiting, so
//! a client that dies gives back everything it had. A line that is not a
//! request, a second live request under one id, or a release of an id the
//! connection does not hold is answered `{"type":"error","error":"..."}`, and
//! the connection is closed, with the same effect.
//!
//! Each waiting request blocks one thread in the pool's own admission loop,
//! which wakes on lease changes. A connection is one task that alone owns its
//! grants: requests and the answers of its waiting threads arrive as messages,
//! so nothing here is shared or locked.
//!
//! The server runs until its standard input closes. Its starter holds the
//! write end, so the server, and every grant it serves, ends with the starter.

use crate::config::{Config, SchedulerPriority};
use crate::scheduler::{Admission, Permit, Pool, SCHED_DISABLE_ENV};
use eyre::{Result, WrapErr};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

#[derive(usage_rs::Args)]
pub(super) struct ReservationsArgs {
    /// Unix socket to listen on. It must not exist yet: mbx creates it,
    /// readable and writable only by its user, and removes it when it exits.
    #[usage(arg, value_name = "SOCKET")]
    socket: String,
}

/// One line a client sends.
#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Request {
    /// Hold at most `cpus` cores, and `memory` bytes when it is named.
    Reserve {
        id: u64,
        cpus: u64,
        priority: SchedulerPriority,
        #[serde(default)]
        memory: Option<u64>,
    },
    /// Give grant `id` back, or withdraw it while it waits.
    Release { id: u64 },
}

/// One line the server answers.
#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum Reply {
    /// Request `id` now holds `cpus` cores.
    Granted { id: u64, cpus: u64 },
    /// The pool refused request `id`; the connection stays open.
    Refused { id: u64, error: String },
    /// The client broke the protocol; the connection closes after this line.
    Error { error: String },
}

/// The pool at each priority a request may name: the same pools
/// `mbx reserve --priority` reserves from.
#[derive(Clone)]
pub(super) struct Pools {
    normal: Arc<Pool>,
    low: Arc<Pool>,
}

impl Pools {
    pub(super) fn new(normal: Pool, low: Pool) -> Self {
        Self {
            normal: Arc::new(normal),
            low: Arc::new(low),
        }
    }

    fn from_config(config: &Config) -> Result<Self> {
        let pool = |priority| {
            crate::scheduler::reservation_pool(config, priority)
                .ok_or_else(|| eyre::eyre!("reservations need scheduler.enabled"))
        };
        Ok(Self::new(
            pool(SchedulerPriority::Normal)?,
            pool(SchedulerPriority::Low)?,
        ))
    }

    fn at(&self, priority: SchedulerPriority) -> &Arc<Pool> {
        match priority {
            SchedulerPriority::Normal => &self.normal,
            SchedulerPriority::Low => &self.low,
        }
    }
}

pub(super) fn run(config: &Config, args: &ReservationsArgs) -> Result<ExitCode> {
    // What `mbx reserve` gives its command: a reservation made here would
    // queue beside the enclosing one, behind the very capacity it holds.
    if std::env::var_os(SCHED_DISABLE_ENV).is_some() {
        eyre::bail!(
            "an enclosing reservation covers this process ({SCHED_DISABLE_ENV} is set), so it serves no reservations of its own"
        );
    }
    let pools = Pools::from_config(config)?;
    serve(std::path::Path::new(&args.socket), pools)?;
    Ok(ExitCode::SUCCESS)
}

#[cfg(unix)]
fn serve(socket: &std::path::Path, pools: Pools) -> Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(listen(socket, pools))
}

#[cfg(not(unix))]
fn serve(_socket: &std::path::Path, _pools: Pools) -> Result<()> {
    eyre::bail!("mbx reservations serves a Unix socket, which this platform does not have")
}

#[cfg(unix)]
async fn listen(socket: &std::path::Path, pools: Pools) -> Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt as _;

    let listener = tokio::net::UnixListener::bind(socket)
        .wrap_err_with(|| format!("could not listen on {}", socket.display()))?;
    let _socket = RemoveOnExit(socket.to_path_buf());
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))
        .wrap_err_with(|| format!("could not restrict {} to its user", socket.display()))?;
    let mut closed = stdin_closed()?;
    // The starter hands the socket to its clients once it reads this line.
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "listening").and_then(|()| stdout.flush())?;
    drop(stdout);
    let mut connections = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted.wrap_err("could not accept a reservation client")?;
                let pools = pools.clone();
                connections.spawn(async move {
                    if let Err(error) = serve_connection(stream, pools).await {
                        log::debug!("reservation connection ended: {error:#}");
                    }
                });
                // Clients come and go for the server's life; reap the finished.
                while connections.try_join_next().is_some() {}
            }
            // Dropping the connections releases what each one holds.
            _ = &mut closed => return Ok(()),
        }
    }
}

/// Resolves once standard input reaches its end, the starter's write end
/// closed, or cannot be read.
#[cfg(unix)]
fn stdin_closed() -> Result<tokio::sync::oneshot::Receiver<()>> {
    let (closed, receiver) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("mbx-reservations-stdin".into())
        .spawn(move || {
            // What the starter writes means nothing; only its end does.
            let _ = std::io::copy(&mut std::io::stdin().lock(), &mut std::io::sink());
            let _ = closed.send(());
        })
        .wrap_err("could not watch standard input")?;
    Ok(receiver)
}

#[cfg(unix)]
struct RemoveOnExit(std::path::PathBuf);

#[cfg(unix)]
impl Drop for RemoveOnExit {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Serve one client until it closes the connection or breaks the protocol.
/// Every grant it holds is released, and every request it has waiting is
/// withdrawn, when this returns.
pub(super) async fn serve_connection<S>(stream: S, pools: Pools) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (reader, mut writer) = tokio::io::split(stream);
    let mut lines = BufReader::new(reader).lines();
    let (answers, mut answered) = mpsc::unbounded_channel();
    // Declared after the stream's halves, so it drops first: every grant is
    // released before the client sees the connection close.
    let mut connection = Connection {
        pools,
        answers,
        slots: HashMap::new(),
        tickets: 0,
    };
    loop {
        let step = tokio::select! {
            line = lines.next_line() => match line? {
                Some(line) => connection.request(&line),
                None => return Ok(()),
            },
            // Never closed: the connection holds a sender.
            Some(answer) = answered.recv() => connection.answer(answer),
        };
        match step {
            Step::Quiet => {}
            Step::Say(reply) => say(&mut writer, &reply).await?,
            Step::Close(error) => return say(&mut writer, &Reply::Error { error }).await,
        }
    }
}

/// One connection's grants and waiting requests, owned by its task alone.
struct Connection {
    pools: Pools,
    /// Cloned into each waiting request's thread, which answers through it.
    answers: mpsc::UnboundedSender<Answer>,
    slots: HashMap<u64, Slot>,
    /// Counts requests, so an answer is told from one an earlier request
    /// under the same id was owed.
    tickets: u64,
}

/// What a request's id names on its connection.
enum Slot {
    /// Waiting in the pool as request `ticket`; dropping the slot withdraws it.
    Waiting { ticket: u64, _withdraw: Withdraw },
    /// Granted; dropping the slot releases it.
    Held { _grant: Hold },
}

/// Withdraws a waiting request once its slot is gone.
struct Withdraw(Arc<AtomicBool>);

impl Drop for Withdraw {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

/// One grant. Dropping it releases its capacity and records the reservation
/// as `mbx reserve` records its own, with no command outcome to name.
struct Hold {
    pool: Arc<Pool>,
    admission: Admission,
    permit: Option<Permit>,
}

impl Drop for Hold {
    fn drop(&mut self) {
        drop(self.permit.take());
        self.pool.observe_reservation(self.admission, None);
    }
}

/// The pool's answer to one waiting request, from the thread that waited.
struct Answer {
    id: u64,
    ticket: u64,
    pool: Arc<Pool>,
    outcome: Result<Option<(Permit, Admission)>>,
}

/// What one line, or one answer, does to the connection.
enum Step {
    Quiet,
    Say(Reply),
    /// The client broke the protocol: say why, then close.
    Close(String),
}

impl Connection {
    fn request(&mut self, line: &str) -> Step {
        match serde_json::from_str::<Request>(line) {
            Ok(Request::Reserve {
                id,
                cpus,
                priority,
                memory,
            }) => self.reserve(id, cpus, priority, memory),
            Ok(Request::Release { id }) => match self.slots.remove(&id) {
                Some(_) => Step::Quiet,
                None => Step::Close(format!("no reservation {id} on this connection to release")),
            },
            Err(error) => Step::Close(format!("not a reservation request: {error}")),
        }
    }

    fn reserve(
        &mut self,
        id: u64,
        cpus: u64,
        priority: SchedulerPriority,
        memory: Option<u64>,
    ) -> Step {
        if self.slots.contains_key(&id) {
            return Step::Close(format!(
                "reservation {id} is already live on this connection"
            ));
        }
        self.tickets += 1;
        let ticket = self.tickets;
        let withdrawn = Arc::new(AtomicBool::new(false));
        let waiting = Arc::clone(&withdrawn);
        let pool = Arc::clone(self.pools.at(priority));
        let answers = self.answers.clone();
        // The pool's admission blocks while it waits, so each request waits on
        // a thread of its own, as each `mbx reserve` waits in a process.
        let started = std::thread::Builder::new()
            .name(format!("mbx-reservation-{id}"))
            .spawn(move || {
                let outcome = pool.reserve_observed(Some(cpus), memory, &waiting);
                // A connection that has ended drops the answer, and any grant in it.
                let _ = answers.send(Answer {
                    id,
                    ticket,
                    pool,
                    outcome,
                });
            });
        match started {
            Ok(_) => {
                self.slots.insert(
                    id,
                    Slot::Waiting {
                        ticket,
                        _withdraw: Withdraw(withdrawn),
                    },
                );
                Step::Quiet
            }
            Err(error) => Step::Say(Reply::Refused {
                id,
                error: format!("could not start waiting for the pool: {error}"),
            }),
        }
    }

    fn answer(&mut self, answer: Answer) -> Step {
        let Answer {
            id,
            ticket,
            pool,
            outcome,
        } = answer;
        let waiting = matches!(
            self.slots.get(&id),
            Some(Slot::Waiting { ticket: live, .. }) if *live == ticket
        );
        if !waiting {
            // Released before the pool answered: what it won goes back as
            // the answer drops.
            return Step::Quiet;
        }
        match outcome {
            Ok(Some((permit, admission))) => {
                let cpus = admission.cpus();
                self.slots.insert(
                    id,
                    Slot::Held {
                        _grant: Hold {
                            pool,
                            admission,
                            permit: Some(permit),
                        },
                    },
                );
                Step::Say(Reply::Granted { id, cpus })
            }
            // Only a request whose slot is gone is withdrawn, so a live one
            // never ends this way; said all the same rather than left unanswered.
            Ok(None) => {
                self.slots.remove(&id);
                Step::Say(Reply::Refused {
                    id,
                    error: "the request was withdrawn while it waited".into(),
                })
            }
            Err(error) => {
                self.slots.remove(&id);
                Step::Say(Reply::Refused {
                    id,
                    error: format!("{error:#}"),
                })
            }
        }
    }
}

async fn say(writer: &mut (impl AsyncWrite + Unpin), reply: &Reply) -> Result<()> {
    let mut line = serde_json::to_vec(reply)?;
    line.push(b'\n');
    writer.write_all(&line).await?;
    Ok(())
}

#[cfg(test)]
#[path = "reservations_tests.rs"]
mod tests;
