//! Queries for the statistics endpoints: leaderboards, activity over time and user summaries.
//!
//! Most of them read the rollup tables `message_counts_daily` (per channel, user and day) and
//! `user_channel_stats` (per user and channel), see `migrations/rollups.rs`. Both only count
//! chat messages. Reading a rollup is much cheaper than aggregating the messages themselves.
//!
//! The rollups also contain the messages of users and channels which opted out, the logs
//! themselves are not deleted either. They are hidden when the logs are read, and these queries
//! leave them out as well: the queries which can return other users or channels than the one
//! asked about take the ids to exclude (`excluded_users`, `excluded_channels`).

use super::schema::{MessageType, StructuredMessage};
use crate::{
    error::Error,
    logs::period::{DateRange, Interval},
    web::schema::UserChannelsSort,
    Result,
};
use chrono::{DateTime, NaiveDate, Utc};
use clickhouse::{query::Query, Client, Row};
use serde::Deserialize;

/// `PRIVMSG`
const CHAT_MESSAGE_TYPE: u8 = MessageType::PrivMsg as u8;
/// Results are reused for a few minutes
const CACHE_SETTINGS: &str = "SETTINGS use_query_cache = 1, query_cache_ttl = 300";
/// Queries which list more ids than this are not cached. The client sends long queries in a way
/// in which the server does not allow changing settings (`readonly`), so a `SETTINGS` clause
/// would make them fail. A list this long only happens with very many opt outs.
const MAX_EXCLUDED_IDS_FOR_CACHE: usize = 200;

/// The `SETTINGS` clause which lets the database reuse results for a few minutes, or nothing
/// if the query is too long for it, see `MAX_EXCLUDED_IDS_FOR_CACHE`
pub(super) fn cache_settings(excluded_ids: &[String]) -> &'static str {
    if excluded_ids.len() <= MAX_EXCLUDED_IDS_FOR_CACHE {
        CACHE_SETTINGS
    } else {
        ""
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatterCount {
    pub user_id: String,
    pub message_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelCount {
    pub channel_id: String,
    pub message_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityBucket {
    /// The first day of the bucket
    pub start: NaiveDate,
    pub message_count: u64,
    /// Only known for the activity of a whole channel
    pub unique_chatters: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserRank {
    pub message_count: u64,
    /// Users with the same number of messages share their rank, `None` without messages
    pub rank: Option<u64>,
    pub total_chatters: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserChannelStats {
    pub channel_id: String,
    pub message_count: u64,
    pub first_message: DateTime<Utc>,
    pub last_message: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserSummary {
    /// In how many channels the user chatted
    pub channel_count: u64,
    pub message_count: u64,
    pub first_message: DateTime<Utc>,
    pub last_message: DateTime<Utc>,
    /// The channel of the most recent message
    pub last_channel_id: String,
}

/// The chatters of a channel with the most messages in a range of days,
/// together with the total number of messages in that range
pub async fn get_top_chatters(
    db: &Client,
    channel_id: &str,
    range: DateRange,
    limit: u64,
    excluded_users: &[String],
) -> Result<(u64, Vec<ChatterCount>)> {
    #[derive(Deserialize, Row)]
    struct ChatterRow {
        user_id: String,
        cnt: u64,
    }

    let exclusion = exclusion_clause("user_id", excluded_users);
    let cache = cache_settings(excluded_users);

    let total_query = format!("SELECT sum(message_count) FROM message_counts_daily WHERE channel_id = ? AND date >= toDate(?) AND date < toDate(?){exclusion} {cache}");
    let total = bind_excluded(
        db.query(&total_query)
            .bind(channel_id)
            .bind(range.start.to_string())
            .bind(range.end.to_string()),
        excluded_users,
    )
    .fetch_one::<u64>()
    .await?;

    let query = format!("SELECT user_id, sum(message_count) AS cnt FROM message_counts_daily WHERE channel_id = ? AND date >= toDate(?) AND date < toDate(?){exclusion} GROUP BY user_id ORDER BY cnt DESC, user_id ASC LIMIT ? {cache}");
    let rows = bind_excluded(
        db.query(&query)
            .bind(channel_id)
            .bind(range.start.to_string())
            .bind(range.end.to_string()),
        excluded_users,
    )
    .bind(limit)
    .fetch_all::<ChatterRow>()
    .await?;

    let chatters = rows
        .into_iter()
        .map(|row| ChatterCount {
            user_id: row.user_id,
            message_count: row.cnt,
        })
        .collect();

    Ok((total, chatters))
}

/// The channels with the most messages in a range of days
pub async fn get_top_channels(
    db: &Client,
    range: DateRange,
    limit: u64,
    excluded_channels: &[String],
) -> Result<Vec<ChannelCount>> {
    #[derive(Deserialize, Row)]
    struct ChannelRow {
        channel_id: String,
        cnt: u64,
    }

    let exclusion = exclusion_clause("channel_id", excluded_channels);
    let cache = cache_settings(excluded_channels);
    let query = format!("SELECT channel_id, sum(message_count) AS cnt FROM message_counts_daily WHERE date >= toDate(?) AND date < toDate(?){exclusion} GROUP BY channel_id ORDER BY cnt DESC, channel_id ASC LIMIT ? {cache}");
    let rows = bind_excluded(
        db.query(&query)
            .bind(range.start.to_string())
            .bind(range.end.to_string()),
        excluded_channels,
    )
    .bind(limit)
    .fetch_all::<ChannelRow>()
    .await?;

    Ok(rows
        .into_iter()
        .map(|row| ChannelCount {
            channel_id: row.channel_id,
            message_count: row.cnt,
        })
        .collect())
}

/// Messages and different chatters of a channel per interval
pub async fn get_channel_activity(
    db: &Client,
    channel_id: &str,
    interval: Interval,
    range: DateRange,
    excluded_users: &[String],
) -> Result<Vec<ActivityBucket>> {
    #[derive(Deserialize, Row)]
    struct BucketRow {
        bucket: String,
        messages: u64,
        chatters: u64,
    }

    let bucket = bucket_expression(interval, "date");
    let exclusion = exclusion_clause("user_id", excluded_users);
    let cache = cache_settings(excluded_users);
    let query = format!("SELECT toString({bucket}) AS bucket, sum(message_count) AS messages, uniqExact(user_id) AS chatters FROM message_counts_daily WHERE channel_id = ? AND date >= toDate(?) AND date < toDate(?){exclusion} GROUP BY bucket ORDER BY bucket ASC {cache}");
    let rows = bind_excluded(
        db.query(&query)
            .bind(channel_id)
            .bind(range.start.to_string())
            .bind(range.end.to_string()),
        excluded_users,
    )
    .fetch_all::<BucketRow>()
    .await?;

    rows.into_iter()
        .map(|row| {
            Ok(ActivityBucket {
                start: parse_bucket(&row.bucket)?,
                message_count: row.messages,
                unique_chatters: Some(row.chatters),
            })
        })
        .collect()
}

/// Messages of a user in a channel per interval
pub async fn get_user_activity(
    db: &Client,
    channel_id: &str,
    user_id: &str,
    interval: Interval,
    range: DateRange,
) -> Result<Vec<ActivityBucket>> {
    #[derive(Deserialize, Row)]
    struct BucketRow {
        bucket: String,
        messages: u64,
    }

    // The day in UTC, like the days of the rollup `message_counts_daily` are
    let bucket = bucket_expression(interval, "toDate(toTimeZone(timestamp, 'UTC'))");
    let query = format!("SELECT toString({bucket}) AS bucket, count() AS messages FROM message_structured WHERE channel_id = ? AND user_id = ? AND message_type = {CHAT_MESSAGE_TYPE} AND timestamp >= ? AND timestamp < ? GROUP BY bucket ORDER BY bucket ASC");
    let rows = db
        .query(&query)
        .bind(channel_id)
        .bind(user_id)
        .bind(range.start_time().timestamp_millis() as f64 / 1000.0)
        .bind(range.end_time().timestamp_millis() as f64 / 1000.0)
        .fetch_all::<BucketRow>()
        .await?;

    rows.into_iter()
        .map(|row| {
            Ok(ActivityBucket {
                start: parse_bucket(&row.bucket)?,
                message_count: row.messages,
                unique_chatters: None,
            })
        })
        .collect()
}

/// The number of messages of a user in a channel in a range of days, and where the user ranks
/// among the chatters of the channel
pub async fn get_user_rank(
    db: &Client,
    channel_id: &str,
    user_id: &str,
    range: DateRange,
    excluded_users: &[String],
) -> Result<UserRank> {
    let message_count = db
        .query("SELECT sum(message_count) FROM message_counts_daily WHERE channel_id = ? AND user_id = ? AND date >= toDate(?) AND date < toDate(?)")
        .bind(channel_id)
        .bind(user_id)
        .bind(range.start.to_string())
        .bind(range.end.to_string())
        .fetch_one::<u64>()
        .await?;

    let exclusion = exclusion_clause("user_id", excluded_users);

    let chatters_query = format!("SELECT uniqExact(user_id) FROM message_counts_daily WHERE channel_id = ? AND date >= toDate(?) AND date < toDate(?){exclusion}");
    let total_chatters = bind_excluded(
        db.query(&chatters_query)
            .bind(channel_id)
            .bind(range.start.to_string())
            .bind(range.end.to_string()),
        excluded_users,
    )
    .fetch_one::<u64>()
    .await?;

    let rank = if message_count == 0 {
        None
    } else {
        let ahead_query = format!("SELECT count() FROM (SELECT user_id FROM message_counts_daily WHERE channel_id = ? AND date >= toDate(?) AND date < toDate(?){exclusion} GROUP BY user_id HAVING sum(message_count) > ?)");
        let ahead = bind_excluded(
            db.query(&ahead_query)
                .bind(channel_id)
                .bind(range.start.to_string())
                .bind(range.end.to_string()),
            excluded_users,
        )
        .bind(message_count)
        .fetch_one::<u64>()
        .await?;

        Some(ahead + 1)
    };

    Ok(UserRank {
        message_count,
        rank,
        total_chatters,
    })
}

/// The channels a user has chatted in, with the number of messages and the time of the first
/// and last message
pub async fn get_user_channels(
    db: &Client,
    user_id: &str,
    sort: UserChannelsSort,
    limit: u64,
    offset: u64,
    excluded_channels: &[String],
) -> Result<Vec<UserChannelStats>> {
    #[derive(Deserialize, Row)]
    struct ChannelRow {
        channel_id: String,
        cnt: u64,
        first_ms: i64,
        last_ms: i64,
    }

    let order_by = match sort {
        UserChannelsSort::Last => "last_ms",
        UserChannelsSort::Count => "cnt",
    };
    let exclusion = exclusion_clause("channel_id", excluded_channels);
    let query = format!("SELECT channel_id, sum(message_count) AS cnt, toUnixTimestamp64Milli(min(first_timestamp)) AS first_ms, toUnixTimestamp64Milli(max(last_timestamp)) AS last_ms FROM user_channel_stats WHERE user_id = ?{exclusion} GROUP BY channel_id ORDER BY {order_by} DESC, channel_id ASC LIMIT ? OFFSET ?");
    let rows = bind_excluded(db.query(&query).bind(user_id), excluded_channels)
        .bind(limit)
        .bind(offset)
        .fetch_all::<ChannelRow>()
        .await?;

    rows.into_iter()
        .map(|row| {
            Ok(UserChannelStats {
                channel_id: row.channel_id,
                message_count: row.cnt,
                first_message: DateTime::from_timestamp_millis(row.first_ms)
                    .ok_or(Error::Internal)?,
                last_message: DateTime::from_timestamp_millis(row.last_ms)
                    .ok_or(Error::Internal)?,
            })
        })
        .collect()
}

/// Totals over all the channels a user has chatted in, `None` if there are none
pub async fn get_user_summary(
    db: &Client,
    user_id: &str,
    excluded_channels: &[String],
) -> Result<Option<UserSummary>> {
    // The names differ from those of the table on purpose, an alias which is also the name of
    // a column would be used in place of the column inside of the aggregate functions
    #[derive(Deserialize, Row)]
    struct SummaryRow {
        channels: u64,
        messages: u64,
        first_message_ms: i64,
        last_message_ms: i64,
        last_channel: String,
    }

    let exclusion = exclusion_clause("channel_id", excluded_channels);
    let query = format!("SELECT count() AS channels, sum(cnt) AS messages, min(first_ms) AS first_message_ms, max(last_ms) AS last_message_ms, argMax(channel_id, last_ms) AS last_channel FROM (SELECT channel_id, sum(message_count) AS cnt, toUnixTimestamp64Milli(min(first_timestamp)) AS first_ms, toUnixTimestamp64Milli(max(last_timestamp)) AS last_ms FROM user_channel_stats WHERE user_id = ?{exclusion} GROUP BY channel_id)");
    let row = bind_excluded(db.query(&query).bind(user_id), excluded_channels)
        .fetch_one::<SummaryRow>()
        .await?;

    // Without any channel the aggregates are empty values
    if row.channels == 0 {
        return Ok(None);
    }

    Ok(Some(UserSummary {
        channel_count: row.channels,
        message_count: row.messages,
        first_message: DateTime::from_timestamp_millis(row.first_message_ms)
            .ok_or(Error::Internal)?,
        last_message: DateTime::from_timestamp_millis(row.last_message_ms)
            .ok_or(Error::Internal)?,
        last_channel_id: row.last_channel,
    }))
}

/// The first or the most recent chat message of a user in a channel
pub async fn read_user_edge_line(
    db: &Client,
    channel_id: &str,
    user_id: &str,
    newest: bool,
) -> Result<StructuredMessage<'static>> {
    let direction = if newest { "DESC" } else { "ASC" };
    let query = format!("SELECT * FROM message_structured WHERE channel_id = ? AND user_id = ? AND message_type = {CHAT_MESSAGE_TYPE} ORDER BY timestamp {direction} LIMIT 1");

    let msg = db
        .query(&query)
        .bind(channel_id)
        .bind(user_id)
        .fetch_optional::<StructuredMessage>()
        .await?
        .ok_or(Error::NotFound)?;

    Ok(msg)
}

/// SQL condition which leaves out the rows of the given ids, to be put at the end of a `WHERE`
/// clause. There is nothing to leave out most of the time, then the condition is not added.
/// If it is added, `bind_excluded` has to bind the ids where the `?`s are. The condition is an
/// `IN` list, which the database turns into a hash set (comparing every row with every id
/// of an array is much slower with a long list).
pub(super) fn exclusion_clause(column: &str, excluded_ids: &[String]) -> String {
    if excluded_ids.is_empty() {
        String::new()
    } else {
        let placeholders = vec!["?"; excluded_ids.len()].join(", ");
        format!(" AND {column} NOT IN ({placeholders})")
    }
}

/// Binds the ids of an `exclusion`, if there is one
pub(super) fn bind_excluded(mut query: Query, excluded_ids: &[String]) -> Query {
    for id in excluded_ids {
        query = query.bind(id.as_str());
    }
    query
}

/// SQL expression which maps a date expression to the first day of its interval
fn bucket_expression(interval: Interval, date: &str) -> String {
    match interval {
        Interval::Day => date.to_owned(),
        // 1: weeks start on Monday
        Interval::Week => format!("toStartOfWeek({date}, 1)"),
        Interval::Month => format!("toStartOfMonth({date})"),
        Interval::Year => format!("toStartOfYear({date})"),
    }
}

fn parse_bucket(bucket: &str) -> Result<NaiveDate> {
    NaiveDate::parse_from_str(bucket, "%Y-%m-%d").map_err(|_| Error::Internal)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn bucket_expressions() {
        assert_eq!(bucket_expression(Interval::Day, "date"), "date");
        assert_eq!(
            bucket_expression(Interval::Week, "date"),
            "toStartOfWeek(date, 1)"
        );
        assert_eq!(
            bucket_expression(Interval::Month, "toDate(timestamp)"),
            "toStartOfMonth(toDate(timestamp))"
        );
        assert_eq!(
            bucket_expression(Interval::Year, "date"),
            "toStartOfYear(date)"
        );
    }

    #[test]
    fn activity_of_a_user_is_grouped_by_the_day_in_utc() {
        // The expression `get_user_activity` passes, which has to agree with the days of the
        // rollup (see `daily_counts_select`)
        let day = "toDate(toTimeZone(timestamp, 'UTC'))";
        assert_eq!(
            bucket_expression(Interval::Week, day),
            "toStartOfWeek(toDate(toTimeZone(timestamp, 'UTC')), 1)"
        );
    }

    #[test]
    fn parses_bucket_dates() {
        assert_eq!(
            parse_bucket("2026-09-28").unwrap(),
            NaiveDate::from_ymd_opt(2026, 9, 28).unwrap()
        );
        assert!(parse_bucket("garbage").is_err());
    }

    #[test]
    fn chat_message_type_is_privmsg() {
        assert_eq!(CHAT_MESSAGE_TYPE, 1);
    }

    #[test]
    fn exclusions_are_only_added_when_there_is_something_to_exclude() {
        assert_eq!(exclusion_clause("user_id", &[]), "");
        assert_eq!(
            exclusion_clause("user_id", &["1".to_owned()]),
            " AND user_id NOT IN (?)"
        );
        assert_eq!(
            exclusion_clause("channel_id", &["1".to_owned(), "2".to_owned()]),
            " AND channel_id NOT IN (?, ?)"
        );
    }

    #[test]
    fn long_lists_of_ids_are_not_cached() {
        let few: Vec<String> = (0..MAX_EXCLUDED_IDS_FOR_CACHE).map(|id| id.to_string()).collect();
        let many: Vec<String> = (0..=MAX_EXCLUDED_IDS_FOR_CACHE).map(|id| id.to_string()).collect();

        assert_eq!(cache_settings(&[]), CACHE_SETTINGS);
        assert_eq!(cache_settings(&few), CACHE_SETTINGS);
        assert_eq!(cache_settings(&many), "");
    }
}
