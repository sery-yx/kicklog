//! Connection to Kick's chat websocket (Pusher protocol).
//!
//! Kick publishes the chat of every channel on the public Pusher channel `chatrooms.{id}.v2`.
//! Listening requires no authentication, which makes this the equivalent of an anonymous
//! Twitch IRC connection.
//!
//! The chatrooms are spread over several websocket connections ("shards"), the way the IRC
//! library of the original rustlog (`twitch-irc`, configured with `ClientConfig::new_simple`)
//! spreads channels over IRC connections:
//!
//! - a connection carries up to [`DEFAULT_CHANNELS_PER_CONNECTION`] chatrooms, a new connection
//!   is opened when all existing ones are full,
//! - there is no limit on the number of connections,
//! - connections are opened one at a time, a new one is started at most every
//!   [`DEFAULT_NEW_CONNECTION_EVERY_MS`] milliseconds.
//!
//! These are the limits of the original, they are the defaults so that this port behaves the
//! same. All of them can be changed in the config.

use crate::ShutdownRx;
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};
use tokio::{
    sync::{mpsc, Mutex, MutexGuard},
    task::JoinHandle,
    time::{interval, sleep, sleep_until, timeout, Instant},
};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, trace, warn};

/// How many chatrooms are subscribed on a single websocket connection by default.
/// The original rustlog gets this value from its IRC library (`max_channels_per_connection`).
pub const DEFAULT_CHANNELS_PER_CONNECTION: usize = 90;
/// How long to wait after a connection was started before the next one may be started, by
/// default. The original rustlog gets this value from its IRC library (`new_connection_every`).
pub const DEFAULT_NEW_CONNECTION_EVERY_MS: u64 = 2000;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// Used until the server tells us its own value
const DEFAULT_ACTIVITY_TIMEOUT: Duration = Duration::from_secs(120);
const PONG_TIMEOUT: Duration = Duration::from_secs(30);
const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// A connection which stayed established this long resets the reconnect backoff. Resetting it
/// right away would make a server which accepts connections and drops them again be retried
/// every second.
const STABLE_CONNECTION: Duration = Duration::from_secs(60);
const WATCHDOG_INTERVAL: Duration = Duration::from_secs(5);
const PING_FRAME: &str = r#"{"event":"pusher:ping","data":{}}"#;
const PONG_FRAME: &str = r#"{"event":"pusher:pong","data":{}}"#;

#[derive(Debug, Clone)]
pub struct PusherConfig {
    /// Pusher application key of Kick's web client
    pub key: String,
    pub cluster: String,
    /// How many chatrooms are subscribed on a single websocket connection
    pub max_channels_per_connection: usize,
    /// How many websocket connections are opened at most, `None` for any number
    pub max_connections: Option<usize>,
    /// How long to wait after a connection was started before the next one may be started
    pub new_connection_every: Duration,
}

impl PusherConfig {
    pub fn url(&self) -> String {
        format!(
            "wss://ws-{}.pusher.com/app/{}?protocol=7&client=js&version=8.4.0-rc2&flash=false",
            self.cluster, self.key
        )
    }

    /// The number of chatrooms that can be listened to at the same time, `None` if there is
    /// no limit
    pub fn capacity(&self) -> Option<usize> {
        self.max_connections.map(|max_connections| {
            self.max_channels_per_connection
                .max(1)
                .saturating_mul(max_connections.max(1))
        })
    }
}

/// An application event received on a subscribed channel
#[derive(Debug, Clone, PartialEq)]
pub struct PusherEvent {
    /// Pusher channel name, for example `chatrooms.668.v2`
    pub channel: String,
    /// Event name, for example `App\Events\ChatMessageEvent`
    pub event: String,
    /// The decoded payload
    pub data: Value,
}

#[derive(Debug)]
pub enum PusherCommand {
    Subscribe(u64),
    Unsubscribe(u64),
}

/// Used to change the set of subscribed chatrooms.
///
/// The commands are queued without limit on purpose: with bounded queues the caller could wait
/// for a connection which waits for the event consumer, which is the caller itself.
#[derive(Clone)]
pub struct PusherHandle {
    command_tx: mpsc::UnboundedSender<PusherCommand>,
    capacity: Option<usize>,
}

impl PusherHandle {
    /// The number of chatrooms that can be listened to at the same time, `None` if there is
    /// no limit
    pub fn capacity(&self) -> Option<usize> {
        self.capacity
    }

    /// Never waits, the command is carried out in the background
    pub async fn subscribe(&self, chatroom_id: u64) {
        if self
            .command_tx
            .send(PusherCommand::Subscribe(chatroom_id))
            .is_err()
        {
            warn!("Could not subscribe to chatroom {chatroom_id}, the Pusher task is gone");
        }
    }

    /// Never waits, the command is carried out in the background
    pub async fn unsubscribe(&self, chatroom_id: u64) {
        if self
            .command_tx
            .send(PusherCommand::Unsubscribe(chatroom_id))
            .is_err()
        {
            warn!("Could not unsubscribe from chatroom {chatroom_id}, the Pusher task is gone");
        }
    }
}

/// Starts the connection manager. Events from all subscribed chatrooms are sent to `event_tx`.
pub fn spawn(
    config: PusherConfig,
    event_tx: mpsc::Sender<PusherEvent>,
    shutdown_rx: ShutdownRx,
) -> (PusherHandle, JoinHandle<()>) {
    let capacity = config.capacity();
    let new_connection_every_ms = config.new_connection_every.as_millis();
    match (config.max_connections, capacity) {
        (Some(max_connections), Some(capacity)) => info!(
            "Pusher: up to {} connections with {} chatrooms each ({capacity} chatrooms in total), a new connection is started every {new_connection_every_ms}ms at most",
            max_connections.max(1),
            config.max_channels_per_connection.max(1),
        ),
        _ => info!(
            "Pusher: {} chatrooms per connection, any number of connections, a new connection is started every {new_connection_every_ms}ms at most",
            config.max_channels_per_connection.max(1),
        ),
    }

    let (command_tx, command_rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(run_manager(config, command_rx, event_tx, shutdown_rx));

    (
        PusherHandle {
            command_tx,
            capacity,
        },
        task,
    )
}

/// The name of the Pusher channel which carries the chat of a chatroom
pub fn chatroom_channel(chatroom_id: u64) -> String {
    format!("chatrooms.{chatroom_id}.v2")
}

/// Extracts the chatroom id from a channel name like `chatrooms.668.v2`
pub fn chatroom_id_from_channel(channel: &str) -> Option<u64> {
    channel
        .strip_prefix("chatrooms.")?
        .strip_suffix(".v2")?
        .parse()
        .ok()
}

fn subscribe_frame(chatroom_id: u64) -> String {
    json!({
        "event": "pusher:subscribe",
        "data": {"auth": "", "channel": chatroom_channel(chatroom_id)},
    })
    .to_string()
}

fn unsubscribe_frame(chatroom_id: u64) -> String {
    json!({
        "event": "pusher:unsubscribe",
        "data": {"channel": chatroom_channel(chatroom_id)},
    })
    .to_string()
}

#[derive(Debug, PartialEq, Eq)]
enum Allocation {
    /// The chatroom is already carried by a connection
    AlreadyAssigned,
    Assigned {
        connection: usize,
        /// The connection has to be opened for this chatroom
        new_connection: bool,
    },
    /// All connections are at their limit
    Full,
}

/// Decides which connection carries which chatroom
#[derive(Debug)]
struct Allocator {
    channels_per_connection: usize,
    /// `None` if any number of connections may be opened
    max_connections: Option<usize>,
    assigned: HashMap<u64, usize>,
    /// Number of chatrooms per connection
    counts: Vec<usize>,
    /// No connection before this index has room left
    open_hint: usize,
}

impl Allocator {
    fn new(channels_per_connection: usize, max_connections: Option<usize>) -> Self {
        Self {
            channels_per_connection: channels_per_connection.max(1),
            max_connections: max_connections.map(|max_connections| max_connections.max(1)),
            assigned: HashMap::new(),
            counts: Vec::new(),
            open_hint: 0,
        }
    }

    fn allocate(&mut self, chatroom_id: u64) -> Allocation {
        if self.assigned.contains_key(&chatroom_id) {
            return Allocation::AlreadyAssigned;
        }

        let limit = self.channels_per_connection;
        let open_connection = self.counts[self.open_hint..]
            .iter()
            .position(|count| *count < limit)
            .map(|offset| self.open_hint + offset);

        let (connection, new_connection) = match open_connection {
            Some(connection) => (connection, false),
            None if self
                .max_connections
                .is_none_or(|max_connections| self.counts.len() < max_connections) =>
            {
                self.counts.push(0);
                (self.counts.len() - 1, true)
            }
            None => return Allocation::Full,
        };

        self.open_hint = connection;
        self.counts[connection] += 1;
        self.assigned.insert(chatroom_id, connection);

        Allocation::Assigned {
            connection,
            new_connection,
        }
    }

    /// Frees the slot of a chatroom, returns the connection it was carried by
    fn release(&mut self, chatroom_id: u64) -> Option<usize> {
        let connection = self.assigned.remove(&chatroom_id)?;
        self.counts[connection] = self.counts[connection].saturating_sub(1);
        self.open_hint = self.open_hint.min(connection);

        Some(connection)
    }
}

#[derive(Debug)]
enum ShardCommand {
    Subscribe(u64),
    Unsubscribe(u64),
}

/// Spaces out the opening of connections the way the IRC library of the original rustlog
/// (`twitch-irc`) does: connections are opened one at a time. The turn lasts until the
/// connection is established, and the next one may only be started `every` after that
/// (`new_connection_every`). If connecting fails, the next one may start right away.
/// Reconnecting counts as opening a connection.
struct ConnectionPacer {
    every: Duration,
    /// The earliest moment the next connection may be started. The lock is held for the whole
    /// turn, which is what keeps connections from being opened in parallel.
    next_start: Mutex<Instant>,
}

/// The turn to open a connection. Dropping it ends the turn at once, `connected` makes the
/// next connection wait.
struct ConnectionPermit<'a> {
    next_start: MutexGuard<'a, Instant>,
    every: Duration,
}

impl ConnectionPermit<'_> {
    /// The connection was established: the next one may be started `every` from now
    fn connected(mut self) {
        *self.next_start = Instant::now() + self.every;
    }
}

impl ConnectionPacer {
    fn new(every: Duration) -> Self {
        Self {
            every,
            next_start: Mutex::new(Instant::now()),
        }
    }

    /// Waits until a connection may be started. The callers are served in the order they came
    /// in, and a caller which stops waiting does not use up a turn.
    async fn wait_for_turn(&self) -> ConnectionPermit<'_> {
        let next_start = self.next_start.lock().await;
        sleep_until(*next_start).await;

        ConnectionPermit {
            next_start,
            every: self.every,
        }
    }
}

enum Turn<'a> {
    /// The connection may be started
    Granted(ConnectionPermit<'a>),
    /// All chatrooms of the connection were left while it was waiting
    NothingToListenTo,
    /// The connection is not needed anymore, because of a shutdown
    Stop,
}

/// Waits for the turn of a connection to be started. Changes to the chatrooms of the connection
/// are applied in the meantime.
async fn await_turn<'a>(
    pacer: &'a ConnectionPacer,
    subscribed: &mut HashSet<u64>,
    command_rx: &mut mpsc::UnboundedReceiver<ShardCommand>,
    shutdown_rx: &mut ShutdownRx,
) -> Turn<'a> {
    let turn = pacer.wait_for_turn();
    tokio::pin!(turn);

    loop {
        tokio::select! {
            permit = &mut turn => return Turn::Granted(permit),
            command = command_rx.recv() => match command {
                Some(command) => {
                    apply_command(subscribed, command);
                    if subscribed.is_empty() {
                        return Turn::NothingToListenTo;
                    }
                }
                None => return Turn::Stop,
            },
            _ = shutdown_rx.changed() => return Turn::Stop,
        }
    }
}

async fn run_manager(
    config: PusherConfig,
    mut command_rx: mpsc::UnboundedReceiver<PusherCommand>,
    event_tx: mpsc::Sender<PusherEvent>,
    mut shutdown_rx: ShutdownRx,
) {
    let config = Arc::new(config);
    let pacer = Arc::new(ConnectionPacer::new(config.new_connection_every));
    let mut allocator = Allocator::new(config.max_channels_per_connection, config.max_connections);
    // One entry per connection, in the same order as the allocator numbers them.
    // Sending never waits, so a connection which is busy (for example while it is connecting)
    // cannot hold up the manager and with it all the other connections.
    let mut shard_txs: Vec<mpsc::UnboundedSender<ShardCommand>> = Vec::new();
    let mut tasks: Vec<JoinHandle<()>> = Vec::new();

    loop {
        tokio::select! {
            command = command_rx.recv() => match command {
                Some(PusherCommand::Subscribe(chatroom_id)) => match allocator.allocate(chatroom_id) {
                    Allocation::AlreadyAssigned => {}
                    Allocation::Full => {
                        error!(
                            "Cannot listen to chatroom {chatroom_id}: all {} connections are full ({} chatrooms each)",
                            config.max_connections.unwrap_or_default(),
                            config.max_channels_per_connection.max(1)
                        );
                    }
                    Allocation::Assigned { connection, new_connection } => {
                        if new_connection {
                            let (shard_tx, shard_command_rx) = mpsc::unbounded_channel();
                            tasks.push(tokio::spawn(run_shard(
                                connection,
                                config.clone(),
                                pacer.clone(),
                                shard_command_rx,
                                event_tx.clone(),
                                shutdown_rx.clone(),
                            )));
                            shard_txs.push(shard_tx);
                        }

                        if shard_txs[connection].send(ShardCommand::Subscribe(chatroom_id)).is_err() {
                            error!("Pusher connection {connection} is gone, could not subscribe to chatroom {chatroom_id}");
                        }
                    }
                },
                Some(PusherCommand::Unsubscribe(chatroom_id)) => {
                    if let Some(connection) = allocator.release(chatroom_id) {
                        if shard_txs[connection].send(ShardCommand::Unsubscribe(chatroom_id)).is_err() {
                            error!("Pusher connection {connection} is gone, could not unsubscribe from chatroom {chatroom_id}");
                        }
                    }
                }
                None => break,
            },
            _ = shutdown_rx.changed() => break,
        }
    }

    // Shards watch the shutdown signal themselves and stop when their command channel closes,
    // wait until they closed their connections
    drop(shard_txs);
    for task in tasks {
        let _ = task.await;
    }
}

enum ConnectionEnd {
    Shutdown,
    /// There is nothing left to listen to
    Idle,
    Lost(String),
}

async fn run_shard(
    shard_id: usize,
    config: Arc<PusherConfig>,
    pacer: Arc<ConnectionPacer>,
    mut command_rx: mpsc::UnboundedReceiver<ShardCommand>,
    event_tx: mpsc::Sender<PusherEvent>,
    mut shutdown_rx: ShutdownRx,
) {
    let mut subscribed: HashSet<u64> = HashSet::new();
    let mut backoff = INITIAL_BACKOFF;

    loop {
        // Only keep a connection open while there is something to listen to
        while subscribed.is_empty() {
            tokio::select! {
                command = command_rx.recv() => match command {
                    Some(command) => apply_command(&mut subscribed, command),
                    None => return,
                },
                _ = shutdown_rx.changed() => return,
            }
        }

        // Connections are opened one after the other, so with many of them this can take a while
        let permit =
            match await_turn(&pacer, &mut subscribed, &mut command_rx, &mut shutdown_rx).await {
                Turn::Granted(permit) => permit,
                Turn::NothingToListenTo => continue,
                Turn::Stop => return,
            };

        let end = run_connection(
            shard_id,
            &config,
            permit,
            &mut subscribed,
            &mut command_rx,
            &event_tx,
            &mut shutdown_rx,
            &mut backoff,
        )
        .await;

        match end {
            ConnectionEnd::Shutdown => return,
            ConnectionEnd::Idle => {
                debug!("Pusher connection {shard_id} has no chatrooms left and was closed");
            }
            ConnectionEnd::Lost(reason) => {
                warn!(
                    "Pusher connection {shard_id} was lost: {reason}. Reconnecting in {}s",
                    backoff.as_secs()
                );

                // Keep applying subscription changes while waiting to reconnect.
                // The jitter keeps all connections from reconnecting at the same moment
                // after a network problem. It is computed in a statement of its own, the
                // random number generator must not be held across an await point.
                let jitter = Duration::from_millis(rand::random_range(
                    0..=backoff.as_millis() as u64 / 2,
                ));
                let wait = sleep(backoff + jitter);
                tokio::pin!(wait);
                loop {
                    tokio::select! {
                        _ = &mut wait => break,
                        command = command_rx.recv() => match command {
                            Some(command) => apply_command(&mut subscribed, command),
                            None => return,
                        },
                        _ = shutdown_rx.changed() => return,
                    }
                }

                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }
    }
}

fn apply_command(subscribed: &mut HashSet<u64>, command: ShardCommand) {
    match command {
        ShardCommand::Subscribe(chatroom_id) => {
            subscribed.insert(chatroom_id);
        }
        ShardCommand::Unsubscribe(chatroom_id) => {
            subscribed.remove(&chatroom_id);
        }
    }
}

async fn run_connection(
    shard_id: usize,
    config: &PusherConfig,
    permit: ConnectionPermit<'_>,
    subscribed: &mut HashSet<u64>,
    command_rx: &mut mpsc::UnboundedReceiver<ShardCommand>,
    event_tx: &mpsc::Sender<PusherEvent>,
    shutdown_rx: &mut ShutdownRx,
    backoff: &mut Duration,
) -> ConnectionEnd {
    let url = config.url();
    debug!("Pusher connection {shard_id} connecting");

    let mut ws = tokio::select! {
        result = timeout(CONNECT_TIMEOUT, connect_async(url.as_str())) => match result {
            Ok(Ok((ws, _response))) => ws,
            Ok(Err(err)) => return ConnectionEnd::Lost(format!("could not connect: {err}")),
            Err(_) => return ConnectionEnd::Lost("the connection attempt timed out".to_owned()),
        },
        _ = shutdown_rx.changed() => return ConnectionEnd::Shutdown,
    };
    // The next connection may be started a little later. When connecting fails, the function
    // returns above and the permit is dropped, which lets the next one start right away.
    permit.connected();

    let started_at = Instant::now();
    let mut established = false;
    let mut established_at = started_at;
    let mut activity_timeout = DEFAULT_ACTIVITY_TIMEOUT;
    let mut last_activity = started_at;
    let mut ping_sent_at: Option<Instant> = None;
    let mut watchdog = interval(WATCHDOG_INTERVAL);

    loop {
        tokio::select! {
            frame = ws.next() => {
                let message = match frame {
                    Some(Ok(message)) => message,
                    Some(Err(err)) => return ConnectionEnd::Lost(format!("read error: {err}")),
                    None => return ConnectionEnd::Lost("the connection was closed".to_owned()),
                };
                last_activity = Instant::now();
                ping_sent_at = None;

                let text = match message {
                    Message::Text(text) => text,
                    Message::Close(frame) => {
                        // 4000 to 4099 mean the server does not want us to reconnect with
                        // the same settings (for example a wrong application key)
                        let permanent = frame
                            .as_ref()
                            .is_some_and(|frame| (4000..4100).contains(&u16::from(frame.code)));
                        if permanent {
                            *backoff = MAX_BACKOFF;
                        }
                        return ConnectionEnd::Lost(format!("closed by the server ({frame:?})"));
                    }
                    // websocket level pings are answered by the library
                    _ => continue,
                };

                match parse_incoming(&text) {
                    Incoming::Established { activity_timeout: seconds } => {
                        if let Some(seconds) = seconds {
                            activity_timeout = Duration::from_secs(seconds.max(10));
                        }
                        established = true;
                        established_at = Instant::now();
                        info!(
                            "Pusher connection {shard_id} established, subscribing to {} chatrooms",
                            subscribed.len()
                        );
                        for chatroom_id in subscribed.iter() {
                            if let Err(err) = ws.send(Message::text(subscribe_frame(*chatroom_id))).await {
                                return ConnectionEnd::Lost(format!("could not subscribe: {err}"));
                            }
                        }
                    }
                    Incoming::Ping => {
                        if let Err(err) = ws.send(Message::text(PONG_FRAME)).await {
                            return ConnectionEnd::Lost(format!("could not answer a ping: {err}"));
                        }
                    }
                    Incoming::Event(event) => {
                        // The consumer may be slow, but shutting down must still work
                        tokio::select! {
                            sent = event_tx.send(event) => {
                                if sent.is_err() {
                                    return ConnectionEnd::Shutdown;
                                }
                            }
                            _ = shutdown_rx.changed() => {
                                let _ = ws.close(None).await;
                                return ConnectionEnd::Shutdown;
                            }
                        }
                    }
                    Incoming::Error(error) => {
                        warn!("Pusher connection {shard_id} received an error: {error}");
                    }
                    Incoming::SubscriptionSucceeded(channel) => {
                        trace!("Subscribed to {channel}");
                    }
                    Incoming::Pong | Incoming::Ignored => {}
                }
            }
            command = command_rx.recv() => {
                match command {
                    None => {
                        let _ = ws.close(None).await;
                        return ConnectionEnd::Shutdown;
                    }
                    Some(ShardCommand::Subscribe(chatroom_id)) => {
                        if subscribed.insert(chatroom_id) && established {
                            if let Err(err) = ws.send(Message::text(subscribe_frame(chatroom_id))).await {
                                return ConnectionEnd::Lost(format!("could not subscribe: {err}"));
                            }
                        }
                    }
                    Some(ShardCommand::Unsubscribe(chatroom_id)) => {
                        if subscribed.remove(&chatroom_id) {
                            // Not while another subscription is already waiting, closing and
                            // reconnecting right away would be pointless
                            if subscribed.is_empty() && command_rx.is_empty() {
                                let _ = ws.close(None).await;
                                return ConnectionEnd::Idle;
                            }
                            if established {
                                if let Err(err) = ws.send(Message::text(unsubscribe_frame(chatroom_id))).await {
                                    return ConnectionEnd::Lost(format!("could not unsubscribe: {err}"));
                                }
                            }
                        }
                    }
                }
            }
            _ = watchdog.tick() => {
                let now = Instant::now();
                if !established && now.duration_since(started_at) > PONG_TIMEOUT {
                    return ConnectionEnd::Lost("the server did not complete the handshake".to_owned());
                }
                if established && now.duration_since(established_at) >= STABLE_CONNECTION {
                    *backoff = INITIAL_BACKOFF;
                }

                match ping_sent_at {
                    Some(sent_at) => {
                        if now.duration_since(sent_at) > PONG_TIMEOUT {
                            return ConnectionEnd::Lost("the server did not answer a ping".to_owned());
                        }
                    }
                    None => {
                        if now.duration_since(last_activity) > activity_timeout {
                            if let Err(err) = ws.send(Message::text(PING_FRAME)).await {
                                return ConnectionEnd::Lost(format!("could not send a ping: {err}"));
                            }
                            ping_sent_at = Some(now);
                        }
                    }
                }
            }
            _ = shutdown_rx.changed() => {
                let _ = ws.close(None).await;
                return ConnectionEnd::Shutdown;
            }
        }
    }
}

#[derive(Debug, PartialEq)]
enum Incoming {
    Established { activity_timeout: Option<u64> },
    Ping,
    Pong,
    SubscriptionSucceeded(String),
    Error(String),
    Event(PusherEvent),
    Ignored,
}

/// Interprets a text frame sent by the server
fn parse_incoming(text: &str) -> Incoming {
    let frame: Value = match serde_json::from_str(text) {
        Ok(frame) => frame,
        Err(_) => return Incoming::Ignored,
    };
    let event = match frame.get("event").and_then(|event| event.as_str()) {
        Some(event) => event,
        None => return Incoming::Ignored,
    };
    let channel = frame
        .get("channel")
        .and_then(|channel| channel.as_str())
        .unwrap_or_default()
        .to_owned();
    let data = decode_data(frame.get("data"));

    match event {
        "pusher:connection_established" => Incoming::Established {
            activity_timeout: data
                .get("activity_timeout")
                .and_then(|seconds| seconds.as_u64()),
        },
        "pusher:ping" => Incoming::Ping,
        "pusher:pong" => Incoming::Pong,
        "pusher:error" => Incoming::Error(data.to_string()),
        "pusher:subscription_error" => {
            Incoming::Error(format!("could not subscribe to {channel}: {data}"))
        }
        "pusher_internal:subscription_succeeded" => Incoming::SubscriptionSucceeded(channel),
        _ if event.starts_with("pusher") => Incoming::Ignored,
        _ => Incoming::Event(PusherEvent {
            channel,
            event: event.to_owned(),
            data,
        }),
    }
}

/// The payload of Pusher frames is usually a JSON document encoded as a string
fn decode_data(data: Option<&Value>) -> Value {
    match data {
        Some(Value::String(encoded)) => {
            serde_json::from_str(encoded).unwrap_or_else(|_| Value::String(encoded.clone()))
        }
        Some(other) => other.clone(),
        None => Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use tokio::sync::watch;

    fn config() -> PusherConfig {
        PusherConfig {
            key: "abc123".to_owned(),
            cluster: "us2".to_owned(),
            max_channels_per_connection: DEFAULT_CHANNELS_PER_CONNECTION,
            max_connections: None,
            new_connection_every: Duration::from_millis(DEFAULT_NEW_CONNECTION_EVERY_MS),
        }
    }

    #[test]
    fn builds_the_websocket_url() {
        assert_eq!(
            config().url(),
            "wss://ws-us2.pusher.com/app/abc123?protocol=7&client=js&version=8.4.0-rc2&flash=false"
        );
    }

    #[test]
    fn defaults_are_those_of_the_original_rustlog() {
        // `ClientConfig::new_simple` of twitch-irc, which the original rustlog uses:
        // `max_channels_per_connection: 90` and `new_connection_every: 2 seconds`,
        // without a limit on the number of connections
        assert_eq!(DEFAULT_CHANNELS_PER_CONNECTION, 90);
        assert_eq!(DEFAULT_NEW_CONNECTION_EVERY_MS, 2000);

        let defaults = config();
        assert_eq!(defaults.max_channels_per_connection, 90);
        assert_eq!(defaults.max_connections, None);
        assert_eq!(defaults.capacity(), None);
        assert_eq!(defaults.new_connection_every, Duration::from_secs(2));
    }

    #[test]
    fn capacity_is_known_when_the_connections_are_limited() {
        let limited = PusherConfig {
            max_channels_per_connection: 300,
            max_connections: Some(1000),
            ..config()
        };
        assert_eq!(limited.capacity(), Some(300_000));

        let degenerate = PusherConfig {
            max_channels_per_connection: 0,
            max_connections: Some(0),
            ..config()
        };
        assert_eq!(degenerate.capacity(), Some(1));
    }

    #[test]
    fn chatroom_channel_names() {
        assert_eq!(chatroom_channel(668), "chatrooms.668.v2");
        assert_eq!(chatroom_id_from_channel("chatrooms.668.v2"), Some(668));
        assert_eq!(chatroom_id_from_channel("chatrooms.668"), None);
        assert_eq!(chatroom_id_from_channel("chatroom_668"), None);
        assert_eq!(chatroom_id_from_channel("chatrooms.abc.v2"), None);
        assert_eq!(chatroom_id_from_channel(""), None);
    }

    #[test]
    fn subscription_frames() {
        let subscribe: Value = serde_json::from_str(&subscribe_frame(668)).unwrap();
        assert_eq!(
            subscribe,
            json!({"event": "pusher:subscribe", "data": {"auth": "", "channel": "chatrooms.668.v2"}})
        );

        let unsubscribe: Value = serde_json::from_str(&unsubscribe_frame(668)).unwrap();
        assert_eq!(
            unsubscribe,
            json!({"event": "pusher:unsubscribe", "data": {"channel": "chatrooms.668.v2"}})
        );
    }

    #[test]
    fn allocator_fills_connections_one_after_the_other() {
        let mut allocator = Allocator::new(2, Some(2));

        assert_eq!(
            allocator.allocate(10),
            Allocation::Assigned {
                connection: 0,
                new_connection: true
            }
        );
        assert_eq!(
            allocator.allocate(11),
            Allocation::Assigned {
                connection: 0,
                new_connection: false
            }
        );
        assert_eq!(
            allocator.allocate(12),
            Allocation::Assigned {
                connection: 1,
                new_connection: true
            }
        );
        assert_eq!(allocator.allocate(11), Allocation::AlreadyAssigned);
    }

    #[test]
    fn allocator_refuses_chatrooms_beyond_its_capacity() {
        let mut allocator = Allocator::new(2, Some(2));
        for chatroom_id in 0..4 {
            assert_ne!(allocator.allocate(chatroom_id), Allocation::Full);
        }
        assert_eq!(allocator.allocate(4), Allocation::Full);
        // a chatroom which is already carried is not "full"
        assert_eq!(allocator.allocate(3), Allocation::AlreadyAssigned);
    }

    #[test]
    fn allocator_reuses_freed_slots() {
        let mut allocator = Allocator::new(2, Some(2));
        for chatroom_id in 0..4 {
            allocator.allocate(chatroom_id);
        }

        assert_eq!(allocator.release(0), Some(0));
        assert_eq!(allocator.release(0), None);
        assert_eq!(
            allocator.allocate(100),
            Allocation::Assigned {
                connection: 0,
                new_connection: false
            }
        );
        assert_eq!(allocator.allocate(101), Allocation::Full);
    }

    #[test]
    fn allocator_opens_connections_as_needed_without_a_limit() {
        let mut allocator = Allocator::new(DEFAULT_CHANNELS_PER_CONNECTION, None);

        // 20 full connections
        let chatrooms = 20 * DEFAULT_CHANNELS_PER_CONNECTION;
        for chatroom_id in 0..chatrooms as u64 {
            assert_ne!(allocator.allocate(chatroom_id), Allocation::Full);
        }
        assert_eq!(allocator.counts.len(), 20);
        assert!(allocator
            .counts
            .iter()
            .all(|count| *count == DEFAULT_CHANNELS_PER_CONNECTION));

        // the next chatroom needs a new connection, there is no limit to run into
        assert_eq!(
            allocator.allocate(chatrooms as u64),
            Allocation::Assigned {
                connection: 20,
                new_connection: true
            }
        );
    }

    #[test]
    fn allocator_handles_a_large_limited_capacity() {
        // The limits can be raised in the config for a very large number of channels
        let mut allocator = Allocator::new(300, Some(1000));

        for chatroom_id in 0..300_000u64 {
            assert_ne!(allocator.allocate(chatroom_id), Allocation::Full);
        }
        assert_eq!(allocator.counts.len(), 1000);
        assert!(allocator.counts.iter().all(|count| *count == 300));
        assert_eq!(allocator.allocate(300_000), Allocation::Full);
    }

    #[test]
    fn parses_connection_established() {
        let text = r#"{"event":"pusher:connection_established","data":"{\"socket_id\":\"1795351.3719818\",\"activity_timeout\":120}"}"#;
        assert_eq!(
            parse_incoming(text),
            Incoming::Established {
                activity_timeout: Some(120)
            }
        );
    }

    #[test]
    fn parses_protocol_events() {
        assert_eq!(
            parse_incoming(r#"{"event":"pusher:ping","data":{}}"#),
            Incoming::Ping
        );
        assert_eq!(
            parse_incoming(r#"{"event":"pusher:pong","data":"{}"}"#),
            Incoming::Pong
        );
        assert_eq!(
            parse_incoming(
                r#"{"event":"pusher_internal:subscription_succeeded","channel":"chatrooms.668.v2","data":"{}"}"#
            ),
            Incoming::SubscriptionSucceeded("chatrooms.668.v2".to_owned())
        );
        assert_eq!(
            parse_incoming(r#"{"event":"pusher:error","data":{"code":4001,"message":"nope"}}"#),
            Incoming::Error(r#"{"code":4001,"message":"nope"}"#.to_owned())
        );
        assert_eq!(
            parse_incoming(r#"{"event":"pusher_internal:other","data":"{}"}"#),
            Incoming::Ignored
        );
    }

    #[test]
    fn failed_subscriptions_are_reported() {
        assert_eq!(
            parse_incoming(
                r#"{"event":"pusher:subscription_error","channel":"chatrooms.668.v2","data":{"type":"AuthError","error":"nope","status":401}}"#
            ),
            Incoming::Error(
                r#"could not subscribe to chatrooms.668.v2: {"type":"AuthError","error":"nope","status":401}"#
                    .to_owned()
            )
        );
    }

    #[test]
    fn parses_application_events() {
        let text = r#"{"event":"App\\Events\\ChatMessageEvent","data":"{\"id\":\"abc\",\"chatroom_id\":668}","channel":"chatrooms.668.v2"}"#;
        assert_eq!(
            parse_incoming(text),
            Incoming::Event(PusherEvent {
                channel: "chatrooms.668.v2".to_owned(),
                event: "App\\Events\\ChatMessageEvent".to_owned(),
                data: json!({"id": "abc", "chatroom_id": 668}),
            })
        );
    }

    #[test]
    fn garbage_is_ignored() {
        assert_eq!(parse_incoming("not json"), Incoming::Ignored);
        assert_eq!(parse_incoming(r#"{"data":"x"}"#), Incoming::Ignored);
        assert_eq!(parse_incoming("[]"), Incoming::Ignored);
    }

    #[test]
    fn commands_update_the_subscription_set() {
        let mut subscribed = HashSet::new();
        apply_command(&mut subscribed, ShardCommand::Subscribe(1));
        apply_command(&mut subscribed, ShardCommand::Subscribe(2));
        apply_command(&mut subscribed, ShardCommand::Subscribe(2));
        apply_command(&mut subscribed, ShardCommand::Unsubscribe(1));
        assert_eq!(subscribed, HashSet::from([2]));
    }

    #[tokio::test(start_paused = true)]
    async fn connections_are_started_one_after_the_other() {
        let pacer = ConnectionPacer::new(Duration::from_secs(2));
        let start = Instant::now();

        // the first connection does not have to wait
        pacer.wait_for_turn().await.connected();
        assert!(start.elapsed() < Duration::from_millis(50));

        pacer.wait_for_turn().await.connected();
        assert!(start.elapsed() >= Duration::from_secs(2));
        assert!(start.elapsed() < Duration::from_millis(2050));

        pacer.wait_for_turn().await.connected();
        assert!(start.elapsed() >= Duration::from_secs(4));
        assert!(start.elapsed() < Duration::from_millis(4050));
    }

    #[tokio::test(start_paused = true)]
    async fn a_connection_which_is_still_connecting_holds_up_the_next_one() {
        let pacer = Arc::new(ConnectionPacer::new(Duration::from_secs(2)));
        let first = pacer.wait_for_turn().await;

        let waiting = {
            let pacer = pacer.clone();
            tokio::spawn(async move {
                pacer.wait_for_turn().await.connected();
            })
        };
        // however long connecting takes, only one connection is opened at a time
        tokio::time::sleep(Duration::from_secs(30)).await;
        assert!(!waiting.is_finished());

        // connecting failed: the permit is dropped and the next connection starts right away
        let start = Instant::now();
        drop(first);
        waiting.await.unwrap();
        assert!(start.elapsed() < Duration::from_millis(50));
    }

    #[tokio::test(start_paused = true)]
    async fn waiting_connections_get_their_turn_in_order() {
        let pacer = Arc::new(ConnectionPacer::new(Duration::from_secs(1)));
        let start = Instant::now();

        let mut waiters = Vec::new();
        for _ in 0..3 {
            let pacer = pacer.clone();
            waiters.push(tokio::spawn(async move {
                pacer.wait_for_turn().await.connected();
                start.elapsed().as_secs()
            }));
        }

        let mut turns = Vec::new();
        for waiter in waiters {
            turns.push(waiter.await.unwrap());
        }
        assert_eq!(turns, vec![0, 1, 2]);
    }

    #[tokio::test(start_paused = true)]
    async fn changes_are_applied_while_waiting_for_a_turn() {
        let pacer = ConnectionPacer::new(Duration::from_secs(5));
        // uses up the turn which is free
        pacer.wait_for_turn().await.connected();
        let start = Instant::now();

        let (command_tx, mut command_rx) = mpsc::unbounded_channel();
        let (_shutdown_tx, mut shutdown_rx) = watch::channel(());
        let mut subscribed = HashSet::from([1]);
        command_tx.send(ShardCommand::Subscribe(2)).unwrap();

        let turn = await_turn(&pacer, &mut subscribed, &mut command_rx, &mut shutdown_rx).await;

        assert!(matches!(turn, Turn::Granted(_)));
        assert!(start.elapsed() >= Duration::from_secs(5));
        assert_eq!(subscribed, HashSet::from([1, 2]));
    }

    #[tokio::test(start_paused = true)]
    async fn leaving_everything_while_waiting_cancels_the_start() {
        let pacer = ConnectionPacer::new(Duration::from_secs(60));
        pacer.wait_for_turn().await.connected();

        let (command_tx, mut command_rx) = mpsc::unbounded_channel();
        let (_shutdown_tx, mut shutdown_rx) = watch::channel(());
        let mut subscribed = HashSet::from([1]);
        command_tx.send(ShardCommand::Unsubscribe(1)).unwrap();

        let turn = await_turn(&pacer, &mut subscribed, &mut command_rx, &mut shutdown_rx).await;

        assert!(matches!(turn, Turn::NothingToListenTo));
        assert!(subscribed.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn shutting_down_stops_the_wait_for_a_turn() {
        let pacer = ConnectionPacer::new(Duration::from_secs(60));
        pacer.wait_for_turn().await.connected();

        let (_command_tx, mut command_rx) = mpsc::unbounded_channel();
        let (shutdown_tx, mut shutdown_rx) = watch::channel(());
        let mut subscribed = HashSet::from([1]);
        shutdown_tx.send(()).unwrap();

        let turn = await_turn(&pacer, &mut subscribed, &mut command_rx, &mut shutdown_rx).await;

        assert!(matches!(turn, Turn::Stop));
    }
}
