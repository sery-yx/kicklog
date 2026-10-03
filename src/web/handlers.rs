use super::{
    responders::logs::LogsResponse,
    schema::{
        AvailableLogs, AvailableLogsParams, Channel, ChannelIdType, ChannelLogsByDatePath,
        ChannelLogsStats, ChannelParam, ChannelsList, LogsParams, LogsPathChannel, SearchParams,
        UserIdType, UserLogPathParams, UserLogsDatePath, UserLogsStats, UserNameHistoryParam,
        UserParam,
    },
};
use crate::{
    app::App,
    db::{
        self, read_available_channel_logs, read_available_user_logs, read_channel,
        read_random_channel_line, read_random_user_line, read_user, schema::StructuredMessage,
    },
    error::Error,
    logs::{
        schema::{
            message::{BasicMessage, ResponseMessage},
            LogRangeParams,
        },
        stream::LogsStream,
    },
    web::schema::LogsPathDate,
    Result, ShutdownRx,
};
use aide::axum::IntoApiResponse;
use axum::{
    extract::{
        ws::{Message, WebSocket},
        Path, Query, RawQuery, State, WebSocketUpgrade,
    },
    response::{IntoResponse, Redirect, Response},
    Extension, Json,
};
use axum_extra::{headers::CacheControl, TypedHeader};
use chrono::{DateTime, Days, Months, NaiveDate, NaiveTime, Utc};
use futures::{SinkExt, StreamExt};
use lazy_static::lazy_static;
use prometheus::{register_int_gauge, IntGauge};
use rand::{distr::Alphanumeric, rng, Rng};
use std::time::Duration;
use tokio::sync::broadcast::{self, error::RecvError};
use tracing::debug;

lazy_static! {
    static ref FIREHOSE_CLIENTS_GAUGE: IntGauge = register_int_gauge!(
        "rustlog_firehose_clients_count",
        "How many firehose clients are connected to the websocket",
    )
    .unwrap();
}

pub async fn get_channels(app: State<App>) -> Result<impl IntoApiResponse> {
    let channel_ids = app.channels.read().unwrap().clone();

    let channels = app
        .get_channels_by_entries(Vec::from_iter(channel_ids), false)
        .await?;

    let json = Json(ChannelsList {
        channels: channels
            .into_iter()
            .map(|(user_id, name)| Channel { name, user_id })
            .collect(),
    });
    Ok((cache_header(600), json))
}

pub async fn get_channel_logs(
    Path(LogsPathChannel {
        channel_id_type,
        channel,
    }): Path<LogsPathChannel>,
    Query(range_params): Query<LogRangeParams>,
    Query(logs_params): Query<LogsParams>,
    RawQuery(query): RawQuery,
    app: State<App>,
) -> Result<Response> {
    let channel_id = match channel_id_type {
        ChannelIdType::Name => app.get_user_id_by_name(&channel).await?,
        ChannelIdType::Id => channel.clone(),
    };

    if let Some(range) = range_params.range() {
        let logs = get_channel_logs_inner(&app, &channel_id, logs_params, range).await?;
        Ok(logs.into_response())
    } else {
        let available_logs = read_available_channel_logs(&app.db, &channel_id).await?;
        let latest_log = available_logs.first().ok_or(Error::NotFound)?;

        let mut new_uri = format!("/{channel_id_type}/{channel}/{latest_log}");
        if let Some(query) = query {
            new_uri.push('?');
            new_uri.push_str(&query);
        }

        Ok(Redirect::to(&new_uri).into_response())
    }
}

pub async fn get_channel_stats(
    Path(LogsPathChannel {
        channel_id_type,
        channel,
    }): Path<LogsPathChannel>,
    Query(range_params): Query<LogRangeParams>,
    app: State<App>,
) -> Result<Json<ChannelLogsStats>> {
    let channel_id = match channel_id_type {
        ChannelIdType::Name => app.get_user_id_by_name(&channel).await?,
        ChannelIdType::Id => channel.clone(),
    };
    let (message_count, stats_rows) =
        db::get_channel_stats(&app.db, &channel_id, range_params).await?;

    let user_ids = stats_rows.iter().map(|row| row.user_id.clone()).collect();
    let mut users = app.get_users(user_ids, vec![], false).await?;

    let top_chatters = stats_rows
        .into_iter()
        .map(|row| UserLogsStats {
            user_login: users.remove(&row.user_id),
            user_id: row.user_id,
            message_count: row.cnt,
        })
        .collect();

    Ok(Json(ChannelLogsStats {
        message_count,
        top_chatters,
    }))
}

pub async fn get_user_stats(
    Path(user_params): Path<UserLogPathParams>,
    Query(range_params): Query<LogRangeParams>,
    app: State<App>,
) -> Result<Json<UserLogsStats>> {
    let (channel_id, user_id) = resolve_user_params(&user_params, &app).await?;

    app.check_opted_out(&channel_id, Some(&user_id))?;

    let user_login = app
        .get_users(vec![user_id.clone()], vec![], false)
        .await?
        .into_values()
        .next();
    let stats = db::get_user_stats(&app.db, &channel_id, user_id, user_login, range_params).await?;

    Ok(Json(stats))
}

pub async fn get_channel_logs_by_date(
    app: State<App>,
    Path(channel_log_params): Path<ChannelLogsByDatePath>,
    Query(logs_params): Query<LogsParams>,
) -> Result<impl IntoApiResponse> {
    debug!("Params: {logs_params:?}");

    let channel_id = match channel_log_params.channel_info.channel_id_type {
        ChannelIdType::Name => {
            app.get_user_id_by_name(&channel_log_params.channel_info.channel)
                .await?
        }
        ChannelIdType::Id => channel_log_params.channel_info.channel.clone(),
    };

    let LogsPathDate { year, month, day } = channel_log_params.date;

    let from = NaiveDate::from_ymd_opt(year.parse()?, month.parse()?, day.parse()?)
        .ok_or_else(|| Error::InvalidParam("Invalid date".to_owned()))?
        .and_time(NaiveTime::default())
        .and_utc();
    let to = from
        .checked_add_days(Days::new(1))
        .ok_or_else(|| Error::InvalidParam("Date out of range".to_owned()))?;

    get_channel_logs_inner(&app, &channel_id, logs_params, (from, to)).await
}

async fn get_channel_logs_inner(
    app: &App,
    channel_id: &str,
    params: LogsParams,
    range: (DateTime<Utc>, DateTime<Utc>),
) -> Result<impl IntoApiResponse> {
    app.check_opted_out(channel_id, None)?;

    let stream = read_channel(&app.db, channel_id, params, &app.flush_buffer, range).await?;

    let logs = LogsResponse {
        response_type: params.response_type(),
        stream,
    };

    let cache = if Utc::now() < range.1 {
        no_cache_header()
    } else {
        cache_header(36000)
    };

    Ok((cache, logs))
}

pub async fn get_user_logs(
    Path(user_params): Path<UserLogPathParams>,
    Query(range_params): Query<LogRangeParams>,
    Query(logs_params): Query<LogsParams>,
    RawQuery(query): RawQuery,
    app: State<App>,
) -> Result<impl IntoApiResponse> {
    let (channel_id, user_id) = resolve_user_params(&user_params, &app).await?;

    app.check_opted_out(&channel_id, Some(&user_id))?;

    if let Some(range) = range_params.range() {
        let logs = get_user_logs_inner(&app, &channel_id, &user_id, logs_params, range).await?;
        Ok(logs.into_response())
    } else {
        let available_logs = read_available_user_logs(&app.db, &channel_id, &user_id).await?;
        let latest_log = available_logs.first().ok_or(Error::NotFound)?;

        let UserLogPathParams {
            channel_id_type,
            channel,
            user_id_type,
            user,
        } = user_params;

        let mut new_uri =
            format!("/{channel_id_type}/{channel}/{user_id_type}/{user}/{latest_log}");
        if let Some(query) = query {
            new_uri.push('?');
            new_uri.push_str(&query);
        }
        Ok(Redirect::to(&new_uri).into_response())
    }
}

pub async fn get_user_logs_by_date(
    app: State<App>,
    Path(user_params): Path<UserLogPathParams>,
    Path(user_logs_date): Path<UserLogsDatePath>,
    Query(logs_params): Query<LogsParams>,
) -> Result<impl IntoApiResponse> {
    let (channel_id, user_id) = resolve_user_params(&user_params, &app).await?;

    app.check_opted_out(&channel_id, Some(&user_id))?;

    let year = user_logs_date.year.parse()?;
    let month = user_logs_date.month.parse()?;

    let from = NaiveDate::from_ymd_opt(year, month, 1)
        .ok_or_else(|| Error::InvalidParam("Invalid date".to_owned()))?
        .and_time(NaiveTime::default())
        .and_utc();
    let to = from
        .checked_add_months(Months::new(1))
        .ok_or_else(|| Error::InvalidParam("Date out of range".to_owned()))?;

    get_user_logs_inner(&app, &channel_id, &user_id, logs_params, (from, to)).await
}

async fn get_user_logs_inner(
    app: &App,
    channel_id: &str,
    user_id: &str,
    logs_params: LogsParams,
    range: (DateTime<Utc>, DateTime<Utc>),
) -> Result<impl IntoApiResponse> {
    let stream = read_user(
        &app.db,
        channel_id,
        user_id,
        logs_params,
        &app.flush_buffer,
        range,
    )
    .await?;

    let logs = LogsResponse {
        stream,
        response_type: logs_params.response_type(),
    };

    let cache = if Utc::now() < range.1 {
        no_cache_header()
    } else {
        cache_header(36000)
    };

    Ok((cache, logs))
}

pub async fn list_available_logs(
    Query(AvailableLogsParams { user, channel }): Query<AvailableLogsParams>,
    app: State<App>,
) -> Result<impl IntoApiResponse> {
    let channel_id = match channel {
        ChannelParam::ChannelId(id) => id,
        ChannelParam::Channel(name) => app.get_user_id_by_name(&name).await?,
    };

    let available_logs = if let Some(user) = user {
        let user_id = match user {
            UserParam::UserId(id) => id,
            UserParam::User(name) => app.get_user_id_by_name(&name).await?,
        };
        app.check_opted_out(&channel_id, Some(&user_id))?;
        read_available_user_logs(&app.db, &channel_id, &user_id).await?
    } else {
        app.check_opted_out(&channel_id, None)?;
        read_available_channel_logs(&app.db, &channel_id).await?
    };

    if !available_logs.is_empty() {
        Ok((cache_header(600), Json(AvailableLogs { available_logs })))
    } else {
        Err(Error::NotFound)
    }
}

pub async fn random_channel_line(
    app: State<App>,
    Path(LogsPathChannel {
        channel_id_type,
        channel,
    }): Path<LogsPathChannel>,
    Query(logs_params): Query<LogsParams>,
) -> Result<impl IntoApiResponse> {
    let channel_id = match channel_id_type {
        ChannelIdType::Name => app.get_user_id_by_name(&channel).await?,
        ChannelIdType::Id => channel,
    };

    let random_line = read_random_channel_line(&app.db, &channel_id).await?;
    let stream = LogsStream::new_provided(vec![random_line])?;

    let logs = LogsResponse {
        stream,
        response_type: logs_params.response_type(),
    };
    Ok((no_cache_header(), logs))
}

pub async fn random_user_line(
    app: State<App>,
    Path(user_params): Path<UserLogPathParams>,
    Query(logs_params): Query<LogsParams>,
) -> Result<impl IntoApiResponse> {
    let (channel_id, user_id) = resolve_user_params(&user_params, &app).await?;

    app.check_opted_out(&channel_id, Some(&user_id))?;

    let random_line = read_random_user_line(&app.db, &channel_id, &user_id).await?;
    let stream = LogsStream::new_provided(vec![random_line])?;

    let logs = LogsResponse {
        stream,
        response_type: logs_params.response_type(),
    };
    Ok((no_cache_header(), logs))
}

pub async fn search_user_logs(
    app: State<App>,
    Path(user_params): Path<UserLogPathParams>,
    Query(search_params): Query<SearchParams>,
    Query(logs_params): Query<LogsParams>,
) -> Result<impl IntoApiResponse> {
    let (channel_id, user_id) = resolve_user_params(&user_params, &app).await?;

    app.check_opted_out(&channel_id, Some(&user_id))?;

    let stream = db::search_user_logs(
        &app.db,
        &channel_id,
        &user_id,
        &search_params.q,
        logs_params,
    )
    .await?;

    let logs = LogsResponse {
        stream,
        response_type: logs_params.response_type(),
    };
    Ok(logs)
}

pub async fn get_user_name_history(
    app: State<App>,
    Path(UserNameHistoryParam { user_id }): Path<UserNameHistoryParam>,
) -> Result<impl IntoApiResponse> {
    app.check_opted_out(&user_id, None)?;

    let names = db::get_user_name_history(&app.db, &user_id).await?;

    Ok(Json(names))
}

/// A websocket which sends every message that is logged, in all channels, as it arrives:
/// as an IRC style line, or as the JSON of `jsonBasic` if the parameter is given. Users who opted
/// out are not part of it, as they are not logged.
pub async fn firehose(
    app: State<App>,
    Extension(shutdown_rx): Extension<ShutdownRx>,
    ws: WebSocketUpgrade,
    Query(logs_params): Query<LogsParams>,
) -> impl IntoResponse {
    let firehose_rx = app.firehose_tx.subscribe();

    ws.on_upgrade(move |socket| {
        firehose_socket(socket, firehose_rx, shutdown_rx, logs_params.json_basic)
    })
}

/// What is sent to a client for a message. `None` if the message cannot be turned into JSON.
fn firehose_text(message: &StructuredMessage<'_>, json_basic: bool) -> Option<String> {
    if !json_basic {
        return Some(message.to_raw_irc());
    }

    match BasicMessage::from_structured(message) {
        Ok(basic) => match serde_json::to_string(&basic) {
            Ok(json) => Some(json),
            Err(err) => {
                debug!("Could not serialize a message for the firehose: {err}");
                None
            }
        },
        Err(err) => {
            debug!("Could not convert a message for the firehose: {err}");
            None
        }
    }
}

async fn firehose_socket(
    socket: WebSocket,
    mut firehose_rx: broadcast::Receiver<StructuredMessage<'static>>,
    mut shutdown_rx: ShutdownRx,
    json_basic: bool,
) {
    let (mut sender, mut receiver) = socket.split();

    let mut send_task = tokio::spawn(async move {
        loop {
            // The server does not finish shutting down while a websocket is open, so the
            // websockets close when it is asked to shut down
            let received = tokio::select! {
                received = firehose_rx.recv() => received,
                _ = shutdown_rx.changed() => {
                    let _ = sender.send(Message::Close(None)).await;
                    break;
                }
            };

            let message = match received {
                Ok(message) => message,
                Err(RecvError::Lagged(skipped)) => {
                    // Messages were missed, which a client has to be told about by being
                    // disconnected: it can connect again
                    debug!("A firehose client fell behind by {skipped} messages");
                    break;
                }
                Err(RecvError::Closed) => break,
            };

            let Some(text) = firehose_text(&message, json_basic) else {
                continue;
            };
            if sender.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
    });

    // Nothing is expected from the client, but reading is what notices that it went away
    let mut recv_task = tokio::spawn(async move {
        while let Some(Ok(_)) = receiver.next().await {
            debug!("Received a message on the firehose websocket");
        }
    });

    debug!("Firehose client connected");
    FIREHOSE_CLIENTS_GAUGE.inc();

    tokio::select! {
        _ = &mut send_task => {},
        _ = &mut recv_task => {},
    }
    // The other half has no reason to go on
    send_task.abort();
    recv_task.abort();

    debug!("Firehose client disconnected");
    FIREHOSE_CLIENTS_GAUGE.dec();
}

pub async fn optout(app: State<App>) -> Json<String> {
    let mut rng = rng();
    let optout_code: String = (0..5).map(|_| rng.sample(Alphanumeric) as char).collect();

    app.optout_codes.insert(optout_code.clone());

    {
        let codes = app.optout_codes.clone();
        let optout_code = optout_code.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(60)).await;
            if codes.remove(&optout_code).is_some() {
                debug!("Dropping optout code {optout_code}");
            }
        });
    }

    Json(optout_code)
}

pub(super) fn cache_header(secs: u64) -> TypedHeader<CacheControl> {
    TypedHeader(
        CacheControl::new()
            .with_public()
            .with_max_age(Duration::from_secs(secs)),
    )
}

pub fn no_cache_header() -> TypedHeader<CacheControl> {
    TypedHeader(CacheControl::new().with_no_cache())
}

pub(super) async fn resolve_user_params(
    params: &UserLogPathParams,
    app: &App,
) -> Result<(String, String)> {
    let channel_id = match params.channel_id_type {
        ChannelIdType::Name => app.get_user_id_by_name(&params.channel).await?,
        ChannelIdType::Id => params.channel.clone(),
    };
    let user_id = match params.user_id_type {
        UserIdType::Name => app.get_user_id_by_name(&params.user).await?,
        UserIdType::Id => params.user.clone(),
    };
    Ok((channel_id, user_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema::MessageType;
    use std::borrow::Cow;

    fn chat_message() -> StructuredMessage<'static> {
        let mut message = StructuredMessage::new(
            "676".to_owned(),
            "xqc".to_owned(),
            1790923082123,
            MessageType::PrivMsg,
        );
        message.user_id = Cow::Borrowed("12345");
        message.user_login = Cow::Borrowed("some-user");
        message.display_name = Cow::Borrowed("Some_User");
        message.text = Cow::Borrowed("hello");
        message
    }

    #[test]
    fn the_firehose_sends_irc_lines_by_default() {
        let text = firehose_text(&chat_message(), false).unwrap();

        assert!(text.starts_with('@'));
        assert!(text.ends_with(":some-user!some-user@some-user.kick.com PRIVMSG #xqc :hello"));
    }

    #[test]
    fn the_firehose_can_send_the_json_of_json_basic() {
        let text = firehose_text(&chat_message(), true).unwrap();
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();

        assert_eq!(json["text"], "hello");
        assert_eq!(json["displayName"], "Some_User");
        assert_eq!(json["channel"], "xqc");
        assert_eq!(json["tags"]["room-id"], "676");
        assert_eq!(json["tags"]["user-id"], "12345");
        // jsonBasic does not have what the full format adds
        assert!(json.get("raw").is_none());
        assert!(json.get("username").is_none());
    }
}
