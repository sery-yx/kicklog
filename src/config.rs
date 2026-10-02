use crate::kick::pusher::{DEFAULT_CHANNELS_PER_CONNECTION, DEFAULT_NEW_CONNECTION_EVERY_MS};
use anyhow::Context;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use std::fs;
use std::{
    collections::{HashMap, HashSet},
    sync::RwLock,
};
use tracing::info;

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    pub clickhouse_url: String,
    pub clickhouse_db: String,
    pub clickhouse_username: Option<String>,
    pub clickhouse_password: Option<String>,
    #[serde(default = "clickhouse_flush_interval")]
    pub clickhouse_flush_interval: u64,
    #[serde(default = "default_listen_address")]
    pub listen_address: String,
    /// Kick user ids of the logged channels
    pub channels: RwLock<HashSet<String>>,
    /// Client id of the Kick app (see https://kick.com/settings/developer)
    #[serde(rename = "clientID")]
    pub client_id: String,
    pub client_secret: String,
    /// Kick usernames (slugs) who are allowed to use administration commands
    pub admins: Vec<String>,
    #[serde(default)]
    pub opt_out: DashMap<String, bool>,
    #[serde(rename = "adminAPIKey")]
    pub admin_api_key: Option<String>,
    // The settings below are only written back to the file if they were changed from their
    // defaults, so the bot adding a channel does not clutter the config of somebody
    // who never touched them.
    /// Chatroom ids to use instead of looking them up, as `channel id -> chatroom id`.
    /// Only needed if Kick's website API cannot be reached.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub chatroom_ids: HashMap<String, u64>,
    /// Pusher application key of Kick's web client
    #[serde(
        default = "default_pusher_key",
        skip_serializing_if = "is_default_pusher_key"
    )]
    pub pusher_key: String,
    #[serde(
        default = "default_pusher_cluster",
        skip_serializing_if = "is_default_pusher_cluster"
    )]
    pub pusher_cluster: String,
    // The three limits below are those of the original rustlog (which gets them from its IRC
    // library) unless they are changed: 90 channels per connection, any number of connections
    // and a new connection at most every 2 seconds.
    /// How many channels are listened to on a single websocket connection
    #[serde(
        default = "default_pusher_max_channels_per_connection",
        skip_serializing_if = "is_default_pusher_max_channels_per_connection"
    )]
    pub pusher_max_channels_per_connection: usize,
    /// How many websocket connections are opened at most. No limit if it is not set (or 0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pusher_max_connections: Option<usize>,
    /// A new websocket connection is only started this many milliseconds after the previous one
    #[serde(
        default = "default_pusher_new_connection_every_ms",
        skip_serializing_if = "is_default_pusher_new_connection_every_ms"
    )]
    pub pusher_new_connection_every_ms: u64,
    #[serde(skip)]
    config_path: Option<std::path::PathBuf>,
}

impl Config {
    pub fn load(config_path: &std::path::Path) -> anyhow::Result<Self> {
        let contents = fs::read_to_string(config_path)
            .with_context(|| format!("Failed to load config from {}", config_path.display()))?;
        let mut s: Self = serde_json::from_str(&contents).context("Config deserializtion error")?;
        s.config_path = Some(config_path.to_owned());

        let channels = s.channels.get_mut().expect("the lock cannot be poisoned yet");
        *channels = normalize_channels(channels);

        Ok(s)
    }

    pub fn save(&self) -> anyhow::Result<()> {
        info!("Updating config");
        let json = serde_json::to_string_pretty(self)?;
        fs::write(self.config_path.as_ref().expect("config path should always be available"), json)?;

        Ok(())
    }

    /// The limit on websocket connections, `None` if there is none
    pub fn pusher_connection_limit(&self) -> Option<usize> {
        self.pusher_max_connections
            .filter(|max_connections| *max_connections > 0)
    }
}

/// Channels are listed by id or by name. Names are not case sensitive, so they are compared
/// in lowercase everywhere.
fn normalize_channels(channels: &HashSet<String>) -> HashSet<String> {
    channels
        .iter()
        .map(|channel| channel.trim().to_lowercase())
        .filter(|channel| !channel.is_empty())
        .collect()
}

fn default_listen_address() -> String {
    String::from("0.0.0.0:8025")
}

fn clickhouse_flush_interval() -> u64 {
    10
}

fn default_pusher_key() -> String {
    String::from("32cbd69e4b950bf97679")
}

fn default_pusher_cluster() -> String {
    String::from("us2")
}

fn default_pusher_max_channels_per_connection() -> usize {
    DEFAULT_CHANNELS_PER_CONNECTION
}

fn default_pusher_new_connection_every_ms() -> u64 {
    DEFAULT_NEW_CONNECTION_EVERY_MS
}

fn is_default_pusher_key(key: &str) -> bool {
    key == default_pusher_key()
}

fn is_default_pusher_cluster(cluster: &str) -> bool {
    cluster == default_pusher_cluster()
}

fn is_default_pusher_max_channels_per_connection(channels: &usize) -> bool {
    *channels == default_pusher_max_channels_per_connection()
}

fn is_default_pusher_new_connection_every_ms(milliseconds: &u64) -> bool {
    *milliseconds == default_pusher_new_connection_every_ms()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    const MINIMAL: &str = r#"{
        "clickhouseUrl": "http://localhost:8123",
        "clickhouseDb": "rustlog",
        "channels": ["676"],
        "clientID": "id",
        "clientSecret": "secret",
        "admins": []
    }"#;

    #[test]
    fn minimal_config_uses_defaults() {
        let config: Config = serde_json::from_str(MINIMAL).unwrap();

        assert_eq!(config.clickhouse_username, None);
        assert_eq!(config.clickhouse_flush_interval, 10);
        assert_eq!(config.listen_address, "0.0.0.0:8025");
        assert_eq!(config.client_id, "id");
        assert_eq!(config.admin_api_key, None);
        assert!(config.opt_out.is_empty());
        assert!(config.chatroom_ids.is_empty());
        assert_eq!(config.pusher_key, "32cbd69e4b950bf97679");
        assert_eq!(config.pusher_cluster, "us2");
        assert!(config.channels.read().unwrap().contains("676"));
    }

    #[test]
    fn join_limits_default_to_those_of_the_original_rustlog() {
        let config: Config = serde_json::from_str(MINIMAL).unwrap();

        // 90 channels per connection, any number of connections, one new connection at most
        // every 2 seconds
        assert_eq!(config.pusher_max_channels_per_connection, 90);
        assert_eq!(config.pusher_max_connections, None);
        assert_eq!(config.pusher_connection_limit(), None);
        assert_eq!(config.pusher_new_connection_every_ms, 2000);
    }

    #[test]
    fn a_connection_limit_of_zero_means_no_limit() {
        let mut config: Config = serde_json::from_str(MINIMAL).unwrap();

        config.pusher_max_connections = Some(0);
        assert_eq!(config.pusher_connection_limit(), None);

        config.pusher_max_connections = Some(25);
        assert_eq!(config.pusher_connection_limit(), Some(25));
    }

    #[test]
    fn kick_settings_can_be_overridden() {
        let config: Config = serde_json::from_str(
            r#"{
                "clickhouseUrl": "http://localhost:8123",
                "clickhouseDb": "rustlog",
                "channels": [],
                "clientID": "id",
                "clientSecret": "secret",
                "admins": ["someone"],
                "optOut": {"123": true},
                "adminAPIKey": "key",
                "chatroomIds": {"7183419": 7022952},
                "pusherKey": "other",
                "pusherCluster": "eu",
                "pusherMaxChannelsPerConnection": 50,
                "pusherMaxConnections": 4,
                "pusherNewConnectionEveryMs": 500
            }"#,
        )
        .unwrap();

        assert_eq!(config.chatroom_ids.get("7183419"), Some(&7022952));
        assert_eq!(config.pusher_key, "other");
        assert_eq!(config.pusher_cluster, "eu");
        assert_eq!(config.pusher_max_channels_per_connection, 50);
        assert_eq!(config.pusher_max_connections, Some(4));
        assert_eq!(config.pusher_connection_limit(), Some(4));
        assert_eq!(config.pusher_new_connection_every_ms, 500);
        assert!(config.opt_out.contains_key("123"));
        assert_eq!(config.admin_api_key.as_deref(), Some("key"));
    }

    #[test]
    fn channel_entries_are_trimmed_and_lowercased() {
        let channels: HashSet<String> = ["676", " XQC ", "xqc", "Some_User", "", "  "]
            .into_iter()
            .map(str::to_owned)
            .collect();

        let normalized = normalize_channels(&channels);

        let expected: HashSet<String> = ["676", "xqc", "some_user"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        assert_eq!(normalized, expected);
    }

    #[test]
    fn default_kick_settings_are_not_written_back() {
        let config: Config = serde_json::from_str(MINIMAL).unwrap();
        let json = serde_json::to_value(&config).unwrap();
        let object = json.as_object().unwrap();

        for key in [
            "chatroomIds",
            "pusherKey",
            "pusherCluster",
            "pusherMaxChannelsPerConnection",
            "pusherMaxConnections",
            "pusherNewConnectionEveryMs",
        ] {
            assert!(!object.contains_key(key), "{key} should have been skipped");
        }
        // the settings which upstream always writes are still there
        assert!(object.contains_key("channels"));
        assert!(object.contains_key("clientID"));
        assert!(object.contains_key("optOut"));
    }

    #[test]
    fn changed_kick_settings_are_written_back() {
        let config: Config = serde_json::from_str(
            r#"{
                "clickhouseUrl": "http://localhost:8123",
                "clickhouseDb": "rustlog",
                "channels": [],
                "clientID": "id",
                "clientSecret": "secret",
                "admins": [],
                "chatroomIds": {"7183419": 7022952},
                "pusherKey": "other",
                "pusherCluster": "eu",
                "pusherMaxChannelsPerConnection": 50,
                "pusherMaxConnections": 4,
                "pusherNewConnectionEveryMs": 500
            }"#,
        )
        .unwrap();
        let json = serde_json::to_value(&config).unwrap();

        assert_eq!(json["chatroomIds"]["7183419"], 7022952);
        assert_eq!(json["pusherKey"], "other");
        assert_eq!(json["pusherCluster"], "eu");
        assert_eq!(json["pusherMaxChannelsPerConnection"], 50);
        assert_eq!(json["pusherMaxConnections"], 4);
        assert_eq!(json["pusherNewConnectionEveryMs"], 500);
    }

    #[test]
    fn config_roundtrips_through_json() {
        let config: Config = serde_json::from_str(MINIMAL).unwrap();
        let json = serde_json::to_string(&config).unwrap();
        let parsed: Config = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed.client_secret, "secret");
        assert_eq!(parsed.pusher_max_connections, None);
        assert_eq!(parsed.pusher_max_channels_per_connection, 90);
        assert!(parsed.channels.read().unwrap().contains("676"));
    }
}
