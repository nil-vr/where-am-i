use std::{path::Path, sync::Arc};

use anyhow::{anyhow, Context};
use async_trait::async_trait;
use http::{header, Extensions, HeaderValue, StatusCode};
use http_cache_reqwest::{
    CACacheManager, Cache, CacheMode, CacheOptions, HttpCache, HttpCacheOptions,
};
use reqwest::{Client, Request, Response, Url};
use reqwest_middleware::{ClientBuilder, ClientWithMiddleware, Middleware, Next, RequestBuilder};
use reqwest_retry::{policies::ExponentialBackoff, RetryTransientMiddleware};
use reqwest_tracing::{SpanBackendWithUrl, TracingMiddleware};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

use crate::{UserId, WorldId};

#[derive(Clone)]
pub struct VrcApiClient {
    base: Arc<Url>,
    api_reqwest: ClientWithMiddleware,
    asset_reqwest: ClientWithMiddleware,
}

impl VrcApiClient {
    const USER_AGENT: &'static str =
        concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));

    pub fn new(cache: impl AsRef<Path>) -> Self {
        let base = Arc::new(Url::parse("https://vrchat.com/api/").unwrap());

        let direct = Client::builder()
            .user_agent(Self::USER_AGENT)
            .build()
            .unwrap();
        let retry_policy = ExponentialBackoff::builder().build_with_max_retries(3);
        let cache = Arc::new(Cache(HttpCache {
            mode: CacheMode::Default,
            manager: CACacheManager {
                path: cache.as_ref().to_owned(),
            },
            options: HttpCacheOptions {
                cache_options: Some(CacheOptions {
                    shared: false,
                    ..Default::default()
                }),
                ..Default::default()
            },
        }));
        let api_reqwest = ClientBuilder::new(direct.clone())
            .with(RetryTransientMiddleware::new_with_policy(retry_policy))
            .with(TracingMiddleware::<SpanBackendWithUrl>::new())
            .with_arc(cache.clone())
            .with(AuthenticationMiddleware)
            .with(AlwaysCacheMiddleware)
            .build();

        let asset_reqwest = ClientBuilder::new(direct)
            .with(RetryTransientMiddleware::new_with_policy(retry_policy))
            .with(TracingMiddleware::<SpanBackendWithUrl>::new())
            .with_arc(cache)
            .build();

        VrcApiClient {
            base,
            api_reqwest,
            asset_reqwest,
        }
    }

    async fn send<T>(&self, request: RequestBuilder) -> anyhow::Result<T>
    where
        T: DeserializeOwned,
    {
        let response = request.send().await.context("request error")?;

        let status = response.status();
        if status.is_success() {
            response.json().await.context("invalid response")
        } else if status.is_client_error() {
            let error: ClientError = response
                .json()
                .await
                .with_context(|| format!("invalid error response with code {status}"))?;
            Err(anyhow!(
                "unexpected status code {}: {}",
                status,
                error.error.message,
            ))
        } else {
            Err(anyhow!("unexpected status code {status}"))
        }
    }

    pub async fn get_world(&self, world: WorldId) -> anyhow::Result<World> {
        let mut url = self.base.as_ref().clone();
        url.path_segments_mut()
            .unwrap()
            .pop()
            .extend(["1", "worlds", &world.to_string()]);
        self.send(self.api_reqwest.get(url)).await
    }

    pub async fn get_world_image(
        &self,
        world: WorldId,
    ) -> anyhow::Result<axum::response::Response> {
        let info = self.get_world(world).await?;
        let Some(image_url) = info.image_url else {
            return Ok(axum::response::Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Default::default())?);
        };
        let mut upstream = self
            .asset_reqwest
            .get(image_url)
            .send()
            .await
            .context("request error")?
            .error_for_status()?;
        let mut response = axum::response::Response::builder().status(StatusCode::OK);
        {
            let upstream_headers = upstream.headers_mut();
            let headers = response.headers_mut().unwrap();
            if let Some(content_type) = upstream_headers.remove(header::CONTENT_TYPE) {
                headers.insert(header::CONTENT_TYPE, content_type);
            }
        }
        Ok(response.body(upstream.bytes().await?.into())?)
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct World {
    pub author_id: Option<UserId>,
    pub author_name: Option<String>,
    pub description: Option<String>,
    pub image_url: Option<Url>,
    pub name: Option<String>,
    pub thumbnail_image_url: Option<Url>,
}

#[derive(Deserialize)]
struct ClientError {
    error: ClientErrorInner,
}

#[derive(Deserialize)]
struct ClientErrorInner {
    message: String,
}

struct AuthenticationMiddleware;

#[async_trait]
impl Middleware for AuthenticationMiddleware {
    async fn handle(
        &self,
        mut req: Request,
        extensions: &mut Extensions,
        next: Next<'_>,
    ) -> reqwest_middleware::Result<Response> {
        static DUMMY_AUTH: HeaderValue =
            HeaderValue::from_static("auth=JlE5Jldo5Jibnk5O5hTx6XVqsJu4WJ26");
        req.headers_mut().append("Cookie", DUMMY_AUTH.clone());
        next.run(req, extensions).await
    }
}

struct AlwaysCacheMiddleware;

#[async_trait]
impl Middleware for AlwaysCacheMiddleware {
    async fn handle(
        &self,
        req: Request,
        extensions: &mut Extensions,
        next: Next<'_>,
    ) -> reqwest_middleware::Result<Response> {
        let mut response = next.run(req, extensions).await?;
        let status = response.status();
        if !status.is_server_error() {
            let headers = response.headers_mut();
            headers.remove("Cache-Control");
            headers.remove("pragma");
            headers.append("Cache-Control", HeaderValue::from_static("max-age=86400"));
        }
        Ok(response)
    }
}
