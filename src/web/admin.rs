use crate::{app::App, bot::BotMessage, error::Error};
use aide::{
    openapi::{
        HeaderStyle, Parameter, ParameterData, ParameterSchemaOrContent, ReferenceOr, SchemaObject,
    },
    transform::TransformOperation,
};
use axum::{
    extract::{Request, State},
    middleware::Next,
    response::{IntoResponse, Response},
    Extension, Json,
};
use reqwest::StatusCode;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::mpsc::Sender;

pub async fn admin_auth(
    app: State<App>,
    request: Request,
    next: Next,
) -> Result<Response, impl IntoResponse> {
    if let Some(admin_key) = &app.config.admin_api_key {
        if request
            .headers()
            .get("X-Api-Key")
            .and_then(|value| value.to_str().ok())
            == Some(admin_key)
        {
            let response = next.run(request).await;
            return Ok(response);
        }
    }

    Err((StatusCode::FORBIDDEN, "No, I don't think so"))
}

pub fn admin_auth_doc(op: &mut TransformOperation) {
    let schema = aide::generate::in_context(|ctx| ctx.schema.subschema_for::<String>());

    op.inner_mut()
        .parameters
        .push(ReferenceOr::Item(Parameter::Header {
            parameter_data: ParameterData {
                name: "X-Api-Key".to_owned(),
                description: Some("Configured admin API key".to_owned()),
                required: true,
                deprecated: None,
                format: ParameterSchemaOrContent::Schema(SchemaObject {
                    json_schema: schema,
                    external_docs: None,
                    example: None,
                }),
                example: None,
                examples: Default::default(),
                explode: None,
                extensions: Default::default(),
            },
            style: HeaderStyle::Simple,
        }));
}

#[derive(Deserialize, JsonSchema)]
pub struct ChannelsRequest {
    /// List of channels, each one either a Kick user id or a channel slug
    pub channels: Vec<String>,
}

pub async fn add_channels(
    Extension(bot_tx): Extension<Sender<BotMessage>>,
    app: State<App>,
    Json(ChannelsRequest { channels }): Json<ChannelsRequest>,
) -> Result<(), Error> {
    // Looking the channels up here reports problems to the caller. The bot gets the ids,
    // which is how the channels are stored in the config.
    let users = app.get_channels_by_entries(channels, false).await?;
    let ids = users.into_keys().collect();

    bot_tx.send(BotMessage::JoinChannels(ids)).await.unwrap();

    Ok(())
}

pub async fn remove_channels(
    Extension(bot_tx): Extension<Sender<BotMessage>>,
    app: State<App>,
    Json(ChannelsRequest { channels }): Json<ChannelsRequest>,
) -> Result<(), Error> {
    // Looking the channels up here reports problems with Kick to the caller (and fills the
    // cache for the bot). Which channels are known does not matter: the bot removes the
    // entries it is given, which includes channels Kick does not know anymore (deleted accounts).
    app.get_channels_by_entries(channels.clone(), false).await?;

    bot_tx
        .send(BotMessage::PartChannels(channels))
        .await
        .unwrap();

    Ok(())
}
