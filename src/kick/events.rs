//! Parsing of the events Kick publishes on its chat websocket.
//!
//! The websocket is not an official API, so payloads are read leniently through
//! `serde_json::Value`: a missing or unexpected field must never crash the logger,
//! at worst the affected event is skipped.

use chrono::{DateTime, Utc};
use serde_json::Value;

pub const CHAT_MESSAGE_EVENT: &str = "App\\Events\\ChatMessageEvent";
pub const MESSAGE_DELETED_EVENT: &str = "App\\Events\\MessageDeletedEvent";
pub const USER_BANNED_EVENT: &str = "App\\Events\\UserBannedEvent";
pub const USER_UNBANNED_EVENT: &str = "App\\Events\\UserUnbannedEvent";
pub const CHATROOM_CLEAR_EVENT: &str = "App\\Events\\ChatroomClearEvent";
pub const SUBSCRIPTION_EVENT: &str = "App\\Events\\SubscriptionEvent";
pub const GIFTED_SUBSCRIPTIONS_EVENT: &str = "App\\Events\\GiftedSubscriptionsEvent";
pub const STREAM_HOST_EVENT: &str = "App\\Events\\StreamHostEvent";
pub const REWARD_REDEEMED_EVENT: &str = "App\\Events\\RewardRedeemedEvent";
pub const KICKS_GIFTED_EVENT: &str = "App\\Events\\KicksGifted";

/// The name used for gifts of which Kick does not tell who sent them
pub const ANONYMOUS_GIFTER: &str = "Anonymous";

/// A Kick user as it is referenced in events.
///
/// `id` is the Kick user id (the same id space as the broadcaster user id of a channel),
/// `username` the display name and `slug` the url-safe, lowercase login.
/// `slug` is empty when the event does not provide it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UserRef {
    pub id: String,
    pub username: String,
    pub slug: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Badge {
    /// Badge identifier, for example `subscriber`, `moderator` or `vip`
    pub kind: String,
    /// Months for subscriber badges, number of gifted subs for sub gifter badges
    pub count: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sender {
    pub user: UserRef,
    /// Username color as `#RRGGBB`
    pub color: Option<String>,
    pub badges: Vec<Badge>,
}

/// Data about the message a reply refers to
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub parent_message_id: String,
    pub parent_user_id: String,
    pub parent_username: String,
    pub parent_content: String,
    pub thread_parent_id: Option<String>,
}

/// Subscription celebration attached to a chat message
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Celebration {
    /// For example `subscription_renewed`
    pub kind: String,
    pub total_months: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatMessage {
    pub id: String,
    /// Message text, emotes are inlined as `[emote:ID:NAME]`
    pub content: String,
    /// `message`, `reply` or `celebration`
    pub kind: String,
    /// Creation time in unix seconds. Kick only provides second precision.
    pub created_at: Option<i64>,
    pub sender: Sender,
    pub reply: Option<Reply>,
    pub celebration: Option<Celebration>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KickEvent {
    Chat(ChatMessage),
    MessageDeleted {
        message_id: String,
        /// Whether Kick's AI moderation deleted the message
        ai_moderated: bool,
        /// The rules the AI moderation found the message to violate, for example `sexual`
        violated_rules: Vec<String>,
        /// The id Kick gave to the moderation event itself
        event_id: Option<String>,
    },
    Banned {
        user: UserRef,
        /// `None` if Kick does not tell who it was, which includes moderators it hides
        moderator: Option<UserRef>,
        permanent: bool,
        /// Length of a timeout in minutes
        duration_minutes: Option<u64>,
        /// When a timeout ends
        expires_at: Option<DateTime<Utc>>,
        event_id: Option<String>,
    },
    Unbanned {
        user: UserRef,
        /// `None` if Kick does not tell who it was, which includes moderators it hides
        moderator: Option<UserRef>,
        /// Whether the ban which was lifted was a permanent one (and not a timeout)
        permanent: bool,
        event_id: Option<String>,
    },
    ChatCleared,
    Subscription {
        username: String,
        months: u64,
    },
    GiftedSubscriptions {
        gifter_username: String,
        gifted_usernames: Vec<String>,
        count: u64,
        /// Total number of subs the gifter has gifted in the channel
        gifter_total: Option<u64>,
    },
    Host {
        host_username: String,
        viewers: u64,
        message: String,
    },
    RewardRedeemed {
        user: UserRef,
        title: String,
        input: String,
    },
    KicksGifted {
        sender: UserRef,
        name: String,
        amount: u64,
        message: String,
    },
}

/// Parses the payload of a chat event. Returns `None` for events which are not logged
/// and for payloads that do not have the expected shape.
pub fn parse_event(name: &str, data: &Value) -> Option<KickEvent> {
    match name {
        CHAT_MESSAGE_EVENT => parse_chat_message(data).map(KickEvent::Chat),
        MESSAGE_DELETED_EVENT => parse_message_deleted(data),
        USER_BANNED_EVENT => parse_banned(data),
        USER_UNBANNED_EVENT => parse_unbanned(data),
        CHATROOM_CLEAR_EVENT => Some(KickEvent::ChatCleared),
        SUBSCRIPTION_EVENT => parse_subscription(data),
        GIFTED_SUBSCRIPTIONS_EVENT => parse_gifted_subscriptions(data),
        STREAM_HOST_EVENT => parse_host(data),
        REWARD_REDEEMED_EVENT => parse_reward_redeemed(data),
        KICKS_GIFTED_EVENT => parse_kicks_gifted(data),
        _ => None,
    }
}

fn parse_chat_message(data: &Value) -> Option<ChatMessage> {
    let id = id_at(data, &["id"])?;
    let sender_node = data.get("sender")?;

    let user = UserRef {
        id: id_at(sender_node, &["id"])?,
        username: str_at(sender_node, &["username"]).unwrap_or_default(),
        slug: str_at(sender_node, &["slug"]).unwrap_or_default(),
    };
    let color = str_at(sender_node, &["identity", "color"]).filter(|color| !color.is_empty());

    let badges = at(sender_node, &["identity", "badges"])
        .and_then(|badges| badges.as_array())
        .map(|badges| {
            badges
                .iter()
                .filter_map(|badge| {
                    let kind = str_at(badge, &["type"]).filter(|kind| !kind.is_empty())?;
                    Some(Badge {
                        kind,
                        count: u64_at(badge, &["count"]),
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let created_at = str_at(data, &["created_at"])
        .and_then(|created_at| DateTime::parse_from_rfc3339(&created_at).ok())
        .map(|created_at| created_at.timestamp());

    let reply = at(data, &["metadata"]).and_then(|metadata| {
        let parent_message_id = id_at(metadata, &["original_message", "id"])?;
        Some(Reply {
            parent_message_id,
            parent_user_id: id_at(metadata, &["original_sender", "id"]).unwrap_or_default(),
            parent_username: str_at(metadata, &["original_sender", "username"])
                .unwrap_or_default(),
            parent_content: str_at(metadata, &["original_message", "content"])
                .unwrap_or_default(),
            thread_parent_id: id_at(data, &["thread_parent_id"]),
        })
    });

    let celebration = at(data, &["metadata", "celebration"])
        .filter(|celebration| celebration.is_object())
        .map(|celebration| Celebration {
            kind: str_at(celebration, &["type"]).unwrap_or_default(),
            total_months: u64_at(celebration, &["total_months"]).unwrap_or(0),
        });

    Some(ChatMessage {
        id,
        content: str_at(data, &["content"]).unwrap_or_default(),
        kind: str_at(data, &["type"]).unwrap_or_else(|| "message".to_owned()),
        created_at,
        sender: Sender {
            user,
            color,
            badges,
        },
        reply,
        celebration,
    })
}

fn parse_message_deleted(data: &Value) -> Option<KickEvent> {
    let message_id = id_at(data, &["message", "id"])?;
    let ai_moderated = at(data, &["aiModerated"])
        .and_then(|ai_moderated| ai_moderated.as_bool())
        .unwrap_or(false);
    let violated_rules: Vec<String> = at(data, &["violatedRules"])
        .and_then(|rules| rules.as_array())
        .map(|rules| {
            rules
                .iter()
                .filter_map(|rule| rule.as_str())
                .filter(|rule| !rule.is_empty())
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default();

    Some(KickEvent::MessageDeleted {
        message_id,
        ai_moderated,
        violated_rules,
        event_id: id_at(data, &["id"]),
    })
}

fn parse_banned(data: &Value) -> Option<KickEvent> {
    let user = user_at(data, &["user"])?;
    let moderator = moderator_at(data, &["banned_by"]);
    let permanent = at(data, &["permanent"])
        .and_then(|permanent| permanent.as_bool())
        .unwrap_or(false);
    let duration_minutes = u64_at(data, &["duration"]).filter(|duration| *duration > 0);
    let expires_at = str_at(data, &["expires_at"])
        .and_then(|expires_at| DateTime::parse_from_rfc3339(&expires_at).ok())
        .map(|expires_at| expires_at.with_timezone(&Utc));

    Some(KickEvent::Banned {
        user,
        moderator,
        permanent,
        duration_minutes,
        expires_at,
        event_id: id_at(data, &["id"]),
    })
}

fn parse_unbanned(data: &Value) -> Option<KickEvent> {
    let user = user_at(data, &["user"])?;
    let permanent = at(data, &["permanent"])
        .and_then(|permanent| permanent.as_bool())
        .unwrap_or(false);

    Some(KickEvent::Unbanned {
        user,
        moderator: moderator_at(data, &["unbanned_by"]),
        permanent,
        event_id: id_at(data, &["id"]),
    })
}

fn parse_subscription(data: &Value) -> Option<KickEvent> {
    let username = str_at(data, &["username"]).filter(|username| !username.is_empty())?;
    let months = u64_at(data, &["months"]).unwrap_or(1).max(1);

    Some(KickEvent::Subscription { username, months })
}

fn parse_gifted_subscriptions(data: &Value) -> Option<KickEvent> {
    let gifter_username = str_at(data, &["gifter_username"])
        .or_else(|| str_at(data, &["username"]))
        .filter(|username| !username.is_empty())
        .unwrap_or_else(|| ANONYMOUS_GIFTER.to_owned());

    let gifted_usernames: Vec<String> = at(data, &["gifted_usernames"])
        .and_then(|names| names.as_array())
        .map(|names| {
            names
                .iter()
                .filter_map(|name| name.as_str().map(ToOwned::to_owned))
                .collect()
        })
        .unwrap_or_default();

    let count = if gifted_usernames.is_empty() {
        u64_at(data, &["quantity"]).unwrap_or(0)
    } else {
        gifted_usernames.len() as u64
    };
    if count == 0 {
        return None;
    }

    Some(KickEvent::GiftedSubscriptions {
        gifter_username,
        gifted_usernames,
        count,
        gifter_total: u64_at(data, &["gifter_total"]),
    })
}

fn parse_host(data: &Value) -> Option<KickEvent> {
    let host_username =
        str_at(data, &["host_username"]).filter(|username| !username.is_empty())?;

    Some(KickEvent::Host {
        host_username,
        viewers: u64_at(data, &["number_viewers"]).unwrap_or(0),
        message: str_at(data, &["optional_message"]).unwrap_or_default(),
    })
}

fn parse_reward_redeemed(data: &Value) -> Option<KickEvent> {
    // Both a flat (`reward_title`, `user_id`, `username`) and a nested (`reward`, `redeemer`)
    // layout are accepted
    let title = str_at(data, &["reward_title"])
        .or_else(|| str_at(data, &["reward", "title"]))
        .filter(|title| !title.is_empty())?;

    let user = UserRef {
        id: id_at(data, &["user_id"])
            .or_else(|| id_at(data, &["redeemer", "id"]))
            .or_else(|| id_at(data, &["redeemer", "user_id"]))
            .unwrap_or_default(),
        username: str_at(data, &["username"])
            .or_else(|| str_at(data, &["redeemer", "username"]))
            .unwrap_or_default(),
        slug: str_at(data, &["redeemer", "slug"])
            .or_else(|| str_at(data, &["redeemer", "channel_slug"]))
            .unwrap_or_default(),
    };
    if user.id.is_empty() && user.username.is_empty() {
        return None;
    }

    Some(KickEvent::RewardRedeemed {
        user,
        title,
        input: str_at(data, &["user_input"]).unwrap_or_default(),
    })
}

fn parse_kicks_gifted(data: &Value) -> Option<KickEvent> {
    let sender = UserRef {
        id: id_at(data, &["sender", "id"])
            .or_else(|| id_at(data, &["sender", "user_id"]))
            .unwrap_or_default(),
        username: str_at(data, &["sender", "username"]).filter(|name| !name.is_empty())?,
        slug: str_at(data, &["sender", "slug"])
            .or_else(|| str_at(data, &["sender", "channel_slug"]))
            .unwrap_or_default(),
    };

    Some(KickEvent::KicksGifted {
        sender,
        name: str_at(data, &["gift", "name"]).unwrap_or_default(),
        amount: u64_at(data, &["gift", "amount"]).unwrap_or(0),
        message: str_at(data, &["message"])
            .or_else(|| str_at(data, &["gift", "message"]))
            .unwrap_or_default(),
    })
}

fn at<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut current = value;
    for key in path {
        current = current.get(*key)?;
    }
    Some(current)
}

fn str_at(value: &Value, path: &[&str]) -> Option<String> {
    at(value, path)?.as_str().map(ToOwned::to_owned)
}

/// Reads an id which may be encoded either as a number or as a string
fn id_at(value: &Value, path: &[&str]) -> Option<String> {
    match at(value, path)? {
        Value::String(id) if !id.is_empty() => Some(id.clone()),
        Value::Number(id) => Some(id.to_string()),
        _ => None,
    }
}

fn u64_at(value: &Value, path: &[&str]) -> Option<u64> {
    match at(value, path)? {
        Value::Number(number) => number.as_u64(),
        Value::String(number) => number.trim().parse().ok(),
        _ => None,
    }
}

fn user_at(value: &Value, path: &[&str]) -> Option<UserRef> {
    let node = at(value, path)?;
    Some(UserRef {
        id: id_at(node, &["id"])?,
        username: str_at(node, &["username"]).unwrap_or_default(),
        slug: str_at(node, &["slug"]).unwrap_or_default(),
    })
}

/// The moderator of a moderation event. Kick sends the id 0 and the name `moderator` when it
/// does not show who it was (seen for timeouts), that is not a user.
fn moderator_at(value: &Value, path: &[&str]) -> Option<UserRef> {
    user_at(value, path).filter(|moderator| moderator.id != "0")
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[test]
    fn parses_chat_message() {
        let data = json!({
            "id": "21d47bf1-1486-4228-b4c7-420fe65e40b0",
            "chatroom_id": 62484699,
            "content": "hello [emote:37226:KEKW]",
            "type": "message",
            "created_at": "2026-10-02T06:38:02+00:00",
            "sender": {
                "id": 12345,
                "username": "Some_User",
                "slug": "some-user",
                "identity": {
                    "color": "#BC66FF",
                    "badges": [
                        {"type": "subscriber", "text": "Subscriber", "count": 3, "sort_order": 4},
                        {"type": "vip", "text": "VIP", "sort_order": 2}
                    ],
                    "badges_v2": [
                        {"name": "level", "badge_type": "global", "image_url": "https://example.com/a.png", "selected": true, "sort_order": 1}
                    ]
                }
            },
            "metadata": {"message_ref": "1759387082000"}
        });

        let event = parse_event(CHAT_MESSAGE_EVENT, &data).unwrap();

        assert_eq!(
            event,
            KickEvent::Chat(ChatMessage {
                id: "21d47bf1-1486-4228-b4c7-420fe65e40b0".to_owned(),
                content: "hello [emote:37226:KEKW]".to_owned(),
                kind: "message".to_owned(),
                created_at: Some(1790923082),
                sender: Sender {
                    user: UserRef {
                        id: "12345".to_owned(),
                        username: "Some_User".to_owned(),
                        slug: "some-user".to_owned(),
                    },
                    color: Some("#BC66FF".to_owned()),
                    badges: vec![
                        Badge {
                            kind: "subscriber".to_owned(),
                            count: Some(3)
                        },
                        Badge {
                            kind: "vip".to_owned(),
                            count: None
                        },
                    ],
                },
                reply: None,
                celebration: None,
            })
        );
    }

    #[test]
    fn parses_reply() {
        let data = json!({
            "id": "11111111-1111-4111-8111-111111111111",
            "chatroom_id": 1,
            "content": "@other yes",
            "type": "reply",
            "created_at": "2026-10-02T06:38:02+00:00",
            "sender": {"id": 1, "username": "a", "slug": "a", "identity": {"color": "", "badges": []}},
            "metadata": {
                "original_sender": {"id": 2, "username": "Other"},
                "original_message": {"id": "22222222-2222-4222-8222-222222222222", "content": "really?"},
                "message_ref": "1"
            },
            "thread_parent_id": "33333333-3333-4333-8333-333333333333"
        });

        let Some(KickEvent::Chat(chat)) = parse_event(CHAT_MESSAGE_EVENT, &data) else {
            panic!("not a chat message");
        };

        assert_eq!(chat.kind, "reply");
        assert_eq!(chat.sender.color, None);
        assert_eq!(
            chat.reply,
            Some(Reply {
                parent_message_id: "22222222-2222-4222-8222-222222222222".to_owned(),
                parent_user_id: "2".to_owned(),
                parent_username: "Other".to_owned(),
                parent_content: "really?".to_owned(),
                thread_parent_id: Some("33333333-3333-4333-8333-333333333333".to_owned()),
            })
        );
    }

    #[test]
    fn parses_celebration() {
        let data = json!({
            "id": "44444444-4444-4444-8444-444444444444",
            "chatroom_id": 1,
            "content": "5 months!",
            "type": "celebration",
            "created_at": "2026-10-02T06:38:02+00:00",
            "sender": {"id": 1, "username": "a", "slug": "a", "identity": {"color": "#FFFFFF", "badges": []}},
            "metadata": {
                "celebration": {
                    "id": "chceleb_01M34A5JXH5SYTQ1E39NM6M124",
                    "type": "subscription_renewed",
                    "total_months": 5,
                    "created_at": "2026-09-22T10:21:47.313752Z"
                },
                "message_ref": "1"
            }
        });

        let Some(KickEvent::Chat(chat)) = parse_event(CHAT_MESSAGE_EVENT, &data) else {
            panic!("not a chat message");
        };

        assert_eq!(chat.kind, "celebration");
        assert_eq!(chat.reply, None);
        assert_eq!(
            chat.celebration,
            Some(Celebration {
                kind: "subscription_renewed".to_owned(),
                total_months: 5
            })
        );
    }

    #[test]
    fn chat_message_without_sender_is_rejected() {
        let data = json!({"id": "abc", "content": "hi", "type": "message"});
        assert_eq!(parse_event(CHAT_MESSAGE_EVENT, &data), None);
    }

    #[test]
    fn parses_deleted_message() {
        let data = json!({
            "id": "55555555-5555-4555-8555-555555555555",
            "message": {"id": "66666666-6666-4666-8666-666666666666"},
            "aiModerated": false,
            "violatedRules": []
        });

        assert_eq!(
            parse_event(MESSAGE_DELETED_EVENT, &data),
            Some(KickEvent::MessageDeleted {
                message_id: "66666666-6666-4666-8666-666666666666".to_owned(),
                ai_moderated: false,
                violated_rules: vec![],
                event_id: Some("55555555-5555-4555-8555-555555555555".to_owned()),
            })
        );
    }

    #[test]
    fn parses_message_deleted_by_the_ai_moderation() {
        // the shape of a deletion by the AI moderation, as sent by Kick
        let data = json!({
            "id": "35e0e479-96ae-43ce-8b3d-042411468010",
            "message": {"id": "c52ca12d-3cd1-4471-80ed-2cf73bac96a1"},
            "aiModerated": true,
            "violatedRules": ["sexual", "", "harassment"]
        });

        assert_eq!(
            parse_event(MESSAGE_DELETED_EVENT, &data),
            Some(KickEvent::MessageDeleted {
                message_id: "c52ca12d-3cd1-4471-80ed-2cf73bac96a1".to_owned(),
                ai_moderated: true,
                violated_rules: vec!["sexual".to_owned(), "harassment".to_owned()],
                event_id: Some("35e0e479-96ae-43ce-8b3d-042411468010".to_owned()),
            })
        );
    }

    #[test]
    fn message_deleted_without_a_message_is_ignored() {
        assert_eq!(
            parse_event(MESSAGE_DELETED_EVENT, &json!({"id": "x", "aiModerated": false})),
            None
        );
    }

    #[test]
    fn parses_timeout() {
        let data = json!({
            "id": "77777777-7777-4777-8777-777777777777",
            "user": {"id": 10, "username": "Target", "slug": "target"},
            "banned_by": {"id": 20, "username": "Mod", "slug": "mod"},
            "permanent": false,
            "duration": 2,
            "expires_at": "2026-10-02T06:45:04+00:00"
        });

        let user = UserRef {
            id: "10".to_owned(),
            username: "Target".to_owned(),
            slug: "target".to_owned(),
        };
        let moderator = UserRef {
            id: "20".to_owned(),
            username: "Mod".to_owned(),
            slug: "mod".to_owned(),
        };
        assert_eq!(
            parse_event(USER_BANNED_EVENT, &data),
            Some(KickEvent::Banned {
                user,
                moderator: Some(moderator),
                permanent: false,
                duration_minutes: Some(2),
                expires_at: Some(
                    DateTime::parse_from_rfc3339("2026-10-02T06:45:04Z")
                        .unwrap()
                        .with_timezone(&Utc)
                ),
                event_id: Some("77777777-7777-4777-8777-777777777777".to_owned()),
            })
        );
    }

    #[test]
    fn parses_a_timeout_by_a_hidden_moderator() {
        // as sent by Kick for a timeout: the moderator has the id 0
        let data = json!({
            "id": "29662ddd-e26c-488c-a15c-10bebc527e14",
            "user": {"id": 112118840, "username": "MeRkCcity", "slug": "merkccity"},
            "banned_by": {"id": 0, "username": "moderator", "slug": "moderator"},
            "permanent": false,
            "duration": 2,
            "expires_at": "2026-10-02T06:45:04+00:00"
        });

        let Some(KickEvent::Banned {
            user,
            moderator,
            duration_minutes,
            ..
        }) = parse_event(USER_BANNED_EVENT, &data)
        else {
            panic!("not a ban");
        };
        assert_eq!(user.id, "112118840");
        assert_eq!(moderator, None);
        assert_eq!(duration_minutes, Some(2));
    }

    #[test]
    fn parses_permanent_ban_without_moderator() {
        let data = json!({
            "id": "77777777-7777-4777-8777-777777777777",
            "user": {"id": 10, "username": "Target", "slug": "target"},
            "permanent": true
        });

        let Some(KickEvent::Banned {
            moderator,
            permanent,
            duration_minutes,
            expires_at,
            ..
        }) = parse_event(USER_BANNED_EVENT, &data)
        else {
            panic!("not a ban");
        };
        assert_eq!(moderator, None);
        assert!(permanent);
        assert_eq!(duration_minutes, None);
        assert_eq!(expires_at, None);
    }

    #[test]
    fn an_unusable_expiry_is_ignored() {
        let data = json!({
            "user": {"id": 10, "username": "Target", "slug": "target"},
            "permanent": false,
            "duration": 5,
            "expires_at": "soon"
        });

        let Some(KickEvent::Banned {
            expires_at,
            duration_minutes,
            event_id,
            ..
        }) = parse_event(USER_BANNED_EVENT, &data)
        else {
            panic!("not a ban");
        };
        assert_eq!(expires_at, None);
        assert_eq!(duration_minutes, Some(5));
        assert_eq!(event_id, None);
    }

    #[test]
    fn parses_unban_and_clear() {
        let data = json!({
            "id": "88888888-8888-4888-8888-888888888888",
            "user": {"id": 10, "username": "Target", "slug": "target"},
            "unbanned_by": {"id": 20, "username": "Mod", "slug": "mod"},
            "permanent": true
        });
        let Some(KickEvent::Unbanned {
            user,
            moderator,
            permanent,
            event_id,
        }) = parse_event(USER_UNBANNED_EVENT, &data)
        else {
            panic!("not an unban");
        };
        assert_eq!(user.slug, "target");
        assert_eq!(moderator.map(|moderator| moderator.id), Some("20".to_owned()));
        assert!(permanent);
        assert_eq!(
            event_id,
            Some("88888888-8888-4888-8888-888888888888".to_owned())
        );

        assert_eq!(
            parse_event(CHATROOM_CLEAR_EVENT, &json!({"id": "x"})),
            Some(KickEvent::ChatCleared)
        );
    }

    #[test]
    fn unbanning_a_timeout_and_a_hidden_moderator() {
        let data = json!({
            "user": {"id": 10, "username": "Target", "slug": "target"},
            "unbanned_by": {"id": 0, "username": "moderator", "slug": "moderator"},
            "permanent": false
        });
        let Some(KickEvent::Unbanned {
            moderator,
            permanent,
            ..
        }) = parse_event(USER_UNBANNED_EVENT, &data)
        else {
            panic!("not an unban");
        };
        assert_eq!(moderator, None);
        assert!(!permanent);
    }

    #[test]
    fn parses_subscription_events() {
        assert_eq!(
            parse_event(
                SUBSCRIPTION_EVENT,
                &json!({"chatroom_id": 1, "username": "Fan", "months": 4})
            ),
            Some(KickEvent::Subscription {
                username: "Fan".to_owned(),
                months: 4
            })
        );

        assert_eq!(
            parse_event(
                GIFTED_SUBSCRIPTIONS_EVENT,
                &json!({
                    "chatroom_id": 1,
                    "gifted_usernames": ["a", "b", "c"],
                    "gifter_username": "Generous",
                    "gifter_total": 20
                })
            ),
            Some(KickEvent::GiftedSubscriptions {
                gifter_username: "Generous".to_owned(),
                gifted_usernames: vec!["a".to_owned(), "b".to_owned(), "c".to_owned()],
                count: 3,
                gifter_total: Some(20),
            })
        );

        // a gift event without any recipients carries nothing worth logging
        assert_eq!(
            parse_event(
                GIFTED_SUBSCRIPTIONS_EVENT,
                &json!({"chatroom_id": 1, "gifted_usernames": [], "gifter_username": "x"})
            ),
            None
        );
    }

    #[test]
    fn parses_host_reward_and_kicks() {
        assert_eq!(
            parse_event(
                STREAM_HOST_EVENT,
                &json!({"chatroom_id": 1, "optional_message": "hi", "number_viewers": 12, "host_username": "Raider"})
            ),
            Some(KickEvent::Host {
                host_username: "Raider".to_owned(),
                viewers: 12,
                message: "hi".to_owned()
            })
        );

        assert_eq!(
            parse_event(
                REWARD_REDEEMED_EVENT,
                &json!({"reward_title": "Hydrate", "user_id": 7, "username": "Viewer", "user_input": "now"})
            ),
            Some(KickEvent::RewardRedeemed {
                user: UserRef {
                    id: "7".to_owned(),
                    username: "Viewer".to_owned(),
                    slug: String::new()
                },
                title: "Hydrate".to_owned(),
                input: "now".to_owned(),
            })
        );

        assert_eq!(
            parse_event(
                KICKS_GIFTED_EVENT,
                &json!({"message": "w", "sender": {"id": 9, "username": "Whale"}, "gift": {"name": "Rage Quit", "amount": 500}})
            ),
            Some(KickEvent::KicksGifted {
                sender: UserRef {
                    id: "9".to_owned(),
                    username: "Whale".to_owned(),
                    slug: String::new()
                },
                name: "Rage Quit".to_owned(),
                amount: 500,
                message: "w".to_owned(),
            })
        );
    }

    #[test]
    fn unknown_events_are_ignored() {
        assert_eq!(parse_event("App\\Events\\PollUpdateEvent", &json!({})), None);
    }

    #[test]
    fn ids_may_be_numbers_or_strings() {
        let data = json!({"a": 5, "b": "6", "c": "", "d": null});
        assert_eq!(id_at(&data, &["a"]), Some("5".to_owned()));
        assert_eq!(id_at(&data, &["b"]), Some("6".to_owned()));
        assert_eq!(id_at(&data, &["c"]), None);
        assert_eq!(id_at(&data, &["d"]), None);
        assert_eq!(id_at(&data, &["missing", "deeper"]), None);
    }
}
