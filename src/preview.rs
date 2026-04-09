use axum::{
    extract::Query,
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Deserialize)]
pub struct PreviewQuery {
    pub url: String,
}

#[derive(Serialize, Default)]
pub struct PreviewResp {
    pub url:         String,
    pub title:       Option<String>,
    pub description: Option<String>,
    pub image:       Option<String>,
    pub site_name:   Option<String>,
}

type ApiErr = (StatusCode, Json<serde_json::Value>);

fn err(msg: &str) -> ApiErr {
    (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": msg })))
}

/// GET /api/preview?url=https://...
/// Fetches OG/twitter meta tags server-side to avoid CORS issues on the client.
/// Returns whatever it can find; client handles missing fields gracefully.
pub async fn get_preview(Query(q): Query<PreviewQuery>) -> Result<Json<PreviewResp>, ApiErr> {
    let url = q.url.trim().to_string();

    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err(err("Invalid URL"));
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .user_agent("Mozilla/5.0 (compatible; Chriscord/1.0)")
        .build()
        .map_err(|_| err("Client error"))?;

    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|_| err("Fetch failed"))?;

    let body = resp
        .text()
        .await
        .map_err(|_| err("Read failed"))?;

    Ok(Json(PreviewResp {
        url:         url.clone(),
        title:       extract_meta(&body, "og:title")
                        .or_else(|| extract_meta(&body, "twitter:title"))
                        .or_else(|| extract_title(&body)),
        description: extract_meta(&body, "og:description")
                        .or_else(|| extract_meta(&body, "twitter:description")),
        image:       extract_meta(&body, "og:image")
                        .or_else(|| extract_meta(&body, "twitter:image"))
                        .map(|img| resolve_image(&img, &url)),
        site_name:   extract_meta(&body, "og:site_name"),
    }))
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Extract content from <meta property="KEY" content="VALUE"> or
/// <meta name="KEY" content="VALUE">
fn extract_meta(html: &str, key: &str) -> Option<String> {
    let lower = html.to_lowercase();
    let key_lower = key.to_lowercase();

    // Try property="key" and name="key" variants
    for attr in &[format!("property=\"{}\"", key_lower), format!("name=\"{}\"", key_lower)] {
        if let Some(pos) = lower.find(attr.as_str()) {
            // Find the surrounding <meta ... > tag
            let start = lower[..pos].rfind('<').unwrap_or(0);
            if let Some(end) = lower[pos..].find('>') {
                let tag = &html[start..pos + end + 1];
                if let Some(val) = extract_attr(tag, "content") {
                    return Some(decode_html_entities(&val));
                }
            }
        }
    }
    None
}

/// Extract <title>...</title>
fn extract_title(html: &str) -> Option<String> {
    let lower = html.to_lowercase();
    let start = lower.find("<title")? + 6;
    let start = lower[start..].find('>')? + start + 1;
    let end   = lower[start..].find("</title>")? + start;
    Some(decode_html_entities(html[start..end].trim()))
}

/// Extract a specific attribute value from a tag string.
fn extract_attr(tag: &str, attr: &str) -> Option<String> {
    let lower = tag.to_lowercase();
    let needle = format!("{}=\"", attr.to_lowercase());
    let pos = lower.find(&needle)? + needle.len();
    let rest = &tag[pos..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// If the image URL is relative, make it absolute.
fn resolve_image(img: &str, base_url: &str) -> String {
    if img.starts_with("http") { return img.to_string(); }
    if img.starts_with("//")   { return format!("https:{}", img); }

    // Parse base URL to get origin
    if let Some(end) = base_url.find("://") {
        if let Some(path_start) = base_url[end+3..].find('/') {
            let origin = &base_url[..end + 3 + path_start];
            if img.starts_with('/') {
                return format!("{}{}", origin, img);
            }
            return format!("{}/{}", origin, img);
        }
    }
    img.to_string()
}

fn decode_html_entities(s: &str) -> String {
    s.replace("&amp;",  "&")
     .replace("&lt;",   "<")
     .replace("&gt;",   ">")
     .replace("&quot;", "\"")
     .replace("&#39;",  "'")
     .replace("&apos;", "'")
}
