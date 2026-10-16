//! 嵌入前端产物服务（build.rs 生成的 FILES 表 + SPA fallback）。
//!
//! 资源按字节嵌入：前端产物含字体（woff2/woff/ttf）等非 UTF-8 文件，
//! 服务时按扩展名给出 Content-Type。

pub mod assets {
    include!(concat!(env!("OUT_DIR"), "/assets.rs"));
}
pub use assets::FILES;

use axum::body::Body;
use axum::http::{Response, StatusCode, header};
use axum::response::IntoResponse;

fn content_type(path: &str) -> &'static str {
    if path.ends_with(".html") {
        "text/html; charset=utf-8"
    } else if path.ends_with(".css") {
        "text/css; charset=utf-8"
    } else if path.ends_with(".js") || path.ends_with(".mjs") {
        "application/javascript; charset=utf-8"
    } else if path.ends_with(".json") || path.ends_with(".map") {
        "application/json; charset=utf-8"
    } else if path.ends_with(".svg") {
        "image/svg+xml"
    } else if path.ends_with(".png") {
        "image/png"
    } else if path.ends_with(".jpg") || path.ends_with(".jpeg") {
        "image/jpeg"
    } else if path.ends_with(".webp") {
        "image/webp"
    } else if path.ends_with(".gif") {
        "image/gif"
    } else if path.ends_with(".ico") {
        "image/x-icon"
    } else if path.ends_with(".woff2") {
        "font/woff2"
    } else if path.ends_with(".woff") {
        "font/woff"
    } else if path.ends_with(".ttf") {
        "font/ttf"
    } else if path.ends_with(".webmanifest") {
        "application/manifest+json"
    } else {
        "application/octet-stream"
    }
}

fn lookup(path: &str) -> Option<&'static [u8]> {
    assets::FILES
        .iter()
        .find(|(p, _)| *p == path)
        .map(|(_, content)| *content)
}

fn respond(path: &str, bytes: &'static [u8]) -> Response<Body> {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type(path))
        .header(
            header::CACHE_CONTROL,
            if path == "index.html" {
                "no-cache"
            } else {
                "public, max-age=31536000, immutable"
            },
        )
        .body(Body::from(bytes))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// SPA fallback 服务：命中静态资源返回内容，否则返回 index.html（前端路由）。
pub async fn embedded_serve(uri: axum::http::Uri) -> Response<Body> {
    let path = uri.path().trim_start_matches('/');
    let candidate = if path.is_empty() { "index.html" } else { path };
    if let Some(content) = lookup(candidate) {
        return respond(candidate, content);
    }
    // SPA fallback：未知路径返回 index.html（前端路由接管）
    if let Some(index) = lookup("index.html") {
        return respond("index.html", index);
    }
    StatusCode::NOT_FOUND.into_response()
}
