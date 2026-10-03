use crate::{
    app::App,
    db::{self, schema::StructuredMessage},
    kick::{
        convert::{self, ChannelRef, TimestampAssigner, ChatIndex},
        events::{self, KickEvent, UserRef},
        pusher::{self, PusherConfig, PusherEvent, PusherHandle},
    },
    ShutdownRx,
};
use anyhow::{anyhow, bail, Context};
use chrono::Utc;
use dashmap::DashMap;
use lazy_static::lazy_static;
use prometheus::{register_int_counter_vec, IntCounterVec};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use thiserror::Error;
use tokio::{
    sync::mpsc::{self, Receiver, Sender},
    task::JoinHandle,
    time::{sleep, timeout},
};
use tracing::{debug, error, info, trace, warn};

/// How often the channels from the config are joined again, which also picks up renamed channels
const CHANNEL_REJOIN_INTERVAL: Duration = Duration::from_secs(3600);
/// When channels could not be joined, the next attempt is made after this long. The wait doubles
/// with every failed attempt until it reaches the regular interval.
const CHANNEL_JOIN_RETRY_MIN_DELAY: Duration = Duration::from_secs(60);
/// Only this many failures per pass are logged individually
const MAX_LOGGED_JOIN_FAILURES: usize = 20;
/// Pause after a lookup on Kick's website, which is rate limited
const WEBSITE_LOOKUP_DELAY: Duration = Duration::from_millis(300);
const EVENT_BUFFER_SIZE: usize = 10_000;
const PUSHER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(4);

#[derive(Debug)]
pub enum BotMessage {
    JoinChannels(Vec<String>),
    PartChannels(Vec<String>),
}

lazy_static! {
    static ref MESSAGES_RECEIVED_COUNTERS: IntCounterVec = register_int_counter_vec!(
        "rustlog_messages_received",
        "How many messages were written",
        &["channel_id"]
    )
    .unwrap();
}

const COMMAND_PREFIX: &str = "!rustlog ";

/// Raised when a channel cannot be joined because all connections to Kick are full.
/// Trying again does not help until channels are removed, so this is not retried.
#[derive(Debug, Error)]
#[error("the limit of {limit} channels is reached (see pusherMaxChannelsPerConnection and pusherMaxConnections)")]
struct CapacityReached {
    limit: usize,
}

pub async fn run(
    app: App,
    writer_tx: Sender<StructuredMessage<'static>>,
    shutdown_rx: ShutdownRx,
    command_rx: Receiver<BotMessage>,
) {
    let (event_tx, event_rx) = mpsc::channel(EVENT_BUFFER_SIZE);
    let pusher_config = PusherConfig {
        key: app.config.pusher_key.clone(),
        cluster: app.config.pusher_cluster.clone(),
        max_channels_per_connection: app.config.pusher_max_channels_per_connection,
        max_connections: app.config.pusher_connection_limit(),
        new_connection_every: Duration::from_millis(app.config.pusher_new_connection_every_ms),
    };
    let (pusher, pusher_task) = pusher::spawn(pusher_config, event_tx, shutdown_rx.clone());

    let bot = Bot {
        app,
        writer_tx,
        pusher,
        state: Arc::new(BotState::default()),
    };
    bot.run(shutdown_rx, command_rx, event_rx, pusher_task)
        .await;
}

#[derive(Default)]
struct BotState {
    /// Chatroom id -> logged channel
    channels: DashMap<u64, ChannelRef>,
    /// Channel id -> chatroom id, for every channel whose chatroom is known
    known_chatrooms: DashMap<String, u64>,
    shared: Mutex<SharedState>,
    /// Only one request to join or leave channels is carried out at a time, so that they
    /// do not overtake each other and do not write the config at the same time
    command_lock: tokio::sync::Mutex<()>,
    /// Makes checking for room and adding a channel one step
    join_lock: Mutex<()>,
}

/// State which is only used briefly and never across await points
#[derive(Default)]
struct SharedState {
    timestamps: TimestampAssigner,
    /// Recently seen users and messages
    chat: ChatIndex,
}

impl BotState {
    fn channel_by_chatroom(&self, chatroom_id: u64) -> Option<ChannelRef> {
        self.channels
            .get(&chatroom_id)
            .map(|entry| entry.value().clone())
    }

    fn chatroom_of(&self, channel_id: &str) -> Option<u64> {
        self.known_chatrooms
            .get(channel_id)
            .map(|entry| *entry.value())
    }
}

/// What happened when the channels from the config were joined
#[derive(Debug, Default, PartialEq, Eq)]
struct JoinSummary {
    joined: usize,
    /// Channels which could not be joined, these are tried again
    failed: usize,
    /// Channels which were not joined because there is no room left
    over_capacity: usize,
}

#[derive(Clone)]
struct Bot {
    app: App,
    writer_tx: Sender<StructuredMessage<'static>>,
    pusher: PusherHandle,
    state: Arc<BotState>,
}

impl Bot {
    async fn run(
        self,
        mut shutdown_rx: ShutdownRx,
        command_rx: Receiver<BotMessage>,
        mut event_rx: Receiver<PusherEvent>,
        pusher_task: JoinHandle<()>,
    ) {
        self.load_known_chatrooms().await;

        let join_bot = self.clone();
        let join_task = tokio::spawn(async move {
            join_bot.join_configured_channels().await;
        });

        let command_bot = self.clone();
        let command_task = tokio::spawn(async move {
            command_bot.handle_commands(command_rx).await;
        });

        loop {
            tokio::select! {
                event = event_rx.recv() => match event {
                    Some(event) => {
                        if let Err(err) = self.handle_event(event).await {
                            error!("Could not handle event: {err:#}");
                        }
                    }
                    None => {
                        error!("The Pusher task ended unexpectedly");
                        break;
                    }
                },
                _ = shutdown_rx.changed() => {
                    debug!("Shutting down bot task");
                    break;
                }
            }
        }

        join_task.abort();
        command_task.abort();
        // A connection may be waiting for room in the event queue, which nobody empties anymore.
        // Closing the queue lets it give up.
        drop(event_rx);
        if timeout(PUSHER_SHUTDOWN_TIMEOUT, pusher_task).await.is_err() {
            warn!("The Pusher task did not shut down in time");
        }
    }

    /// Loads the chatrooms which were found out earlier, so channels can be joined without
    /// asking Kick's rate limited website again
    async fn load_known_chatrooms(&self) {
        match db::read_chatrooms(&self.app.db).await {
            Ok(rows) => {
                info!("Loaded {} known chatrooms", rows.len());
                for row in rows {
                    self.state
                        .known_chatrooms
                        .insert(row.channel_id, row.chatroom_id);
                }
            }
            Err(err) => warn!("Could not load the known chatrooms: {err:?}"),
        }

        // Chatrooms from the config take precedence
        for (channel_id, chatroom_id) in &self.app.config.chatroom_ids {
            self.state
                .known_chatrooms
                .insert(channel_id.clone(), *chatroom_id);
        }
    }

    /// Joins the channels from the config and repeats that regularly
    async fn join_configured_channels(&self) {
        let mut retry_delay = CHANNEL_JOIN_RETRY_MIN_DELAY;
        // Every regular pass looks up the channels again to find out about renames. A retry only
        // looks up what is still missing, otherwise one failing request would make the bot
        // repeat all the requests which worked.
        let mut retrying = false;

        loop {
            let entries = Vec::from_iter(self.app.channels.read().unwrap().iter().cloned());
            if let Some(capacity) = self.pusher.capacity() {
                if entries.len() > capacity {
                    warn!(
                        "{} channels are configured, but at most {capacity} can be logged (see pusherMaxChannelsPerConnection and pusherMaxConnections)",
                        entries.len(),
                    );
                }
            }

            let must_retry = match self
                .app
                .get_channels_for_joining(entries, !retrying)
                .await
            {
                Ok(resolved) => {
                    info!("Joining {} channels", resolved.channels.len());

                    let failed_lookups = resolved.failed_lookups;
                    let summary = self.join_channels(resolved.channels).await;
                    debug!("Joined {} channels", summary.joined);

                    if summary.over_capacity > 0 {
                        warn!(
                            "{} channels were not joined because the limit of {} channels is reached",
                            summary.over_capacity,
                            self.pusher.capacity().unwrap_or_default()
                        );
                    }
                    if summary.failed > 0 || failed_lookups > 0 {
                        warn!(
                            "{} channels could not be joined and {failed_lookups} lookups on Kick failed",
                            summary.failed
                        );
                    }

                    summary.failed > 0 || failed_lookups > 0
                }
                Err(err) => {
                    error!("Could not fetch users list: {err}");
                    true
                }
            };

            let delay = if must_retry {
                let delay = retry_delay;
                retry_delay = next_retry_delay(retry_delay);
                // Once the waits are as long as the regular interval, the next pass is a
                // regular one again, so that renamed channels are found even if a channel
                // keeps failing
                retrying = delay < CHANNEL_REJOIN_INTERVAL;
                warn!("Trying again in {}s", delay.as_secs());
                delay
            } else {
                retry_delay = CHANNEL_JOIN_RETRY_MIN_DELAY;
                retrying = false;
                CHANNEL_REJOIN_INTERVAL
            };
            sleep(delay).await;
        }
    }

    /// Joins the given channels (user id -> login) which are still configured
    async fn join_channels(&self, channels: HashMap<String, String>) -> JoinSummary {
        let mut summary = JoinSummary::default();

        for (channel_id, channel_login) in channels {
            // Resolving the chatrooms of many channels takes a long time, and an admin may
            // have removed this channel in the meantime
            if !self.is_configured(&channel_id, &channel_login) {
                debug!("Not joining {channel_login}, it was removed from the config");
                continue;
            }

            debug!("Logging channel {channel_login}");
            match self.join_channel(&channel_id, &channel_login, true).await {
                Ok(()) => summary.joined += 1,
                Err(err) if err.is::<CapacityReached>() => summary.over_capacity += 1,
                Err(err) => {
                    summary.failed += 1;
                    if summary.failed <= MAX_LOGGED_JOIN_FAILURES {
                        error!("Could not join channel {channel_login}: {err:#}");
                    } else {
                        debug!("Could not join channel {channel_login}: {err:#}");
                    }
                }
            }
        }

        summary
    }

    /// Whether the channel is one of the logged ones, by id or by name. The entries are
    /// lowercase, and names with an underscore may be written with a hyphen in the slug.
    fn is_configured(&self, channel_id: &str, channel_login: &str) -> bool {
        let configured = self.app.channels.read().unwrap();

        configured.contains(channel_id)
            || configured.contains(channel_login)
            || configured.contains(&channel_login.replace('-', "_"))
    }

    /// Handles join and leave requests from the admin API
    async fn handle_commands(&self, mut command_rx: Receiver<BotMessage>) {
        while let Some(msg) = command_rx.recv().await {
            let (channels, action) = match msg {
                BotMessage::JoinChannels(channels) => (channels, ChannelAction::Join),
                BotMessage::PartChannels(channels) => (channels, ChannelAction::Part),
            };

            let names: Vec<&str> = channels.iter().map(String::as_str).collect();
            if let Err(err) = self.update_channels(&names, action).await {
                error!("Could not update channels: {err:#}");
            }
        }
    }

    async fn handle_event(&self, event: PusherEvent) -> anyhow::Result<()> {
        let Some(chatroom_id) = pusher::chatroom_id_from_channel(&event.channel) else {
            return Ok(());
        };
        let Some(channel) = self.state.channel_by_chatroom(chatroom_id) else {
            trace!("Ignoring an event for chatroom {chatroom_id} which is not logged");
            return Ok(());
        };
        let Some(kick_event) = events::parse_event(&event.event, &event.data) else {
            trace!("Ignoring event {}", event.event);
            return Ok(());
        };

        if let KickEvent::Chat(chat) = &kick_event {
            trace!("Processing message {}", chat.content);
            if let Some(cmd) = chat.content.strip_prefix(COMMAND_PREFIX) {
                self.run_command(cmd, &chat.sender.user).await;
            }
        }

        self.write_event(&channel, &kick_event).await
    }

    /// The events of all channels are handled one after another. Commands which look things up
    /// on Kick take long, so they run in the background instead of holding up every channel.
    ///
    /// Opting out with a valid code is the exception. It has to be finished before the message
    /// with the code is written, otherwise the code would end up in the logs.
    async fn run_command(&self, cmd: &str, sender: &UserRef) {
        if self.is_optout_with_code(cmd) {
            if let Err(err) = self.handle_command(cmd, sender).await {
                warn!("Could not handle command {cmd}: {err:#}");
            }
        } else {
            let bot = self.clone();
            let cmd = cmd.to_owned();
            let sender = sender.clone();

            tokio::spawn(async move {
                if let Err(err) = bot.handle_command(&cmd, &sender).await {
                    warn!("Could not handle command {cmd}: {err:#}");
                }
            });
        }
    }

    fn is_optout_with_code(&self, cmd: &str) -> bool {
        let mut parts = cmd.split_whitespace();

        parts.next() == Some("optout")
            && parts
                .next()
                .is_some_and(|code| self.app.optout_codes.contains(code))
    }

    /// A ban, timeout or unban by a moderator who opted out, without the moderator. Kick says
    /// who they were, but they are not named in the logs. `None` if there is nothing to hide.
    fn without_opted_out_moderator(&self, event: &KickEvent) -> Option<KickEvent> {
        let opted_out = |moderator: &Option<UserRef>| {
            moderator
                .as_ref()
                .is_some_and(|moderator| self.app.optout_users.contains(moderator.id.as_str()))
        };

        match event {
            KickEvent::Banned { moderator, .. } | KickEvent::Unbanned { moderator, .. }
                if opted_out(moderator) =>
            {
                let mut event = event.clone();
                if let KickEvent::Banned { moderator, .. } | KickEvent::Unbanned { moderator, .. } =
                    &mut event
                {
                    *moderator = None;
                }
                Some(event)
            }
            _ => None,
        }
    }

    async fn write_event(&self, channel: &ChannelRef, event: &KickEvent) -> anyhow::Result<()> {
        let hidden = self.without_opted_out_moderator(event);
        let event = hidden.as_ref().unwrap_or(event);

        let message = {
            let mut shared = self.state.shared.lock().unwrap();

            let created_at = match event {
                KickEvent::Chat(chat) => chat.created_at,
                _ => None,
            };
            let timestamp = shared.timestamps.assign(&channel.id, created_at, now_ms());
            if let KickEvent::Chat(chat) = event {
                shared.chat.remember(&chat.sender.user);
            }

            let message = convert::convert_event(channel, event, timestamp, &shared.chat);
            if matches!(event, KickEvent::Chat(_)) {
                // So that it can be told who wrote it and what it said if it gets deleted
                shared.chat.remember_message(&message);
            }
            message
        };

        MESSAGES_RECEIVED_COUNTERS
            .with_label_values(&[channel.id.as_str()])
            .inc();

        // Nothing about a user who opted out is logged: neither their messages, nor what
        // moderators did to them (this includes their deleted messages)
        let user_id = convert::subject_user_id(&message);
        if !user_id.is_empty() && self.app.optout_users.contains(user_id) {
            return Ok(());
        }

        // Only what is logged goes to the firehose. It is an error to send when nobody listens,
        // which is the usual case.
        self.app.firehose_tx.send(message.clone()).ok();
        self.writer_tx.send(message).await?;

        Ok(())
    }

    fn check_admin(&self, sender: &UserRef) -> anyhow::Result<()> {
        if is_admin(&self.app.config.admins, sender) {
            Ok(())
        } else {
            Err(anyhow!("User {} is not an admin", convert::user_login(sender)))
        }
    }

    async fn handle_command(&self, cmd: &str, sender: &UserRef) -> anyhow::Result<()> {
        debug!("Processing command {cmd}");
        let mut split = cmd.split_whitespace();
        if let Some(action) = split.next() {
            let args: Vec<&str> = split.collect();

            match action {
                "join" => {
                    self.check_admin(sender)?;
                    self.update_channels(&args, ChannelAction::Join).await?
                }
                "leave" | "part" => {
                    self.check_admin(sender)?;
                    self.update_channels(&args, ChannelAction::Part).await?
                }
                "optout" => {
                    self.optout_user(&args, sender).await?;
                }
                _ => (),
            }
        }

        Ok(())
    }

    async fn optout_user(&self, args: &[&str], sender: &UserRef) -> anyhow::Result<()> {
        let arg = args.first().context("No optout code provided")?;
        if self.app.optout_codes.remove(*arg).is_some() {
            self.app.optout_user(&sender.id).await?;

            Ok(())
        } else if self.check_admin(sender).is_ok() {
            let user_id = self.app.get_user_id_by_name(arg).await?;

            self.app.optout_user(&user_id).await?;

            Ok(())
        } else {
            Err(anyhow!("Invalid optout code"))
        }
    }

    async fn update_channels(&self, channels: &[&str], action: ChannelAction) -> anyhow::Result<()> {
        if channels.is_empty() {
            return Err(anyhow!("no channels specified"));
        }

        let _one_at_a_time = self.state.command_lock.lock().await;

        // The channels are given as ids or slugs
        let entries: Vec<String> = channels
            .iter()
            .map(|channel| channel.trim().to_lowercase())
            .filter(|channel| !channel.is_empty())
            .collect();
        let resolved = self
            .app
            .get_channels_by_entries(entries.clone(), false)
            .await?;

        // The channels are joined and left even if the database cannot be written, but the
        // change would be lost when the bot is restarted
        let stored = stored_entries(action, &resolved, &entries);
        if let Err(err) = self.app.update_channels(&stored, action).await {
            error!("Could not save the channels: {err:#}");
        }

        for (channel_id, channel_login) in &resolved {
            match action {
                ChannelAction::Join => {
                    info!("Joining channel {channel_login}");
                    if let Err(err) = self.join_channel(channel_id, channel_login, false).await {
                        error!("Could not join channel {channel_login}: {err:#}");
                    }
                }
                ChannelAction::Part => {
                    info!("Parting channel {channel_login}");
                    self.part_channel(channel_id).await;
                }
            }
        }

        if let ChannelAction::Part = action {
            // Channels which Kick does not know anymore (deleted accounts) can still be logged.
            // Ids can be left without asking Kick about them, names are found among the
            // channels which are logged.
            for entry in &entries {
                if resolved.contains_key(entry) {
                    continue;
                }

                let channel_ids: Vec<String> = if entry.parse::<u64>().is_ok() {
                    vec![entry.clone()]
                } else {
                    let hyphenated = entry.replace('_', "-");
                    self.state
                        .channels
                        .iter()
                        .filter(|channel| {
                            channel.value().login == *entry || channel.value().login == hyphenated
                        })
                        .map(|channel| channel.value().id.clone())
                        .collect()
                };
                for channel_id in channel_ids {
                    info!("Parting channel {entry}");
                    self.part_channel(&channel_id).await;
                }
            }
        }

        Ok(())
    }

    /// Whether another chatroom can be listened to. A chatroom which is already joined
    /// always fits, joining it again only updates its login. Without a limit (the default)
    /// there is always room.
    fn has_room_for(&self, chatroom_id: Option<u64>) -> bool {
        let joined_already =
            chatroom_id.is_some_and(|chatroom_id| self.state.channels.contains_key(&chatroom_id));

        joined_already
            || self
                .pusher
                .capacity()
                .is_none_or(|capacity| self.state.channels.len() < capacity)
    }

    fn capacity_reached(&self) -> CapacityReached {
        CapacityReached {
            limit: self.pusher.capacity().unwrap_or_default(),
        }
    }

    /// Starts listening to the chat of a channel. Joining a channel again is allowed,
    /// it updates the stored login of the channel. With `only_if_configured` the channel
    /// is not joined if it was removed from the config while its chatroom was looked up.
    async fn join_channel(
        &self,
        channel_id: &str,
        channel_login: &str,
        only_if_configured: bool,
    ) -> anyhow::Result<()> {
        // Checked before the chatroom is looked up, which is slow and rate limited. There is no
        // point in doing that when the channel cannot be joined anyway.
        let known_chatroom = self.state.chatroom_of(channel_id);
        if !self.has_room_for(known_chatroom) {
            return Err(self.capacity_reached().into());
        }

        let chatroom_id = match known_chatroom {
            Some(chatroom_id) => chatroom_id,
            None => {
                let chatroom_id = self.resolve_chatroom(channel_id, channel_login).await?;

                // The lookup took a while, the channel may have been removed in the meantime
                if only_if_configured && !self.is_configured(channel_id, channel_login) {
                    debug!("Not joining {channel_login}, it was removed from the config");
                    return Ok(());
                }
                chatroom_id
            }
        };

        {
            // Other channels may have been joined while the chatroom was looked up, and this
            // may be one of several channels being joined at the same moment
            let _checking = self.state.join_lock.lock().unwrap();
            if !self.has_room_for(Some(chatroom_id)) {
                return Err(self.capacity_reached().into());
            }
            self.state.channels.insert(
                chatroom_id,
                ChannelRef {
                    id: channel_id.to_owned(),
                    login: channel_login.to_owned(),
                },
            );
        }
        self.pusher.subscribe(chatroom_id).await;

        Ok(())
    }

    async fn part_channel(&self, channel_id: &str) {
        if let Some(chatroom_id) = self.state.chatroom_of(channel_id) {
            if self.state.channels.remove(&chatroom_id).is_some() {
                self.pusher.unsubscribe(chatroom_id).await;
            }
        }
    }

    /// Finds the chatroom the chat of a channel is published under
    async fn resolve_chatroom(&self, channel_id: &str, channel_login: &str) -> anyhow::Result<u64> {
        let lookup = self.app.kick.web_channel(channel_login).await;
        // Kick's website is rate limited, so wait after every lookup. Failed ones count as well,
        // otherwise a problem would make the bot hammer the website.
        sleep(WEBSITE_LOOKUP_DELAY).await;

        let channel = lookup
            .with_context(|| format!("Could not look up the chatroom of {channel_login}"))?;

        if channel.user_id != channel_id {
            bail!(
                "{channel_login} belongs to user {} and not to {channel_id}",
                channel.user_id
            );
        }

        self.state
            .known_chatrooms
            .insert(channel_id.to_owned(), channel.chatroom_id);
        if let Err(err) = db::save_chatroom(&self.app.db, channel_id, channel.chatroom_id).await {
            warn!("Could not save the chatroom of {channel_login}: {err:?}");
        }

        Ok(channel.chatroom_id)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelAction {
    Join,
    Part,
}

/// The entries of the logged channels which a request to join or leave channels changes.
/// `resolved` is what Kick knows of the channels (user id -> slug), `entries` what was asked for.
///
/// Joining stores the ids. Leaving removes every form a channel may be stored in, as the id, as
/// the slug, or with `_` where the slug has `-`, and what was asked for even if Kick does not
/// know the channel anymore (which is the case for deleted accounts).
fn stored_entries(
    action: ChannelAction,
    resolved: &HashMap<String, String>,
    entries: &[String],
) -> Vec<String> {
    let mut stored: Vec<String> = match action {
        ChannelAction::Join => resolved.keys().cloned().collect(),
        ChannelAction::Part => {
            let mut stored = Vec::new();
            for (channel_id, channel_login) in resolved {
                stored.push(channel_id.clone());
                stored.push(channel_login.clone());
                stored.push(channel_login.replace('-', "_"));
            }
            stored.extend(entries.iter().cloned());
            stored
        }
    };
    stored.sort();
    stored.dedup();

    stored
}

/// The wait before the next attempt, which doubles with every failed attempt
fn next_retry_delay(current: Duration) -> Duration {
    (current * 2).min(CHANNEL_REJOIN_INTERVAL)
}

/// Whether the sender is one of the configured admins. Admins may be configured by
/// username or by slug.
fn is_admin(admins: &[String], sender: &UserRef) -> bool {
    let login = convert::user_login(sender);
    let username = sender.username.to_lowercase();

    admins.iter().any(|admin| {
        let admin = admin.trim().to_lowercase();
        admin == login || admin == username || convert::slugify(&admin) == login
    })
}

fn now_ms() -> u64 {
    Utc::now().timestamp_millis().max(0) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn user(id: &str, username: &str, slug: &str) -> UserRef {
        UserRef {
            id: id.to_owned(),
            username: username.to_owned(),
            slug: slug.to_owned(),
        }
    }

    #[test]
    fn admins_are_matched_by_slug_or_username() {
        let admins = vec!["Some_Admin".to_owned(), "other".to_owned()];

        assert!(is_admin(&admins, &user("1", "Some_Admin", "some-admin")));
        assert!(is_admin(&admins, &user("1", "some_admin", "some-admin")));
        assert!(is_admin(&admins, &user("2", "Other", "other")));
        assert!(!is_admin(&admins, &user("3", "Intruder", "intruder")));
        assert!(!is_admin(&[], &user("1", "Some_Admin", "some-admin")));
    }

    #[test]
    fn bot_state_tracks_channels_by_chatroom() {
        let state = BotState::default();
        assert_eq!(state.channel_by_chatroom(668), None);
        assert_eq!(state.chatroom_of("676"), None);

        state.known_chatrooms.insert("676".to_owned(), 668);
        state.channels.insert(
            668,
            ChannelRef {
                id: "676".to_owned(),
                login: "xqc".to_owned(),
            },
        );

        assert_eq!(state.chatroom_of("676"), Some(668));
        assert_eq!(
            state.channel_by_chatroom(668),
            Some(ChannelRef {
                id: "676".to_owned(),
                login: "xqc".to_owned()
            })
        );
    }

    #[test]
    fn retry_delay_doubles_until_the_regular_interval() {
        let mut delay = CHANNEL_JOIN_RETRY_MIN_DELAY;
        let mut delays = vec![delay.as_secs()];
        for _ in 0..7 {
            delay = next_retry_delay(delay);
            delays.push(delay.as_secs());
        }

        assert_eq!(delays, vec![60, 120, 240, 480, 960, 1920, 3600, 3600]);
    }

    #[test]
    fn capacity_errors_can_be_told_apart_from_other_errors() {
        let capacity: anyhow::Error = CapacityReached { limit: 300_000 }.into();
        let other = anyhow!("could not look up the chatroom");

        assert!(capacity.is::<CapacityReached>());
        assert!(!other.is::<CapacityReached>());
        assert!(capacity.to_string().contains("300000"));
    }

    fn tag<'a>(message: &'a StructuredMessage<'_>, name: &str) -> Option<&'a str> {
        message
            .extra_tags
            .iter()
            .find(|(tag, _)| tag == name)
            .map(|(_, value)| &**value)
    }

    #[tokio::test]
    async fn logged_messages_reach_the_firehose_and_the_writer_but_not_opt_outs() {
        let app = App::for_tests("127.0.0.1:0");
        let mut firehose_rx = app.firehose_tx.subscribe();

        let (writer_tx, mut writer_rx) = mpsc::channel(16);
        let (event_tx, _event_rx) = mpsc::channel(16);
        let (_shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
        let (pusher, pusher_task) = pusher::spawn(
            PusherConfig {
                key: "key".to_owned(),
                cluster: "us2".to_owned(),
                max_channels_per_connection: 90,
                max_connections: None,
                new_connection_every: Duration::from_secs(2),
            },
            event_tx,
            shutdown_rx,
        );
        let bot = Bot {
            app: app.clone(),
            writer_tx,
            pusher,
            state: Arc::new(BotState::default()),
        };
        let channel = ChannelRef {
            id: "676".to_owned(),
            login: "xqc".to_owned(),
        };

        let chat = |user_id: u64, text: &str| {
            let data = serde_json::json!({
                "id": "21d47bf1-1486-4228-b4c7-420fe65e40b0",
                "content": text,
                "type": "message",
                "created_at": "2026-10-02T06:38:02+00:00",
                "sender": {
                    "id": user_id, "username": "Some_User", "slug": "some-user",
                    "identity": {"color": "#BC66FF", "badges": []}
                }
            });
            events::parse_event(events::CHAT_MESSAGE_EVENT, &data).unwrap()
        };
        let ban = |user_id: u64, moderator_id: u64| {
            let data = serde_json::json!({
                "id": "77777777-7777-4777-8777-777777777777",
                "user": {"id": user_id, "username": "Target", "slug": "target"},
                "banned_by": {"id": moderator_id, "username": "Mod", "slug": "mod"},
                "permanent": true
            });
            events::parse_event(events::USER_BANNED_EVENT, &data).unwrap()
        };

        // a message is logged, and goes to the firehose
        bot.write_event(&channel, &chat(12345, "hello"))
            .await
            .unwrap();
        let logged = writer_rx.try_recv().unwrap();
        let fired = firehose_rx.try_recv().unwrap();
        assert_eq!(logged.text, "hello");
        assert_eq!(logged.user_id, "12345");
        assert_eq!(logged, fired);

        // nothing about a user who opted out is logged or sent to the firehose: neither
        // their messages, nor what moderators do to them
        app.optout_users.insert("12345".to_owned());
        bot.write_event(&channel, &chat(12345, "secret"))
            .await
            .unwrap();
        bot.write_event(&channel, &ban(12345, 20)).await.unwrap();
        assert!(writer_rx.try_recv().is_err());
        assert!(firehose_rx.try_recv().is_err());

        // somebody else is still logged...
        bot.write_event(&channel, &chat(777, "still here"))
            .await
            .unwrap();
        assert_eq!(writer_rx.try_recv().unwrap().text, "still here");
        assert_eq!(firehose_rx.try_recv().unwrap().text, "still here");

        // ...and a moderator who opted out is not named in what they did
        bot.write_event(&channel, &ban(10, 20)).await.unwrap();
        let named = writer_rx.try_recv().unwrap();
        assert_eq!(tag(&named, "moderator-user-id"), Some("20"));
        app.optout_users.insert("20".to_owned());
        bot.write_event(&channel, &ban(10, 20)).await.unwrap();
        let hidden = writer_rx.try_recv().unwrap();
        assert_eq!(
            hidden.message_type,
            crate::db::schema::MessageType::ClearChat
        );
        assert_eq!(tag(&hidden, "target-user-id"), Some("10"));
        assert_eq!(tag(&hidden, "moderator-user-id"), None);
        assert_eq!(tag(&hidden, "moderator-user-login"), None);
        assert_eq!(firehose_rx.try_recv().unwrap(), named);
        assert_eq!(firehose_rx.try_recv().unwrap(), hidden);

        pusher_task.abort();
    }

    #[test]
    fn joining_stores_the_ids() {
        let resolved = HashMap::from([("676".to_owned(), "xqc".to_owned())]);

        assert_eq!(
            stored_entries(ChannelAction::Join, &resolved, &["xqc".to_owned()]),
            vec!["676"]
        );
    }

    #[test]
    fn leaving_removes_every_form_a_channel_may_be_stored_in() {
        let resolved = HashMap::from([("1".to_owned(), "some-user".to_owned())]);
        let entries = vec!["some_user".to_owned(), "999".to_owned()];

        assert_eq!(
            stored_entries(ChannelAction::Part, &resolved, &entries),
            vec!["1", "999", "some-user", "some_user"]
        );

        // deleted accounts are not known to Kick, what was asked for is removed anyway
        assert_eq!(
            stored_entries(ChannelAction::Part, &HashMap::new(), &["999".to_owned()]),
            vec!["999"]
        );
    }

    #[test]
    fn join_summary_starts_empty() {
        assert_eq!(
            JoinSummary::default(),
            JoinSummary {
                joined: 0,
                failed: 0,
                over_capacity: 0
            }
        );
    }
}
