// preview.rs — link previews (GET /api/preview?url=…), fetched here so the
// client isn't stopped by CORS.
//
// What comes back depends on what the link is:
//   - a page: its title, description, picture and site name, from the
//     Open Graph / Twitter meta tags (plus a playable video, when the page
//     names an mp4/webm one as og:video);
//   - an image or a video file itself (a direct link to a .png, .gif, .mp4…,
//     told apart by the Content-Type the site sends): kind "image"/"video",
//     so the client shows the media instead of a card;
//   - a Reddit post: from Reddit's public JSON for the post (its pages turn
//     away bots), with oEmbed as the fallback.
//
// Only for signed-in members, and never for addresses on a private network:
// without that, anyone could use the server to fetch pages for them, or to
// read pages from the network it sits on (a router's admin page, say). Every
// redirect is checked the same way. Only the first 1 MB of a page is read
// (the meta tags are in its <head>, which ends long before), and nothing of
// an image or video is downloaded at all.

use axum::{
    extract::{Query, State},
    http::{header, HeaderMap, StatusCode},
    Json,
};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use crate::state::AppState;

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
    /// "image" or "video" when the link is that media file itself; absent
    /// for a page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind:        Option<String>,
    /// A video file the page offers to play (og:video), if it's one a
    /// browser can play directly.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video:       Option<String>,
}

type ApiErr = (StatusCode, Json<serde_json::Value>);

fn err(msg: &str) -> ApiErr {
    (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": msg })))
}

const MAX_PAGE_BYTES: usize = 1 << 20;
const MAX_REDIRECTS: usize = 5;
const USER_AGENT: &str = "Mozilla/5.0 (compatible; Chriscord/1.0; link preview)";
/// Reddit asks API clients for a descriptive user agent of this shape.
const REDDIT_USER_AGENT: &str = "server:chriscord-link-preview:1.0 (self-hosted chat server)";

pub async fn get_preview(
    headers: HeaderMap,
    State(s): State<Arc<AppState>>,
    Query(q): Query<PreviewQuery>,
) -> Result<Json<PreviewResp>, ApiErr> {
    let token = headers.get("X-Session-Token").and_then(|v| v.to_str().ok()).unwrap_or("");
    if crate::db::verify_token(&s.pool, token).await.ok().flatten().is_none() {
        return Err((StatusCode::UNAUTHORIZED, Json(serde_json::json!({ "error": "Not signed in" }))));
    }
    let url = Url::parse(q.url.trim()).map_err(|_| err("Invalid URL"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(err("Invalid URL"));
    }

    // Redirects are followed here, one at a time, so each one is checked.
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(6))
        .user_agent(USER_AGENT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| err("Client error"))?;

    if is_reddit(&url) {
        if let Some(p) = reddit_preview(&client, &url).await {
            return Ok(Json(p));
        }
    }

    let (final_url, mut resp) = fetch(&client, url.clone()).await?;
    let shown = url.to_string();

    // A direct link to an image or a video: show the media, read none of it.
    let ctype = resp.headers().get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok()).unwrap_or("").to_ascii_lowercase();
    if ctype.starts_with("image/") {
        return Ok(Json(PreviewResp {
            url: shown, kind: Some("image".into()), image: Some(final_url.to_string()), ..Default::default()
        }));
    }
    if ctype.starts_with("video/") {
        return Ok(Json(PreviewResp {
            url: shown, kind: Some("video".into()), video: Some(final_url.to_string()), ..Default::default()
        }));
    }
    if !ctype.is_empty() && !ctype.contains("html") && !ctype.contains("xml") {
        return Err(err("Not a web page")); // a PDF, a zip… nothing to show
    }

    // The page, up to the end of its <head> (or 1 MB).
    let mut body: Vec<u8> = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|_| err("Read failed"))? {
        let from = body.len().saturating_sub(8);
        body.extend_from_slice(&chunk);
        if body.len() >= MAX_PAGE_BYTES || contains_ci(&body[from..], b"</head") {
            break;
        }
    }
    let html = String::from_utf8_lossy(&body).into_owned();
    let base = final_url.as_str();

    let video_type = extract_meta(&html, "og:video:type").unwrap_or_default().to_ascii_lowercase();
    let video = ["og:video:secure_url", "og:video:url", "og:video"].iter()
        .find_map(|k| extract_meta(&html, k))
        .map(|v| resolve_url(&v, base))
        .filter(|v| video_type.starts_with("video/") || is_video_file(v));

    Ok(Json(PreviewResp {
        url:         shown,
        title:       extract_meta(&html, "og:title")
                        .or_else(|| extract_meta(&html, "twitter:title"))
                        .or_else(|| extract_title(&html)),
        description: extract_meta(&html, "og:description")
                        .or_else(|| extract_meta(&html, "twitter:description"))
                        .or_else(|| extract_meta(&html, "description")),
        image:       extract_meta(&html, "og:image")
                        .or_else(|| extract_meta(&html, "twitter:image"))
                        .map(|img| resolve_url(&img, base)),
        site_name:   extract_meta(&html, "og:site_name"),
        kind:        None,
        video,
    }))
}

// ── Fetching safely ───────────────────────────────────────────────────────────

/// GETs url, following up to MAX_REDIRECTS redirects, refusing any address
/// on a private network. Returns the final URL and its (successful) response.
async fn fetch(client: &reqwest::Client, mut url: Url) -> Result<(Url, reqwest::Response), ApiErr> {
    for _ in 0..=MAX_REDIRECTS {
        check_public(&url).await?;
        let resp = client.get(url.clone()).send().await.map_err(|_| err("Fetch failed"))?;
        if resp.status().is_redirection() {
            let next = resp.headers().get(header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|loc| url.join(loc).ok())
                .ok_or_else(|| err("Bad redirect"))?;
            if !matches!(next.scheme(), "http" | "https") {
                return Err(err("Bad redirect"));
            }
            url = next;
            continue;
        }
        if !resp.status().is_success() {
            return Err(err("Fetch failed"));
        }
        return Ok((url, resp));
    }
    Err(err("Too many redirects"))
}

/// Refuses a URL whose host is, or resolves to, a private, local or
/// otherwise non-public address.
async fn check_public(url: &Url) -> Result<(), ApiErr> {
    let refuse = || err("That address isn't on the public internet");
    let port = url.port_or_known_default().unwrap_or(80);
    let host = url.host_str().ok_or_else(|| err("Invalid URL"))?;
    let literal = host.trim_start_matches('[').trim_end_matches(']').parse::<IpAddr>().ok();
    let ips: Vec<IpAddr> = match literal {
        Some(ip) => vec![ip],
        None => tokio::net::lookup_host((host, port)).await
            .map_err(|_| err("Fetch failed"))?
            .map(|a| a.ip())
            .collect(),
    };
    if ips.is_empty() || !ips.into_iter().all(is_public) {
        return Err(refuse());
    }
    Ok(())
}

fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => {
            let o = v.octets();
            !(v.is_private() || v.is_loopback() || v.is_link_local() || v.is_unspecified()
                || v.is_broadcast() || v.is_documentation() || v.is_multicast()
                || o[0] == 0 || o[0] >= 240
                || (o[0] == 100 && (o[1] & 0xc0) == 64)) // carrier-grade NAT, 100.64.0.0/10
        }
        IpAddr::V6(v) => {
            if let Some(v4) = v.to_ipv4_mapped() {
                return is_public(IpAddr::V4(v4));
            }
            let s = v.segments();
            !(v.is_loopback() || v.is_unspecified() || v.is_multicast()
                || (s[0] & 0xfe00) == 0xfc00   // unique local, fc00::/7
                || (s[0] & 0xffc0) == 0xfe80)  // link local, fe80::/10
        }
    }
}

// ── Reddit ────────────────────────────────────────────────────────────────────

fn is_reddit(url: &Url) -> bool {
    let host = url.host_str().unwrap_or("").to_ascii_lowercase();
    host == "reddit.com" || host.ends_with(".reddit.com") || host == "redd.it" || host == "v.redd.it"
}

/// A Reddit post's preview, from its JSON (title, subreddit, text, picture),
/// or from Reddit's oEmbed if that's refused. None if the link isn't a post
/// (a subreddit, a user) or Reddit answers neither.
async fn reddit_preview(client: &reqwest::Client, url: &Url) -> Option<PreviewResp> {
    // Share links (reddit.com/r/x/s/…, redd.it/…, v.redd.it/…) redirect to
    // the post: follow them, reading only where they point.
    let mut post = url.clone();
    for _ in 0..MAX_REDIRECTS {
        if post_id(&post).is_some() {
            break;
        }
        let resp = client.get(post.clone()).header(header::USER_AGENT, REDDIT_USER_AGENT).send().await.ok()?;
        if !resp.status().is_redirection() {
            break;
        }
        post = resp.headers().get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|loc| post.join(loc).ok())?;
        if !is_reddit(&post) {
            return None;
        }
    }
    let id = post_id(&post)?;
    let shown = url.to_string();

    let json: Option<serde_json::Value> = async {
        let resp = client
            .get(format!("https://www.reddit.com/comments/{id}.json?raw_json=1&limit=1"))
            .header(header::USER_AGENT, REDDIT_USER_AGENT)
            .send().await.ok()?;
        if !resp.status().is_success() {
            tracing::debug!("preview: reddit JSON for {id} answered {}", resp.status());
            return None;
        }
        resp.json().await.ok()
    }.await;
    if let Some(d) = json.as_ref().map(|v| &v[0]["data"]["children"][0]["data"]).filter(|d| d.is_object()) {
        let s = |k: &str| d[k].as_str().unwrap_or("").to_string();
        let nsfw = d["over_18"].as_bool().unwrap_or(false);
        let spoiler = d["spoiler"].as_bool().unwrap_or(false);
        // Its picture: the image itself for an image post, else Reddit's
        // preview of the link or video, else a gallery's first image. None
        // for NSFW or spoiler posts.
        let image = if nsfw || spoiler { None } else {
            let direct = Some(s("url_overridden_by_dest")).filter(|u| is_image_file(u));
            direct
                .or_else(|| d["preview"]["images"][0]["source"]["url"].as_str().map(str::to_string))
                .or_else(|| {
                    let first = d["gallery_data"]["items"][0]["media_id"].as_str()?;
                    d["media_metadata"][first]["s"]["u"].as_str().map(str::to_string)
                })
        };
        let mut description: String = s("selftext").chars().take(300).collect();
        if description.is_empty() {
            let comments = d["num_comments"].as_i64().unwrap_or(0);
            description = format!("Posted by u/{} · {} comment{}", s("author"), comments, if comments == 1 { "" } else { "s" });
        }
        let mut site = format!("Reddit · {}", s("subreddit_name_prefixed"));
        if nsfw {
            site.push_str(" · NSFW");
        }
        return Some(PreviewResp {
            url: shown,
            title: Some(s("title")).filter(|t| !t.is_empty()),
            description: Some(description),
            image,
            site_name: Some(site),
            ..Default::default()
        });
    }

    // Fallback: oEmbed (title and author only).
    let mut oembed = Url::parse("https://www.reddit.com/oembed").ok()?;
    oembed.query_pairs_mut().append_pair("url", post.as_str());
    let o: serde_json::Value = client.get(oembed).header(header::USER_AGENT, REDDIT_USER_AGENT)
        .send().await.ok()?.error_for_status().ok()?.json().await.ok()?;
    let title = o["title"].as_str().filter(|t| !t.is_empty())?.to_string();
    Some(PreviewResp {
        url: shown,
        title: Some(title),
        description: o["author_name"].as_str().map(|a| format!("Posted by u/{a}")),
        site_name: Some("Reddit".into()),
        ..Default::default()
    })
}

/// The post id in a Reddit post URL (…/comments/<id>/…), if it is one.
fn post_id(url: &Url) -> Option<String> {
    let segs: Vec<&str> = url.path_segments()?.collect();
    let i = segs.iter().position(|s| *s == "comments")?;
    let id = segs.get(i + 1)?;
    (!id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric())).then(|| id.to_string())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn file_ext(u: &str) -> String {
    let path = u.split(['?', '#']).next().unwrap_or("");
    path.rsplit('/').next().and_then(|f| f.rsplit_once('.')).map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default()
}
fn is_image_file(u: &str) -> bool { matches!(file_ext(u).as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp" | "avif" | "bmp") }
fn is_video_file(u: &str) -> bool { matches!(file_ext(u).as_str(), "mp4" | "webm" | "mov" | "m4v") }

/// Case-insensitive search for an ASCII needle.
fn contains_ci(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w.eq_ignore_ascii_case(needle))
}

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

/// Makes a URL found on a page (an image, a video) absolute.
fn resolve_url(found: &str, base_url: &str) -> String {
    Url::parse(base_url)
        .and_then(|b| b.join(found))
        .map(|u| u.to_string())
        .unwrap_or_else(|_| found.to_string())
}

fn decode_html_entities(s: &str) -> String {
    s.replace("&amp;",  "&")
     .replace("&lt;",   "<")
     .replace("&gt;",   ">")
     .replace("&quot;", "\"")
     .replace("&#39;",  "'")
     .replace("&apos;", "'")
}
