//! The table of moderation actions (bans, timeouts, unbans, deleted messages, cleared chats).
//!
//! Like the rollups, it is kept up to date by a materialized view on `message_structured`, and
//! built from the existing messages when the migration runs. It exists because the messages
//! are ordered for reading the logs of a channel or of a user in a channel, while moderation
//! is asked about across all the messages of a channel, or all the channels of a user.

use super::{migratable::Migratable, rollups::fill_by_partition};
use crate::db::schema::MessageType;
use tracing::info;

/// `CLEARCHAT`: a ban or a timeout, or the chat was cleared
const CLEAR_CHAT: u8 = MessageType::ClearChat as u8;
/// `USERNOTICE`: an unban is one of them
const USER_NOTICE: u8 = MessageType::UserNotice as u8;
/// `CLEARMSG`: a message was deleted
const CLEAR_MSG: u8 = MessageType::ClearMsg as u8;

pub struct ModerationActionsMigration;

impl<'a> Migratable<'a> for ModerationActionsMigration {
    async fn run(&self, db: &'a clickhouse::Client) -> anyhow::Result<()> {
        db.query(
            "
            CREATE TABLE IF NOT EXISTS moderation_actions
            (
                channel_id LowCardinality(String) CODEC(ZSTD(8)),
                channel_login LowCardinality(String) CODEC(ZSTD(8)),
                timestamp DateTime64(3) CODEC(T64, ZSTD(5)),
                action LowCardinality(String) CODEC(ZSTD(8)),
                target_user_id String CODEC(ZSTD(8)),
                target_user_login String CODEC(ZSTD(8)),
                moderator_id String CODEC(ZSTD(8)),
                moderator_login String CODEC(ZSTD(8)),
                duration_seconds UInt64 CODEC(ZSTD(5)),
                expires_at DateTime64(3) CODEC(ZSTD(5)),
                permanent UInt8 CODEC(ZSTD(5)),
                message_id String CODEC(ZSTD(5)),
                deleted_text String CODEC(ZSTD(8)),
                ai_moderated UInt8 CODEC(ZSTD(5)),
                violated_rules String CODEC(ZSTD(8)),
                PROJECTION by_user
                (
                    SELECT * ORDER BY target_user_id, timestamp
                )
            )
            ENGINE = MergeTree
            PARTITION BY toYYYYMM(timestamp)
            ORDER BY (channel_id, timestamp)
        ",
        )
        .execute()
        .await?;

        // In case an earlier attempt failed halfway, the table must not contain partial data
        db.query("TRUNCATE TABLE moderation_actions")
            .execute()
            .await?;

        fill_by_partition(
            db,
            "moderation_actions",
            &format!(
                "INSERT INTO moderation_actions ({COLUMNS}) {select} AND toYYYYMM(timestamp) = ?",
                select = select_actions(),
            ),
        )
        .await?;

        db.query(&format!(
            "CREATE MATERIALIZED VIEW IF NOT EXISTS moderation_actions_mv
            TO moderation_actions
            AS {select}",
            select = select_actions(),
        ))
        .execute()
        .await?;

        info!("Moderation actions built");

        Ok(())
    }
}

/// The columns of the table, in the order `select_actions` returns them. They are also what the
/// materialized view matches its columns to by name.
const COLUMNS: &str = "channel_id, channel_login, timestamp, action, target_user_id, target_user_login, moderator_id, moderator_login, duration_seconds, expires_at, permanent, message_id, deleted_text, ai_moderated, violated_rules";

/// Selects the moderation actions of messages (all of them, a condition on the partition
/// can be added with `AND`).
///
/// The names of the columns it returns are not the names of columns of `message_structured`
/// which are used in the expressions, an alias that has the name of a column would be used in
/// place of the column inside of the expressions.
fn select_actions() -> String {
    format!(
        "SELECT
            channel_id,
            channel_login,
            timestamp,
            multiIf(
                message_type = {CLEAR_MSG}, 'delete',
                message_type = {USER_NOTICE}, 'unban',
                user_id = '', 'clear',
                toUInt64OrZero(extra_tags['ban-duration']) > 0 OR extra_tags['ban-expires-at'] != '', 'timeout',
                'ban'
            ) AS action,
            if(message_type = {CLEAR_MSG}, extra_tags['target-user-id'], user_id) AS target_user_id,
            user_login AS target_user_login,
            extra_tags['moderator-user-id'] AS moderator_id,
            extra_tags['moderator-user-login'] AS moderator_login,
            toUInt64OrZero(extra_tags['ban-duration']) AS duration_seconds,
            parseDateTime64BestEffortOrZero(extra_tags['ban-expires-at'], 3) AS expires_at,
            toUInt8(extra_tags['ban-permanent'] = '1') AS permanent,
            extra_tags['target-msg-id'] AS message_id,
            if(message_type = {CLEAR_MSG}, text, '') AS deleted_text,
            toUInt8(extra_tags['ai-moderated'] = '1') AS ai_moderated,
            extra_tags['violated-rules'] AS violated_rules
        FROM message_structured
        WHERE (message_type = {CLEAR_CHAT} OR message_type = {CLEAR_MSG} OR (message_type = {USER_NOTICE} AND extra_tags['msg-id'] = 'unban'))"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_types_are_those_of_clearchat_usernotice_and_clearmsg() {
        assert_eq!(CLEAR_CHAT, 2);
        assert_eq!(USER_NOTICE, 4);
        assert_eq!(CLEAR_MSG, 13);
    }

    #[test]
    fn the_select_returns_the_columns_of_the_table_in_order() {
        let select = select_actions();

        // the position of the column in the select and in the list of columns
        let mut last_position = 0;
        for column in COLUMNS.split(", ") {
            let position = select
                .find(&format!("AS {column},"))
                .or_else(|| select.find(&format!("AS {column}\n")))
                // columns which are selected as they are
                .or_else(|| select.find(&format!("{column},\n")))
                .unwrap_or_else(|| panic!("{column} is not selected"));

            assert!(position >= last_position, "{column} is not in order");
            last_position = position;
        }
    }

    #[test]
    fn a_condition_can_be_added_to_the_select() {
        // the filter on the partition is added with AND, so the select must end with a
        // complete condition
        let select = select_actions();
        assert!(select.trim_end().ends_with("= 'unban'))"));
    }
}
