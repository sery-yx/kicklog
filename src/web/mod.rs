mod admin;
mod analytics;
mod frontend;
mod handlers;
mod moderation;
mod responders;
pub mod schema;
mod trace_layer;

use self::handlers::no_cache_header;
use crate::{app::App, bot::BotMessage, web::admin::admin_auth, ShutdownRx};
use aide::{
    axum::{
        routing::{get, get_with, post, post_with},
        ApiRouter, IntoApiResponse,
    },
    openapi::OpenApi,
    scalar::Scalar,
};
use axum::{
    extract::Request,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    Extension, Json, ServiceExt,
};
use axum_prometheus::PrometheusMetricLayerBuilder;
use prometheus::TextEncoder;
use std::{
    net::{AddrParseError, SocketAddr},
    str::FromStr,
    sync::Arc,
};
use tokio::{net::TcpListener, sync::mpsc::Sender};
use tower_http::{
    compression::CompressionLayer, cors::CorsLayer, normalize_path::NormalizePath,
    trace::TraceLayer, CompressionLevel,
};
use tracing::{debug, info};

const CAPABILITIES: &[&str] = &[
    "arbitrary-range-query",
    "search",
    "stats",
    "namehistory",
    "leaderboards",
    "activity",
    "user-channels",
    "user-summary",
    "moderation",
];

pub async fn run(app: App, mut shutdown_rx: ShutdownRx, bot_tx: Sender<BotMessage>) {
    aide::generate::on_error(|error| {
        panic!("Could not generate docs: {error}");
    });
    aide::generate::infer_responses(true);
    aide::generate::extract_schemas(true);

    metrics_prometheus::install();

    let listen_address =
        parse_listen_addr(&app.config.listen_address).expect("Invalid listen address");

    let cors = CorsLayer::permissive();

    let mut api = OpenApi::default();

    let admin_routes = ApiRouter::new()
        .api_route(
            "/channels",
            post_with(admin::add_channels, |mut op| {
                admin::admin_auth_doc(&mut op);
                op.tag("Admin").description("Join the specified channels")
            })
            .delete_with(admin::remove_channels, |mut op| {
                admin::admin_auth_doc(&mut op);
                op.tag("Admin").description("Leave the specified channels")
            }),
        )
        .route_layer(middleware::from_fn_with_state(app.clone(), admin_auth))
        .layer(Extension(bot_tx));

    let app = ApiRouter::new()
        .nest("/admin", admin_routes)
        .api_route(
            "/channels",
            get_with(handlers::get_channels, |op| {
                op.description("List logged channels")
            }),
        )
        .api_route(
            "/list",
            get_with(handlers::list_available_logs, |op| {
                op.description("List available logs")
            }),
        )
        // Paths with static parts should go first so they aren't overridden by the dynamic date paths later
        .api_route(
            "/namehistory/{user_id}",
            get_with(handlers::get_user_name_history, |op| {
                op.description("Get user name history by provided user id")
            }),
        )
        .api_route(
            "/channels/top",
            get_with(analytics::get_top_channels, |op| {
                op.tag("Statistics").description(
                    "Get the channels with the most messages in a calendar period (UTC)",
                )
            }),
        )
        .api_route(
            "/{user_id_type}/{user}/channels",
            get_with(analytics::get_user_channels, |op| {
                op.tag("Statistics").description(
                    "List the channels a user has chatted in, with message counts and the time of the first and last message",
                )
            }),
        )
        .api_route(
            "/{user_id_type}/{user}/last",
            get_with(analytics::get_user_last_message, |op| {
                op.tag("Statistics")
                    .description("Get the most recent message of a user in any channel")
            }),
        )
        .api_route(
            "/{user_id_type}/{user}/summary",
            get_with(analytics::get_user_summary, |op| {
                op.tag("Statistics").description(
                    "Get the totals of a user over all channels: messages, channels, first and last message",
                )
            }),
        )
        .api_route(
            "/{user_id_type}/{user}/bans",
            get_with(moderation::get_user_bans, |op| {
                op.tag("Moderation").description(
                    "List the bans, timeouts and unbans of a user in all channels: who did them, whether and when they were lifted, and by whom",
                )
            }),
        )
        .api_route(
            "/{channel_id_type}/{channel}/moderation",
            get_with(moderation::get_channel_moderation, |op| {
                op.tag("Moderation").description(
                    "List the moderation actions of a channel (bans, timeouts, unbans, deleted messages, cleared chats), the newest first",
                )
            }),
        )
        .api_route(
            "/{channel_id_type}/{channel}/moderators",
            get_with(moderation::get_channel_moderators, |op| {
                op.tag("Moderation").description(
                    "Get the moderators of a channel with the most bans, timeouts and unbans in a calendar period (UTC)",
                )
            }),
        )
        .api_route(
            "/{channel_id_type}/{channel}/{user_id_type}/{user}/bans",
            get_with(moderation::get_user_channel_bans, |op| {
                op.tag("Moderation").description(
                    "List the bans, timeouts and unbans of a user in a channel: who did them, whether and when they were lifted, and by whom",
                )
            }),
        )
        .api_route(
            "/{channel_id_type}/{channel}/top",
            get_with(analytics::get_channel_top, |op| {
                op.tag("Statistics").description(
                    "Get the chatters with the most messages in a calendar period (UTC)",
                )
            }),
        )
        .api_route(
            "/{channel_id_type}/{channel}/activity",
            get_with(analytics::get_channel_activity, |op| {
                op.tag("Statistics").description(
                    "Get the messages and different chatters of a channel per day, week, month or year",
                )
            }),
        )
        .api_route(
            "/{channel_id_type}/{channel}/{user_id_type}/{user}/activity",
            get_with(analytics::get_user_activity, |op| {
                op.tag("Statistics").description(
                    "Get the messages of a user in a channel per day, week, month or year",
                )
            }),
        )
        .api_route(
            "/{channel_id_type}/{channel}/{user_id_type}/{user}/rank",
            get_with(analytics::get_user_rank, |op| {
                op.tag("Statistics").description(
                    "Get the number of messages and the rank of a user in a channel in a calendar period (UTC)",
                )
            }),
        )
        .api_route(
            "/{channel_id_type}/{channel}/{user_id_type}/{user}/first",
            get_with(analytics::get_user_first_line, |op| {
                op.tag("Statistics")
                    .description("Get the first message of a user in a channel")
            }),
        )
        .api_route(
            "/{channel_id_type}/{channel}/{user_id_type}/{user}/last",
            get_with(analytics::get_user_last_line, |op| {
                op.tag("Statistics")
                    .description("Get the most recent message of a user in a channel")
            }),
        )
        .api_route(
            "/{channel_id_type}/{channel}/{user_id_type}/{user}/search",
            get_with(handlers::search_user_logs, |op| {
                op.description("Search user logs using the provided query")
            }),
        )
        .api_route(
            "/{channel_id_type}/{channel}/{user_id_type}/{user}/stats",
            get_with(handlers::get_user_stats, |op| {
                op.description("Get user stats")
            }),
        )
        .api_route(
            "/{channel_id_type}/{channel}/stats",
            get_with(handlers::get_channel_stats, |op| {
                op.description("Get channel stats")
            }),
        )
        .api_route(
            "/{channel_id_type}/{channel}/random",
            get_with(handlers::random_channel_line, |op| {
                op.description("Get a random line from the channel's logs")
            }),
        )
        .api_route(
            "/{channel_id_type}/{channel}/{user_id_type}/{user}/random",
            get_with(handlers::random_user_line, |op| {
                op.description("Get a random line from the user's logs in a channel")
            }),
        )
        .api_route(
            "/{channel_id_type}/{channel}",
            get_with(handlers::get_channel_logs, |op| {
                op.description("Get channel logs. If the `to` and `from` query params are not given, redirect to latest available day")
            }),
        )
        .api_route(
            "/{channel_id_type}/{channel}/{user_id_type}/{user}",
            get_with(handlers::get_user_logs, |op| {
                op.description("Get user logs by name. If the `to` and `from` query params are not given, redirect to latest available month")
            }),
        )
        .api_route(
            "/{channel_id_type}/{channel}/{year}/{month}/{day}",
            get_with(handlers::get_channel_logs_by_date, |op| {
                op.description("Get channel logs from the given day")
            }),
        )
        .api_route(
            "/{channel_id_type}/{channel}/{user_id_type}/{user}/{year}/{month}",
            get_with(handlers::get_user_logs_by_date, |op| {
                op.description("Get user logs in a channel from the given month")
            }),
        )
        .api_route("/optout", post(handlers::optout))
        .api_route("/capabilities", get(capabilities))
        .route("/docs", Scalar::new("/openapi.json").axum_route())
        .route("/openapi.json", get(serve_openapi))
        .route("/assets/{*asset}", get(frontend::static_asset))
        .fallback(frontend::static_asset)
        .layer(middleware::from_fn(capabilities_header_middleware))
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(trace_layer::make_span_with)
                .on_response(trace_layer::on_response),
        )
        .layer(
            PrometheusMetricLayerBuilder::new()
                .with_prefix("rustlog")
                .build(),
        )
        .route("/metrics", get(metrics))
        .finish_api(&mut api)
        .layer(Extension(Arc::new(api)))
        .with_state(app)
        .layer(cors)
        .layer(CompressionLayer::new().quality(CompressionLevel::Fastest));
    let app = NormalizePath::trim_trailing_slash(app);

    info!("Listening on {listen_address}");

    let listener = TcpListener::bind(&listen_address)
        .await
        .expect("Could not create TCP listener");

    axum::serve(listener, ServiceExt::<Request>::into_make_service(app))
        .with_graceful_shutdown(async move {
            shutdown_rx.changed().await.ok();
            debug!("Shutting down web task");
        })
        .await
        .unwrap();
}

pub fn parse_listen_addr(addr: &str) -> Result<SocketAddr, AddrParseError> {
    if addr.starts_with(':') {
        SocketAddr::from_str(&format!("0.0.0.0{addr}"))
    } else {
        SocketAddr::from_str(addr)
    }
}

async fn capabilities() -> Json<Vec<&'static str>> {
    Json(CAPABILITIES.to_vec())
}

async fn capabilities_header_middleware(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    response.headers_mut().insert(
        "x-rustlog-capabilities",
        CAPABILITIES.join(",").try_into().unwrap(),
    );
    response
}

async fn metrics() -> impl IntoApiResponse {
    let metric_families = prometheus::gather();

    let encoder = TextEncoder::new();
    let metrics = encoder.encode_to_string(&metric_families).unwrap();
    (no_cache_header(), metrics)
}
async fn serve_openapi(Extension(api): Extension<Arc<OpenApi>>) -> impl IntoApiResponse {
    Json(api.as_ref()).into_response()
}
