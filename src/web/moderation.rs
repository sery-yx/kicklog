//! Handlers of the moderation endpoints: the bans and timeouts of a user and what became of
//! them, the moderation actions of a channel and the moderators who do them.
//!
//! Kick tells who banned or timed out a user and who lifted it, which the chat of Twitch does
//! not, so these endpoints can answer who did it, whether the user was unbanned, when and by
//! whom. Everything is as far as it was logged: actions from before rustlog joined a chat are
//! not known.

use super::{
    analytics::{
        cache_for_range, check_user_not_opted_out, clamp_limit, opted_out_ids, period_bounds,
        resolve_channel, resolve_period, resolve_user, CURRENT_CACHE_SECONDS, DEFAULT_LIMIT,
    },
    handlers::{cache_header, resolve_user_params},
    schema::{
        BansParams, ChannelModeratorsResponse, ChannelModerationResponse, LogsPathChannel,
        ModerationActionResponse, ModerationChannel, ModerationEndResponse, ModerationFeedParams,
        ModerationPerson, ModeratorResponse, SortOrder, TopParams, UserBansAcrossChannelsResponse,
        UserBansResponse, UserLogPathParams, UserPathParams,
    },
};
use crate::{
    app::App,
    db::moderation::{self as moderation_db, ChannelActionsFilter},
    error::Error,
    logs::{
        moderation::{self, HistoryEntry, ModerationKind},
        period::{self, DateRange},
    },
    Result,
};
use aide::axum::IntoApiResponse;
use axum::{
    extract::{Path, Query, State},
    Json,
};
use chrono::Utc;
use dashmap::DashSet;

const DEFAULT_BANS_LIMIT: u64 = 100;
const DEFAULT_FEED_LIMIT: u64 = 50;
const MAX_ENTRIES: u64 = 500;
/// How many actions of a user are used to work out their history
const MAX_HISTORY_ROWS: u64 = 2000;

/// The bans, timeouts and unbans of a user in a channel: who did them, how they ended
pub async fn get_user_channel_bans(
    app: State<App>,
    Path(user_params): Path<UserLogPathParams>,
    Query(params): Query<BansParams>,
) -> Result<impl IntoApiResponse> {
    let (channel_id, user_id) = resolve_user_params(&user_params, &app).await?;
    app.check_opted_out(&channel_id, Some(&user_id))?;

    // One row more than is used, to know whether there are more
    let mut actions = moderation_db::get_user_punishment_actions(
        &app.db,
        &user_id,
        Some(&channel_id),
        MAX_HISTORY_ROWS + 1,
        &[],
    )
    .await?;
    let truncated = actions.len() as u64 > MAX_HISTORY_ROWS;
    // The newest action comes first, so the oldest one is dropped
    actions.truncate(MAX_HISTORY_ROWS as usize);
    let channel_login = actions
        .first()
        .map(|action| action.channel_login.clone())
        .filter(|login| !login.is_empty());
    let user_login = actions
        .first()
        .map(|action| action.user_login.clone())
        .filter(|login| !login.is_empty());

    let entries = moderation::history(actions, Utc::now());
    let banned = !moderation::channels_with_active_punishment(&entries).is_empty();
    let totals = Totals::of(&entries);
    let actions = page(entries, &params, &app.optout_users);

    Ok((
        cache_header(CURRENT_CACHE_SECONDS),
        Json(UserBansResponse {
            channel_login: login_of(&app, &channel_id, channel_login).await,
            channel_id,
            user_login: login_of(&app, &user_id, user_login).await,
            user_id,
            banned,
            bans: totals.bans,
            timeouts: totals.timeouts,
            unbans: totals.unbans,
            total: totals.total,
            truncated,
            actions,
        }),
    ))
}

/// The bans, timeouts and unbans of a user in all channels: who did them, how they ended
pub async fn get_user_bans(
    app: State<App>,
    Path(UserPathParams {
        user_id_type,
        user,
    }): Path<UserPathParams>,
    Query(params): Query<BansParams>,
) -> Result<impl IntoApiResponse> {
    let user_id = resolve_user(&app, &user_id_type, &user).await?;
    check_user_not_opted_out(&app, &user_id)?;

    // Channels which opted out are not listed
    let excluded = opted_out_ids(&app.optout_users);
    // One row more than is used, to know whether there are more
    let mut actions = moderation_db::get_user_punishment_actions(
        &app.db,
        &user_id,
        None,
        MAX_HISTORY_ROWS + 1,
        &excluded,
    )
    .await?;
    let truncated = actions.len() as u64 > MAX_HISTORY_ROWS;
    // The newest action comes first, so the oldest one is dropped
    actions.truncate(MAX_HISTORY_ROWS as usize);
    let user_login = actions
        .first()
        .map(|action| action.user_login.clone())
        .filter(|login| !login.is_empty());

    let entries = moderation::history(actions, Utc::now());
    let banned_in = moderation::channels_with_active_punishment(&entries)
        .into_iter()
        .map(|(id, login)| ModerationChannel {
            id,
            login: non_empty(&login),
        })
        .collect();
    let totals = Totals::of(&entries);
    let actions = page(entries, &params, &app.optout_users);

    Ok((
        cache_header(CURRENT_CACHE_SECONDS),
        Json(UserBansAcrossChannelsResponse {
            user_login: login_of(&app, &user_id, user_login).await,
            user_id,
            banned_in,
            bans: totals.bans,
            timeouts: totals.timeouts,
            unbans: totals.unbans,
            total: totals.total,
            truncated,
            actions,
        }),
    ))
}

/// The moderation actions of a channel: bans, timeouts, unbans, deleted messages and cleared
/// chats, the newest first
pub async fn get_channel_moderation(
    app: State<App>,
    Path(LogsPathChannel {
        channel_id_type,
        channel,
    }): Path<LogsPathChannel>,
    Query(params): Query<ModerationFeedParams>,
) -> Result<impl IntoApiResponse> {
    let channel_id = resolve_channel(&app, &channel_id_type, &channel).await?;
    app.check_opted_out(&channel_id, None)?;

    let moderator_id = match (params.moderator_id.as_deref(), params.moderator.as_deref()) {
        (Some(id), _) => Some(id.trim().to_owned()),
        (None, Some(name)) => Some(app.get_user_id_by_name(name).await?),
        (None, None) => None,
    };
    // Asking for the actions of a moderator who opted out would name them as well
    if let Some(moderator_id) = moderator_id.as_deref() {
        check_user_not_opted_out(&app, moderator_id)?;
    }

    let all_time = DateRange::all_time();
    let from = match params.from.as_deref() {
        Some(text) => period::parse_instant(text)
            .ok_or_else(|| Error::InvalidParam("Invalid `from` time".to_owned()))?,
        None => all_time.start_time(),
    };
    let to = match params.to.as_deref() {
        Some(text) => period::parse_instant(text)
            .ok_or_else(|| Error::InvalidParam("Invalid `to` time".to_owned()))?,
        None => all_time.end_time(),
    };
    if from >= to {
        return Err(Error::InvalidParam("`from` must be before `to`".to_owned()));
    }

    let limit = clamp_entries(params.limit, DEFAULT_FEED_LIMIT);
    let offset = params.offset.unwrap_or(0);

    // Actions about users who opted out are left out
    let excluded = opted_out_ids(&app.optout_users);
    let filter = ChannelActionsFilter {
        kind: params.kind,
        moderator_id: moderator_id.as_deref(),
        from,
        to,
    };
    let actions = moderation_db::get_channel_actions(
        &app.db,
        &channel_id,
        &filter,
        limit,
        offset,
        &excluded,
    )
    .await?;

    let channel_login = actions
        .first()
        .map(|action| action.channel_login.clone())
        .filter(|login| !login.is_empty());
    let actions = actions
        .into_iter()
        .map(|action| action_response(HistoryEntry { action, end: None }, &app.optout_users))
        .collect();

    Ok((
        cache_header(CURRENT_CACHE_SECONDS),
        Json(ChannelModerationResponse {
            channel_login: login_of(&app, &channel_id, channel_login).await,
            channel_id,
            actions,
        }),
    ))
}

/// The moderators of a channel with the most bans, timeouts and unbans in a calendar period
pub async fn get_channel_moderators(
    app: State<App>,
    Path(LogsPathChannel {
        channel_id_type,
        channel,
    }): Path<LogsPathChannel>,
    Query(params): Query<TopParams>,
) -> Result<impl IntoApiResponse> {
    let channel_id = resolve_channel(&app, &channel_id_type, &channel).await?;
    app.check_opted_out(&channel_id, None)?;

    let range = resolve_period(params.period, params.date.as_deref())?;
    let limit = clamp_limit(params.limit, DEFAULT_LIMIT);

    // Moderators who opted out are not listed
    let excluded = opted_out_ids(&app.optout_users);
    let stats =
        moderation_db::get_moderator_stats(&app.db, &channel_id, range, limit, &excluded).await?;

    let moderators = stats
        .into_iter()
        .map(|stats| ModeratorResponse {
            total: stats.bans + stats.timeouts + stats.unbans,
            moderator: ModerationPerson {
                login: non_empty(&stats.moderator_login),
                id: stats.moderator_id,
            },
            bans: stats.bans,
            timeouts: stats.timeouts,
            unbans: stats.unbans,
        })
        .collect();

    let (from, to) = period_bounds(params.period, &range);
    Ok((
        cache_for_range(&range),
        Json(ChannelModeratorsResponse {
            channel_login: login_of(&app, &channel_id, None).await,
            channel_id,
            period: params.period,
            from,
            to,
            moderators,
        }),
    ))
}

/// The login of a user or channel: the one known from the logs, or else what Kick says. This is
/// only a convenience, a failure to look it up is not worth an error.
async fn login_of(app: &App, id: &str, known: Option<String>) -> Option<String> {
    if known.is_some() {
        return known;
    }

    app.get_users(vec![id.to_owned()], vec![], false)
        .await
        .ok()?
        .remove(id)
}

fn non_empty(text: &str) -> Option<String> {
    if text.is_empty() {
        None
    } else {
        Some(text.to_owned())
    }
}

fn clamp_entries(limit: Option<u64>, default: u64) -> u64 {
    limit.unwrap_or(default).clamp(1, MAX_ENTRIES)
}

/// A user named in a moderation action, if there is one. Nobody is named who opted out: Kick
/// shows who moderators are, but they have the right not to be in these logs.
fn person(id: &str, login: &str, opt_out: &DashSet<String>) -> Option<ModerationPerson> {
    if id.is_empty() || opt_out.contains(id) {
        return None;
    }

    Some(ModerationPerson {
        id: id.to_owned(),
        login: non_empty(login),
    })
}

fn action_response(entry: HistoryEntry, opt_out: &DashSet<String>) -> ModerationActionResponse {
    let HistoryEntry { action, end } = entry;
    let kind = action.kind;

    ModerationActionResponse {
        kind,
        timestamp: action.timestamp,
        user: person(&action.user_id, &action.user_login, opt_out),
        moderator: person(&action.moderator_id, &action.moderator_login, opt_out),
        duration_seconds: action.duration_seconds,
        expires_at: action.expires_at,
        permanent: (kind == ModerationKind::Unban).then_some(action.permanent),
        message_id: non_empty(&action.message_id),
        text: non_empty(&action.text),
        ai_moderated: action.ai_moderated,
        violated_rules: action.violated_rules,
        end: end.map(|end| ModerationEndResponse {
            at: end.at,
            reason: end.reason,
            moderator: person(&end.moderator_id, &end.moderator_login, opt_out),
        }),
        channel_login: non_empty(&action.channel_login),
        channel_id: action.channel_id,
    }
}

/// How many entries of each kind a history has
struct Totals {
    bans: u64,
    timeouts: u64,
    unbans: u64,
    total: u64,
}

impl Totals {
    fn of(entries: &[HistoryEntry]) -> Self {
        Self {
            bans: moderation::count_kind(entries, ModerationKind::Ban),
            timeouts: moderation::count_kind(entries, ModerationKind::Timeout),
            unbans: moderation::count_kind(entries, ModerationKind::Unban),
            total: entries.len() as u64,
        }
    }
}

/// The entries which were asked for, in the order that was asked for. `entries` is in
/// chronological order.
fn page(
    mut entries: Vec<HistoryEntry>,
    params: &BansParams,
    opt_out: &DashSet<String>,
) -> Vec<ModerationActionResponse> {
    if params.order == SortOrder::Desc {
        entries.reverse();
    }
    let limit = clamp_entries(params.limit, DEFAULT_BANS_LIMIT);
    let offset = params.offset.unwrap_or(0);

    entries
        .into_iter()
        .skip(usize::try_from(offset).unwrap_or(usize::MAX))
        .take(limit as usize)
        .map(|entry| action_response(entry, opt_out))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logs::moderation::{EndReason, ModerationAction, PunishmentEnd};
    use chrono::DateTime;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    fn at(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp_millis(seconds * 1000).unwrap()
    }

    fn action(seconds: i64, kind: ModerationKind) -> ModerationAction {
        ModerationAction {
            channel_id: "676".to_owned(),
            channel_login: "xqc".to_owned(),
            timestamp: at(seconds),
            kind,
            user_id: "10".to_owned(),
            user_login: "target".to_owned(),
            moderator_id: "20".to_owned(),
            moderator_login: "mod".to_owned(),
            duration_seconds: None,
            expires_at: None,
            permanent: false,
            message_id: String::new(),
            text: String::new(),
            ai_moderated: false,
            violated_rules: vec![],
        }
    }

    fn entry(action: ModerationAction) -> HistoryEntry {
        HistoryEntry { action, end: None }
    }

    #[test]
    fn missing_names_are_left_out() {
        assert_eq!(non_empty(""), None);
        assert_eq!(non_empty("mod"), Some("mod".to_owned()));
    }

    #[test]
    fn nobody_is_named_who_opted_out() {
        let opt_out: DashSet<String> = DashSet::new();
        assert_eq!(
            person("20", "mod", &opt_out).map(|person| person.id),
            Some("20".to_owned())
        );
        assert!(person("", "mod", &opt_out).is_none());

        opt_out.insert("20".to_owned());
        assert!(person("20", "mod", &opt_out).is_none());
    }

    #[test]
    fn a_timeout_tells_who_did_it_and_how_it_ended() {
        let timeout = ModerationAction {
            duration_seconds: Some(120),
            expires_at: Some(at(220)),
            ..action(100, ModerationKind::Timeout)
        };
        let response = action_response(
            HistoryEntry {
                action: timeout,
                end: Some(PunishmentEnd {
                    at: at(160),
                    reason: EndReason::Unban,
                    moderator_id: "7".to_owned(),
                    moderator_login: "unbanner".to_owned(),
                }),
            },
            &DashSet::new(),
        );

        assert_eq!(
            serde_json::to_value(&response).unwrap(),
            json!({
                "type": "timeout",
                "timestamp": "1970-01-01T00:01:40Z",
                "channelId": "676",
                "channelLogin": "xqc",
                "user": {"id": "10", "login": "target"},
                "moderator": {"id": "20", "login": "mod"},
                "durationSeconds": 120,
                "expiresAt": "1970-01-01T00:03:40Z",
                "end": {
                    "at": "1970-01-01T00:02:40Z",
                    "reason": "unban",
                    "moderator": {"id": "7", "login": "unbanner"}
                }
            })
        );
    }

    #[test]
    fn a_moderator_which_is_not_known_is_absent() {
        let ban = ModerationAction {
            moderator_id: String::new(),
            moderator_login: String::new(),
            ..action(100, ModerationKind::Ban)
        };

        let response = serde_json::to_value(action_response(entry(ban), &DashSet::new())).unwrap();

        assert_eq!(response["type"], "ban");
        assert!(response.get("moderator").is_none());
        assert!(response.get("end").is_none());
        assert!(response.get("permanent").is_none());
    }

    #[test]
    fn an_unban_says_whether_the_ban_was_permanent() {
        let unban = ModerationAction {
            permanent: true,
            ..action(100, ModerationKind::Unban)
        };

        let response = serde_json::to_value(action_response(entry(unban), &DashSet::new())).unwrap();

        assert_eq!(response["type"], "unban");
        assert_eq!(response["permanent"], true);
    }

    #[test]
    fn a_deleted_message_has_the_details_of_the_ai_moderation() {
        let deleted = ModerationAction {
            moderator_id: String::new(),
            moderator_login: String::new(),
            message_id: "c52ca12d-3cd1-4471-80ed-2cf73bac96a1".to_owned(),
            text: "hello".to_owned(),
            ai_moderated: true,
            violated_rules: vec!["sexual".to_owned()],
            ..action(100, ModerationKind::Delete)
        };

        let response = serde_json::to_value(action_response(entry(deleted), &DashSet::new())).unwrap();

        assert_eq!(response["type"], "delete");
        assert_eq!(response["messageId"], "c52ca12d-3cd1-4471-80ed-2cf73bac96a1");
        assert_eq!(response["text"], "hello");
        assert_eq!(response["aiModerated"], true);
        assert_eq!(response["violatedRules"], json!(["sexual"]));
        assert_eq!(response["user"], json!({"id": "10", "login": "target"}));
    }

    #[test]
    fn what_is_not_there_is_not_in_the_json() {
        let clear = ModerationAction {
            user_id: String::new(),
            user_login: String::new(),
            moderator_id: String::new(),
            moderator_login: String::new(),
            ..action(100, ModerationKind::Clear)
        };

        let response = serde_json::to_value(action_response(entry(clear), &DashSet::new())).unwrap();

        assert_eq!(
            response,
            json!({
                "type": "clear",
                "timestamp": "1970-01-01T00:01:40Z",
                "channelId": "676",
                "channelLogin": "xqc"
            })
        );
    }

    #[test]
    fn a_moderator_who_opted_out_is_not_named() {
        let opt_out: DashSet<String> = DashSet::new();
        opt_out.insert("20".to_owned());

        let response = action_response(entry(action(100, ModerationKind::Ban)), &opt_out);

        assert!(response.moderator.is_none());
        // the action is still there, it is about somebody else
        assert_eq!(response.user.map(|user| user.id), Some("10".to_owned()));
    }

    #[test]
    fn pages_follow_the_order_the_limit_and_the_offset() {
        let entries: Vec<HistoryEntry> = (1..=5)
            .map(|index| entry(action(index * 100, ModerationKind::Ban)))
            .collect();
        let timestamps = |responses: Vec<ModerationActionResponse>| -> Vec<i64> {
            responses
                .iter()
                .map(|response| response.timestamp.timestamp())
                .collect()
        };

        let newest_first = BansParams {
            order: SortOrder::Desc,
            limit: Some(2),
            offset: Some(1),
        };
        assert_eq!(
            timestamps(page(entries.clone(), &newest_first, &DashSet::new())),
            vec![400, 300]
        );

        let oldest_first = BansParams {
            order: SortOrder::Asc,
            limit: Some(2),
            offset: None,
        };
        assert_eq!(
            timestamps(page(entries.clone(), &oldest_first, &DashSet::new())),
            vec![100, 200]
        );

        // beyond the end there is nothing
        let beyond = BansParams {
            order: SortOrder::Asc,
            limit: None,
            offset: Some(10),
        };
        assert_eq!(timestamps(page(entries, &beyond, &DashSet::new())), Vec::<i64>::new());
    }

    #[test]
    fn limits_are_clamped() {
        assert_eq!(clamp_entries(None, 100), 100);
        assert_eq!(clamp_entries(Some(0), 100), 1);
        assert_eq!(clamp_entries(Some(250), 100), 250);
        assert_eq!(clamp_entries(Some(100_000), 100), MAX_ENTRIES);
    }

    #[test]
    fn totals_count_the_whole_history() {
        let entries = moderation::history(
            vec![
                action(100, ModerationKind::Ban),
                action(200, ModerationKind::Timeout),
                action(300, ModerationKind::Timeout),
                action(400, ModerationKind::Unban),
            ],
            at(1000),
        );

        let totals = Totals::of(&entries);

        assert_eq!(
            (totals.bans, totals.timeouts, totals.unbans, totals.total),
            (1, 2, 1, 4)
        );
    }
}
