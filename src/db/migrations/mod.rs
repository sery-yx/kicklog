mod migratable;
mod moderation;
mod rollups;
mod structured;
mod username_history;
mod users;

use crate::{config::Config, Result};
use clickhouse::Client;
use moderation::ModerationActionsMigration;
use rollups::{MessageCountsDailyMigration, UserChannelStatsMigration};
use structured::StructuredMigration;
use tracing::{debug, info};
use username_history::UsernameHistoryMigration;
use users::UserTablesMigration;

use self::migratable::Migratable;

pub async fn run(db: &Client, config: &Config) -> Result<()> {
    let db_name = config.clickhouse_db.as_str();

    create_migrations_table(db).await?;

    run_migration(
        db,
        "1_create_message",
        "
CREATE TABLE IF NOT EXISTS message
(
    channel_id LowCardinality(String),
    user_id String CODEC(ZSTD(5)),
    timestamp DateTime64(3) CODEC (DoubleDelta, ZSTD(5)),
    raw String CODEC(ZSTD(5))
)
ENGINE = MergeTree
PARTITION BY toYYYYMM(timestamp)
ORDER BY (channel_id, user_id, timestamp)",
    )
    .await?;

    run_migration(
        db,
        "2_add_channel_log_dates_projection",
        "
ALTER TABLE message
ADD PROJECTION channel_log_dates
(SELECT channel_id, toDateTime(toStartOfDay(timestamp)) as date GROUP BY channel_id, date)",
    )
    .await?;

    run_migration(
        db,
        "3_materialize_channel_log_dates_projection",
        "
ALTER TABLE message
MATERIALIZE PROJECTION channel_log_dates",
    )
    .await?;

    run_migration(
        db,
        "4_set_t64_timestamp_codec",
        "
ALTER TABLE message
MODIFY COLUMN timestamp
DateTime64(3) CODEC(T64, ZSTD(10))
    ",
    )
    .await?;

    run_migration(
        db,
        "5_increase_raw_compression",
        "
ALTER TABLE message
MODIFY COLUMN raw
String CODEC(ZSTD(10))
    ",
    )
    .await?;

    run_migration(db, "6_structured_message", StructuredMigration { db_name }).await?;

    run_migration(db, "7_username_history", UsernameHistoryMigration).await?;

    // Kick specific: the chatroom a channel's chat is published under can only be looked up
    // through a rate limited API, so the results are kept to survive restarts.
    // The table is not an aggregate of messages, newer rows replace older ones.
    run_migration(
        db,
        "8_kick_chatrooms",
        "
CREATE TABLE IF NOT EXISTS kick_chatrooms
(
    channel_id String CODEC(ZSTD(8)),
    chatroom_id UInt64 CODEC(ZSTD(8)),
    updated_at DateTime DEFAULT now()
)
ENGINE = ReplacingMergeTree(updated_at)
ORDER BY channel_id",
    )
    .await?;

    // Rollups for the statistics endpoints. They are filled from the existing messages when they
    // are created, which is only correct if nothing writes messages in the meantime: stop all
    // instances of rustlog which use the database before upgrading to the first version with them.
    run_migration(db, "9_message_counts_daily", MessageCountsDailyMigration).await?;
    run_migration(db, "10_user_channel_stats", UserChannelStatsMigration).await?;

    // The moderation endpoints (bans, timeouts, unbans, deleted messages), built the same way
    run_migration(db, "11_moderation_actions", ModerationActionsMigration).await?;

    // The logged channels and the opt outs, which used to be stored in the config file. What an
    // older config has is imported here.
    run_migration(db, "12_user_tables", UserTablesMigration { config }).await?;

    Ok(())
}

async fn run_migration<'a, T: Migratable<'a>>(
    db: &'a Client,
    name: &str,
    migratable: T,
) -> Result<()> {
    let count = db
        .query("SELECT count(*) FROM __rustlog_migrations WHERE name = ?")
        .bind(name)
        .fetch_one::<u64>()
        .await?;

    if count == 0 {
        info!("Running migration {name}");
        migratable.run(db).await?;

        db.query("INSERT INTO __rustlog_migrations VALUES (?, now())")
            .bind(name)
            .execute()
            .await?;
    } else {
        debug!("Skipping migration {name}");
    }

    Ok(())
}

async fn create_migrations_table(db: &Client) -> Result<()> {
    db.query(
        "
CREATE TABLE IF NOT EXISTS __rustlog_migrations
(
    name String,
    executed_at DateTime
)
ENGINE = MergeTree
ORDER BY name",
    )
    .execute()
    .await?;
    Ok(())
}
