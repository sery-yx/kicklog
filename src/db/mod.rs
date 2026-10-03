pub mod analytics;
mod migrations;
pub mod moderation;
pub mod schema;
pub mod writer;
use std::collections::{HashMap, HashSet};

pub use migrations::run as setup_db;
use serde::{Deserialize, Serialize};
use writer::FlushBuffer;

use crate::{
    error::Error,
    logs::{
        schema::LogRangeParams,
        stream::{FlushBufferResponse, LogsStream},
    },
    web::schema::{AvailableLogDate, LogsParams, PreviousName, UserHasLogs, UserLogsStats},
    Result,
};
use chrono::{DateTime, Datelike, Duration, Utc};
use clickhouse::{query::RowCursor, Client, Row};
use dashmap::DashSet;
use rand::{rng, seq::IteratorRandom};
use schema::{Channel, StructuredMessage};
use tracing::debug;

const CHANNEL_MULTI_QUERY_SIZE_DAYS: i64 = 14;

pub async fn read_channel(
    db: &Client,
    channel_id: &str,
    params: LogsParams,
    flush_buffer: &FlushBuffer,
    (from, to): (DateTime<Utc>, DateTime<Utc>),
) -> Result<LogsStream> {
    let buffer_response =
        FlushBufferResponse::new(flush_buffer, channel_id, None, params, (from, to)).await;

    let suffix = if params.reverse { "DESC" } else { "ASC" };

    let mut query = format!("SELECT ?fields FROM message_structured WHERE channel_id = ? AND timestamp >= ? AND timestamp < ? ORDER BY timestamp {suffix}");

    if to - from > Duration::days(CHANNEL_MULTI_QUERY_SIZE_DAYS) {
        let count = db
            .query("SELECT count() FROM (SELECT timestamp FROM message_structured WHERE channel_id = ? AND timestamp >= ? AND timestamp < ? LIMIT 1)")
            .bind(channel_id)
            .bind(from.timestamp_millis() as f64 / 1000.0)
            .bind(to.timestamp_millis() as f64 / 1000.0)
            .fetch_one::<i32>().await?;
        if count == 0 {
            return Err(Error::NotFound);
        }

        let mut streams = Vec::with_capacity(1);

        let interval = Duration::days(CHANNEL_MULTI_QUERY_SIZE_DAYS);

        let mut current_from = from;
        let mut current_to = current_from + interval;

        loop {
            let cursor = next_cursor(db, &query, channel_id, current_from, current_to)?;
            streams.push(cursor);

            current_from += interval;
            current_to += interval;

            if current_to > to {
                let cursor = next_cursor(db, &query, channel_id, current_from, to)?;
                streams.push(cursor);
                break;
            }
        }

        if params.reverse {
            streams.reverse();
        }

        debug!("Using {} queries for multi-query stream", streams.len());

        LogsStream::new_multi_query(streams, buffer_response)
    } else {
        apply_limit_offset(&mut query, &buffer_response);

        let cursor = db
            .query(&query)
            .bind(channel_id)
            .bind(from.timestamp_millis() as f64 / 1000.0)
            .bind(to.timestamp_millis() as f64 / 1000.0)
            .fetch()?;
        LogsStream::new_cursor(cursor, buffer_response).await
    }
}

fn next_cursor(
    db: &Client,
    query: &str,
    channel_id: &str,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<RowCursor<StructuredMessage<'static>>> {
    let cursor = db
        .query(query)
        .bind(channel_id)
        .bind(from.timestamp_millis() as f64 / 1000.0)
        .bind(to.timestamp_millis() as f64 / 1000.0)
        .fetch()?;
    Ok(cursor)
}

pub async fn read_user(
    db: &Client,
    channel_id: &str,
    user_id: &str,
    params: LogsParams,
    flush_buffer: &FlushBuffer,
    (from, to): (DateTime<Utc>, DateTime<Utc>),
) -> Result<LogsStream> {
    let buffer_response =
        FlushBufferResponse::new(flush_buffer, channel_id, Some(user_id), params, (from, to)).await;

    let suffix = if params.reverse { "DESC" } else { "ASC" };
    let mut query = format!("SELECT * FROM message_structured WHERE channel_id = ? AND user_id = ? AND timestamp >= ? AND timestamp < ? ORDER BY timestamp {suffix}");
    apply_limit_offset(&mut query, &buffer_response);

    let cursor = db
        .query(&query)
        .bind(channel_id)
        .bind(user_id)
        .bind(from.timestamp_millis() as f64 / 1000.0)
        .bind(to.timestamp_millis() as f64 / 1000.0)
        .fetch()?;
    LogsStream::new_cursor(cursor, buffer_response).await
}

pub async fn read_available_channel_logs(
    db: &Client,
    channel_id: &str,
) -> Result<Vec<AvailableLogDate>> {
    let timestamps: Vec<i32> = db
        .query(
            "SELECT toDateTime(toStartOfDay(timestamp, 'UTC'), 'UTC') AS date FROM message_structured WHERE channel_id = ? GROUP BY date ORDER BY date DESC",
        )
        .bind(channel_id)
        .fetch_all().await?;

    let dates = timestamps
        .into_iter()
        .map(|timestamp| {
            let naive = DateTime::from_timestamp(timestamp.into(), 0).expect("Invalid DateTime");

            AvailableLogDate {
                year: naive.year().to_string(),
                month: naive.month().to_string(),
                day: Some(naive.day().to_string()),
            }
        })
        .collect();

    Ok(dates)
}

pub async fn read_available_user_logs(
    db: &Client,
    channel_id: &str,
    user_id: &str,
) -> Result<Vec<AvailableLogDate>> {
    let timestamps: Vec<i32> = db
        .query("SELECT toDateTime(toStartOfMonth(timestamp, 'UTC'), 'UTC') AS date FROM message_structured WHERE channel_id = ? AND user_id = ? GROUP BY date ORDER BY date DESC")
        .bind(channel_id)
        .bind(user_id)
        .fetch_all().await?;

    let dates = timestamps
        .into_iter()
        .map(|timestamp| {
            let naive = DateTime::from_timestamp(timestamp.into(), 0).expect("Invalid DateTime");

            AvailableLogDate {
                year: naive.year().to_string(),
                month: naive.month().to_string(),
                day: None,
            }
        })
        .collect();

    Ok(dates)
}

pub async fn read_random_user_line(
    db: &Client,
    channel_id: &str,
    user_id: &str,
) -> Result<StructuredMessage<'static>> {
    let total_count = db
        .query("SELECT count(*) FROM message_structured WHERE channel_id = ? AND user_id = ? ")
        .bind(channel_id)
        .bind(user_id)
        .fetch_one::<u64>()
        .await?;

    if total_count == 0 {
        return Err(Error::NotFound);
    }

    let offset = {
        let mut rng = rng();
        (0..total_count).choose(&mut rng).ok_or(Error::NotFound)
    }?;

    let msg = db
        .query(
            "WITH
            (SELECT timestamp FROM message_structured WHERE channel_id = ? AND user_id = ? LIMIT 1 OFFSET ?)
            AS random_timestamp
            SELECT * FROM message_structured WHERE channel_id = ? AND user_id = ? AND timestamp = random_timestamp",
        )
        .bind(channel_id)
        .bind(user_id)
        .bind(offset)
        .bind(channel_id)
        .bind(user_id)
        .fetch_optional::<StructuredMessage>()
        .await?
        .ok_or(Error::NotFound)?;

    Ok(msg)
}

pub async fn read_random_channel_line(
    db: &Client,
    channel_id: &str,
) -> Result<StructuredMessage<'static>> {
    let total_count = db
        .query("SELECT count(*) FROM message_structured WHERE channel_id = ? ")
        .bind(channel_id)
        .fetch_one::<u64>()
        .await?;

    if total_count == 0 {
        return Err(Error::NotFound);
    }

    let offset = {
        let mut rng = rng();
        (0..total_count).choose(&mut rng).ok_or(Error::NotFound)
    }?;

    let msg = db
        .query(
            "WITH
            (SELECT timestamp FROM message_structured WHERE channel_id = ? LIMIT 1 OFFSET ?)
            AS random_timestamp
            SELECT * FROM message_structured WHERE channel_id = ? AND timestamp = random_timestamp",
        )
        .bind(channel_id)
        .bind(offset)
        .bind(channel_id)
        .fetch_optional::<StructuredMessage>()
        .await?
        .ok_or(Error::NotFound)?;

    Ok(msg)
}

pub async fn delete_user_logs(_db: &Client, _user_id: &str) -> Result<()> {
    // info!("Deleting all logs for user {user_id}");
    // db.query("ALTER TABLE message DELETE WHERE user_id = ?")
    //     .bind(user_id)
    //     .execute()
    //     .await?;
    Ok(())
}

pub async fn search_user_logs(
    db: &Client,
    channel_id: &str,
    user_id: &str,
    search: &str,
    params: LogsParams,
) -> Result<LogsStream> {
    let buffer_response = FlushBufferResponse::empty(params);

    let suffix = if params.reverse { "DESC" } else { "ASC" };

    let mut query = format!("SELECT * FROM message_structured WHERE channel_id = ? AND user_id = ? AND positionCaseInsensitive(text, ?) != 0 ORDER BY timestamp {suffix}");
    apply_limit_offset(&mut query, &buffer_response);

    let cursor = db
        .query(&query)
        .bind(channel_id)
        .bind(user_id)
        .bind(search)
        .fetch()?;

    LogsStream::new_cursor(cursor, buffer_response).await
}

#[derive(Deserialize, Row)]
pub struct StatsRow {
    pub cnt: u64,
    pub user_id: String,
}

pub async fn get_channel_stats(
    db: &Client,
    channel_id: &str,
    range_params: LogRangeParams,
) -> Result<(u64, Vec<StatsRow>)> {
    let mut query = "SELECT count(*) FROM message_structured WHERE channel_id = ?".to_owned();

    if range_params.range().is_some() {
        query.push_str(" AND timestamp >= ? AND timestamp < ?");
    }

    let mut query = db.query(&query).bind(channel_id);

    if let Some((from, to)) = range_params.range() {
        query = query
            .bind(from.timestamp_millis() as f64 / 1000.0)
            .bind(to.timestamp_millis() as f64 / 1000.0);
    }

    let total_count = query.fetch_one().await?;

    let mut query =
        "SELECT count(*) as cnt, user_id FROM message_structured WHERE channel_id = ? AND user_id != ''".to_owned();

    if range_params.range().is_some() {
        query.push_str(" AND timestamp >= ? AND timestamp < ?");
    }

    query.push_str(" GROUP BY user_id ORDER BY cnt DESC LIMIT 5 SETTINGS use_query_cache = 1, query_cache_ttl = 300");

    let mut query = db.query(&query).bind(channel_id);

    if let Some((from, to)) = range_params.range() {
        query = query
            .bind(from.timestamp_millis() as f64 / 1000.0)
            .bind(to.timestamp_millis() as f64 / 1000.0);
    }

    let stats_rows = query.fetch_all::<StatsRow>().await?;

    Ok((total_count, stats_rows))
}

pub async fn get_user_stats(
    db: &Client,
    channel_id: &str,
    user_id: String,
    user_login: Option<String>,
    range_params: LogRangeParams,
) -> Result<UserLogsStats> {
    let mut query =
        "SELECT count(*) FROM message_structured WHERE channel_id = ? AND user_id = ?".to_owned();

    if range_params.range().is_some() {
        query.push_str(" AND timestamp >= ? AND timestamp < ?");
    }

    let mut query = db.query(&query).bind(channel_id).bind(&user_id);

    if let Some((from, to)) = range_params.range() {
        query = query
            .bind(from.timestamp_millis() as f64 / 1000.0)
            .bind(to.timestamp_millis() as f64 / 1000.0);
    }

    let count = query.fetch_one().await?;

    Ok(UserLogsStats {
        message_count: count,
        user_login,
        user_id,
    })
}

pub async fn get_user_name_history(db: &Client, user_id: &str) -> Result<Vec<PreviousName>> {
    #[derive(Deserialize, Row)]
    struct SingleNameHistory {
        user_login: String,
        last_timestamp: i64,
        first_timestamp: i64,
    }

    let query = "
        SELECT trim(LEADING ':' FROM user_login) as user_login,
        max(last_timestamp) AS last_timestamp,
        min(first_timestamp) AS first_timestamp
        FROM username_history
        WHERE user_id = ?
        GROUP BY user_login";

    let name_history_rows: Vec<SingleNameHistory> =
        db.query(query).bind(user_id).fetch_all().await?;

    let mut seen_logins = HashSet::new();

    let names = name_history_rows
        .into_iter()
        .filter_map(|row| {
            if seen_logins.insert(row.user_login.clone()) {
                Some(PreviousName {
                    user_login: row.user_login,
                    last_timestamp: DateTime::from_timestamp_millis(row.last_timestamp)
                        .expect("Invalid DateTime"),
                    first_timestamp: DateTime::from_timestamp_millis(row.first_timestamp)
                        .expect("Invalid DateTime"),
                })
            } else {
                None
            }
        })
        .collect();

    Ok(names)
}

/// How many ids are looked up in a single query
const LOOKUP_CHUNK_SIZE: usize = 500;

/// Reads the logged channels (Kick user ids, or slugs which were never resolved to an id)
pub async fn read_channels(db: &Client) -> Result<HashSet<String>> {
    let channels = db
        .query("SELECT channel_id FROM channel")
        .fetch_all::<String>()
        .await?;

    Ok(HashSet::from_iter(channels))
}

/// Remembers channels as logged
pub async fn add_channels(db: &Client, channels: &[String]) -> Result<()> {
    if channels.is_empty() {
        return Ok(());
    }

    let mut insert = db.insert("channel")?;
    for channel_id in channels {
        insert
            .write(&Channel {
                channel_id: channel_id.clone(),
            })
            .await?;
    }
    insert.end().await?;

    Ok(())
}

/// Forgets channels which were logged
pub async fn remove_channels(db: &Client, channels: &[String]) -> Result<()> {
    for chunk in channels.chunks(100) {
        let placeholders = vec!["?"; chunk.len()].join(", ");
        let mut query = db.query(&format!(
            "DELETE FROM channel WHERE channel_id IN ({placeholders})"
        ));
        for channel_id in chunk {
            query = query.bind(channel_id);
        }
        query.execute().await?;
    }

    Ok(())
}

/// Reads the ids of the users who opted out. `FINAL` returns only the latest state of each user.
pub async fn read_opt_outs(db: &Client) -> Result<DashSet<String>> {
    let opt_outs = db
        .query("SELECT user_id FROM opt_out FINAL WHERE state")
        .fetch_all::<String>()
        .await?;

    Ok(DashSet::from_iter(opt_outs))
}

pub async fn update_opt_out(db: &Client, user_id: &str, state: bool) -> Result<()> {
    db.query("INSERT INTO opt_out (user_id, state) VALUES (?, ?)")
        .bind(user_id)
        .bind(state)
        .execute()
        .await?;

    Ok(())
}

/// Tells which of the users have logged messages in a channel, in the order they were given in.
/// Opt outs are not taken into account.
pub async fn check_users_exist(
    db: &Client,
    channel_id: &str,
    user_ids: &[String],
) -> Result<Vec<UserHasLogs>> {
    let mut with_logs: HashSet<String> = HashSet::new();

    for chunk in user_ids.chunks(LOOKUP_CHUNK_SIZE) {
        let placeholders = vec!["?"; chunk.len()].join(", ");
        let mut query = db
            .query(&format!(
                "SELECT user_id FROM message_structured WHERE channel_id = ? AND user_id IN ({placeholders}) GROUP BY user_id"
            ))
            .bind(channel_id);
        for user_id in chunk {
            query = query.bind(user_id);
        }

        with_logs.extend(query.fetch_all::<String>().await?);
    }

    let mut seen = HashSet::new();
    Ok(user_ids
        .iter()
        .filter(|user_id| seen.insert(user_id.as_str()))
        .map(|user_id| UserHasLogs {
            user: user_id.clone(),
            has_logs: with_logs.contains(user_id),
        })
        .collect())
}

/// The chatroom a Kick channel's chat is published under
#[derive(Row, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ChatroomRow {
    pub channel_id: String,
    pub chatroom_id: u64,
}

/// Reads all known chatroom ids.
/// `FINAL` makes the table return only the most recent row of each channel.
pub async fn read_chatrooms(db: &Client) -> Result<Vec<ChatroomRow>> {
    let rows = db
        .query("SELECT channel_id, chatroom_id FROM kick_chatrooms FINAL")
        .fetch_all::<ChatroomRow>()
        .await?;

    Ok(rows)
}

/// Remembers the chatroom id of a channel
pub async fn save_chatroom(db: &Client, channel_id: &str, chatroom_id: u64) -> Result<()> {
    let row = ChatroomRow {
        channel_id: channel_id.to_owned(),
        chatroom_id,
    };

    let mut insert = db.insert("kick_chatrooms")?;
    insert.write(&row).await?;
    insert.end().await?;

    Ok(())
}

/// Looks up the most recent login of users in the logged messages.
/// Ids without logged messages are missing from the result.
pub async fn get_user_logins(db: &Client, user_ids: &[String]) -> Result<HashMap<String, String>> {
    // The field names differ from the column names on purpose, ClickHouse would otherwise
    // substitute the alias for the column inside of the aggregate function
    #[derive(Deserialize, Row)]
    struct LoginRow {
        id: String,
        latest_login: String,
    }

    let mut logins = HashMap::new();

    for chunk in user_ids.chunks(LOOKUP_CHUNK_SIZE) {
        let placeholders = vec!["?"; chunk.len()].join(", ");
        // Rows without a name (for example of events which only know the id) must not hide
        // the older rows which have one
        let query = format!(
            "SELECT user_id AS id, argMax(trim(LEADING ':' FROM user_login), last_timestamp) AS latest_login FROM username_history WHERE user_id IN ({placeholders}) AND user_login NOT IN ('', ':') GROUP BY user_id"
        );

        let mut query = db.query(&query);
        for user_id in chunk {
            query = query.bind(user_id);
        }

        let rows = query.fetch_all::<LoginRow>().await?;
        logins.extend(
            rows.into_iter()
                .filter(|row| !row.latest_login.is_empty())
                .map(|row| (row.id, row.latest_login)),
        );
    }

    Ok(logins)
}

/// Finds the id of the user who most recently used a login in the logged messages.
/// Messages without a user id (notices of users who were not known) are not considered,
/// and neither is the legacy spelling of moderation messages which have a leading `:`.
pub async fn get_user_id_by_login(db: &Client, user_login: &str) -> Result<Option<String>> {
    let user_id = db
        .query("SELECT user_id FROM username_history WHERE user_login IN (?, ?) AND user_id != '' ORDER BY last_timestamp DESC LIMIT 1")
        .bind(user_login)
        .bind(format!(":{user_login}"))
        .fetch_optional::<String>()
        .await?;

    Ok(user_id)
}

fn apply_limit_offset(query: &mut String, buffer_response: &FlushBufferResponse) {
    if let Some(limit) = buffer_response.normalized_limit() {
        *query = format!("{query} LIMIT {limit}");
    }
    if let Some(offset) = buffer_response.normalized_offset() {
        *query = format!("{query} OFFSET {offset}");
    }
}
