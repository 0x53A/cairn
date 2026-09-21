use crate::{
    objects::{MAX_OBJECT, envelope, valid_id},
    store::{Import, Store, replicate, valid_key},
};
use anyhow::{Result, ensure};
use axum::{
    Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Path, Request, State},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use std::{net::SocketAddr, path::PathBuf, sync::Arc};

#[derive(Clone)]
struct App {
    root: PathBuf,
    token: Option<String>,
    gate: Arc<tokio::sync::Semaphore>,
    imports: Arc<tokio::sync::Semaphore>,
}
impl App {
    fn store(&self, domain: &str) -> Result<Store> {
        ensure!(domain == "plain" || valid_id(domain), "invalid namespace");
        Ok(Store::Local(self.root.join(domain)))
    }
}
type ApiResult = Result<Response, (StatusCode, String)>;
fn error(e: anyhow::Error) -> (StatusCode, String) {
    let status = e
        .chain()
        .find_map(|cause| cause.downcast_ref::<std::io::Error>())
        .map(|io| match io.raw_os_error() {
            Some(28 | 122) => StatusCode::INSUFFICIENT_STORAGE,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        })
        .unwrap_or(StatusCode::BAD_REQUEST);
    (status, format!("{e:#}"))
}
async fn authorize(
    State(app): State<App>,
    headers: HeaderMap,
    request: Request,
    next: Next,
) -> Response {
    if let Some(token) = &app.token
        && headers.get("authorization").and_then(|h| h.to_str().ok())
            != Some(&format!("Bearer {token}"))
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    // Imports must not consume all slots needed to serve their peer's GETs.
    // Separate admission also permits reciprocal and self-copy operations.
    let gate = if request.uri().path().ends_with("/import") {
        &app.imports
    } else {
        &app.gate
    };
    let Ok(_permit) = gate.acquire().await else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    next.run(request).await
}
async fn fetch(
    State(app): State<App>,
    Path((domain, bucket, id)): Path<(String, String, String)>,
) -> ApiResult {
    tokio::task::spawn_blocking(move || {
        let key = format!("{bucket}/{id}");
        valid_key(&key).map_err(error)?;
        let store = app.store(&domain).map_err(error)?;
        if !store.exists(&key).map_err(error)? {
            return Ok(StatusCode::NOT_FOUND.into_response());
        }
        Ok(store.get(&key).map_err(error)?.into_response())
    })
    .await
    .map_err(|e| error(e.into()))?
}
async fn exists(
    State(app): State<App>,
    Path((domain, bucket, id)): Path<(String, String, String)>,
) -> ApiResult {
    tokio::task::spawn_blocking(move || {
        let present = app
            .store(&domain)
            .map_err(error)?
            .exists(&format!("{bucket}/{id}"))
            .map_err(error)?;
        Ok(if present {
            StatusCode::OK
        } else {
            StatusCode::NOT_FOUND
        }
        .into_response())
    })
    .await
    .map_err(|e| error(e.into()))?
}
async fn put(
    State(app): State<App>,
    Path((domain, bucket, id)): Path<(String, String, String)>,
    body: Bytes,
) -> ApiResult {
    tokio::task::spawn_blocking(move || {
        let key = format!("{bucket}/{id}");
        valid_key(&key).map_err(error)?;
        let store = app.store(&domain).map_err(error)?;
        if bucket == "tags" {
            let target = std::str::from_utf8(&body).map_err(|e| error(e.into()))?;
            if !valid_id(target)
                || !store
                    .exists(&format!("snapshots/{target}"))
                    .map_err(error)?
            {
                return Err(error(anyhow::anyhow!("tag target missing")));
            }
            if store.exists(&key).map_err(error)? && store.get(&key).map_err(error)? != body {
                return Ok((StatusCode::CONFLICT, "tag already exists").into_response());
            }
        } else {
            envelope(&body).map_err(error)?;
        }
        let created = store.put(&key, &body).map_err(error)?;
        if bucket == "tags" && !created && store.get(&key).map_err(error)? != body {
            return Ok((StatusCode::CONFLICT, "tag already exists").into_response());
        }
        Ok(if created {
            StatusCode::CREATED
        } else {
            StatusCode::OK
        }
        .into_response())
    })
    .await
    .map_err(|e| error(e.into()))?
}
async fn list(State(app): State<App>, Path((domain, bucket)): Path<(String, String)>) -> ApiResult {
    tokio::task::spawn_blocking(move || {
        let entries = app
            .store(&domain)
            .map_err(error)?
            .list(&bucket)
            .map_err(error)?;
        Ok(axum::Json(entries).into_response())
    })
    .await
    .map_err(|e| error(e.into()))?
}
async fn import(
    State(app): State<App>,
    Path(domain): Path<String>,
    axum::Json(request): axum::Json<Import>,
) -> ApiResult {
    tokio::task::spawn_blocking(move || {
        if !(request.source.starts_with("http://") || request.source.starts_with("https://")) {
            return Err(error(anyhow::anyhow!(
                "server import source must be HTTP(S)"
            )));
        }
        let source =
            Store::connect(&request.source, &domain, request.source_token).map_err(error)?;
        let destination = app.store(&domain).map_err(error)?;
        let stats = replicate(&source, &destination, &request.snapshot).map_err(error)?;
        Ok(axum::Json(stats).into_response())
    })
    .await
    .map_err(|e| error(e.into()))?
}
pub async fn serve(root: PathBuf, address: SocketAddr, token: Option<String>) -> Result<()> {
    ensure!(
        address.ip().is_loopback() || token.as_ref().is_some_and(|t| t.len() >= 32),
        "non-loopback server requires CAIRN_TOKEN of at least 32 characters"
    );
    let listener = tokio::net::TcpListener::bind(address).await?;
    eprintln!("Cairn listening on {}", listener.local_addr()?);
    axum::serve(listener, router(root, token))
        .with_graceful_shutdown(shutdown_signal()?)
        .await?;
    Ok(())
}

pub async fn serve_unix(
    root: PathBuf,
    path: &std::path::Path,
    token: Option<String>,
) -> Result<()> {
    let shutdown = shutdown_signal()?;
    let (listener, _socket_path) = crate::socket::SocketPath::bind(path).await?;
    eprintln!("Cairn listening on Unix socket {}", path.display());
    axum::serve(listener, router(root, token))
        .with_graceful_shutdown(shutdown)
        .await?;
    Ok(())
}

fn shutdown_signal() -> Result<impl Future<Output = ()>> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    Ok(async move {
        tokio::select! {
            _ = interrupt.recv() => {},
            _ = terminate.recv() => {},
        }
    })
}

fn router(root: PathBuf, token: Option<String>) -> Router {
    let app = App {
        root,
        token,
        gate: Arc::new(tokio::sync::Semaphore::new(4)),
        imports: Arc::new(tokio::sync::Semaphore::new(1)),
    };
    Router::new()
        .route("/v1/{domain}/import", post(import))
        .route("/v1/{domain}/{bucket}", get(list))
        .route(
            "/v1/{domain}/{bucket}/{id}",
            get(fetch).head(exists).put(put),
        )
        .layer(DefaultBodyLimit::max(MAX_OBJECT))
        .layer(middleware::from_fn_with_state(app.clone(), authorize))
        .with_state(app)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn storage_errors_keep_their_http_meaning() {
        for errno in [28, 122] {
            let e = anyhow::Error::new(std::io::Error::from_raw_os_error(errno))
                .context("store object");
            assert_eq!(error(e).0, StatusCode::INSUFFICIENT_STORAGE);
        }
        assert_eq!(
            error(std::io::Error::from_raw_os_error(5).into()).0,
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }
}
