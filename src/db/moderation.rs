//! Queries for the moderation endpoints: bans, timeouts, unbans and deleted messages.
//!
//! They read the table `moderation_actions` (see `migrations/moderation.rs`), which has a row
//! for every moderation action of every channel. Like for the statistics, the actions of users
//! and channels which opted out are left out where the queries can return more than the user
//! or channel which was asked about.

use super::analytics::{bind_excluded, cache_settings, exclusion_clause};
use crate::{
    logs::{
        moderation::{ModerationAction, ModerationKind},
        period::DateRange,
    },
    Result,
};
use chrono::{DateTime, Utc};
use clickhouse::{Client, Row};
use serde::Deserialize;

/// What is selected of an action. Times are read as milliseconds, the names of these two
/// differ from those of the columns on purpose (an alias that has the name of a column would
/// be used in place of the column in the conditions).
const ACTION_COLUMNS: &str = "channel_id, channel_login, toUnixTimestamp64Milli(timestamp) AS timestamp_ms, action, target_user_id, target_user_login, moderator_id, moderator_login, duration_seconds, toUnixTimestamp64Milli(expires_at) AS expires_ms, permanent, message_id, deleted_text, ai_moderated, violated_rules";

#[derive(Deserialize, Row)]
struct ActionRow {
    channel_id: String,
    channel_login: String,
    timestamp_ms: i64,
    action: String,
    target_user_id: String,
    target_user_login: String,
    moderator_id: String,
    moderator_login: String,
    duration_seconds: u64,
    expires_ms: i64,
    permanent: u8,
    message_id: String,
    deleted_text: String,
    ai_moderated: u8,
    violated_rules: String,
}

/// Rows which cannot be understood are left out, there should not be any
fn into_action(row: ActionRow) -> Option<ModerationAction> {
    Some(ModerationAction {
        kind: ModerationKind::from_db(&row.action)?,
        timestamp: DateTime::from_timestamp_millis(row.timestamp_ms)?,
        // No expiry is stored as the start of 1970
        expires_at: (row.expires_ms > 0)
            .then(|| DateTime::from_timestamp_millis(row.expires_ms))
            .flatten(),
        duration_seconds: (row.duration_seconds > 0).then_some(row.duration_seconds),
        permanent: row.permanent != 0,
        ai_moderated: row.ai_moderated != 0,
        violated_rules: row
            .violated_rules
            .split(',')
            .filter(|rule| !rule.is_empty())
            .map(str::to_owned)
            .collect(),
        channel_id: row.channel_id,
        channel_login: row.channel_login,
        user_id: row.target_user_id,
        user_login: row.target_user_login,
        moderator_id: row.moderator_id,
        moderator_login: row.moderator_login,
        message_id: row.message_id,
        text: row.deleted_text,
    })
}

/// The bans, timeouts and unbans of a user, the newest first. Only the channel `channel_id` is
/// looked at if there is one. Not more than `limit` are returned.
pub async fn get_user_punishment_actions(
    db: &Client,
    user_id: &str,
    channel_id: Option<&str>,
    limit: u64,
    excluded_channels: &[String],
) -> Result<Vec<ModerationAction>> {
    let channel_condition = if channel_id.is_some() {
        " AND channel_id = ?"
    } else {
        ""
    };
    let exclusion = exclusion_clause("channel_id", excluded_channels);
    let sql = format!("SELECT {ACTION_COLUMNS} FROM moderation_actions WHERE target_user_id = ? AND action IN ('ban', 'timeout', 'unban'){channel_condition}{exclusion} ORDER BY timestamp DESC LIMIT ?");

    let mut query = db.query(&sql).bind(user_id);
    if let Some(channel_id) = channel_id {
        query = query.bind(channel_id);
    }
    let rows = bind_excluded(query, excluded_channels)
        .bind(limit)
        .fetch_all::<ActionRow>()
        .await?;

    Ok(rows.into_iter().filter_map(into_action).collect())
}

/// Which moderation actions of a channel to list
pub struct ChannelActionsFilter<'a> {
    /// Only this kind of action
    pub kind: Option<ModerationKind>,
    /// Only actions of this moderator
    pub moderator_id: Option<&'a str>,
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
}

/// The moderation actions of a channel, the newest first. Actions about users who opted out are
/// left out.
pub async fn get_channel_actions(
    db: &Client,
    channel_id: &str,
    filter: &ChannelActionsFilter<'_>,
    limit: u64,
    offset: u64,
    excluded_users: &[String],
) -> Result<Vec<ModerationAction>> {
    let kind_condition = if filter.kind.is_some() {
        " AND action = ?"
    } else {
        ""
    };
    let moderator_condition = if filter.moderator_id.is_some() {
        " AND moderator_id = ?"
    } else {
        ""
    };
    let exclusion = exclusion_clause("target_user_id", excluded_users);
    let sql = format!("SELECT {ACTION_COLUMNS} FROM moderation_actions WHERE channel_id = ? AND timestamp >= ? AND timestamp < ?{kind_condition}{moderator_condition}{exclusion} ORDER BY timestamp DESC LIMIT ? OFFSET ?");

    let mut query = db
        .query(&sql)
        .bind(channel_id)
        .bind(seconds(filter.from))
        .bind(seconds(filter.to));
    if let Some(kind) = filter.kind {
        query = query.bind(kind.as_str());
    }
    if let Some(moderator_id) = filter.moderator_id {
        query = query.bind(moderator_id);
    }
    let rows = bind_excluded(query, excluded_users)
        .bind(limit)
        .bind(offset)
        .fetch_all::<ActionRow>()
        .await?;

    Ok(rows.into_iter().filter_map(into_action).collect())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModeratorStats {
    pub moderator_id: String,
    /// The login the moderator had at the time of their latest action
    pub moderator_login: String,
    pub bans: u64,
    pub timeouts: u64,
    pub unbans: u64,
}

/// The moderators of a channel with the most bans, timeouts and unbans in a range of days.
/// Deleted messages are not counted, Kick does not say who deletes them.
pub async fn get_moderator_stats(
    db: &Client,
    channel_id: &str,
    range: DateRange,
    limit: u64,
    excluded_moderators: &[String],
) -> Result<Vec<ModeratorStats>> {
    #[derive(Deserialize, Row)]
    struct StatsRow {
        moderator_id: String,
        latest_login: String,
        bans: u64,
        timeouts: u64,
        unbans: u64,
    }

    let exclusion = exclusion_clause("moderator_id", excluded_moderators);
    let cache = cache_settings(excluded_moderators);
    let sql = format!("SELECT moderator_id, argMax(moderator_login, timestamp) AS latest_login, countIf(action = 'ban') AS bans, countIf(action = 'timeout') AS timeouts, countIf(action = 'unban') AS unbans FROM moderation_actions WHERE channel_id = ? AND timestamp >= ? AND timestamp < ? AND moderator_id != ''{exclusion} GROUP BY moderator_id ORDER BY bans + timeouts + unbans DESC, moderator_id ASC LIMIT ? {cache}");

    let rows = bind_excluded(
        db.query(&sql)
            .bind(channel_id)
            .bind(seconds(range.start_time()))
            .bind(seconds(range.end_time())),
        excluded_moderators,
    )
    .bind(limit)
    .fetch_all::<StatsRow>()
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| ModeratorStats {
            moderator_id: row.moderator_id,
            moderator_login: row.latest_login,
            bans: row.bans,
            timeouts: row.timeouts,
            unbans: row.unbans,
        })
        .collect())
}

/// A time as the seconds since the epoch, which is how the timestamp column is compared to
fn seconds(time: DateTime<Utc>) -> f64 {
    time.timestamp_millis() as f64 / 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn row() -> ActionRow {
        ActionRow {
            channel_id: "676".to_owned(),
            channel_login: "xqc".to_owned(),
            timestamp_ms: 1_790_923_384_000,
            action: "timeout".to_owned(),
            target_user_id: "10".to_owned(),
            target_user_login: "target".to_owned(),
            moderator_id: "20".to_owned(),
            moderator_login: "mod".to_owned(),
            duration_seconds: 120,
            expires_ms: 1_790_923_504_000,
            permanent: 0,
            message_id: String::new(),
            deleted_text: String::new(),
            ai_moderated: 0,
            violated_rules: String::new(),
        }
    }

    #[test]
    fn rows_become_actions() {
        let action = into_action(row()).unwrap();

        assert_eq!(action.kind, ModerationKind::Timeout);
        assert_eq!(action.channel_id, "676");
        assert_eq!(action.channel_login, "xqc");
        assert_eq!(action.timestamp.timestamp_millis(), 1_790_923_384_000);
        assert_eq!(action.user_id, "10");
        assert_eq!(action.user_login, "target");
        assert_eq!(action.moderator_id, "20");
        assert_eq!(action.moderator_login, "mod");
        assert_eq!(action.duration_seconds, Some(120));
        assert_eq!(
            action.expires_at.map(|expires_at| expires_at.timestamp_millis()),
            Some(1_790_923_504_000)
        );
        assert!(!action.permanent);
        assert!(!action.ai_moderated);
        assert!(action.violated_rules.is_empty());
    }

    #[test]
    fn empty_values_are_absent() {
        let action = into_action(ActionRow {
            action: "ban".to_owned(),
            duration_seconds: 0,
            expires_ms: 0,
            permanent: 1,
            ..row()
        })
        .unwrap();

        assert_eq!(action.kind, ModerationKind::Ban);
        assert_eq!(action.duration_seconds, None);
        assert_eq!(action.expires_at, None);
        assert!(action.permanent);
    }

    #[test]
    fn deleted_messages_carry_the_ai_moderation_details() {
        let action = into_action(ActionRow {
            action: "delete".to_owned(),
            moderator_id: String::new(),
            moderator_login: String::new(),
            duration_seconds: 0,
            expires_ms: 0,
            message_id: "c52ca12d-3cd1-4471-80ed-2cf73bac96a1".to_owned(),
            deleted_text: "hello".to_owned(),
            ai_moderated: 1,
            violated_rules: "sexual,harassment,".to_owned(),
            ..row()
        })
        .unwrap();

        assert_eq!(action.kind, ModerationKind::Delete);
        assert_eq!(action.text, "hello");
        assert!(action.ai_moderated);
        assert_eq!(action.violated_rules, vec!["sexual", "harassment"]);
    }

    #[test]
    fn rows_of_an_unknown_kind_are_left_out() {
        let action = into_action(ActionRow {
            action: "something-new".to_owned(),
            ..row()
        });

        assert_eq!(action, None);
    }

    #[test]
    fn times_are_compared_in_seconds() {
        let time = DateTime::from_timestamp_millis(1_500).unwrap();
        assert_eq!(seconds(time), 1.5);
    }
}
