//! Translation of Kick events into rustlog's message model.
//!
//! rustlog stores messages in the shape Twitch IRC uses (`message_structured` table, tags such
//! as `badges`, `emotes`, `room-id`). Kick events are mapped onto that model so that the HTTP
//! API, the text/JSON/raw formats and the web interface behave exactly as they do for Twitch:
//!
//! | Kick event                      | Message type                          |
//! |---------------------------------|---------------------------------------|
//! | chat message / reply            | `PRIVMSG` (+ `reply-*` tags)          |
//! | celebration (resub message)     | `USERNOTICE` (`msg-id=resub`)         |
//! | message deleted                 | `CLEARMSG` (`target-msg-id`)          |
//! | user banned / timed out         | `CLEARCHAT` (`ban-duration` seconds)  |
//! | chat cleared                    | `CLEARCHAT` without target            |
//! | unban, subs, gifts, hosts, ...  | `USERNOTICE` with a `msg-id`          |
//!
//! Kick tells more about moderation than Twitch does, which is kept in extra tags:
//!
//! - bans and timeouts: `moderator-user-id`, `moderator-user-login` (who did it, if Kick says),
//!   `ban-permanent=1` for bans, `ban-duration` (seconds) and `ban-expires-at` for timeouts,
//! - unbans: `moderator-user-id`, `moderator-user-login`, and `ban-permanent=1` if the ban which
//!   was lifted was a permanent one,
//! - deleted messages: `ai-moderated=1` and `violated-rules` if Kick's AI moderation deleted the
//!   message, and, if the message was seen in chat recently, the login, display name and text
//!   of its author (`target-user-id` is their id).

use super::events::{Badge, ChatMessage, KickEvent, Reply, UserRef, ANONYMOUS_GIFTER};
use crate::db::schema::{MessageFlags, MessageType, StructuredMessage};
use chrono::SecondsFormat;
use std::{
    borrow::Cow,
    collections::{HashMap, VecDeque},
};
use uuid::Uuid;

const MAX_EMOTE_ID_LENGTH: usize = 20;
const MAX_EMOTE_NAME_LENGTH: usize = 100;
/// The reported creation time of a message is only trusted if it is this close to the local
/// clock, so a wrong value cannot push the timestamps of a channel far into the future
const MAX_CLOCK_SKEW_MS: u64 = 60_000;
/// The remembered users are cleared when there are more than this many
const MAX_REMEMBERED_USERS: usize = 100_000;
/// How many chat messages are remembered, to be able to tell who wrote a message which gets
/// deleted. Messages are usually deleted within minutes, and this is enough for a few of them
/// even if thousands of channels are logged.
const MAX_REMEMBERED_MESSAGES: usize = 200_000;
/// Gifted subscription recipients are only listed in the notice text up to this count
const MAX_LISTED_RECIPIENTS: usize = 5;

/// A channel which is being logged
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelRef {
    /// Kick user id of the broadcaster
    pub id: String,
    /// Channel slug
    pub login: String,
}

/// Assigns millisecond timestamps to events.
///
/// Kick only reports the creation time of a chat message with second precision. Using it as is
/// would make messages sent within the same second come back in arbitrary order. The timestamp
/// is therefore the local arrival time, clamped into the second Kick reported, and it is
/// strictly increasing per channel.
#[derive(Debug, Default)]
pub struct TimestampAssigner {
    last: HashMap<String, u64>,
}

impl TimestampAssigner {
    /// `created_at_secs` is the creation time reported by Kick (if any), `now_ms` the local clock.
    pub fn assign(&mut self, channel_id: &str, created_at_secs: Option<i64>, now_ms: u64) -> u64 {
        let mut timestamp = match created_at_secs {
            Some(secs) if secs >= 0 => {
                let window_start = (secs as u64).saturating_mul(1000);
                if now_ms.abs_diff(window_start) <= MAX_CLOCK_SKEW_MS {
                    now_ms.clamp(window_start, window_start.saturating_add(999))
                } else {
                    now_ms
                }
            }
            _ => now_ms,
        };

        match self.last.get_mut(channel_id) {
            Some(last) => {
                if timestamp <= *last {
                    timestamp = *last + 1;
                }
                *last = timestamp;
            }
            None => {
                self.last.insert(channel_id.to_owned(), timestamp);
            }
        }

        timestamp
    }
}

/// Remembers what was seen in chat recently, to fill in what events leave out:
///
/// - events about subscriptions, gifts and hosts only carry a username, the id and slug of
///   the users seen in chat attribute them to a user,
/// - a deleted message is only identified by its id, the messages seen in chat tell who wrote it
///   and what it said.
#[derive(Debug, Default)]
pub struct ChatIndex {
    /// lowercase username -> (user id, slug)
    users: HashMap<String, (String, String)>,
    messages: RecentMessages,
}

/// A chat message as it is remembered, in the form it was logged in
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RememberedMessage {
    pub user_id: String,
    pub login: String,
    pub username: String,
    pub text: String,
}

impl ChatIndex {
    pub fn remember(&mut self, user: &UserRef) {
        if user.id.is_empty() || user.username.is_empty() {
            return;
        }
        if self.users.len() >= MAX_REMEMBERED_USERS {
            self.users.clear();
        }
        self.users
            .insert(user.username.to_lowercase(), (user.id.clone(), user_login(user)));
    }

    pub fn lookup(&self, username: &str) -> Option<&(String, String)> {
        self.users.get(&username.to_lowercase())
    }

    /// Remembers a converted chat message, so that it can be found when it is deleted.
    /// Messages without an id or a sender cannot be found later and are skipped.
    pub fn remember_message(&mut self, message: &StructuredMessage<'_>) {
        if message.id.is_nil() || message.user_id.is_empty() {
            return;
        }

        self.messages.insert(
            message.id,
            RememberedMessage {
                user_id: message.user_id.to_string(),
                login: message.user_login.to_string(),
                username: message.display_name.to_string(),
                text: message.text.to_string(),
            },
        );
    }

    /// The chat message with this id, if it is still remembered
    pub fn message(&self, id: &str) -> Option<&RememberedMessage> {
        // Whatever the case of the id in the event
        let id = Uuid::parse_str(id).ok()?;

        self.messages.get(&id)
    }
}

/// The most recent chat messages by id. The oldest ones are forgotten first.
#[derive(Debug)]
struct RecentMessages {
    capacity: usize,
    by_id: HashMap<Uuid, RememberedMessage>,
    /// The ids in the order the messages were remembered in
    order: VecDeque<Uuid>,
}

impl Default for RecentMessages {
    fn default() -> Self {
        Self::with_capacity(MAX_REMEMBERED_MESSAGES)
    }
}

impl RecentMessages {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            by_id: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn insert(&mut self, id: Uuid, message: RememberedMessage) {
        // An id which is remembered already keeps its place in the order
        if self.by_id.insert(id, message).is_some() {
            return;
        }

        self.order.push_back(id);
        if self.order.len() > self.capacity {
            if let Some(oldest) = self.order.pop_front() {
                self.by_id.remove(&oldest);
            }
        }
    }

    fn get(&self, id: &Uuid) -> Option<&RememberedMessage> {
        self.by_id.get(id)
    }
}

/// Converts a display name into the form slugs usually have: lowercase, `_` replaced by `-`
pub fn slugify(name: &str) -> String {
    name.trim().to_lowercase().replace('_', "-")
}

/// The login of a user: the slug from the event, derived from the username if it is missing
pub fn user_login(user: &UserRef) -> String {
    if user.slug.is_empty() {
        slugify(&user.username)
    } else {
        user.slug.clone()
    }
}

/// Converts an event into a message ready to be stored. `timestamp` is in milliseconds.
pub fn convert_event(
    channel: &ChannelRef,
    event: &KickEvent,
    timestamp: u64,
    users: &ChatIndex,
) -> StructuredMessage<'static> {
    match event {
        KickEvent::Chat(chat) => convert_chat(channel, chat, timestamp),
        KickEvent::MessageDeleted {
            message_id,
            ai_moderated,
            violated_rules,
            event_id,
        } => {
            let mut message = new_message(channel, timestamp, MessageType::ClearMsg);
            apply_event_id(&mut message, event_id.as_deref());
            // In the form the id of the message itself is stored in, whatever Kick's case is
            let target_msg_id = Uuid::parse_str(message_id)
                .map(|id| id.hyphenated().to_string())
                .unwrap_or_else(|_| message_id.clone());
            push_tag(&mut message, "target-msg-id", target_msg_id);
            if *ai_moderated {
                push_tag(&mut message, "ai-moderated", "1".to_owned());
            }
            push_tag(&mut message, "violated-rules", violated_rules.join(","));

            // The event only identifies the message. Who wrote it and what it said is known
            // if it was seen in chat recently, like Twitch tells it with `CLEARMSG`.
            // The author is not the user of the row (`user_id` stays empty like on Twitch),
            // as the row is not a message of the author.
            if let Some(deleted) = users.message(message_id) {
                message.user_login = Cow::Owned(deleted.login.clone());
                message.display_name = Cow::Owned(deleted.username.clone());
                message.text = Cow::Owned(deleted.text.clone());
                push_tag(&mut message, "target-user-id", deleted.user_id.clone());
            }
            message
        }
        KickEvent::Banned {
            user,
            moderator,
            permanent,
            duration_minutes,
            expires_at,
            event_id,
        } => {
            let mut message = new_message(channel, timestamp, MessageType::ClearChat);
            apply_target(&mut message, user);
            apply_event_id(&mut message, event_id.as_deref());
            push_tag(&mut message, "target-user-id", user.id.clone());
            if *permanent {
                // Twitch only marks the length of timeouts, a ban is the absence of it.
                // Kick says it explicitly.
                push_tag(&mut message, "ban-permanent", "1".to_owned());
            } else {
                // A timeout without a length is worked out from when it ends, a ban which
                // is neither permanent nor ends would look like a permanent one
                let duration_seconds = match (duration_minutes, expires_at) {
                    (Some(minutes), _) => Some(minutes.saturating_mul(60)),
                    (None, Some(expires_at)) => {
                        let millis = expires_at.timestamp_millis() - timestamp as i64;
                        Some(((millis + 999) / 1000).max(1) as u64)
                    }
                    (None, None) => None,
                };
                if let Some(seconds) = duration_seconds {
                    push_tag(&mut message, "ban-duration", seconds.to_string());
                }
                if let Some(expires_at) = expires_at {
                    push_tag(
                        &mut message,
                        "ban-expires-at",
                        expires_at.to_rfc3339_opts(SecondsFormat::Secs, true),
                    );
                }
            }
            apply_moderator(&mut message, moderator.as_ref());
            message
        }
        KickEvent::ChatCleared => new_message(channel, timestamp, MessageType::ClearChat),
        KickEvent::Unbanned {
            user,
            moderator,
            permanent,
            event_id,
        } => {
            let text = match moderator {
                Some(moderator) => format!(
                    "{} has been unbanned by {}",
                    display_name(user),
                    display_name(moderator)
                ),
                None => format!("{} has been unbanned", display_name(user)),
            };
            let mut message = notice(channel, timestamp, "unban", text);
            apply_user(&mut message, user);
            apply_event_id(&mut message, event_id.as_deref());
            if *permanent {
                // The ban which was lifted was a permanent one, and not a timeout
                push_tag(&mut message, "ban-permanent", "1".to_owned());
            }
            apply_moderator(&mut message, moderator.as_ref());
            message
        }
        KickEvent::Subscription { username, months } => {
            let (msg_id, text) = if *months <= 1 {
                ("sub", format!("{username} subscribed!"))
            } else {
                (
                    "resub",
                    format!("{username} subscribed for {}!", plural(*months, "month")),
                )
            };
            let mut message = notice(channel, timestamp, msg_id, text);
            apply_known_user(&mut message, users, username);
            push_tag(&mut message, "msg-param-cumulative-months", months.to_string());
            message
        }
        KickEvent::GiftedSubscriptions {
            gifter_username,
            gifted_usernames,
            count,
            gifter_total,
        } => {
            let gifted = plural(*count, "subscription");
            let listed = !gifted_usernames.is_empty()
                && gifted_usernames.len() as u64 == *count
                && gifted_usernames.len() <= MAX_LISTED_RECIPIENTS;
            let text = if listed {
                format!(
                    "{gifter_username} gifted {gifted} to {}!",
                    gifted_usernames.join(", ")
                )
            } else {
                format!("{gifter_username} gifted {gifted}!")
            };
            let msg_id = if *count == 1 { "subgift" } else { "submysterygift" };

            let mut message = notice(channel, timestamp, msg_id, text);
            if gifter_username.eq_ignore_ascii_case(ANONYMOUS_GIFTER) {
                // Not a real user, so it must not be attributed to one with the same name
                message.display_name = Cow::Owned(gifter_username.clone());
                message.user_login = Cow::Owned(slugify(gifter_username));
            } else {
                apply_known_user(&mut message, users, gifter_username);
            }
            push_tag(&mut message, "msg-param-mass-gift-count", count.to_string());
            if let Some(total) = gifter_total {
                push_tag(&mut message, "msg-param-sender-count", total.to_string());
            }
            push_tag(&mut message, "msg-param-recipients", gifted_usernames.join(","));
            message
        }
        KickEvent::Host {
            host_username,
            viewers,
            message: host_message,
        } => {
            let text = format!(
                "{host_username} is hosting the channel with {}!",
                plural(*viewers, "viewer")
            );
            let mut message = notice(channel, timestamp, "raid", text);
            apply_known_user(&mut message, users, host_username);
            push_tag(&mut message, "msg-param-displayName", host_username.clone());
            push_tag(&mut message, "msg-param-viewerCount", viewers.to_string());
            message.text = Cow::Owned(flatten_text(host_message));
            message
        }
        KickEvent::RewardRedeemed { user, title, input } => {
            let text = format!("{} redeemed {title}", display_name(user));
            let mut message = notice(channel, timestamp, "reward-redeemed", text);
            apply_user(&mut message, user);
            push_tag(&mut message, "msg-param-reward-title", title.clone());
            message.text = Cow::Owned(flatten_text(input));
            message
        }
        KickEvent::KicksGifted {
            sender,
            name,
            amount,
            message: gift_message,
        } => {
            let gift = if name.is_empty() {
                String::new()
            } else {
                format!(" ({name})")
            };
            let text = format!("{} gifted {amount} KICKs{gift}!", display_name(sender));
            let mut message = notice(channel, timestamp, "kicks-gifted", text);
            apply_user(&mut message, sender);
            push_tag(&mut message, "msg-param-amount", amount.to_string());
            push_tag(&mut message, "msg-param-gift-name", name.clone());
            message.text = Cow::Owned(flatten_text(gift_message));
            message
        }
    }
}

fn convert_chat(
    channel: &ChannelRef,
    chat: &ChatMessage,
    timestamp: u64,
) -> StructuredMessage<'static> {
    let (text, emotes) = normalize_content(&chat.content);

    let mut message = match &chat.celebration {
        Some(celebration) => {
            let name = display_name(&chat.sender.user);
            let (msg_id, system_msg) = match celebration.kind.as_str() {
                "subscription_renewed" => (
                    "resub",
                    format!(
                        "{name} subscribed for {}!",
                        plural(celebration.total_months, "month")
                    ),
                ),
                "subscription_started" | "subscription_new" => {
                    ("sub", format!("{name} subscribed!"))
                }
                other => (
                    "celebration",
                    format!("{name} celebrated {}", other.replace('_', " ")),
                ),
            };
            let mut message = notice(channel, timestamp, msg_id, system_msg);
            if celebration.total_months > 0 {
                push_tag(
                    &mut message,
                    "msg-param-cumulative-months",
                    celebration.total_months.to_string(),
                );
            }
            message
        }
        None => new_message(channel, timestamp, MessageType::PrivMsg),
    };

    // The id of the chat message replaces the random id `notice` generated
    message.id = Uuid::nil();
    apply_id(&mut message, &chat.id);

    let sender = &chat.sender;
    apply_user(&mut message, &sender.user);
    message.color = sender.color.as_deref().and_then(parse_color);
    apply_badges(&mut message, &sender.badges);
    message.text = Cow::Owned(text);
    message.emotes = Cow::Owned(emotes);

    if let Some(reply) = &chat.reply {
        apply_reply(&mut message, reply);
    }

    message
}

fn new_message(
    channel: &ChannelRef,
    timestamp: u64,
    message_type: MessageType,
) -> StructuredMessage<'static> {
    StructuredMessage::new(
        channel.id.clone(),
        channel.login.clone(),
        timestamp,
        message_type,
    )
}

/// A `USERNOTICE` with a `msg-id` and a human readable `system-msg`
fn notice(
    channel: &ChannelRef,
    timestamp: u64,
    msg_id: &'static str,
    system_msg: String,
) -> StructuredMessage<'static> {
    let mut message = new_message(channel, timestamp, MessageType::UserNotice);
    message.id = Uuid::new_v4();
    push_tag(&mut message, "msg-id", msg_id.to_owned());
    push_tag(&mut message, "system-msg", system_msg);
    message
}

/// Sets the id of the message. Ids which are not UUIDs are kept as an extra `id` tag.
fn apply_id(message: &mut StructuredMessage<'static>, id: &str) {
    match Uuid::parse_str(id) {
        Ok(uuid) => message.id = uuid,
        Err(_) => {
            // Not the random id a notice may have been given, there is only one id
            message.id = Uuid::nil();
            message
                .extra_tags
                .push((Cow::Borrowed("id"), Cow::Owned(id.to_owned())));
        }
    }
}

/// Sets the id Kick gave to a moderation event, if it did
fn apply_event_id(message: &mut StructuredMessage<'static>, event_id: Option<&str>) {
    if let Some(event_id) = event_id {
        apply_id(message, event_id);
    }
}

/// Records who a moderation event was done by. Nothing is recorded if that is not known.
fn apply_moderator(message: &mut StructuredMessage<'static>, moderator: Option<&UserRef>) {
    if let Some(moderator) = moderator {
        push_tag(message, "moderator-user-id", moderator.id.clone());
        push_tag(message, "moderator-user-login", user_login(moderator));
    }
}

/// The user a message is about: the sender of a message, the target of a moderation action.
/// This is not the user of the row for a deleted message (the row is not a message of its
/// author, see `convert_event`), the author is one of its tags.
pub fn subject_user_id<'a>(message: &'a StructuredMessage<'_>) -> &'a str {
    if message.message_type == MessageType::ClearMsg {
        for (name, value) in &message.extra_tags {
            if name == "target-user-id" {
                return value;
            }
        }
        return "";
    }

    &message.user_id
}

/// Adds an extra tag, empty values are skipped
fn push_tag(message: &mut StructuredMessage<'static>, name: &'static str, value: String) {
    if !value.is_empty() {
        message
            .extra_tags
            .push((Cow::Borrowed(name), Cow::Owned(value)));
    }
}

fn display_name(user: &UserRef) -> String {
    if user.username.is_empty() {
        user_login(user)
    } else {
        user.username.clone()
    }
}

/// Sets the user of a regular message or notice
fn apply_user(message: &mut StructuredMessage<'static>, user: &UserRef) {
    message.user_id = Cow::Owned(user.id.clone());
    message.user_login = Cow::Owned(user_login(user));
    message.display_name = Cow::Owned(user.username.clone());
}

/// Sets the user a moderation action (`CLEARCHAT`) is directed at.
/// Like on Twitch, the text of the message is the login of the target.
fn apply_target(message: &mut StructuredMessage<'static>, user: &UserRef) {
    let mut login = user_login(user);
    if login.is_empty() {
        // Without a name the id still identifies the target
        login = user.id.clone();
    }
    message.user_id = Cow::Owned(user.id.clone());
    message.user_login = Cow::Owned(login.clone());
    message.text = Cow::Owned(login);
}

/// Sets the user for events that only provide a username
fn apply_known_user(
    message: &mut StructuredMessage<'static>,
    users: &ChatIndex,
    username: &str,
) {
    message.display_name = Cow::Owned(username.to_owned());
    match users.lookup(username) {
        Some((id, slug)) => {
            message.user_id = Cow::Owned(id.clone());
            message.user_login = Cow::Owned(slug.clone());
        }
        None => {
            message.user_login = Cow::Owned(slugify(username));
        }
    }
}

fn apply_badges(message: &mut StructuredMessage<'static>, badges: &[Badge]) {
    let mut badge_values: Vec<Cow<'static, str>> = Vec::with_capacity(badges.len());
    let mut badge_info: Vec<String> = Vec::new();

    for badge in badges {
        badge_values.push(Cow::Owned(format!(
            "{}/{}",
            badge.kind,
            badge.count.unwrap_or(1)
        )));

        match badge.kind.as_str() {
            "subscriber" => {
                message.message_flags.insert(MessageFlags::SUBSCRIBER);
                if let Some(months) = badge.count {
                    badge_info.push(format!("subscriber/{months}"));
                }
            }
            "vip" => message.message_flags.insert(MessageFlags::VIP),
            "moderator" => {
                message.message_flags.insert(MessageFlags::MOD);
                message.user_type = Cow::Borrowed("mod");
            }
            "staff" => message.user_type = Cow::Borrowed("staff"),
            _ => {}
        }
    }

    message.badges = badge_values;
    message.badge_info = Cow::Owned(badge_info.join(","));
}

fn apply_reply(message: &mut StructuredMessage<'static>, reply: &Reply) {
    push_tag(
        message,
        "reply-parent-msg-id",
        reply.parent_message_id.clone(),
    );
    push_tag(message, "reply-parent-user-id", reply.parent_user_id.clone());
    push_tag(
        message,
        "reply-parent-user-login",
        slugify(&reply.parent_username),
    );
    push_tag(
        message,
        "reply-parent-display-name",
        reply.parent_username.clone(),
    );
    push_tag(
        message,
        "reply-parent-msg-body",
        normalize_content(&reply.parent_content).0,
    );
    if let Some(thread_parent_id) = &reply.thread_parent_id {
        push_tag(
            message,
            "reply-thread-parent-msg-id",
            thread_parent_id.clone(),
        );
    }
}

/// Parses `#RRGGBB`
fn parse_color(color: &str) -> Option<u32> {
    let digits = color.trim().trim_start_matches('#');
    if digits.len() != 6 || !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(digits, 16).ok()
}

fn plural(count: u64, unit: &str) -> String {
    if count == 1 {
        format!("{count} {unit}")
    } else {
        format!("{count} {unit}s")
    }
}

/// Line breaks would break the line based formats, they are replaced by spaces
fn flatten_text(text: &str) -> String {
    text.chars()
        .map(|c| if c == '\r' || c == '\n' { ' ' } else { c })
        .collect()
}

/// Replaces `[emote:ID:NAME]` tokens in a message by the emote name and builds the Twitch-style
/// `emotes` tag value (`ID:START-END,START-END/ID2:START-END`) describing them.
///
/// Positions are code point offsets into the returned text, the end is inclusive.
/// Line breaks are replaced by spaces.
pub fn normalize_content(content: &str) -> (String, String) {
    let chars: Vec<char> = flatten_text(content).chars().collect();
    let mut text = String::with_capacity(content.len());
    let mut text_len = 0usize;
    // (emote id, ranges) in order of first appearance
    let mut emotes: Vec<(String, Vec<(usize, usize)>)> = Vec::new();

    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '[' {
            if let Some((id, name, next)) = parse_emote_token(&chars, i) {
                let start = text_len;
                text.push_str(&name);
                text_len += name.chars().count();
                let range = (start, text_len - 1);

                match emotes.iter_mut().find(|(existing, _)| *existing == id) {
                    Some((_, ranges)) => ranges.push(range),
                    None => emotes.push((id, vec![range])),
                }

                i = next;
                continue;
            }
        }

        text.push(chars[i]);
        text_len += 1;
        i += 1;
    }

    let emotes = emotes
        .iter()
        .map(|(id, ranges)| {
            let ranges = ranges
                .iter()
                .map(|(start, end)| format!("{start}-{end}"))
                .collect::<Vec<_>>()
                .join(",");
            format!("{id}:{ranges}")
        })
        .collect::<Vec<_>>()
        .join("/");

    (text, emotes)
}

/// Parses an `[emote:ID:NAME]` token starting at `start`.
/// Returns the id, the name and the index after the closing bracket.
fn parse_emote_token(chars: &[char], start: usize) -> Option<(String, String, usize)> {
    const PREFIX: [char; 7] = ['[', 'e', 'm', 'o', 't', 'e', ':'];

    if chars.len() < start + PREFIX.len() || chars[start..start + PREFIX.len()] != PREFIX {
        return None;
    }

    let mut i = start + PREFIX.len();
    let id_start = i;
    while i < chars.len() && chars[i].is_ascii_digit() {
        i += 1;
    }
    if i == id_start
        || i - id_start > MAX_EMOTE_ID_LENGTH
        || i >= chars.len()
        || chars[i] != ':'
    {
        return None;
    }
    let id: String = chars[id_start..i].iter().collect();

    i += 1;
    let name_start = i;
    while i < chars.len() && chars[i] != ']' {
        if chars[i] == '[' || chars[i].is_whitespace() {
            return None;
        }
        i += 1;
    }
    if i >= chars.len() || i == name_start || i - name_start > MAX_EMOTE_NAME_LENGTH {
        return None;
    }
    let name: String = chars[name_start..i].iter().collect();

    Some((id, name, i + 1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        kick::events::{Celebration, Sender},
        logs::schema::message::{BasicMessage, FullMessage, ResponseMessage},
    };
    use chrono::{DateTime, Utc};
    use pretty_assertions::assert_eq;

    const CHAT_ID: &str = "21d47bf1-1486-4228-b4c7-420fe65e40b0";

    fn channel() -> ChannelRef {
        ChannelRef {
            id: "676".to_owned(),
            login: "xqc".to_owned(),
        }
    }

    fn user(id: &str, username: &str, slug: &str) -> UserRef {
        UserRef {
            id: id.to_owned(),
            username: username.to_owned(),
            slug: slug.to_owned(),
        }
    }

    fn chat(content: &str) -> ChatMessage {
        ChatMessage {
            id: CHAT_ID.to_owned(),
            content: content.to_owned(),
            kind: "message".to_owned(),
            created_at: Some(1790923082),
            sender: Sender {
                user: user("12345", "Some_User", "some-user"),
                color: Some("#BC66FF".to_owned()),
                badges: vec![
                    Badge {
                        kind: "subscriber".to_owned(),
                        count: Some(3),
                    },
                    Badge {
                        kind: "vip".to_owned(),
                        count: None,
                    },
                ],
            },
            reply: None,
            celebration: None,
        }
    }

    fn extra<'a>(message: &'a StructuredMessage<'static>, name: &str) -> Option<&'a str> {
        message
            .extra_tags
            .iter()
            .find(|(tag, _)| &**tag == name)
            .map(|(_, value)| &**value)
    }

    #[test]
    fn plain_text_has_no_emotes() {
        assert_eq!(
            normalize_content("just some text"),
            ("just some text".to_owned(), String::new())
        );
    }

    #[test]
    fn emote_tokens_are_replaced_by_their_name() {
        assert_eq!(
            normalize_content("hello [emote:37226:KEKW] bye"),
            ("hello KEKW bye".to_owned(), "37226:6-9".to_owned())
        );
    }

    #[test]
    fn repeated_and_multiple_emotes_are_grouped_by_id() {
        assert_eq!(
            normalize_content("[emote:1:AA] x [emote:2:BBB] [emote:1:AA]"),
            ("AA x BBB AA".to_owned(), "1:0-1,9-10/2:5-7".to_owned())
        );
    }

    #[test]
    fn emote_positions_count_code_points() {
        assert_eq!(
            normalize_content("😀 [emote:5:Hi]"),
            ("😀 Hi".to_owned(), "5:2-3".to_owned())
        );
    }

    #[test]
    fn malformed_emote_tokens_are_left_alone() {
        for content in [
            "[emote:abc:x]",
            "[emote:1:]",
            "[emote:1:a b]",
            "[emote:1:x",
            "[emote::x]",
            "[emote:1]",
            "[emote:1:[x]",
            "[ emote:1:x]",
        ] {
            assert_eq!(
                normalize_content(content),
                (content.to_owned(), String::new()),
                "content {content:?}"
            );
        }
    }

    #[test]
    fn line_breaks_are_flattened() {
        assert_eq!(
            normalize_content("a\r\nb\nc"),
            ("a  b c".to_owned(), String::new())
        );
    }

    #[test]
    fn timestamps_use_arrival_time_inside_the_reported_second() {
        let mut assigner = TimestampAssigner::default();
        assert_eq!(assigner.assign("1", Some(100), 100_450), 100_450);
    }

    #[test]
    fn timestamps_are_clamped_into_the_reported_second() {
        let mut assigner = TimestampAssigner::default();
        // local clock behind Kick's
        assert_eq!(assigner.assign("1", Some(100), 99_000), 100_000);
        // local clock ahead of Kick's
        assert_eq!(assigner.assign("2", Some(100), 105_000), 100_999);
    }

    #[test]
    fn timestamps_are_strictly_increasing_per_channel() {
        let mut assigner = TimestampAssigner::default();
        assert_eq!(assigner.assign("1", Some(100), 100_500), 100_500);
        assert_eq!(assigner.assign("1", Some(100), 100_500), 100_501);
        assert_eq!(assigner.assign("1", Some(100), 100_200), 100_502);
        // another channel is independent
        assert_eq!(assigner.assign("2", Some(100), 100_200), 100_200);
        // events without a reported time use the local clock
        assert_eq!(assigner.assign("1", None, 200_000), 200_000);
    }

    #[test]
    fn implausible_reported_times_are_not_trusted() {
        let mut assigner = TimestampAssigner::default();
        // Kick reports a time a year in the future: the local clock is used instead
        assert_eq!(
            assigner.assign("1", Some(1_000_000_000 + 31_536_000), 1_000_000_000_000),
            1_000_000_000_000
        );
        // and the channel is not stuck in the future afterwards
        assert_eq!(
            assigner.assign("1", Some(1_000_000_001), 1_000_000_001_500),
            1_000_000_001_500
        );
        // a clock which is only a few seconds off is still clamped into the reported second
        assert_eq!(assigner.assign("2", Some(100), 104_000), 100_999);
    }

    #[test]
    fn colors_must_be_six_hex_digits() {
        assert_eq!(parse_color("#BC66FF"), Some(0xBC66FF));
        assert_eq!(parse_color("bc66ff"), Some(0xBC66FF));
        assert_eq!(parse_color("#+ABCDE"), None);
        assert_eq!(parse_color("#BC66F"), None);
        assert_eq!(parse_color("#GGGGGG"), None);
        assert_eq!(parse_color(""), None);
    }

    #[test]
    fn nameless_ban_targets_are_identified_by_their_id() {
        let message = convert_event(
            &channel(),
            &KickEvent::Banned {
                user: user("10", "", ""),
                moderator: None,
                permanent: true,
                duration_minutes: None,
                expires_at: None,
                event_id: None,
            },
            1,
            &ChatIndex::default(),
        );

        assert_eq!(message.user_login, "10");
        assert_eq!(message.text, "10");
    }

    #[test]
    fn huge_timeouts_do_not_overflow() {
        let message = convert_event(
            &channel(),
            &KickEvent::Banned {
                user: user("10", "Target", "target"),
                moderator: None,
                permanent: false,
                duration_minutes: Some(u64::MAX),
                expires_at: None,
                event_id: None,
            },
            1,
            &ChatIndex::default(),
        );

        assert_eq!(extra(&message, "ban-duration"), Some("18446744073709551615"));
    }

    #[test]
    fn anonymous_gifts_are_not_attributed_to_a_user_with_that_name() {
        let mut index = ChatIndex::default();
        index.remember(&user("555", "Anonymous", "anonymous"));

        let message = convert_event(
            &channel(),
            &KickEvent::GiftedSubscriptions {
                gifter_username: ANONYMOUS_GIFTER.to_owned(),
                gifted_usernames: vec!["a".to_owned()],
                count: 1,
                gifter_total: None,
            },
            1,
            &index,
        );

        assert_eq!(message.user_id, "");
        assert_eq!(message.display_name, "Anonymous");
    }

    #[test]
    fn remembered_users_are_found_regardless_of_case() {
        let mut index = ChatIndex::default();
        index.remember(&user("12345", "Some_User", "some-user"));
        index.remember(&user("", "NoId", "no-id"));

        assert_eq!(
            index.lookup("some_USER"),
            Some(&("12345".to_owned(), "some-user".to_owned()))
        );
        assert_eq!(index.lookup("noid"), None);
    }

    #[test]
    fn slugs() {
        assert_eq!(slugify("Some_User"), "some-user");
        assert_eq!(user_login(&user("1", "Some_User", "")), "some-user");
        assert_eq!(user_login(&user("1", "Some_User", "some_user")), "some_user");
    }

    #[test]
    fn converts_chat_message() {
        let message = convert_event(
            &channel(),
            &KickEvent::Chat(chat("hello [emote:37226:KEKW] bye")),
            1790923082123,
            &ChatIndex::default(),
        );

        assert_eq!(message.message_type, MessageType::PrivMsg);
        assert_eq!(message.channel_id, "676");
        assert_eq!(message.channel_login, "xqc");
        assert_eq!(message.user_id, "12345");
        assert_eq!(message.user_login, "some-user");
        assert_eq!(message.display_name, "Some_User");
        assert_eq!(message.color, Some(0xBC66FF));
        assert_eq!(message.text, "hello KEKW bye");
        assert_eq!(message.emotes, "37226:6-9");
        assert_eq!(message.badge_info, "subscriber/3");
        assert_eq!(message.message_flags, MessageFlags::SUBSCRIBER | MessageFlags::VIP);
        assert_eq!(message.id(), Some(CHAT_ID.to_owned()));

        assert_eq!(
            message.to_raw_irc(),
            "@tmi-sent-ts=1790923082123;subscriber=1;vip=1;id=21d47bf1-1486-4228-b4c7-420fe65e40b0;room-id=676;user-id=12345;display-name=Some_User;badges=subscriber/3,vip/1;badge-info=subscriber/3;color=#BC66FF;flags=;user-type=;emotes=37226:6-9 :some-user!some-user@some-user.kick.com PRIVMSG #xqc :hello KEKW bye"
        );
    }

    #[test]
    fn chat_message_roundtrips_through_the_raw_format() {
        use crate::db::schema::UnstructuredMessage;

        let message = convert_event(
            &channel(),
            &KickEvent::Chat(chat("hello [emote:37226:KEKW] bye")),
            1790923082123,
            &ChatIndex::default(),
        );
        let raw = message.to_raw_irc();

        let unstructured = UnstructuredMessage {
            channel_id: "676",
            user_id: "12345",
            timestamp: 1790923082123,
            raw: &raw,
        };
        let parsed = StructuredMessage::from_unstructured(&unstructured).unwrap();

        assert_eq!(parsed, message);
    }

    #[test]
    fn chat_message_json_tags() {
        let message = convert_event(
            &channel(),
            &KickEvent::Chat(chat("hi")),
            1790923082123,
            &ChatIndex::default(),
        );

        let full = FullMessage::from_structured(&message).unwrap();
        assert_eq!(full.username, "some-user");
        assert_eq!(full.channel, "xqc");
        assert_eq!(full.basic.display_name, "Some_User");
        assert_eq!(full.basic.text, "hi");
        assert_eq!(full.basic.id, CHAT_ID);

        let tags = &full.basic.tags;
        assert_eq!(tags.get("room-id").map(|value| &**value), Some("676"));
        assert_eq!(tags.get("user-id").map(|value| &**value), Some("12345"));
        assert_eq!(tags.get("color").map(|value| &**value), Some("#BC66FF"));
        assert_eq!(
            tags.get("badges").map(|value| &**value),
            Some("subscriber/3,vip/1")
        );

        let basic = BasicMessage::from_structured(&message).unwrap();
        assert_eq!(basic.timestamp.timestamp_millis(), 1790923082123);
    }

    #[test]
    fn moderators_and_staff_get_a_user_type() {
        let mut moderator = chat("hi");
        moderator.sender.badges = vec![Badge {
            kind: "moderator".to_owned(),
            count: None,
        }];
        let message = convert_event(
            &channel(),
            &KickEvent::Chat(moderator),
            1,
            &ChatIndex::default(),
        );
        assert_eq!(message.user_type, "mod");
        assert_eq!(message.message_flags, MessageFlags::MOD);
        assert_eq!(message.badges, vec!["moderator/1"]);
        assert_eq!(message.badge_info, "");
    }

    #[test]
    fn non_uuid_message_ids_are_kept_as_a_tag() {
        let mut odd = chat("hi");
        odd.id = "not-a-uuid".to_owned();
        let message = convert_event(&channel(), &KickEvent::Chat(odd), 1, &ChatIndex::default());

        assert_eq!(message.id(), None);
        assert_eq!(extra(&message, "id"), Some("not-a-uuid"));
    }

    #[test]
    fn converts_reply() {
        let mut reply = chat("@Other yes");
        reply.kind = "reply".to_owned();
        reply.reply = Some(Reply {
            parent_message_id: "22222222-2222-4222-8222-222222222222".to_owned(),
            parent_user_id: "2".to_owned(),
            parent_username: "Other_One".to_owned(),
            parent_content: "really [emote:1:LUL]?".to_owned(),
            thread_parent_id: Some("33333333-3333-4333-8333-333333333333".to_owned()),
        });

        let message = convert_event(&channel(), &KickEvent::Chat(reply), 1, &ChatIndex::default());

        assert_eq!(message.message_type, MessageType::PrivMsg);
        assert_eq!(
            extra(&message, "reply-parent-msg-id"),
            Some("22222222-2222-4222-8222-222222222222")
        );
        assert_eq!(extra(&message, "reply-parent-user-id"), Some("2"));
        assert_eq!(extra(&message, "reply-parent-user-login"), Some("other-one"));
        assert_eq!(extra(&message, "reply-parent-display-name"), Some("Other_One"));
        assert_eq!(extra(&message, "reply-parent-msg-body"), Some("really LUL?"));
        assert_eq!(
            extra(&message, "reply-thread-parent-msg-id"),
            Some("33333333-3333-4333-8333-333333333333")
        );
    }

    #[test]
    fn converts_celebration_to_a_resub_notice() {
        let mut celebration = chat("5 months!");
        celebration.kind = "celebration".to_owned();
        celebration.celebration = Some(Celebration {
            kind: "subscription_renewed".to_owned(),
            total_months: 5,
        });

        let message = convert_event(
            &channel(),
            &KickEvent::Chat(celebration),
            1,
            &ChatIndex::default(),
        );

        assert_eq!(message.message_type, MessageType::UserNotice);
        assert_eq!(message.id(), Some(CHAT_ID.to_owned()));
        assert_eq!(message.text, "5 months!");
        assert_eq!(extra(&message, "msg-id"), Some("resub"));
        assert_eq!(extra(&message, "msg-param-cumulative-months"), Some("5"));
        assert_eq!(
            extra(&message, "system-msg"),
            Some("Some_User subscribed for 5 months!")
        );
        assert_eq!(message.user_friendly_text(), "Some_User subscribed for 5 months! 5 months!");
    }

    #[test]
    fn converts_timeout() {
        let message = convert_event(
            &channel(),
            &KickEvent::Banned {
                user: user("10", "Target", "target"),
                moderator: Some(user("20", "Mod", "mod")),
                permanent: false,
                duration_minutes: Some(2),
                expires_at: None,
                event_id: None,
            },
            1790923384000,
            &ChatIndex::default(),
        );

        assert_eq!(message.message_type, MessageType::ClearChat);
        assert_eq!(message.user_id, "10");
        assert_eq!(message.user_login, "target");
        assert_eq!(
            message.to_raw_irc(),
            "@tmi-sent-ts=1790923384000;room-id=676;user-id=10;target-user-id=10;ban-duration=120;moderator-user-id=20;moderator-user-login=mod :kick.com CLEARCHAT #xqc :target"
        );
        assert_eq!(
            message.user_friendly_text(),
            "target has been timed out for 120 seconds by mod"
        );
    }

    #[test]
    fn converts_timeout_with_expiry_and_event_id() {
        let message = convert_event(
            &channel(),
            &KickEvent::Banned {
                user: user("10", "Target", "target"),
                moderator: Some(user("20", "Mod", "mod")),
                permanent: false,
                duration_minutes: Some(2),
                expires_at: Some(
                    DateTime::parse_from_rfc3339("2026-10-02T08:45:04+02:00")
                        .unwrap()
                        .with_timezone(&Utc),
                ),
                event_id: Some("77777777-7777-4777-8777-777777777777".to_owned()),
            },
            1790923384000,
            &ChatIndex::default(),
        );

        // the expiry is written in UTC
        assert_eq!(extra(&message, "ban-expires-at"), Some("2026-10-02T06:45:04Z"));
        assert_eq!(
            message.to_raw_irc(),
            "@tmi-sent-ts=1790923384000;id=77777777-7777-4777-8777-777777777777;room-id=676;user-id=10;target-user-id=10;ban-duration=120;ban-expires-at=2026-10-02T06:45:04Z;moderator-user-id=20;moderator-user-login=mod :kick.com CLEARCHAT #xqc :target"
        );
    }

    #[test]
    fn converts_permanent_ban() {
        let message = convert_event(
            &channel(),
            &KickEvent::Banned {
                user: user("10", "Target", "target"),
                moderator: None,
                permanent: true,
                duration_minutes: Some(5),
                // a timeout is not what this is, nothing about one is recorded
                expires_at: Some(
                    DateTime::parse_from_rfc3339("2026-10-02T06:45:04Z")
                        .unwrap()
                        .with_timezone(&Utc),
                ),
                event_id: None,
            },
            1790923384000,
            &ChatIndex::default(),
        );

        assert_eq!(extra(&message, "ban-duration"), None);
        assert_eq!(extra(&message, "ban-expires-at"), None);
        assert_eq!(extra(&message, "ban-permanent"), Some("1"));
        assert_eq!(
            message.to_raw_irc(),
            "@tmi-sent-ts=1790923384000;room-id=676;user-id=10;target-user-id=10;ban-permanent=1 :kick.com CLEARCHAT #xqc :target"
        );
        assert_eq!(message.user_friendly_text(), "target has been banned");
    }

    #[test]
    fn a_timeout_without_a_length_gets_one_from_its_expiry() {
        let message = convert_event(
            &channel(),
            &KickEvent::Banned {
                user: user("10", "Target", "target"),
                moderator: None,
                permanent: false,
                duration_minutes: None,
                // 120.4 seconds after the event: rounded up
                expires_at: Some(
                    DateTime::parse_from_rfc3339("2026-10-02T06:45:04.4Z")
                        .unwrap()
                        .with_timezone(&Utc),
                ),
                event_id: None,
            },
            1_790_923_384_000,
            &ChatIndex::default(),
        );

        // 2026-10-02T06:43:04Z is 1790923384
        assert_eq!(extra(&message, "ban-duration"), Some("121"));
        assert_eq!(extra(&message, "ban-permanent"), None);
    }

    #[test]
    fn an_id_which_is_not_a_uuid_replaces_the_random_one_of_a_notice() {
        let message = convert_event(
            &channel(),
            &KickEvent::Unbanned {
                user: user("10", "Target", "target"),
                moderator: None,
                permanent: false,
                event_id: Some("not-a-uuid".to_owned()),
            },
            1,
            &ChatIndex::default(),
        );

        assert_eq!(message.id(), None);
        assert_eq!(extra(&message, "id"), Some("not-a-uuid"));
    }

    #[test]
    fn a_moderator_which_is_not_known_is_not_recorded() {
        let message = convert_event(
            &channel(),
            &KickEvent::Banned {
                user: user("10", "Target", "target"),
                moderator: None,
                permanent: false,
                duration_minutes: Some(1),
                expires_at: None,
                event_id: None,
            },
            1,
            &ChatIndex::default(),
        );

        assert_eq!(extra(&message, "moderator-user-id"), None);
        assert_eq!(extra(&message, "moderator-user-login"), None);
        assert_eq!(
            message.user_friendly_text(),
            "target has been timed out for 60 seconds"
        );
    }

    #[test]
    fn converts_chat_clear_and_message_deletion() {
        let cleared = convert_event(
            &channel(),
            &KickEvent::ChatCleared,
            1790923384000,
            &ChatIndex::default(),
        );
        assert_eq!(
            cleared.to_raw_irc(),
            "@tmi-sent-ts=1790923384000;room-id=676 :kick.com CLEARCHAT #xqc"
        );
        assert_eq!(cleared.user_friendly_text(), "Chat has been cleared");

        let deleted = convert_event(
            &channel(),
            &KickEvent::MessageDeleted {
                message_id: CHAT_ID.to_owned(),
                ai_moderated: false,
                violated_rules: vec![],
                event_id: None,
            },
            1790923384000,
            &ChatIndex::default(),
        );
        assert_eq!(deleted.message_type, MessageType::ClearMsg);
        assert_eq!(
            deleted.to_raw_irc(),
            "@tmi-sent-ts=1790923384000;room-id=676;target-msg-id=21d47bf1-1486-4228-b4c7-420fe65e40b0 :kick.com CLEARMSG #xqc"
        );
    }

    #[test]
    fn deleted_messages_tell_who_wrote_them_if_they_were_seen() {
        let mut index = ChatIndex::default();
        let chat_message = convert_event(
            &channel(),
            &KickEvent::Chat(chat("hello [emote:37226:KEKW] bye")),
            1790923082123,
            &index,
        );
        index.remember_message(&chat_message);

        let deleted = convert_event(
            &channel(),
            &KickEvent::MessageDeleted {
                // Kick may write the id in another case than the one that was remembered
                message_id: CHAT_ID.to_uppercase(),
                ai_moderated: true,
                violated_rules: vec!["sexual".to_owned(), "harassment".to_owned()],
                event_id: Some("35e0e479-96ae-43ce-8b3d-042411468010".to_owned()),
            },
            1790923384000,
            &index,
        );

        assert_eq!(deleted.message_type, MessageType::ClearMsg);
        assert_eq!(deleted.user_login, "some-user");
        assert_eq!(deleted.display_name, "Some_User");
        assert_eq!(deleted.text, "hello KEKW bye");
        // written like the id of the message itself
        assert_eq!(extra(&deleted, "target-msg-id"), Some(CHAT_ID));
        assert_eq!(extra(&deleted, "target-user-id"), Some("12345"));
        assert_eq!(extra(&deleted, "ai-moderated"), Some("1"));
        assert_eq!(extra(&deleted, "violated-rules"), Some("sexual,harassment"));
        // the row is not a message of the author, like the `CLEARMSG` of Twitch
        assert_eq!(deleted.user_id, "");
        assert_eq!(subject_user_id(&deleted), "12345");
        assert_eq!(deleted.id(), Some("35e0e479-96ae-43ce-8b3d-042411468010".to_owned()));
        assert_eq!(
            deleted.user_friendly_text(),
            "Message deleted by the AI moderation (sexual,harassment): hello KEKW bye"
        );
    }

    #[test]
    fn deleted_messages_which_were_not_seen_stay_anonymous() {
        let deleted = convert_event(
            &channel(),
            &KickEvent::MessageDeleted {
                message_id: CHAT_ID.to_owned(),
                ai_moderated: false,
                violated_rules: vec![],
                event_id: None,
            },
            1,
            &ChatIndex::default(),
        );

        assert_eq!(deleted.user_login, "");
        assert_eq!(deleted.text, "");
        assert_eq!(extra(&deleted, "target-user-id"), None);
        assert_eq!(subject_user_id(&deleted), "");
        assert_eq!(deleted.user_friendly_text(), "A message was deleted");
    }

    #[test]
    fn the_subject_of_a_message_is_its_sender_or_target() {
        let chat_message = convert_event(
            &channel(),
            &KickEvent::Chat(chat("hi")),
            1,
            &ChatIndex::default(),
        );
        assert_eq!(subject_user_id(&chat_message), "12345");

        let ban = convert_event(
            &channel(),
            &KickEvent::Banned {
                user: user("10", "Target", "target"),
                moderator: None,
                permanent: true,
                duration_minutes: None,
                expires_at: None,
                event_id: None,
            },
            1,
            &ChatIndex::default(),
        );
        assert_eq!(subject_user_id(&ban), "10");
    }

    #[test]
    fn only_messages_with_an_id_and_a_sender_are_remembered() {
        let mut index = ChatIndex::default();
        let mut message = StructuredMessage::new("676".to_owned(), "xqc".to_owned(), 1, MessageType::PrivMsg);

        // no id
        message.user_id = Cow::Borrowed("1");
        index.remember_message(&message);
        assert_eq!(index.message(CHAT_ID), None);

        // no sender
        message.id = Uuid::parse_str(CHAT_ID).unwrap();
        message.user_id = Cow::Borrowed("");
        index.remember_message(&message);
        assert_eq!(index.message(CHAT_ID), None);

        message.user_id = Cow::Borrowed("1");
        index.remember_message(&message);
        assert_eq!(
            index.message(CHAT_ID).map(|remembered| remembered.user_id.as_str()),
            Some("1")
        );
        // not an id at all
        assert_eq!(index.message("not-an-id"), None);
    }

    #[test]
    fn the_oldest_messages_are_forgotten_first() {
        let mut messages = RecentMessages::with_capacity(2);
        let remembered = |user_id: &str| RememberedMessage {
            user_id: user_id.to_owned(),
            login: String::new(),
            username: String::new(),
            text: String::new(),
        };

        let (a, b, c) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));

        messages.insert(a, remembered("1"));
        messages.insert(b, remembered("2"));
        // an id which is known already does not push anything out, the newest data is kept
        messages.insert(b, remembered("3"));
        assert!(messages.get(&a).is_some());
        assert_eq!(messages.get(&b).map(|message| message.user_id.as_str()), Some("3"));

        messages.insert(c, remembered("4"));
        assert!(messages.get(&a).is_none());
        assert!(messages.get(&b).is_some());
        assert!(messages.get(&c).is_some());
        assert_eq!(messages.by_id.len(), 2);
        assert_eq!(messages.order.len(), 2);
    }

    #[test]
    fn converts_unban() {
        let message = convert_event(
            &channel(),
            &KickEvent::Unbanned {
                user: user("10", "Target", "target"),
                moderator: Some(user("20", "Mod", "mod")),
                permanent: false,
                event_id: None,
            },
            1,
            &ChatIndex::default(),
        );

        assert_eq!(message.message_type, MessageType::UserNotice);
        assert_eq!(message.user_id, "10");
        assert_eq!(extra(&message, "msg-id"), Some("unban"));
        assert_eq!(extra(&message, "moderator-user-id"), Some("20"));
        assert_eq!(extra(&message, "moderator-user-login"), Some("mod"));
        assert_eq!(extra(&message, "ban-permanent"), None);
        assert_eq!(
            message.user_friendly_text(),
            "Target has been unbanned by Mod"
        );
    }

    #[test]
    fn converts_the_unban_of_a_permanent_ban() {
        let message = convert_event(
            &channel(),
            &KickEvent::Unbanned {
                user: user("10", "Target", "target"),
                moderator: None,
                permanent: true,
                event_id: Some("88888888-8888-4888-8888-888888888888".to_owned()),
            },
            1,
            &ChatIndex::default(),
        );

        assert_eq!(extra(&message, "ban-permanent"), Some("1"));
        assert_eq!(extra(&message, "moderator-user-id"), None);
        assert_eq!(message.id(), Some("88888888-8888-4888-8888-888888888888".to_owned()));
        assert_eq!(message.user_friendly_text(), "Target has been unbanned");
    }

    #[test]
    fn converts_subscriptions_and_attributes_known_users() {
        let mut index = ChatIndex::default();
        index.remember(&user("555", "Fan", "fan"));

        let resub = convert_event(
            &channel(),
            &KickEvent::Subscription {
                username: "Fan".to_owned(),
                months: 4,
            },
            1,
            &index,
        );
        assert_eq!(resub.message_type, MessageType::UserNotice);
        assert_eq!(resub.user_id, "555");
        assert_eq!(resub.user_login, "fan");
        assert_eq!(extra(&resub, "msg-id"), Some("resub"));
        assert_eq!(extra(&resub, "msg-param-cumulative-months"), Some("4"));
        assert_eq!(resub.user_friendly_text(), "Fan subscribed for 4 months!");

        let new = convert_event(
            &channel(),
            &KickEvent::Subscription {
                username: "Unknown_Fan".to_owned(),
                months: 1,
            },
            1,
            &index,
        );
        assert_eq!(new.user_id, "");
        assert_eq!(new.user_login, "unknown-fan");
        assert_eq!(extra(&new, "msg-id"), Some("sub"));
        assert_eq!(new.user_friendly_text(), "Unknown_Fan subscribed!");
    }

    #[test]
    fn converts_gifted_subscriptions() {
        let few = convert_event(
            &channel(),
            &KickEvent::GiftedSubscriptions {
                gifter_username: "Generous".to_owned(),
                gifted_usernames: vec!["a".to_owned(), "b".to_owned()],
                count: 2,
                gifter_total: Some(20),
            },
            1,
            &ChatIndex::default(),
        );
        assert_eq!(extra(&few, "msg-id"), Some("submysterygift"));
        assert_eq!(extra(&few, "msg-param-mass-gift-count"), Some("2"));
        assert_eq!(extra(&few, "msg-param-sender-count"), Some("20"));
        assert_eq!(extra(&few, "msg-param-recipients"), Some("a,b"));
        assert_eq!(
            few.user_friendly_text(),
            "Generous gifted 2 subscriptions to a, b!"
        );

        let single = convert_event(
            &channel(),
            &KickEvent::GiftedSubscriptions {
                gifter_username: "Generous".to_owned(),
                gifted_usernames: vec!["a".to_owned()],
                count: 1,
                gifter_total: None,
            },
            1,
            &ChatIndex::default(),
        );
        assert_eq!(extra(&single, "msg-id"), Some("subgift"));

        let names: Vec<String> = (0..7).map(|i| format!("user{i}")).collect();
        let many = convert_event(
            &channel(),
            &KickEvent::GiftedSubscriptions {
                gifter_username: "Generous".to_owned(),
                gifted_usernames: names,
                count: 7,
                gifter_total: None,
            },
            1,
            &ChatIndex::default(),
        );
        assert_eq!(many.user_friendly_text(), "Generous gifted 7 subscriptions!");
    }

    #[test]
    fn converts_host_reward_and_kicks() {
        let host = convert_event(
            &channel(),
            &KickEvent::Host {
                host_username: "Raider".to_owned(),
                viewers: 1,
                message: "have fun".to_owned(),
            },
            1,
            &ChatIndex::default(),
        );
        assert_eq!(extra(&host, "msg-id"), Some("raid"));
        assert_eq!(
            host.user_friendly_text(),
            "Raider is hosting the channel with 1 viewer! have fun"
        );

        let reward = convert_event(
            &channel(),
            &KickEvent::RewardRedeemed {
                user: user("7", "Viewer", ""),
                title: "Hydrate".to_owned(),
                input: "now".to_owned(),
            },
            1,
            &ChatIndex::default(),
        );
        assert_eq!(reward.user_id, "7");
        assert_eq!(reward.user_login, "viewer");
        assert_eq!(reward.user_friendly_text(), "Viewer redeemed Hydrate now");

        let kicks = convert_event(
            &channel(),
            &KickEvent::KicksGifted {
                sender: user("9", "Whale", ""),
                name: "Rage Quit".to_owned(),
                amount: 500,
                message: "w".to_owned(),
            },
            1,
            &ChatIndex::default(),
        );
        assert_eq!(extra(&kicks, "msg-id"), Some("kicks-gifted"));
        assert_eq!(
            kicks.user_friendly_text(),
            "Whale gifted 500 KICKs (Rage Quit)! w"
        );
    }
}
