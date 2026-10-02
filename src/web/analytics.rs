//! Handlers of the statistics endpoints: leaderboards, activity over time and user summaries.
//!
//! These endpoints only count chat messages. Periods are calendar periods in UTC.

use super::{
    handlers::{cache_header, no_cache_header, resolve_user_params},
    responders::logs::LogsResponse,
    schema::{
        ActivityBucketResponse, ActivityParams, ActivityResponse, ChannelIdType,
        ChannelMessageCount, ChannelTopResponse, ChannelsTopResponse, LogsParams, LogsPathChannel,
        PeriodParams, TopParams, UserChannelResponse, UserChannelsParams, UserChannelsResponse,
        UserChannelsSort, UserIdType, UserLogPathParams, UserLogsStats, UserPathParams,
        UserRankResponse, UserSummaryResponse,
    },
};
use crate::{
    app::App,
    db::{analytics, schema::StructuredMessage},
    error::Error,
    logs::{
        period::{self, DateRange, Interval, Period},
        stream::LogsStream,
    },
    Result,
};
use aide::axum::IntoApiResponse;
use axum::{
    extract::{Path, Query, State},
    Json,
};
use axum_extra::{headers::CacheControl, TypedHeader};
use chrono::{DateTime, NaiveDate, Utc};
use dashmap::DashMap;
use std::collections::HashMap;

pub(super) const DEFAULT_LIMIT: u64 = 10;
const DEFAULT_CHANNELS_LIMIT: u64 = 25;
pub(super) const MAX_LIMIT: u64 = 100;
/// Results which include the current day keep changing
pub(super) const CURRENT_CACHE_SECONDS: u64 = 60;
/// Results of days which are over do not change anymore
const FINISHED_CACHE_SECONDS: u64 = 36000;

/// The chatters of a channel with the most messages in a calendar period
pub async fn get_channel_top(
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

    // Users who opted out are not counted
    let excluded = opted_out_ids(&app.config.opt_out);
    let (message_count, rows) =
        analytics::get_top_chatters(&app.db, &channel_id, range, limit, &excluded).await?;

    let user_ids = rows.iter().map(|row| row.user_id.clone()).collect();
    let mut users = app.get_users(user_ids, vec![], false).await?;

    let top_chatters = rows
        .into_iter()
        .map(|row| UserLogsStats {
            user_login: users.remove(&row.user_id),
            user_id: row.user_id,
            message_count: row.message_count,
        })
        .collect();

    let (from, to) = period_bounds(params.period, &range);
    Ok((
        cache_for_range(&range),
        Json(ChannelTopResponse {
            period: params.period,
            from,
            to,
            message_count,
            top_chatters,
        }),
    ))
}

/// The channels with the most messages in a calendar period
pub async fn get_top_channels(
    app: State<App>,
    Query(params): Query<TopParams>,
) -> Result<impl IntoApiResponse> {
    let range = resolve_period(params.period, params.date.as_deref())?;
    let limit = clamp_limit(params.limit, DEFAULT_LIMIT);

    // Channels which opted out are not listed
    let excluded = opted_out_ids(&app.config.opt_out);
    let rows = analytics::get_top_channels(&app.db, range, limit, &excluded).await?;

    let channel_ids = rows.iter().map(|row| row.channel_id.clone()).collect();
    let mut logins = app.get_users(channel_ids, vec![], false).await?;

    let channels = rows
        .into_iter()
        .map(|row| ChannelMessageCount {
            channel_login: logins.remove(&row.channel_id),
            channel_id: row.channel_id,
            message_count: row.message_count,
        })
        .collect();

    let (from, to) = period_bounds(params.period, &range);
    Ok((
        cache_for_range(&range),
        Json(ChannelsTopResponse {
            period: params.period,
            from,
            to,
            channels,
        }),
    ))
}

/// Messages and different chatters of a channel per day, week, month or year
pub async fn get_channel_activity(
    app: State<App>,
    Path(LogsPathChannel {
        channel_id_type,
        channel,
    }): Path<LogsPathChannel>,
    Query(params): Query<ActivityParams>,
) -> Result<impl IntoApiResponse> {
    let channel_id = resolve_channel(&app, &channel_id_type, &channel).await?;
    app.check_opted_out(&channel_id, None)?;

    let range = resolve_activity_range(&params, Utc::now().date_naive())?;
    // Users who opted out are not counted
    let excluded = opted_out_ids(&app.config.opt_out);
    let buckets = analytics::get_channel_activity(
        &app.db,
        &channel_id,
        params.interval,
        range,
        &excluded,
    )
    .await?;

    Ok((
        cache_for_range(&range),
        Json(activity_response(params.interval, range, buckets, true)),
    ))
}

/// Messages of a user in a channel per day, week, month or year
pub async fn get_user_activity(
    app: State<App>,
    Path(user_params): Path<UserLogPathParams>,
    Query(params): Query<ActivityParams>,
) -> Result<impl IntoApiResponse> {
    let (channel_id, user_id) = resolve_user_params(&user_params, &app).await?;
    app.check_opted_out(&channel_id, Some(&user_id))?;

    let range = resolve_activity_range(&params, Utc::now().date_naive())?;
    let buckets =
        analytics::get_user_activity(&app.db, &channel_id, &user_id, params.interval, range)
            .await?;

    Ok((
        cache_for_range(&range),
        Json(activity_response(params.interval, range, buckets, false)),
    ))
}

/// The number of messages of a user in a calendar period, and the rank of the user among the
/// chatters of the channel
pub async fn get_user_rank(
    app: State<App>,
    Path(user_params): Path<UserLogPathParams>,
    Query(params): Query<PeriodParams>,
) -> Result<impl IntoApiResponse> {
    let (channel_id, user_id) = resolve_user_params(&user_params, &app).await?;
    app.check_opted_out(&channel_id, Some(&user_id))?;

    let range = resolve_period(params.period, params.date.as_deref())?;
    // Users who opted out are not counted
    let excluded = opted_out_ids(&app.config.opt_out);
    let rank =
        analytics::get_user_rank(&app.db, &channel_id, &user_id, range, &excluded).await?;
    let user_login = app
        .get_users(vec![user_id.clone()], vec![], false)
        .await?
        .into_values()
        .next();

    let (from, to) = period_bounds(params.period, &range);
    Ok((
        cache_for_range(&range),
        Json(UserRankResponse {
            user_id,
            user_login,
            period: params.period,
            from,
            to,
            message_count: rank.message_count,
            rank: rank.rank,
            total_chatters: rank.total_chatters,
        }),
    ))
}

/// The first chat message of a user in a channel
pub async fn get_user_first_line(
    app: State<App>,
    Path(user_params): Path<UserLogPathParams>,
    Query(logs_params): Query<LogsParams>,
) -> Result<impl IntoApiResponse> {
    let (channel_id, user_id) = resolve_user_params(&user_params, &app).await?;
    app.check_opted_out(&channel_id, Some(&user_id))?;

    let line = match analytics::read_user_edge_line(&app.db, &channel_id, &user_id, false).await {
        Ok(line) => line,
        // The first message may still be waiting to be written to the database
        Err(Error::NotFound) => app
            .flush_buffer
            .first_message_by_channel_and_user(&channel_id, &user_id)
            .await
            .ok_or(Error::NotFound)?,
        Err(err) => return Err(err),
    };

    respond_with_line(line, logs_params)
}

/// The most recent chat message of a user in a channel
pub async fn get_user_last_line(
    app: State<App>,
    Path(user_params): Path<UserLogPathParams>,
    Query(logs_params): Query<LogsParams>,
) -> Result<impl IntoApiResponse> {
    let (channel_id, user_id) = resolve_user_params(&user_params, &app).await?;
    app.check_opted_out(&channel_id, Some(&user_id))?;

    // Messages which were not written to the database yet are newer than the ones in it
    let line = match app
        .flush_buffer
        .last_message_by_channel_and_user(&channel_id, &user_id)
        .await
    {
        Some(line) => line,
        None => analytics::read_user_edge_line(&app.db, &channel_id, &user_id, true).await?,
    };

    respond_with_line(line, logs_params)
}

/// The channels a user has chatted in, with the number of messages and the time of the first and
/// last message
pub async fn get_user_channels(
    app: State<App>,
    Path(UserPathParams {
        user_id_type,
        user,
    }): Path<UserPathParams>,
    Query(params): Query<UserChannelsParams>,
) -> Result<impl IntoApiResponse> {
    let user_id = resolve_user(&app, &user_id_type, &user).await?;
    check_user_not_opted_out(&app, &user_id)?;

    let limit = clamp_limit(params.limit, DEFAULT_CHANNELS_LIMIT);
    let offset = params.offset.unwrap_or(0);

    // Channels which opted out are not listed
    let excluded = opted_out_ids(&app.config.opt_out);
    let rows = analytics::get_user_channels(
        &app.db,
        &user_id,
        params.sort,
        limit,
        offset,
        &excluded,
    )
    .await?;
    if rows.is_empty() && offset == 0 {
        return Err(Error::NotFound);
    }

    let mut ids: Vec<String> = rows.iter().map(|row| row.channel_id.clone()).collect();
    ids.push(user_id.clone());
    ids.sort();
    ids.dedup();
    let logins = app.get_users(ids, vec![], false).await?;

    let channels = rows
        .into_iter()
        .map(|row| UserChannelResponse {
            channel_login: logins.get(&row.channel_id).cloned(),
            channel_id: row.channel_id,
            message_count: row.message_count,
            first_message: row.first_message,
            last_message: row.last_message,
        })
        .collect();

    Ok((
        cache_header(CURRENT_CACHE_SECONDS),
        Json(UserChannelsResponse {
            user_login: logins.get(&user_id).cloned(),
            user_id,
            channels,
        }),
    ))
}

/// The most recent chat message of a user in any channel
pub async fn get_user_last_message(
    app: State<App>,
    Path(UserPathParams {
        user_id_type,
        user,
    }): Path<UserPathParams>,
    Query(logs_params): Query<LogsParams>,
) -> Result<impl IntoApiResponse> {
    let user_id = resolve_user(&app, &user_id_type, &user).await?;
    check_user_not_opted_out(&app, &user_id)?;

    // Messages which were not written to the database yet are newer than the ones in it.
    // Channels which opted out are skipped, the message before is the answer then.
    let line = match app
        .flush_buffer
        .last_message_by_user(&user_id, |message| {
            !app.config.opt_out.contains_key(&*message.channel_id)
        })
        .await
    {
        Some(line) => line,
        None => {
            // The newest channel which has not opted out
            let excluded = opted_out_ids(&app.config.opt_out);
            let channel = analytics::get_user_channels(
                &app.db,
                &user_id,
                UserChannelsSort::Last,
                1,
                0,
                &excluded,
            )
            .await?
            .into_iter()
            .next()
            .ok_or(Error::NotFound)?;

            analytics::read_user_edge_line(&app.db, &channel.channel_id, &user_id, true).await?
        }
    };

    respond_with_line(line, logs_params)
}

/// Totals of a user over all channels: messages, channels, first and last message
pub async fn get_user_summary(
    app: State<App>,
    Path(UserPathParams {
        user_id_type,
        user,
    }): Path<UserPathParams>,
) -> Result<impl IntoApiResponse> {
    let user_id = resolve_user(&app, &user_id_type, &user).await?;
    check_user_not_opted_out(&app, &user_id)?;

    // Channels which opted out are not counted
    let excluded = opted_out_ids(&app.config.opt_out);
    let summary = analytics::get_user_summary(&app.db, &user_id, &excluded)
        .await?
        .ok_or(Error::NotFound)?;

    let mut ids = vec![user_id.clone(), summary.last_channel_id.clone()];
    ids.sort();
    ids.dedup();
    let logins = app.get_users(ids, vec![], false).await?;

    Ok((
        cache_header(CURRENT_CACHE_SECONDS),
        Json(UserSummaryResponse {
            user_login: logins.get(&user_id).cloned(),
            user_id,
            message_count: summary.message_count,
            channel_count: summary.channel_count,
            first_message: summary.first_message,
            last_message: summary.last_message,
            last_channel_login: logins.get(&summary.last_channel_id).cloned(),
            last_channel_id: summary.last_channel_id,
        }),
    ))
}

fn respond_with_line(
    line: StructuredMessage<'static>,
    logs_params: LogsParams,
) -> Result<impl IntoApiResponse> {
    let stream = LogsStream::new_provided(vec![line])?;

    let logs = LogsResponse {
        stream,
        response_type: logs_params.response_type(),
    };
    Ok((no_cache_header(), logs))
}

pub(super) async fn resolve_channel(app: &App, id_type: &ChannelIdType, channel: &str) -> Result<String> {
    match id_type {
        ChannelIdType::Name => app.get_user_id_by_name(channel).await,
        ChannelIdType::Id => Ok(channel.to_owned()),
    }
}

pub(super) async fn resolve_user(app: &App, id_type: &UserIdType, user: &str) -> Result<String> {
    match id_type {
        UserIdType::Name => app.get_user_id_by_name(user).await,
        UserIdType::Id => Ok(user.to_owned()),
    }
}

pub(super) fn check_user_not_opted_out(app: &App, user_id: &str) -> Result<()> {
    app.check_opted_out("", Some(user_id))
}

pub(super) fn clamp_limit(limit: Option<u64>, default: u64) -> u64 {
    limit.unwrap_or(default).clamp(1, MAX_LIMIT)
}

/// The ids of everybody who opted out. Users and channels share the list. It is sorted, so the
/// same queries are sent while nobody changes, which lets the database reuse their results.
pub(super) fn opted_out_ids(opt_out: &DashMap<String, bool>) -> Vec<String> {
    let mut ids: Vec<String> = opt_out.iter().map(|entry| entry.key().clone()).collect();
    ids.sort_unstable();
    ids
}

/// The days of the requested period. Without a `date` it is the current one, and it is
/// not looked at for `all`.
pub(super) fn resolve_period(period: Period, date: Option<&str>) -> Result<DateRange> {
    if period == Period::All {
        return Ok(DateRange::all_time());
    }

    let anchor = match date {
        Some(text) => period::parse_date(text)
            .ok_or_else(|| Error::InvalidParam("Invalid date".to_owned()))?,
        None => Utc::now().date_naive(),
    };

    period
        .range_or_all(anchor)
        .ok_or_else(|| Error::InvalidParam("Date out of range".to_owned()))
}

/// The start and the end of a period for the response, which are not given for `all`
pub(super) fn period_bounds(
    period: Period,
    range: &DateRange,
) -> (Option<DateTime<Utc>>, Option<DateTime<Utc>>) {
    if period == Period::All {
        (None, None)
    } else {
        (Some(range.start_time()), Some(range.end_time()))
    }
}

fn resolve_activity_range(params: &ActivityParams, today: NaiveDate) -> Result<DateRange> {
    let parse = |text: &str, name: &str| -> Result<NaiveDate> {
        period::parse_date_or_timestamp(text)
            .ok_or_else(|| Error::InvalidParam(format!("Invalid `{name}` date")))
    };

    let from = params
        .from
        .as_deref()
        .map(|text| parse(text, "from"))
        .transpose()?;
    let to = params
        .to
        .as_deref()
        .map(|text| parse(text, "to"))
        .transpose()?;

    period::activity_range(params.interval, from, to, today)
        .map_err(|message| Error::InvalidParam(message.to_owned()))
}

/// Results which include the current day are cached shorter than finished ones. A range counts
/// as finished only a while after its end: messages are written in batches and the database
/// reuses results for a few minutes, so the numbers of the last minutes may still be coming in.
pub(super) fn cache_for_range(range: &DateRange) -> TypedHeader<CacheControl> {
    if range.end_time() + chrono::Duration::minutes(10) > Utc::now() {
        cache_header(CURRENT_CACHE_SECONDS)
    } else {
        cache_header(FINISHED_CACHE_SECONDS)
    }
}

/// Builds the response from the buckets which have messages, intervals without messages
/// are added with a count of zero so the series has no gaps
fn activity_response(
    interval: Interval,
    range: DateRange,
    found: Vec<analytics::ActivityBucket>,
    with_chatters: bool,
) -> ActivityResponse {
    let mut found: HashMap<NaiveDate, analytics::ActivityBucket> = found
        .into_iter()
        .map(|bucket| (bucket.start, bucket))
        .collect();

    let buckets = interval
        .bucket_starts(range)
        .into_iter()
        .map(|start| {
            let (message_count, unique_chatters) = match found.remove(&start) {
                Some(bucket) => (bucket.message_count, bucket.unique_chatters),
                None => (0, with_chatters.then_some(0)),
            };

            ActivityBucketResponse {
                start: period::start_of_day(start),
                message_count,
                unique_chatters,
            }
        })
        .collect();

    ActivityResponse {
        interval,
        from: range.start_time(),
        to: range.end_time(),
        buckets,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn date(year: i32, month: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(year, month, day).unwrap()
    }

    #[test]
    fn limits_are_clamped() {
        assert_eq!(clamp_limit(None, 10), 10);
        assert_eq!(clamp_limit(Some(0), 10), 1);
        assert_eq!(clamp_limit(Some(50), 10), 50);
        assert_eq!(clamp_limit(Some(100_000), 10), MAX_LIMIT);
    }

    #[test]
    fn periods_resolve_to_calendar_ranges() {
        let range = resolve_period(Period::Month, Some("2026-09-15")).unwrap();
        assert_eq!(range.start, date(2026, 9, 1));
        assert_eq!(range.end, date(2026, 10, 1));

        let range = resolve_period(Period::Year, Some("2025")).unwrap();
        assert_eq!(range.start, date(2025, 1, 1));
        assert_eq!(range.end, date(2026, 1, 1));

        // the current period when no date is given
        let range = resolve_period(Period::Week, None).unwrap();
        assert_eq!(range.days(), 7);

        assert_eq!(
            resolve_period(Period::All, None).unwrap(),
            DateRange::all_time()
        );
        // the date does not matter for all time, not even if it is not one
        assert_eq!(
            resolve_period(Period::All, Some("not a date")).unwrap(),
            DateRange::all_time()
        );
        assert!(resolve_period(Period::Day, Some("not a date")).is_err());
    }

    #[test]
    fn opted_out_ids_are_sorted() {
        let opt_out: DashMap<String, bool> = DashMap::new();
        assert_eq!(opted_out_ids(&opt_out), Vec::<String>::new());

        opt_out.insert("676".to_owned(), true);
        opt_out.insert("123".to_owned(), true);
        opt_out.insert("45".to_owned(), true);

        assert_eq!(opted_out_ids(&opt_out), vec!["123", "45", "676"]);
    }

    #[test]
    fn bounds_are_hidden_for_all_time() {
        let range = DateRange {
            start: date(2026, 9, 1),
            end: date(2026, 10, 1),
        };

        assert_eq!(period_bounds(Period::All, &range), (None, None));
        let (from, to) = period_bounds(Period::Month, &range);
        assert_eq!(from.map(|from| from.to_rfc3339()), Some("2026-09-01T00:00:00+00:00".to_owned()));
        assert_eq!(to.map(|to| to.to_rfc3339()), Some("2026-10-01T00:00:00+00:00".to_owned()));
    }

    #[test]
    fn activity_ranges_accept_dates_and_timestamps() {
        let today = date(2026, 10, 2);

        let params = ActivityParams {
            interval: Interval::Day,
            from: Some("2026-09-01".to_owned()),
            to: Some("2026-09-30T18:00:00Z".to_owned()),
        };
        let range = resolve_activity_range(&params, today).unwrap();
        assert_eq!(range.start, date(2026, 9, 1));
        assert_eq!(range.end, date(2026, 10, 1));

        let defaults = ActivityParams {
            interval: Interval::Day,
            from: None,
            to: None,
        };
        let range = resolve_activity_range(&defaults, today).unwrap();
        assert_eq!(range.days(), 30);
        assert_eq!(range.end, date(2026, 10, 3));

        // weekly buckets are never partial: from the Monday of the week 26 weeks back until the
        // end of the current week
        let weekly = ActivityParams {
            interval: Interval::Week,
            from: None,
            to: None,
        };
        let range = resolve_activity_range(&weekly, today).unwrap();
        assert_eq!(range.start, date(2026, 3, 30));
        assert_eq!(range.end, date(2026, 10, 5));
    }

    #[test]
    fn timestamps_outside_of_the_logged_years_are_rejected() {
        let params = ActivityParams {
            interval: Interval::Day,
            from: Some("0001-01-01T00:00:00Z".to_owned()),
            to: None,
        };

        assert!(matches!(
            resolve_activity_range(&params, date(2026, 10, 2)),
            Err(Error::InvalidParam(_))
        ));
    }

    #[test]
    fn invalid_activity_ranges_are_rejected() {
        let today = date(2026, 10, 2);

        let bad_date = ActivityParams {
            interval: Interval::Day,
            from: Some("yesterday".to_owned()),
            to: None,
        };
        assert!(matches!(
            resolve_activity_range(&bad_date, today),
            Err(Error::InvalidParam(_))
        ));

        let reversed = ActivityParams {
            interval: Interval::Day,
            from: Some("2026-10-02".to_owned()),
            to: Some("2026-09-01".to_owned()),
        };
        assert!(matches!(
            resolve_activity_range(&reversed, today),
            Err(Error::InvalidParam(_))
        ));
    }

    #[test]
    fn activity_series_have_no_gaps() {
        let range = DateRange {
            start: date(2026, 9, 29),
            end: date(2026, 10, 3),
        };
        let found = vec![
            analytics::ActivityBucket {
                start: date(2026, 9, 30),
                message_count: 5,
                unique_chatters: Some(2),
            },
            analytics::ActivityBucket {
                start: date(2026, 10, 2),
                message_count: 7,
                unique_chatters: Some(3),
            },
        ];

        let response = activity_response(Interval::Day, range, found, true);

        let counts: Vec<(String, u64, Option<u64>)> = response
            .buckets
            .iter()
            .map(|bucket| {
                (
                    bucket.start.format("%Y-%m-%d").to_string(),
                    bucket.message_count,
                    bucket.unique_chatters,
                )
            })
            .collect();
        assert_eq!(
            counts,
            vec![
                ("2026-09-29".to_owned(), 0, Some(0)),
                ("2026-09-30".to_owned(), 5, Some(2)),
                ("2026-10-01".to_owned(), 0, Some(0)),
                ("2026-10-02".to_owned(), 7, Some(3)),
            ]
        );
        assert_eq!(response.from.to_rfc3339(), "2026-09-29T00:00:00+00:00");
        assert_eq!(response.to.to_rfc3339(), "2026-10-03T00:00:00+00:00");
    }

    #[test]
    fn user_activity_has_no_chatter_counts() {
        let range = DateRange {
            start: date(2026, 10, 1),
            end: date(2026, 10, 2),
        };

        let response = activity_response(Interval::Day, range, vec![], false);

        assert_eq!(response.buckets.len(), 1);
        assert_eq!(response.buckets[0].message_count, 0);
        assert_eq!(response.buckets[0].unique_chatters, None);
    }

    #[test]
    fn weekly_series_start_on_mondays() {
        let range = DateRange {
            start: date(2026, 9, 30),
            end: date(2026, 10, 10),
        };
        let found = vec![analytics::ActivityBucket {
            start: date(2026, 9, 28),
            message_count: 9,
            unique_chatters: Some(4),
        }];

        let response = activity_response(Interval::Week, range, found, true);

        let starts: Vec<String> = response
            .buckets
            .iter()
            .map(|bucket| bucket.start.format("%Y-%m-%d").to_string())
            .collect();
        assert_eq!(starts, vec!["2026-09-28", "2026-10-05"]);
        assert_eq!(response.buckets[0].message_count, 9);
        assert_eq!(response.buckets[1].message_count, 0);
    }
}
