//! Bans, timeouts and the other things moderators do, and how they relate to each other.
//!
//! A ban or a timeout lasts until something ends it: a moderator lifts it (an unban), a timeout
//! runs out, or a newer ban or timeout takes its place. Kick publishes who did what, so the
//! history of a user can tell who banned or timed out the user, whether and when they were
//! unbanned, and by whom.

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// What a moderation action is
#[derive(Deserialize, Serialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ModerationKind {
    /// A permanent ban
    Ban,
    /// A ban for a limited time
    Timeout,
    /// A ban or a timeout was lifted
    Unban,
    /// A message was deleted
    Delete,
    /// The chat was cleared
    Clear,
}

impl ModerationKind {
    /// The name used in the database
    pub fn as_str(self) -> &'static str {
        match self {
            ModerationKind::Ban => "ban",
            ModerationKind::Timeout => "timeout",
            ModerationKind::Unban => "unban",
            ModerationKind::Delete => "delete",
            ModerationKind::Clear => "clear",
        }
    }

    pub fn from_db(value: &str) -> Option<Self> {
        match value {
            "ban" => Some(ModerationKind::Ban),
            "timeout" => Some(ModerationKind::Timeout),
            "unban" => Some(ModerationKind::Unban),
            "delete" => Some(ModerationKind::Delete),
            "clear" => Some(ModerationKind::Clear),
            _ => None,
        }
    }

    /// Bans and timeouts last until something ends them
    pub fn is_punishment(self) -> bool {
        matches!(self, ModerationKind::Ban | ModerationKind::Timeout)
    }
}

/// One thing a moderator did, as it was logged
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModerationAction {
    pub channel_id: String,
    pub channel_login: String,
    pub timestamp: DateTime<Utc>,
    pub kind: ModerationKind,
    /// The user the action was about: who was banned, whose message was deleted.
    /// Empty if there is none (the chat was cleared) or if it is not known.
    pub user_id: String,
    pub user_login: String,
    /// Who did it, empty if Kick does not say
    pub moderator_id: String,
    pub moderator_login: String,
    /// The length of a timeout
    pub duration_seconds: Option<u64>,
    /// When a timeout ends
    pub expires_at: Option<DateTime<Utc>>,
    /// For unbans: whether Kick says the ban which was lifted was a permanent one
    pub permanent: bool,
    /// The deleted message
    pub message_id: String,
    /// What the deleted message said, if it is known
    pub text: String,
    /// Whether the AI moderation of Kick deleted the message
    pub ai_moderated: bool,
    /// The rules the AI moderation found the message to violate
    pub violated_rules: Vec<String>,
}

impl ModerationAction {
    /// When a timeout ends, from the expiry Kick sent or, if there is none, from its length
    fn ends_at(&self) -> Option<DateTime<Utc>> {
        if self.kind != ModerationKind::Timeout {
            return None;
        }
        if let Some(expires_at) = self.expires_at {
            return Some(expires_at);
        }

        let seconds = i64::try_from(self.duration_seconds?).ok()?;
        let millis = self
            .timestamp
            .timestamp_millis()
            .checked_add(seconds.checked_mul(1000)?)?;
        DateTime::from_timestamp_millis(millis)
    }
}

/// Why a ban or a timeout is over
#[derive(Serialize, JsonSchema, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum EndReason {
    /// A moderator lifted it
    Unban,
    /// The timeout ran out
    Expired,
    /// A newer ban or timeout took its place
    Superseded,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PunishmentEnd {
    pub at: DateTime<Utc>,
    pub reason: EndReason,
    /// The moderator who lifted it, or who gave the newer ban or timeout. Empty if Kick does
    /// not say, and for timeouts which ran out.
    pub moderator_id: String,
    pub moderator_login: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryEntry {
    pub action: ModerationAction,
    /// For bans and timeouts: how it ended. `None` if it is still going on (as far as is
    /// logged), and for everything else.
    pub end: Option<PunishmentEnd>,
}

/// Puts the actions in chronological order and works out how every ban and timeout ended.
/// Actions of different channels do not affect each other. `now` decides whether a timeout
/// which nothing ended has run out.
pub fn history(mut actions: Vec<ModerationAction>, now: DateTime<Utc>) -> Vec<HistoryEntry> {
    // Stable, so actions at the same time stay in the order they were stored in
    actions.sort_by_key(|action| action.timestamp);

    let ends = {
        // The positions of the actions of each channel, in order
        let mut positions: HashMap<&str, Vec<usize>> = HashMap::new();
        for (position, action) in actions.iter().enumerate() {
            positions
                .entry(action.channel_id.as_str())
                .or_default()
                .push(position);
        }

        let mut ends: Vec<Option<PunishmentEnd>> = vec![None; actions.len()];
        for channel_positions in positions.values() {
            for (index, &position) in channel_positions.iter().enumerate() {
                let action = &actions[position];
                if !action.kind.is_punishment() {
                    continue;
                }

                // What came next in the channel and has an effect on it
                let next = channel_positions[index + 1..]
                    .iter()
                    .map(|&next| &actions[next])
                    .find(|next| next.kind.is_punishment() || next.kind == ModerationKind::Unban);
                ends[position] = end_of(action, next, now);
            }
        }
        ends
    };

    actions
        .into_iter()
        .zip(ends)
        .map(|(action, end)| HistoryEntry { action, end })
        .collect()
}

fn end_of(
    punishment: &ModerationAction,
    next: Option<&ModerationAction>,
    now: DateTime<Utc>,
) -> Option<PunishmentEnd> {
    let ends_at = punishment.ends_at();
    let expired = |at: DateTime<Utc>| PunishmentEnd {
        at,
        reason: EndReason::Expired,
        moderator_id: String::new(),
        moderator_login: String::new(),
    };

    match next {
        Some(next) => {
            // The timeout ran out before anything else happened
            if let Some(ends_at) = ends_at {
                if ends_at <= next.timestamp {
                    return Some(expired(ends_at));
                }
            }

            let reason = if next.kind == ModerationKind::Unban {
                EndReason::Unban
            } else {
                EndReason::Superseded
            };
            Some(PunishmentEnd {
                at: next.timestamp,
                reason,
                moderator_id: next.moderator_id.clone(),
                moderator_login: next.moderator_login.clone(),
            })
        }
        None => ends_at.filter(|ends_at| *ends_at <= now).map(expired),
    }
}

/// The channels (id and login) in which a ban or a timeout is going on, in the order of their
/// ids. `entries` has to be in chronological order, as `history` returns it.
pub fn channels_with_active_punishment(entries: &[HistoryEntry]) -> Vec<(String, String)> {
    // The latest ban or timeout of every channel decides
    let mut latest: HashMap<&str, (&str, bool)> = HashMap::new();
    for entry in entries {
        if entry.action.kind.is_punishment() {
            latest.insert(
                entry.action.channel_id.as_str(),
                (entry.action.channel_login.as_str(), entry.end.is_none()),
            );
        }
    }

    let mut active: Vec<(String, String)> = latest
        .into_iter()
        .filter(|(_, (_, active))| *active)
        .map(|(channel_id, (channel_login, _))| (channel_id.to_owned(), channel_login.to_owned()))
        .collect();
    active.sort();
    active
}

/// How many of the entries are of this kind
pub fn count_kind(entries: &[HistoryEntry], kind: ModerationKind) -> u64 {
    entries
        .iter()
        .filter(|entry| entry.action.kind == kind)
        .count() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn at(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp_millis(seconds * 1000).unwrap()
    }

    fn action(channel: &str, seconds: i64, kind: ModerationKind) -> ModerationAction {
        ModerationAction {
            channel_id: channel.to_owned(),
            channel_login: format!("channel-{channel}"),
            timestamp: at(seconds),
            kind,
            user_id: "10".to_owned(),
            user_login: "target".to_owned(),
            moderator_id: String::new(),
            moderator_login: String::new(),
            duration_seconds: None,
            expires_at: None,
            permanent: false,
            message_id: String::new(),
            text: String::new(),
            ai_moderated: false,
            violated_rules: vec![],
        }
    }

    /// A timeout like Kick reports it, with an expiry
    fn timeout(channel: &str, seconds: i64, duration_seconds: u64) -> ModerationAction {
        ModerationAction {
            duration_seconds: Some(duration_seconds),
            expires_at: Some(at(seconds + duration_seconds as i64)),
            ..action(channel, seconds, ModerationKind::Timeout)
        }
    }

    fn by(action: ModerationAction, id: &str, login: &str) -> ModerationAction {
        ModerationAction {
            moderator_id: id.to_owned(),
            moderator_login: login.to_owned(),
            ..action
        }
    }

    fn end(at: DateTime<Utc>, reason: EndReason, id: &str, login: &str) -> Option<PunishmentEnd> {
        Some(PunishmentEnd {
            at,
            reason,
            moderator_id: id.to_owned(),
            moderator_login: login.to_owned(),
        })
    }

    #[test]
    fn a_timeout_which_ran_out_has_expired() {
        let entries = history(vec![timeout("1", 100, 120)], at(1000));

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].end, end(at(220), EndReason::Expired, "", ""));
    }

    #[test]
    fn a_timeout_which_is_still_running_has_no_end() {
        let entries = history(vec![timeout("1", 100, 120)], at(150));

        assert_eq!(entries[0].end, None);
    }

    #[test]
    fn the_expiry_is_computed_from_the_length_if_kick_sent_none() {
        let timeout = ModerationAction {
            duration_seconds: Some(60),
            ..action("1", 100, ModerationKind::Timeout)
        };

        let entries = history(vec![timeout], at(1000));

        assert_eq!(entries[0].end, end(at(160), EndReason::Expired, "", ""));
    }

    #[test]
    fn a_timeout_without_a_length_does_not_run_out() {
        let entries = history(vec![action("1", 100, ModerationKind::Timeout)], at(100_000));

        assert_eq!(entries[0].end, None);
    }

    #[test]
    fn an_unban_ends_a_timeout_early() {
        let entries = history(
            vec![
                by(timeout("1", 100, 120), "20", "mod"),
                by(action("1", 160, ModerationKind::Unban), "7", "unbanner"),
            ],
            at(1000),
        );

        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries[0].end,
            end(at(160), EndReason::Unban, "7", "unbanner")
        );
        // the unban itself has no end
        assert_eq!(entries[1].end, None);
    }

    #[test]
    fn an_unban_ends_a_ban() {
        let entries = history(
            vec![
                by(action("1", 100, ModerationKind::Ban), "20", "mod"),
                by(action("1", 5000, ModerationKind::Unban), "21", "other"),
            ],
            at(10_000),
        );

        assert_eq!(entries[0].end, end(at(5000), EndReason::Unban, "21", "other"));
    }

    #[test]
    fn a_ban_nothing_ended_is_still_going_on() {
        let entries = history(vec![action("1", 100, ModerationKind::Ban)], at(10_000_000));

        assert_eq!(entries[0].end, None);
    }

    #[test]
    fn a_newer_punishment_takes_the_place_of_the_old_one() {
        let entries = history(
            vec![
                by(action("1", 100, ModerationKind::Ban), "20", "first"),
                by(action("1", 200, ModerationKind::Ban), "21", "second"),
            ],
            at(300),
        );

        assert_eq!(
            entries[0].end,
            end(at(200), EndReason::Superseded, "21", "second")
        );
        assert_eq!(entries[1].end, None);
    }

    #[test]
    fn a_timeout_which_ran_out_is_not_superseded_by_a_later_one() {
        // the first one is over at 160, long before the second one is given at 500
        let entries = history(vec![timeout("1", 100, 60), timeout("1", 500, 60)], at(1000));

        assert_eq!(entries[0].end, end(at(160), EndReason::Expired, "", ""));
        assert_eq!(entries[1].end, end(at(560), EndReason::Expired, "", ""));
    }

    #[test]
    fn a_timeout_which_is_extended_is_superseded() {
        // given again after 60 of 120 seconds
        let entries = history(
            vec![
                by(timeout("1", 100, 120), "20", "first"),
                by(timeout("1", 160, 120), "21", "second"),
            ],
            at(200),
        );

        assert_eq!(
            entries[0].end,
            end(at(160), EndReason::Superseded, "21", "second")
        );
        assert_eq!(entries[1].end, None);
    }

    #[test]
    fn channels_do_not_affect_each_other() {
        let entries = history(
            vec![
                action("1", 100, ModerationKind::Ban),
                action("2", 200, ModerationKind::Unban),
            ],
            at(1000),
        );

        // the unban was in another channel
        assert_eq!(entries[0].action.channel_id, "1");
        assert_eq!(entries[0].end, None);
    }

    #[test]
    fn an_unban_without_a_ban_stays_in_the_history() {
        let entries = history(vec![action("1", 100, ModerationKind::Unban)], at(1000));

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].end, None);
    }

    #[test]
    fn the_history_is_in_chronological_order() {
        let entries = history(
            vec![
                by(action("1", 160, ModerationKind::Unban), "7", "unbanner"),
                timeout("1", 100, 120),
            ],
            at(1000),
        );

        let kinds: Vec<ModerationKind> = entries.iter().map(|entry| entry.action.kind).collect();
        assert_eq!(kinds, vec![ModerationKind::Timeout, ModerationKind::Unban]);
        // and the unban was matched with the timeout in spite of the order they came in
        assert_eq!(
            entries[0].end,
            end(at(160), EndReason::Unban, "7", "unbanner")
        );
    }

    #[test]
    fn deletions_and_clears_do_not_end_punishments() {
        let entries = history(
            vec![
                action("1", 100, ModerationKind::Ban),
                action("1", 200, ModerationKind::Delete),
                action("1", 300, ModerationKind::Clear),
            ],
            at(1000),
        );

        assert_eq!(entries[0].end, None);
        assert_eq!(entries[1].end, None);
        assert_eq!(entries[2].end, None);
    }

    #[test]
    fn channels_with_an_ongoing_punishment() {
        let entries = history(
            vec![
                // still banned
                action("1", 100, ModerationKind::Ban),
                // the timeout has run out
                timeout("2", 100, 60),
                // banned and unbanned
                action("3", 100, ModerationKind::Ban),
                action("3", 200, ModerationKind::Unban),
                // unbanned and banned again
                action("4", 100, ModerationKind::Ban),
                action("4", 200, ModerationKind::Unban),
                action("4", 300, ModerationKind::Ban),
            ],
            at(1000),
        );

        assert_eq!(
            channels_with_active_punishment(&entries),
            vec![
                ("1".to_owned(), "channel-1".to_owned()),
                ("4".to_owned(), "channel-4".to_owned()),
            ]
        );
    }

    #[test]
    fn kinds_are_counted() {
        let entries = history(
            vec![
                action("1", 100, ModerationKind::Ban),
                timeout("1", 200, 60),
                timeout("2", 300, 60),
                action("1", 400, ModerationKind::Unban),
            ],
            at(1000),
        );

        assert_eq!(count_kind(&entries, ModerationKind::Ban), 1);
        assert_eq!(count_kind(&entries, ModerationKind::Timeout), 2);
        assert_eq!(count_kind(&entries, ModerationKind::Unban), 1);
        assert_eq!(count_kind(&entries, ModerationKind::Delete), 0);
    }

    #[test]
    fn kinds_have_database_names() {
        for kind in [
            ModerationKind::Ban,
            ModerationKind::Timeout,
            ModerationKind::Unban,
            ModerationKind::Delete,
            ModerationKind::Clear,
        ] {
            assert_eq!(ModerationKind::from_db(kind.as_str()), Some(kind));
        }
        assert_eq!(ModerationKind::from_db("nonsense"), None);

        assert!(ModerationKind::Ban.is_punishment());
        assert!(ModerationKind::Timeout.is_punishment());
        assert!(!ModerationKind::Unban.is_punishment());
    }

    #[test]
    fn kinds_use_lowercase_names_in_json() {
        assert_eq!(
            serde_json::to_string(&ModerationKind::Timeout).unwrap(),
            r#""timeout""#
        );
        assert_eq!(
            serde_json::from_str::<ModerationKind>(r#""unban""#).unwrap(),
            ModerationKind::Unban
        );
        assert_eq!(
            serde_json::to_string(&EndReason::Superseded).unwrap(),
            r#""superseded""#
        );
    }
}
