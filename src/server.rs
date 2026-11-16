use crate::model::{LibraryState, Track};
use axum::{
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, Path as AxPath, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

const INDEX_HTML: &str = include_str!("../web/index.html");
const APP_JS: &str = include_str!("../web/app.js");
const STYLE_CSS: &str = include_str!("../web/style.css");
const MANIFEST: &str = include_str!("../web/manifest.webmanifest");
const SERVICE_WORKER: &str = include_str!("../web/sw.js");
const ICON_192: &[u8] = include_bytes!("../assets/icon-192.png");
const ICON_512: &[u8] = include_bytes!("../assets/icon-512.png");
const PIXEL_FONT: &[u8] = include_bytes!("../assets/fonts/Jersey10-Regular.ttf");

#[derive(Clone)]
pub struct Web {
    pub state: Arc<RwLock<LibraryState>>,
    pub name: String,
}

async fn cors(req: axum::extract::Request, next: Next) -> Response {
    let mut res = next.run(req).await;
    let headers = res.headers_mut();
    headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    headers.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, HeaderValue::from_static("*"));
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, OPTIONS"),
    );
    res
}

pub fn spawn(state: Arc<RwLock<LibraryState>>, name: String, port: u16) -> std::io::Result<()> {
    // Пробуем занять порт сразу, чтобы сообщить об ошибке в UI.
    let listener = std::net::TcpListener::bind(("0.0.0.0", port))?;
    listener.set_nonblocking(true)?;

    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
            Ok(rt) => rt,
            Err(_) => return,
        };
        rt.block_on(async move {
            let web = Web { state, name };
            let app = Router::new()
                .route("/", get(index))
                .route("/app.js", get(app_js))
                .route("/style.css", get(style))
                .route("/manifest.webmanifest", get(manifest))
                .route("/sw.js", get(sw))
                .route("/icon-192.png", get(icon_192))
                .route("/icon-512.png", get(icon_512))
                .route("/fonts/pixel.ttf", get(pixel_font))
                .route("/api/health", get(health))
                .route("/api/library", get(library))
                .route("/api/stream/:id", get(stream))
                .route("/api/playlist/:id/m3u", get(playlist_m3u))
                .route("/api/upload", post(upload))
                .layer(DefaultBodyLimit::max(512 * 1024 * 1024))
                .layer(middleware::from_fn(cors))
                .with_state(web);

            if let Ok(listener) = tokio::net::TcpListener::from_std(listener) {
                let _ = axum::serve(listener, app).await;
            }
        });
    });
    Ok(())
}

async fn index() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], INDEX_HTML)
}

async fn app_js() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8")], APP_JS)
}

async fn style() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], STYLE_CSS)
}

async fn manifest() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "application/manifest+json")], MANIFEST)
}

async fn sw() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8")], SERVICE_WORKER)
}

async fn icon_192() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "image/png")], ICON_192)
}

async fn icon_512() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "image/png")], ICON_512)
}

async fn pixel_font() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "font/ttf")], PIXEL_FONT)
}

async fn health(State(web): State<Web>) -> impl IntoResponse {
    axum::Json(json!({ "app": "mpt.mp", "name": web.name }))
}

async fn library(State(web): State<Web>) -> impl IntoResponse {
    let state = web.state.read().unwrap();
    let tracks: Vec<Track> = state.tracks.clone();
    let playlists: Vec<serde_json::Value> = state
        .playlists
        .iter()
        .map(|p| json!({ "id": p.id, "name": p.name, "count": p.track_ids.len(), "track_ids": p.track_ids }))
        .collect();
    axum::Json(json!({
        "app": "mpt.mp",
        "name": web.name,
        "tracks": tracks,
        "playlists": playlists,
    }))
}

fn parse_range(value: &str, total: u64) -> Option<(u64, u64)> {
    let spec = value.strip_prefix("bytes=")?;
    let (start_s, end_s) = spec.split_once('-')?;
    if start_s.is_empty() {
        let suffix: u64 = end_s.parse().ok()?;
        let start = total.saturating_sub(suffix);
        return Some((start, total.saturating_sub(1)));
    }
    let start: u64 = start_s.parse().ok()?;
    let end = if end_s.is_empty() {
        total.saturating_sub(1)
    } else {
        end_s.parse::<u64>().ok()?.min(total.saturating_sub(1))
    };
    if start > end {
        return None;
    }
    Some((start, end))
}

async fn stream(State(web): State<Web>, AxPath(id): AxPath<String>, headers: HeaderMap) -> Response {
    let path: Option<PathBuf> = {
        let state = web.state.read().unwrap();
        state.track(&id).map(|t| PathBuf::from(&t.path))
    };
    let Some(path) = path else {
        return (StatusCode::NOT_FOUND, "трек не найден").into_response();
    };
    let Ok(meta) = tokio::fs::metadata(&path).await else {
        return (StatusCode::NOT_FOUND, "файл недоступен").into_response();
    };
    let total = meta.len();
    if total == 0 {
        return (StatusCode::NO_CONTENT, "").into_response();
    }
    let mime = mime_guess::from_path(&path)
        .first_or_octet_stream()
        .to_string();

    let range_header = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.to_string());
    let range = range_header.as_deref().and_then(|v| parse_range(v, total));
    let (start, end) = range.unwrap_or((0, total - 1));
    let len = end.saturating_sub(start) + 1;

    let Ok(mut file) = tokio::fs::File::open(&path).await else {
        return (StatusCode::INTERNAL_SERVER_ERROR, "ошибка чтения").into_response();
    };
    if start > 0 && file.seek(std::io::SeekFrom::Start(start)).await.is_err() {
        return (StatusCode::INTERNAL_SERVER_ERROR, "ошибка seek").into_response();
    }

    let stream = ReaderStream::new(file.take(len));
    let mut builder = Response::builder()
        .status(if range.is_some() {
            StatusCode::PARTIAL_CONTENT
        } else {
            StatusCode::OK
        })
        .header(header::CONTENT_TYPE, mime)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_LENGTH, len.to_string());
    if range.is_some() {
        builder = builder.header(
            header::CONTENT_RANGE,
            format!("bytes {start}-{end}/{total}"),
        );
    }
    builder
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

async fn playlist_m3u(State(web): State<Web>, AxPath(id): AxPath<String>) -> Response {
    let body = {
        let state = web.state.read().unwrap();
        state
            .playlist(&id)
            .map(|p| (p.name.clone(), crate::transfer::m3u_text(p, &state.tracks_of(p))))
    };
    match body {
        Some((name, text)) => {
            let disposition = format!(
                "attachment; filename=\"{0}.m3u\"; filename*=UTF-8''{1}.m3u",
                crate::transfer::safe_stem(&name),
                crate::transfer::url_encode(&name)
            );
            (
                [
                    (header::CONTENT_TYPE, "audio/x-mpegurl".to_string()),
                    (header::CONTENT_DISPOSITION, disposition),
                ],
                text,
            )
                .into_response()
        }
        None => (StatusCode::NOT_FOUND, "плейлист не найден").into_response(),
    }
}

async fn upload(
    State(_web): State<Web>,
    Query(params): Query<HashMap<String, String>>,
    body: Bytes,
) -> Response {
    let raw_name = params
        .get("name")
        .cloned()
        .unwrap_or_else(|| "upload.mp3".to_string());
    let file_name = sanitize(&raw_name);
    if !crate::library::is_audio(&PathBuf::from(&file_name)) {
        return (StatusCode::BAD_REQUEST, "не аудиофайл").into_response();
    }
    let dir = crate::model::synced_dir();
    let _ = tokio::fs::create_dir_all(&dir).await;
    let mut target = dir.join(&file_name);
    // Раньше файл с тем же именем молча перезаписывался. Теперь: такой же размер —
    // считаем дублем и ничего не пишем, иначе сохраняем под новым именем «имя (2).ext».
    if let Ok(meta) = tokio::fs::metadata(&target).await {
        if meta.len() == body.len() as u64 {
            return axum::Json(json!({
                "saved": target.to_string_lossy(),
                "bytes": body.len(),
                "duplicate": true,
            }))
            .into_response();
        }
        target = crate::transfer::unique_target(&dir, &file_name);
    }
    if let Err(e) = tokio::fs::write(&target, &body).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("не удалось сохранить: {e}"),
        )
            .into_response();
    }
    axum::Json(json!({
        "saved": target.to_string_lossy(),
        "bytes": body.len(),
    }))
    .into_response()
}

pub fn sanitize(name: &str) -> String {
    let base = name.rsplit('/').next().unwrap_or(name);
    let cleaned: String = base
        .chars()
        .map(|c| match c {
            '/' | '\\' | '\0' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let cleaned = cleaned.trim().trim_matches('.').to_string();
    if cleaned.is_empty() {
        "track.mp3".to_string()
    } else {
        cleaned
    }
}
