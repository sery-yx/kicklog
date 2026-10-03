use super::responders::logs::{JsonResponseType, LogsResponseType};
use crate::logs::{
    moderation::{EndReason, ModerationKind},
    period::{Interval, Period},
};
use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};
use std::fmt::Display;
use strum::Display;

#[derive(Serialize, JsonSchema)]
pub struct ChannelsList {
    pub channels: Vec<Channel>,
}

#[derive(Serialize, JsonSchema)]
pub struct Channel {
    pub name: String,
    #[serde(rename = "userID")]
    pub user_id: String,
}

#[derive(Debug, Deserialize, JsonSchema, Display)]
pub enum ChannelIdType {
    #[serde(rename = "channel")]
    #[strum(serialize = "channel")]
    Name,
    #[serde(rename = "channelid")]
    #[strum(serialize = "channelid")]
    Id,
}

#[derive(Debug, Deserialize, JsonSchema, Display)]
pub enum UserIdType {
    #[serde(rename = "user")]
    #[strum(serialize = "user")]
    Name,
    #[serde(rename = "userid")]
    #[strum(serialize = "userid")]
    Id,
}

#[derive(Deserialize, JsonSchema)]
pub struct UserLogsDatePath {
    pub year: String,
    pub month: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct ChannelLogsByDatePath {
    #[serde(flatten)]
    pub channel_info: LogsPathChannel,
    #[serde(flatten)]
    pub date: LogsPathDate,
}

#[derive(Deserialize, JsonSchema)]
pub struct LogsPathDate {
    pub year: String,
    pub month: String,
    pub day: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct LogsPathChannel {
    pub channel_id_type: ChannelIdType,
    pub channel: String,
}

#[derive(Deserialize, Debug, JsonSchema, Clone, Copy)]
#[serde(rename_all = "camelCase")]
pub struct LogsParams {
    #[serde(default, deserialize_with = "deserialize_bool_param")]
    pub json: bool,
    #[serde(default, deserialize_with = "deserialize_bool_param")]
    pub json_basic: bool,
    #[serde(default, deserialize_with = "deserialize_bool_param")]
    pub raw: bool,
    #[serde(default, deserialize_with = "deserialize_bool_param")]
    pub reverse: bool,
    #[serde(default, deserialize_with = "deserialize_bool_param")]
    pub ndjson: bool,
    pub limit: Option<u64>,
    pub offset: Option<u64>,
}

impl LogsParams {
    pub fn response_type(&self) -> LogsResponseType {
        if self.raw {
            LogsResponseType::Raw
        } else if self.json_basic {
            LogsResponseType::Json(JsonResponseType::Basic)
        } else if self.json {
            LogsResponseType::Json(JsonResponseType::Full)
        } else if self.ndjson {
            LogsResponseType::NdJson
        } else {
            LogsResponseType::Text
        }
    }
}

fn deserialize_bool_param<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Option::<&str>::deserialize(deserializer)?.is_some())
}

#[derive(Deserialize, Debug, JsonSchema)]
pub struct SearchParams {
    pub q: String,
}

#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AvailableLogs {
    pub available_logs: Vec<AvailableLogDate>,
}

#[derive(Serialize, JsonSchema)]
pub struct AvailableLogDate {
    pub year: String,
    pub month: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub day: Option<String>,
}

impl Display for AvailableLogDate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.year, self.month)?;

        if let Some(day) = &self.day {
            write!(f, "/{day}")?;
        }

        Ok(())
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct AvailableLogsParams {
    #[serde(flatten)]
    pub channel: ChannelParam,
    #[serde(flatten)]
    pub user: Option<UserParam>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum UserParam {
    User(String),
    UserId(String),
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ChannelParam {
    Channel(String),
    ChannelId(String),
}

#[derive(Deserialize, JsonSchema)]
pub struct UserLogPathParams {
    pub channel_id_type: ChannelIdType,
    pub channel: String,
    pub user_id_type: UserIdType,
    pub user: String,
}

#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ChannelLogsStats {
    pub message_count: u64,
    pub top_chatters: Vec<UserLogsStats>,
}

#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct UserLogsStats {
    pub user_id: String,
    pub user_login: Option<String>,
    pub message_count: u64,
}

#[derive(Deserialize, JsonSchema)]
pub struct UserNameHistoryParam {
    pub user_id: String,
}

#[derive(Serialize, JsonSchema)]
pub struct PreviousName {
    pub user_login: String,
    pub last_timestamp: DateTime<Utc>,
    pub first_timestamp: DateTime<Utc>,
}

/// Whether a user has logged messages in a channel
#[derive(Serialize, JsonSchema, Debug, PartialEq, Eq)]
pub struct UserHasLogs {
    /// Kick user id
    pub user: String,
    /// Whether the user has logs in the channel
    pub has_logs: bool,
}

/// Path of the endpoints which are about a user in general and not about a channel
#[derive(Deserialize, JsonSchema)]
pub struct UserPathParams {
    pub user_id_type: UserIdType,
    pub user: String,
}

/// Selects a calendar period (UTC)
#[derive(Deserialize, Debug, JsonSchema)]
pub struct PeriodParams {
    /// The calendar period. Defaults to `day`.
    #[serde(default)]
    pub period: Period,
    /// Any date inside the period, as `YYYY-MM-DD`, `YYYY-MM` or `YYYY`. Defaults to today (UTC).
    /// Weeks start on Monday. Ignored for the period `all`.
    pub date: Option<String>,
}

/// Parameters of the leaderboard endpoints
#[derive(Deserialize, Debug, JsonSchema)]
pub struct TopParams {
    /// The calendar period. Defaults to `day`.
    #[serde(default)]
    pub period: Period,
    /// Any date inside the period, as `YYYY-MM-DD`, `YYYY-MM` or `YYYY`. Defaults to today (UTC).
    /// Weeks start on Monday. Ignored for the period `all`.
    pub date: Option<String>,
    /// How many entries to return, from 1 to 100. Defaults to 10.
    pub limit: Option<u64>,
}

/// Parameters of the activity endpoints
#[derive(Deserialize, Debug, JsonSchema)]
pub struct ActivityParams {
    /// The size of the time buckets. Defaults to `day`.
    #[serde(default)]
    pub interval: Interval,
    /// First day, as `YYYY-MM-DD`. An RFC 3339 timestamp is reduced to its UTC date.
    /// Defaults to a number of days before `to` which depends on the interval.
    /// The range starts with the interval this day is in (weeks start on Monday),
    /// so that no bucket is partial.
    pub from: Option<String>,
    /// Last day (inclusive), as `YYYY-MM-DD`. An RFC 3339 timestamp is reduced to its UTC date.
    /// Defaults to today (UTC). The range ends with the interval this day is in.
    pub to: Option<String>,
}

#[derive(Deserialize, Debug, JsonSchema, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum UserChannelsSort {
    /// The channels the user chatted in most recently first
    #[default]
    Last,
    /// The channels with the most messages of the user first
    Count,
}

/// Parameters of the endpoint which lists the channels of a user
#[derive(Deserialize, Debug, JsonSchema)]
pub struct UserChannelsParams {
    /// How the channels are ordered. Defaults to `last`.
    #[serde(default)]
    pub sort: UserChannelsSort,
    /// How many channels to return, from 1 to 100. Defaults to 25.
    pub limit: Option<u64>,
    /// How many channels to skip. Defaults to 0.
    pub offset: Option<u64>,
}

#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ChannelTopResponse {
    pub period: Period,
    /// Start of the period, absent for `all`
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<DateTime<Utc>>,
    /// End of the period (exclusive), absent for `all`
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<DateTime<Utc>>,
    /// Number of chat messages in the period
    pub message_count: u64,
    pub top_chatters: Vec<UserLogsStats>,
}

#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ChannelsTopResponse {
    pub period: Period,
    /// Start of the period, absent for `all`
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<DateTime<Utc>>,
    /// End of the period (exclusive), absent for `all`
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<DateTime<Utc>>,
    pub channels: Vec<ChannelMessageCount>,
}

#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ChannelMessageCount {
    pub channel_id: String,
    pub channel_login: Option<String>,
    pub message_count: u64,
}

#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ActivityResponse {
    pub interval: Interval,
    /// Start of the first interval
    pub from: DateTime<Utc>,
    /// End of the last interval (exclusive), which can be in the future
    pub to: DateTime<Utc>,
    /// One entry per interval, including the ones without messages
    pub buckets: Vec<ActivityBucketResponse>,
}

#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ActivityBucketResponse {
    /// Start of the bucket (UTC)
    pub start: DateTime<Utc>,
    pub message_count: u64,
    /// Number of different chatters. Only available for the activity of a whole channel.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unique_chatters: Option<u64>,
}

#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct UserRankResponse {
    pub user_id: String,
    pub user_login: Option<String>,
    pub period: Period,
    /// Start of the period, absent for `all`
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<DateTime<Utc>>,
    /// End of the period (exclusive), absent for `all`
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<DateTime<Utc>>,
    /// Number of chat messages of the user in the period
    pub message_count: u64,
    /// Position among the chatters of the channel, 1 is the chatter with the most messages.
    /// Chatters with the same number of messages share a rank. Absent without messages.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rank: Option<u64>,
    /// Number of different chatters in the channel in the period
    pub total_chatters: u64,
}

#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct UserChannelsResponse {
    pub user_id: String,
    pub user_login: Option<String>,
    pub channels: Vec<UserChannelResponse>,
}

#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct UserChannelResponse {
    pub channel_id: String,
    pub channel_login: Option<String>,
    pub message_count: u64,
    pub first_message: DateTime<Utc>,
    pub last_message: DateTime<Utc>,
}

#[derive(Deserialize, Debug, JsonSchema, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SortOrder {
    /// The oldest first
    Asc,
    /// The newest first
    #[default]
    Desc,
}

/// Parameters of the endpoints which list the bans and timeouts of a user
#[derive(Deserialize, Debug, JsonSchema)]
pub struct BansParams {
    /// The order of the entries. Defaults to `desc`.
    #[serde(default)]
    pub order: SortOrder,
    /// How many entries to return, from 1 to 500. Defaults to 100.
    pub limit: Option<u64>,
    /// How many entries to skip. Defaults to 0.
    pub offset: Option<u64>,
}

/// Parameters of the endpoint which lists the moderation actions of a channel
#[derive(Deserialize, Debug, JsonSchema)]
pub struct ModerationFeedParams {
    /// Only this kind of action
    #[serde(rename = "type")]
    pub kind: Option<ModerationKind>,
    /// Only actions of the moderator with this login
    pub moderator: Option<String>,
    /// Only actions of the moderator with this user id
    #[serde(rename = "moderatorId")]
    pub moderator_id: Option<String>,
    /// Only actions at or after this time, as an RFC 3339 timestamp or as a date (`YYYY-MM-DD`,
    /// the start of the day in UTC)
    pub from: Option<String>,
    /// Only actions before this time, in the same formats as `from`
    pub to: Option<String>,
    /// How many actions to return, from 1 to 500. Defaults to 50.
    pub limit: Option<u64>,
    /// How many actions to skip. Defaults to 0.
    pub offset: Option<u64>,
}

/// A user as it is named in a moderation action
#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ModerationPerson {
    pub id: String,
    /// The login the user had at the time
    pub login: Option<String>,
}

#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ModerationChannel {
    pub id: String,
    pub login: Option<String>,
}

/// How a ban or a timeout ended
#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ModerationEndResponse {
    pub at: DateTime<Utc>,
    pub reason: EndReason,
    /// Who lifted it, or who gave the newer ban or timeout. Absent if Kick does not say, and
    /// for timeouts which ran out.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub moderator: Option<ModerationPerson>,
}

/// One thing a moderator did
#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ModerationActionResponse {
    #[serde(rename = "type")]
    pub kind: ModerationKind,
    pub timestamp: DateTime<Utc>,
    pub channel_id: String,
    pub channel_login: Option<String>,
    /// Who was banned, timed out or unbanned, or whose message was deleted. Absent when the chat
    /// was cleared, and when the author of a deleted message is not known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<ModerationPerson>,
    /// Who did it. Absent if Kick does not say (it does not for some moderators), and for
    /// deleted messages.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub moderator: Option<ModerationPerson>,
    /// The length of a timeout
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_seconds: Option<u64>,
    /// When a timeout ends
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
    /// Only for unbans: whether Kick says the ban which was lifted was a permanent one
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permanent: Option<bool>,
    /// Only for deleted messages: the id of the message
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    /// Only for deleted messages: what the message said, if it was seen
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Only for deleted messages: whether the AI moderation of Kick deleted the message
    #[serde(skip_serializing_if = "is_false")]
    pub ai_moderated: bool,
    /// Only for deleted messages: the rules the AI moderation found the message to violate
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub violated_rules: Vec<String>,
    /// Only for bans and timeouts in the history of a user: how it ended. Absent if it is still
    /// going on, as far as the logs tell.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end: Option<ModerationEndResponse>,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// The bans, timeouts and unbans of a user in a channel
#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct UserBansResponse {
    pub channel_id: String,
    pub channel_login: Option<String>,
    pub user_id: String,
    pub user_login: Option<String>,
    /// Whether a ban or a timeout of the user is going on in the channel, as far as the logs
    /// tell
    pub banned: bool,
    /// The number of permanent bans in the whole history, not only in the entries returned
    pub bans: u64,
    pub timeouts: u64,
    pub unbans: u64,
    /// The number of entries in the whole history
    pub total: u64,
    /// The history is longer than what is read, the oldest entries are missing
    pub truncated: bool,
    pub actions: Vec<ModerationActionResponse>,
}

/// The bans, timeouts and unbans of a user in all channels
#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct UserBansAcrossChannelsResponse {
    pub user_id: String,
    pub user_login: Option<String>,
    /// The channels in which a ban or a timeout of the user is going on, as far as the logs tell
    pub banned_in: Vec<ModerationChannel>,
    /// The number of permanent bans in the whole history, not only in the entries returned
    pub bans: u64,
    pub timeouts: u64,
    pub unbans: u64,
    /// The number of entries in the whole history
    pub total: u64,
    /// The history is longer than what is read, the oldest entries are missing
    pub truncated: bool,
    pub actions: Vec<ModerationActionResponse>,
}

/// The moderation actions of a channel
#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ChannelModerationResponse {
    pub channel_id: String,
    pub channel_login: Option<String>,
    /// The newest first
    pub actions: Vec<ModerationActionResponse>,
}

/// The moderators of a channel with the most actions in a calendar period
#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ChannelModeratorsResponse {
    pub channel_id: String,
    pub channel_login: Option<String>,
    pub period: Period,
    /// Start of the period, absent for `all`
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<DateTime<Utc>>,
    /// End of the period (exclusive), absent for `all`
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<DateTime<Utc>>,
    pub moderators: Vec<ModeratorResponse>,
}

#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ModeratorResponse {
    pub moderator: ModerationPerson,
    /// Permanent bans
    pub bans: u64,
    pub timeouts: u64,
    pub unbans: u64,
    /// All of them
    pub total: u64,
}

/// Totals of a user over all the channels they chatted in
#[derive(Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct UserSummaryResponse {
    pub user_id: String,
    pub user_login: Option<String>,
    /// Number of chat messages in all channels
    pub message_count: u64,
    /// In how many channels the user chatted
    pub channel_count: u64,
    pub first_message: DateTime<Utc>,
    pub last_message: DateTime<Utc>,
    /// The channel of the most recent message
    pub last_channel_id: String,
    pub last_channel_login: Option<String>,
}
