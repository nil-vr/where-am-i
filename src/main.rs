use std::{
    borrow::Cow,
    convert::Infallible,
    fmt,
    path::PathBuf,
    str::{self, FromStr},
};

use anyhow::{anyhow, Context};
use api::{VrcApiClient, World};
use async_stream::stream;
use axum::{
    extract::{Path, State},
    response::{sse::Event, Response, Sse},
    routing::get,
    Router,
};
use fast_qr::{convert::svg::SvgBuilder, QRBuilder};
use figment::{
    providers::{Format, Toml},
    Figment,
};
use futures::{pin_mut, Stream, StreamExt};
use http::{header, StatusCode};
use log::LogEventKind;
use reqwest::Url;
use serde::Deserialize;
use serde::{de::Error, Serialize};
use tokio::try_join;
use tokio::{net::TcpListener, sync::watch};
use tower_http::{services::ServeDir, trace::TraceLayer};
use tracing::{debug, error};
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

mod api;
mod log;

#[derive(Deserialize)]
#[serde(default)]
struct Configuration {
    logs_path: Option<PathBuf>,
    address: String,
    content: String,
    cache: String,
}

impl Default for Configuration {
    fn default() -> Self {
        Self {
            logs_path: None,
            address: "127.0.0.1:37544".into(),
            content: "static".into(),
            cache: "cache".into(),
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let config: Configuration = Figment::new()
        .join(Toml::file_exact("where-am-i.toml"))
        .extract()
        .context("Invalid configuration")?;

    let found_path;
    let path = if let Some(path) = &config.logs_path {
        path
    } else {
        found_path = log::autodetect_path()?;
        &found_path
    };

    let vrc_api = VrcApiClient::new(&config.cache);

    let events = log::log_events(path);

    let (location_sender, location) = watch::channel(None::<Location>);
    let location_future = {
        let vrc_api = vrc_api.clone();
        async move {
            pin_mut!(events);
            while let Some(event) = events.next().await.transpose()? {
                debug!(?event, "Got event");
                match event.kind {
                    LogEventKind::LeftRoom => {
                        location_sender.send_replace(None);
                    }
                    LogEventKind::JoiningRoom(room_id) => {
                        let world = match vrc_api.get_world(room_id.world).await {
                            Ok(world) => Some(world),
                            Err(error) => {
                                error!(?error, "world info error");
                                None
                            }
                        };
                        location_sender.send_replace(Some(Location {
                            world_id: room_id.world,
                            room_id,
                            world,
                        }));
                    }
                }
            }
            anyhow::Ok(())
        }
    };

    let state = ApiState { location, vrc_api };

    let app = Router::new()
        .route("/api/status", get(status))
        .route("/api/world/:world/image", get(world_image))
        .route("/api/world/:world/qr.svg", get(world_qr_svg))
        .route("/api/world/current/info.txt", get(current_world_info))
        .route("/api/room/:room/qr.svg", get(room_qr_svg))
        .route("/api/room/current/link.txt", get(current_room_link))
        .fallback_service(ServeDir::new(&config.content))
        .with_state(state)
        .layer(TraceLayer::new_for_http());

    let listener = TcpListener::bind(&*config.address)
        .await
        .context("network bind error")?;

    println!("Add an OBS browser source for http://{}", config.address);

    try_join! {
        location_future,
        async {
            axum::serve(listener, app).await.context("server error")
        },
    }?;
    Ok(())
}

#[derive(Clone)]
struct ApiState {
    location: watch::Receiver<Option<Location>>,
    vrc_api: VrcApiClient,
}

#[derive(Clone, Copy, Debug)]
struct WorldId(Uuid);

impl fmt::Display for WorldId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "wrld_{}", self.0.as_hyphenated())
    }
}

impl FromStr for WorldId {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let uuid = s
            .strip_prefix("wrld_")
            .ok_or_else(|| anyhow!("world ID does not begin with wrld"))?;
        Ok(WorldId(uuid.parse()?))
    }
}

impl Serialize for WorldId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for WorldId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = <Cow<'de, str>>::deserialize(deserializer)?;
        FromStr::from_str(&s).map_err(|e| D::Error::custom(e))
    }
}

#[derive(Clone, Copy, Debug)]
struct UserId(Uuid);

impl fmt::Display for UserId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "usr_{}", self.0.as_hyphenated())
    }
}

impl FromStr for UserId {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let uuid = s
            .strip_prefix("usr_")
            .ok_or_else(|| anyhow!("user ID does not begin with usr"))?;
        Ok(UserId(uuid.parse()?))
    }
}

impl<'de> Deserialize<'de> for UserId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = <Cow<'de, str>>::deserialize(deserializer)?;
        FromStr::from_str(&s).map_err(|e| D::Error::custom(e))
    }
}

impl Serialize for UserId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

#[derive(Debug)]
struct InstanceId {
    id: u32,
    attributes: Vec<(String, String)>,
}

impl fmt::Display for InstanceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.id)?;
        for (k, v) in &self.attributes {
            write!(f, "~{k}({v})")?;
        }
        Ok(())
    }
}

impl FromStr for InstanceId {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut attributes = s.split('~');
        let id = attributes.next().unwrap();
        let attributes = attributes
            .map(|a| {
                let (key, rest) = a.split_once('(').context("invalid attribute")?;
                let value = rest.strip_suffix(')').context("invalid attribute value")?;
                Ok((key.to_owned(), value.to_owned()))
            })
            .collect::<anyhow::Result<Vec<_>>>();
        Ok(Self {
            id: id.parse().context("invalid instance id")?,
            attributes: attributes.context("invalid attributes")?,
        })
    }
}

#[derive(Debug)]
struct RoomId {
    world: WorldId,
    instance: InstanceId,
}

impl fmt::Display for RoomId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.world, self.instance)
    }
}

impl FromStr for RoomId {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (world, instance) = s.split_once(':').context("invalid room ID")?;
        Ok(RoomId {
            world: world.parse().context("invalid world ID")?,
            instance: instance.parse().context("invalid instance ID")?,
        })
    }
}

impl Serialize for RoomId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for RoomId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = <Cow<'de, str>>::deserialize(deserializer)?;
        FromStr::from_str(&s).map_err(|e| D::Error::custom(e))
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Location {
    room_id: RoomId,
    world_id: WorldId,
    world: Option<World>,
}

async fn status(
    State(ApiState { mut location, .. }): State<ApiState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    Sse::new(stream! {
        {
            let change = {
                let location = location.borrow_and_update();
                Event::default()
                    .event("location")
                    .json_data(&*location).unwrap()
            };

            yield Ok(change);
        }
        while let Ok(_) = location.changed().await {
            let change = {
                let location = location.borrow_and_update();
                Event::default()
                    .event("location")
                    .json_data(&*location).unwrap()
            };
            yield Ok(change);
        }
    })
}

async fn world_image(
    State(ApiState { vrc_api, .. }): State<ApiState>,
    Path(world): Path<WorldId>,
) -> Result<Response, StatusCode> {
    match vrc_api.get_world_image(world).await {
        Ok(image) => Ok(image),
        Err(error) => {
            error!(?error, "image download error");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

async fn world_qr_svg(Path(world): Path<WorldId>) -> Response {
    let url = format!("https://vrchat.com/home/world/{world}");
    let qr = QRBuilder::new(url).build().unwrap();
    let svg = SvgBuilder::default().to_str(&qr);
    Response::builder()
        .header(header::CONTENT_TYPE, "image/svg+xml; charset=utf-8")
        .body(svg.into())
        .unwrap()
}

async fn current_world_info(
    State(ApiState { location, .. }): State<ApiState>,
) -> Cow<'static, str> {
    if let Some(location) = &*location.borrow() {
        if let Some(world) = &location.world {
            format!(
                "\"{}\" by {}: https://vrchat.com/home/world/{}",
                world.name.as_deref().unwrap_or("N/A"),
                world.author_name.as_deref().unwrap_or("N/A"),
                location.world_id,
            )
        } else {
            format!("https://vrchat.com/home/world/{}", location.world_id)
        }
        .into()
    } else {
        "N/A".into()
    }
}

async fn room_qr_svg(Path(room): Path<RoomId>) -> Response {
    let url = Url::parse_with_params(
        "https://vrchat.com/home/launch",
        &[
            ("worldId", room.world.to_string()),
            ("instanceId", room.instance.to_string()),
        ],
    )
    .unwrap();
    let qr = QRBuilder::new(String::from(url)).build().unwrap();
    let svg = SvgBuilder::default().to_str(&qr);
    Response::builder()
        .header(header::CONTENT_TYPE, "image/svg+xml; charset=utf-8")
        .body(svg.into())
        .unwrap()
}

async fn current_room_link(State(ApiState { location, .. }): State<ApiState>) -> Cow<'static, str> {
    if let Some(location) = &*location.borrow() {
        let url = Url::parse_with_params(
            "https://vrchat.com/home/launch",
            &[
                ("worldId", location.room_id.world.to_string()),
                ("instanceId", location.room_id.instance.to_string()),
            ],
        )
        .unwrap();
        String::from(url).into()
    } else {
        "N/A".into()
    }
}
