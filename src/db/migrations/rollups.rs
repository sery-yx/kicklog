//! Rollup tables for the statistics endpoints (leaderboards, activity, user summaries).
//!
//! They only count chat messages (`message_type = 1`, `PRIVMSG`) and are kept up to date by
//! materialized views. When the migration runs on a database which already contains messages,
//! the existing messages are aggregated first, one partition at a time.

use super::migratable::Migratable;
use crate::db::schema::MessageType;
use anyhow::Context;
use tracing::info;

/// `PRIVMSG`
const CHAT_MESSAGE_TYPE: u8 = MessageType::PrivMsg as u8;

/// Number of chat messages per channel, user and day
pub struct MessageCountsDailyMigration;

impl<'a> Migratable<'a> for MessageCountsDailyMigration {
    async fn run(&self, db: &'a clickhouse::Client) -> anyhow::Result<()> {
        let select = daily_counts_select();

        db.query(
            "
            CREATE TABLE IF NOT EXISTS message_counts_daily
            (
                channel_id LowCardinality(String) CODEC(ZSTD(8)),
                date Date CODEC(ZSTD(5)),
                user_id String CODEC(ZSTD(8)),
                message_count UInt64 CODEC(ZSTD(5))
            )
            ENGINE = SummingMergeTree
            PARTITION BY toYYYYMM(date)
            ORDER BY (channel_id, date, user_id)
        ",
        )
        .execute()
        .await?;

        // In case an earlier attempt failed halfway, the table must not contain partial data
        db.query("TRUNCATE TABLE message_counts_daily")
            .execute()
            .await?;

        fill_by_partition(
            db,
            "message_counts_daily",
            &format!(
                "INSERT INTO message_counts_daily (channel_id, date, user_id, message_count)
                {select} AND toYYYYMM(timestamp) = ?
                GROUP BY channel_id, date, user_id"
            ),
        )
        .await?;

        db.query(&format!(
            "CREATE MATERIALIZED VIEW IF NOT EXISTS message_counts_daily_mv
            TO message_counts_daily
            AS {select}
            GROUP BY channel_id, date, user_id"
        ))
        .execute()
        .await?;

        info!("Daily message counts built");

        Ok(())
    }
}

/// Selects the chat messages of `message_structured`, counted per channel, user and day (a
/// condition on the partition can be added with `AND`, the `GROUP BY` is up to the caller).
///
/// The day is the day in UTC, whatever time zone the ClickHouse server is set to. The
/// statistics endpoints work with calendar days in UTC.
fn daily_counts_select() -> String {
    format!(
        "SELECT channel_id, toDate(toTimeZone(timestamp, 'UTC')) AS date, user_id, count() AS message_count
        FROM message_structured
        WHERE message_type = {CHAT_MESSAGE_TYPE} AND user_id != ''"
    )
}

/// First message, last message and number of messages per user and channel
pub struct UserChannelStatsMigration;

impl<'a> Migratable<'a> for UserChannelStatsMigration {
    async fn run(&self, db: &'a clickhouse::Client) -> anyhow::Result<()> {
        let select = format!(
            "SELECT
                user_id,
                channel_id,
                sumSimpleState(toUInt64(1)) AS message_count,
                minSimpleState(timestamp) AS first_timestamp,
                maxSimpleState(timestamp) AS last_timestamp
            FROM message_structured
            WHERE message_type = {CHAT_MESSAGE_TYPE} AND user_id != ''"
        );

        db.query(
            "
            CREATE TABLE IF NOT EXISTS user_channel_stats
            (
                user_id String CODEC(ZSTD(8)),
                channel_id LowCardinality(String) CODEC(ZSTD(8)),
                message_count SimpleAggregateFunction(sum, UInt64) CODEC(ZSTD(5)),
                first_timestamp SimpleAggregateFunction(min, DateTime64(3)) CODEC(ZSTD(5)),
                last_timestamp SimpleAggregateFunction(max, DateTime64(3)) CODEC(ZSTD(5))
            )
            ENGINE = AggregatingMergeTree
            ORDER BY (user_id, channel_id)
        ",
        )
        .execute()
        .await?;

        // In case an earlier attempt failed halfway, the table must not contain partial data
        db.query("TRUNCATE TABLE user_channel_stats")
            .execute()
            .await?;

        fill_by_partition(
            db,
            "user_channel_stats",
            &format!(
                "INSERT INTO user_channel_stats (user_id, channel_id, message_count, first_timestamp, last_timestamp)
                {select} AND toYYYYMM(timestamp) = ?
                GROUP BY user_id, channel_id"
            ),
        )
        .await?;

        db.query(&format!(
            "CREATE MATERIALIZED VIEW IF NOT EXISTS user_channel_stats_mv
            TO user_channel_stats
            AS {select}
            GROUP BY user_id, channel_id"
        ))
        .execute()
        .await?;

        info!("User channel statistics built");

        Ok(())
    }
}

/// Runs `insert` (which has to contain a `?` for the partition as `YYYYMM`) once per partition
/// of the message table
pub(super) async fn fill_by_partition(
    db: &clickhouse::Client,
    table: &str,
    insert: &str,
) -> anyhow::Result<()> {
    let partitions = db
        .query("SELECT DISTINCT toYYYYMM(timestamp) AS partition FROM message_structured ORDER BY partition ASC")
        .fetch_all::<u32>()
        .await
        .context("Could not fetch partition list")?;

    info!(
        "Filling {table} from {} partitions of existing messages",
        partitions.len()
    );

    for partition in partitions {
        info!("Filling {table} for partition {partition}");
        db.query(insert)
            .bind(partition)
            .execute()
            .await
            .with_context(|| format!("Could not fill {table} for partition {partition}"))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn days_are_counted_in_utc() {
        let select = daily_counts_select();

        assert!(select.contains("toDate(toTimeZone(timestamp, 'UTC')) AS date"));
        // a day taken from the time zone of the server would be a different one on a server
        // which is not set to UTC
        assert!(!select.contains("toDate(timestamp)"));
    }

    #[test]
    fn a_condition_can_be_added_to_the_daily_counts() {
        // the filter on the partition is added with AND, so the select must end with a
        // complete condition
        assert!(daily_counts_select().trim_end().ends_with("user_id != ''"));
    }
}
