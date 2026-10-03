//! The tables of the logged channels and of the users who opted out.
//!
//! Both used to be stored in the config file, which the bot rewrote whenever a channel was
//! joined or left or a user opted out. They live in the database now, so that the config can
//! be read-only. What an older config has is imported once, and then removed from the file.

use super::migratable::Migratable;
use crate::config::Config;
use crate::db::schema::{Channel, OptOut};
use tracing::{info, warn};

pub struct UserTablesMigration<'a> {
    pub config: &'a Config,
}

impl<'a> Migratable<'a> for UserTablesMigration<'a> {
    async fn run(&self, db: &'a clickhouse::Client) -> anyhow::Result<()> {
        // `IF NOT EXISTS` and the replacing engines make running this again harmless, which is
        // what happens if an earlier attempt failed after it created the tables
        db.query(
            "
CREATE TABLE IF NOT EXISTS channel
(
    channel_id String CODEC(ZSTD(8))
)
ENGINE ReplacingMergeTree
ORDER BY channel_id",
        )
        .execute()
        .await?;

        db.query(
            "
CREATE TABLE IF NOT EXISTS opt_out
(
    user_id String CODEC(ZSTD(8)),
    state UInt8 CODEC(ZSTD(1))
)
ENGINE ReplacingMergeTree
ORDER BY user_id",
        )
        .execute()
        .await?;

        let channels = &self.config.legacy_channels;
        if !channels.is_empty() {
            let mut insert = db.insert("channel")?;
            for channel_id in channels {
                insert
                    .write(&Channel {
                        channel_id: channel_id.clone(),
                    })
                    .await?;
            }
            insert.end().await?;
            info!("Imported {} channels from the config", channels.len());
        }

        let opt_outs = &self.config.legacy_opt_out;
        if !opt_outs.is_empty() {
            let mut insert = db.insert("opt_out")?;
            for (user_id, state) in opt_outs {
                insert
                    .write(&OptOut {
                        user_id: user_id.clone(),
                        state: *state,
                    })
                    .await?;
            }
            insert.end().await?;
            info!("Imported {} opt outs from the config", opt_outs.len());
        }

        // Rewrites the config without the settings which were imported (they are never written
        // back). Failing to do that is not worth failing the migration for: the settings are
        // ignored from now on either way.
        if let Err(err) = self.config.save() {
            warn!("Could not remove the imported channels and opt outs from the config: {err:#}");
        }

        Ok(())
    }
}
