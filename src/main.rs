use axum::{
    extract::{Json, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;
use tracing::{error, info, warn};

#[derive(Clone)]
struct AppState {
    client: Arc<Client>,
}

#[derive(Deserialize)]
struct ScrapeRequest {
    url: String,
}

#[derive(Serialize)]
struct ScrapeResponse {
    success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// 富文本片段（无 html/head/body，样式全内联，可直接塞入编辑器）
    html: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Serialize)]
struct ChapterItem {
    title: String,
    content: String,
}

#[derive(Serialize)]
struct ScrapeAllResponse {
    success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    total: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    items: Option<Vec<ChapterItem>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "fire_scraper=info,tower_http=info".into()),
        )
        .init();



    let client = Client::builder()
        .use_rustls_tls()
        .danger_accept_invalid_certs(true)
        .user_agent(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) \
             AppleWebKit/537.36 (KHTML, like Gecko) \
             Chrome/120.0.0.0 Safari/537.36",
        )
        .timeout(Duration::from_secs(30))
        .build()
        .expect("构建 HTTP 客户端失败");

    let state = AppState { client: Arc::new(client) };

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let app = Router::new()
        .route("/scrape",     post(scrape_handler))
        .route("/scrape-all", post(scrape_all_handler))
        .route("/health",     get(health_handler))
        .layer(cors)
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let addr = "0.0.0.0:3000";
    info!("🚀 服务启动 http://{}", addr);
    info!("POST /scrape      单页抓取  → 富文本片段");
    info!("POST /scrape-all  全文抓取  → 富文本片段（含目录）");

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

// ─────────────────────────────────────────────
//  Handlers
// ─────────────────────────────────────────────

async fn health_handler() -> Response {
    Json(serde_json::json!({ "status": "ok" })).into_response()
}

async fn scrape_handler(
    State(state): State<AppState>,
    Json(payload): Json<ScrapeRequest>,
) -> Response {
    let url = payload.url.trim().to_string();
    info!("单页抓取: {}", url);

    if !is_valid_url(&url) {
        return bad_request("URL 必须以 http:// 或 https:// 开头");
    }

    match do_scrape(&state.client, &url).await {
        Ok((title, html)) => {
            info!("✓ 单页成功: {}", url);
            Json(ScrapeResponse {
                success: true, html: Some(html), title: Some(title), error: None,
            }).into_response()
        }
        Err(e) => {
            error!("✗ 单页失败: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ScrapeResponse {
                success: false, html: None, title: None, error: Some(e),
            })).into_response()
        }
    }
}

async fn scrape_all_handler(
    State(state): State<AppState>,
    Json(payload): Json<ScrapeRequest>,
) -> Response {
    let url = payload.url.trim().to_string();
    info!("全文抓取起始: {}", url);

    if !is_valid_url(&url) {
        return bad_request("URL 必须以 http:// 或 https:// 开头");
    }

    match do_scrape_all(&state.client, &url).await {
        Ok(items) => {
            info!("✓ 全文完成，共 {} 章", items.len());
            let total = items.len();
            Json(ScrapeAllResponse {
                success: true,
                total: Some(total),
                items: Some(items),
                error: None,
            }).into_response()
        }
        Err(e) => {
            error!("✗ 全文失败: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ScrapeAllResponse {
                success: false, total: None, items: None, error: Some(e),
            })).into_response()
        }
    }
}

// ─────────────────────────────────────────────
//  全文抓取逻辑
// ─────────────────────────────────────────────

async fn do_scrape_all(
    client: &Client,
    first_url: &str,
) -> Result<Vec<ChapterItem>, String> {
    let base_url = extract_base_url(first_url);
    let first_html = fetch_html(client, first_url).await?;
    let chapter_links = extract_chapter_links(&first_html, &base_url);

    let links: Vec<(String, String)> = if chapter_links.is_empty() {
        info!("侧边栏未找到章节，仅抓当前页");
        let (title, _) = parse_html(&first_html);
        vec![(title, first_url.to_string())]
    } else {
        info!("识别到 {} 个章节", chapter_links.len());
        chapter_links
    };

    let mut items: Vec<ChapterItem> = Vec::new();
    for (i, (sidebar_title, link)) in links.iter().enumerate() {
        info!("抓取 [{}/{}] {}", i + 1, links.len(), link);
        match fetch_html(client, link).await {
            Ok(raw) => {
                let (page_title, body) = parse_html(&raw);
                let title = if sidebar_title.is_empty() { page_title } else { sidebar_title.clone() };
                let img_map = collect_and_download_images(
                    client, &body, &extract_base_url(link),
                ).await;
                let body_with_imgs = replace_img_src(&body, &img_map);
                let content = div_to_p(&apply_inline_styles(&body_with_imgs));
                items.push(ChapterItem { title, content });
            }
            Err(e) => {
                warn!("章节抓取失败 {}: {}", link, e);
                items.push(ChapterItem {
                    title: sidebar_title.clone(),
                    content: format!("<p style=\"color:#e53e3e;\">抓取失败: {}</p>", e),
                });
            }
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }

    Ok(items)
}

// ─────────────────────────────────────────────
//  单页抓取
// ─────────────────────────────────────────────

async fn do_scrape(client: &Client, url: &str) -> Result<(String, String), String> {
    let raw_html = fetch_html(client, url).await?;
    let base_url = extract_base_url(url);
    let (title, body_html) = parse_html(&raw_html);

    info!("正文 {} 字节，处理图片...", body_html.len());
    let img_map = collect_and_download_images(client, &body_html, &base_url).await;
    let replaced = replace_img_src(&body_html, &img_map);
    let styled = div_to_p(&apply_inline_styles(&replaced));
    let fragment = build_rich_fragment_single(&title, &styled);

    info!("单页完成，图片 {} 张", img_map.len());
    Ok((title, fragment))
}

// ─────────────────────────────────────────────
//  关键：构建富文本片段（无 html/head/body，样式全内联）
// ─────────────────────────────────────────────

/// 单页富文本片段
fn build_rich_fragment_single(title: &str, body: &str) -> String {
    format!(
        "<h1 style=\"{h1}\">{title}</h1>\
         <hr style=\"{hr}\"/>\
         {body}",
        h1    = S_H1,
        hr    = S_HR,
        title = title,
        body  = body,
    )
}


// ─────────────────────────────────────────────
//  内联样式应用：把正文中的裸标签补上内联 style
//  富文本编辑器会过滤掉 <style> 块，必须内联
// ─────────────────────────────────────────────

fn apply_inline_styles(html: &str) -> String {
    use scraper::{Html, Selector};

    let doc = Html::parse_fragment(html);
    let mut output = html.to_string();

    // 需要补内联样式的标签及对应样式
    let rules: &[(&str, &str)] = &[
        ("p",      S_P),
        ("h1",     S_H1),
        ("h2",     S_H2),
        ("h3",     S_H3),
        ("h4",     S_H4),
        ("table",  S_TABLE),
        ("th",     S_TH),
        ("td",     S_TD),
        ("img",    S_IMG),
        ("strong", S_STRONG),
        ("ul",     S_UL),
        ("ol",     S_OL),
        ("li",     S_LI),
    ];

    for (tag, style) in rules {
        if let Ok(sel) = Selector::parse(tag) {
            for el in doc.select(&sel) {
                let existing_style = el.value().attr("style").unwrap_or("").to_string();
                // 合并：已有 style 保留，补充缺失的属性
                let merged = merge_styles(&existing_style, style);
                let old_open = opening_tag_str(&el);
                let new_open = rebuild_tag(&el, &merged);
                if old_open != new_open {
                    output = output.replacen(&old_open, &new_open, 1);
                }
            }
        }
    }
    output
}

/// 合并两个 style 字符串，base 优先（不覆盖已有属性）
fn merge_styles(existing: &str, defaults: &str) -> String {
    let mut props: Vec<(String, String)> = Vec::new();

    // 先收集已有属性
    for part in existing.split(';') {
        let part = part.trim();
        if part.is_empty() { continue; }
        if let Some(idx) = part.find(':') {
            let k = part[..idx].trim().to_lowercase();
            let v = part[idx+1..].trim().to_string();
            props.push((k, v));
        }
    }

    // 补入默认属性（已有的跳过）
    for part in defaults.split(';') {
        let part = part.trim();
        if part.is_empty() { continue; }
        if let Some(idx) = part.find(':') {
            let k = part[..idx].trim().to_lowercase();
            let v = part[idx+1..].trim().to_string();
            if !props.iter().any(|(ek, _)| ek == &k) {
                props.push((k, v));
            }
        }
    }

    props.iter()
        .map(|(k, v)| format!("{}: {}", k, v))
        .collect::<Vec<_>>()
        .join("; ")
}

/// 重建开标签字符串（用于替换原始 HTML 中的开标签）
fn rebuild_tag(el: &scraper::ElementRef, new_style: &str) -> String {
    let name = el.value().name();
    let mut attrs: Vec<String> = Vec::new();
    for (k, v) in el.value().attrs() {
        if k == "style" { continue; }
        attrs.push(format!("{}=\"{}\"", k, v));
    }
    if !new_style.is_empty() {
        attrs.push(format!("style=\"{}\"", new_style));
    }
    if attrs.is_empty() {
        format!("<{}>", name)
    } else {
        format!("<{} {}>", name, attrs.join(" "))
    }
}

/// 提取元素的原始开标签字符串（用于 replacen 定位）
fn opening_tag_str(el: &scraper::ElementRef) -> String {
    let name = el.value().name();
    let attrs: Vec<String> = el.value().attrs()
        .map(|(k, v)| format!("{}=\"{}\"", k, v))
        .collect();
    if attrs.is_empty() {
        format!("<{}>", name)
    } else {
        format!("<{} {}>", name, attrs.join(" "))
    }
}

// ─────────────────────────────────────────────
//  内联样式常量（富文本编辑器友好）
// ─────────────────────────────────────────────

const S_H1: &str =
    "font-size:20px; font-weight:bold; text-align:center; \
     letter-spacing:2px; margin:16px 0 10px; \
     font-family:Microsoft YaHei,SimSun,Arial,sans-serif; color:#222";

const S_H2: &str =
    "font-size:17px; font-weight:bold; margin:14px 0 8px; \
     border-left:4px solid #1a73e8; padding-left:10px; \
     font-family:Microsoft YaHei,SimSun,Arial,sans-serif; color:#222";

const S_H3: &str =
    "font-size:15px; font-weight:bold; margin:10px 0 6px; \
     font-family:Microsoft YaHei,SimSun,Arial,sans-serif; color:#333";

const S_H4: &str =
    "font-size:14px; font-weight:bold; margin:8px 0 4px; \
     font-family:Microsoft YaHei,SimSun,Arial,sans-serif; color:#333";

const S_P: &str =
    "font-size:14px; line-height:1.8; margin:6px 0; \
     font-family:Microsoft YaHei,SimSun,Arial,sans-serif; color:#333";

// ─────────────────────────────────────────────
//  div → p 替换（富文本编辑器不支持 div）
//  规则：<div ...> → <p ...>，</div> → </p>
//  嵌套 div 也一并处理，编辑器会自动打平
// ─────────────────────────────────────────────

fn div_to_p(html: &str) -> String {
    // chars() イテレーションで UTF-8 バイト境界 panic を完全回避
    let mut result = String::with_capacity(html.len());
    let chars: Vec<char> = html.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        // </div> を検出（6文字）
        if chars[i] == '<'
            && i + 5 < len
            && chars[i+1] == '/'
            && chars[i+2] == 'd'
            && chars[i+3] == 'i'
            && chars[i+4] == 'v'
            && chars[i+5] == '>'
        {
            result.push_str("</p>");
            i += 6;
            continue;
        }

        // <div を検出：次の文字がスペースか > のみ（<divider> 等を除外）
        if chars[i] == '<'
            && i + 3 < len
            && chars[i+1] == 'd'
            && chars[i+2] == 'i'
            && chars[i+3] == 'v'
        {
            let next = chars.get(i + 4).copied().unwrap_or('>');
            if next == '>' || next == ' ' || next == '\n' || next == '\r' || next == '\t' {
                result.push_str("<p");
                i += 4;
                continue;
            }
        }

        result.push(chars[i]);
        i += 1;
    }
    result
}

const S_TABLE: &str =
    "border-collapse:collapse; width:100%; margin:12px 0; \
     font-size:13px; font-family:Microsoft YaHei,SimSun,Arial,sans-serif";

const S_TH: &str =
    "border:1px solid #ccc; padding:7px 10px; \
     background-color:#f5f5f5; font-weight:bold; \
     text-align:center; color:#333";

const S_TD: &str =
    "border:1px solid #ccc; padding:6px 10px; \
     vertical-align:top; color:#333";

const S_IMG: &str =
    "max-width:100%; height:auto; display:block; margin:10px auto";

const S_STRONG: &str =
    "font-weight:bold; color:#222";

const S_UL: &str =
    "margin:6px 0; padding-left:24px; \
     font-size:14px; line-height:1.8; color:#333";

const S_OL: &str =
    "margin:6px 0; padding-left:24px; \
     font-size:14px; line-height:1.8; color:#333";

const S_LI: &str =
    "margin:3px 0; font-size:14px; line-height:1.8; color:#333";

const S_HR: &str =
    "border:none; border-top:1px solid #e0e0e0; margin:12px 0";

// ─────────────────────────────────────────────
//  HTTP 抓取 + 编码处理
// ─────────────────────────────────────────────

async fn fetch_html(client: &Client, url: &str) -> Result<String, String> {
    let resp = client
        .get(url)
        .header("Accept", "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8")
        .header("Accept-Language", "zh-CN,zh;q=0.9,en;q=0.8")
        .header("Referer", url)
        .send()
        .await
        .map_err(|e| format!("请求失败: {}", e))?;

    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }

    let ct_charset = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .and_then(|ct| {
            ct.split(';')
                .find(|p| p.trim().to_lowercase().starts_with("charset"))
                .and_then(|p| p.split('=').nth(1))
                .map(|s| s.trim().to_lowercase())
        });

    let bytes = resp.bytes().await.map_err(|e| format!("读取字节失败: {}", e))?;

    let charset = ct_charset
        .or_else(|| sniff_charset_from_bytes(&bytes))
        .unwrap_or_else(|| "utf-8".to_string());

    info!("页面编码: {}", charset);
    decode_bytes(&bytes, &charset)
}

fn sniff_charset_from_bytes(bytes: &[u8]) -> Option<String> {
    let head = &bytes[..bytes.len().min(4096)];
    let snippet = String::from_utf8_lossy(head).to_lowercase();
    if let Some(pos) = snippet.find("charset=") {
        let after = &snippet[pos + 8..];
        let charset: String = after
            .trim_start_matches(|c| c == '"' || c == '\'' || c == ' ')
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
            .collect();
        if !charset.is_empty() {
            return Some(charset);
        }
    }
    None
}

fn decode_bytes(bytes: &[u8], charset: &str) -> Result<String, String> {
    use encoding_rs::Encoding;
    let encoding = Encoding::for_label(charset.as_bytes())
        .unwrap_or(encoding_rs::UTF_8);
    let (decoded, _, had_errors) = encoding.decode(bytes);
    if had_errors {
        warn!("编码 {} 解码时有部分字符替换", charset);
    }
    Ok(decoded.into_owned())
}

// ─────────────────────────────────────────────
//  侧边栏链接 & HTML 解析（scraper 限定在函数内）
// ─────────────────────────────────────────────

fn extract_chapter_links(html: &str, base_url: &str) -> Vec<(String, String)> {
    use scraper::{Html, Selector};
    let doc = Html::parse_document(html);

    let selectors = [
        ".rul ul.show li a",
        ".sidebar ul li a",
        ".catalog ul li a",
        ".nav-list li a",
    ];

    for sel_str in &selectors {
        if let Ok(sel) = Selector::parse(sel_str) {
            let links: Vec<(String, String)> = doc
                .select(&sel)
                .filter_map(|el| {
                    el.value().attr("href").map(|href| {
                        let title = el.text().collect::<String>().trim().to_string();
                        let url = to_absolute(href, base_url);
                        (title, url)
                    })
                })
                .fold(Vec::new(), |mut acc, x| {
                    if !acc.iter().any(|(_, u)| u == &x.1) { acc.push(x); }
                    acc
                });
            if !links.is_empty() {
                info!("侧边栏命中: {}", sel_str);
                return links;
            }
        }
    }
    vec![]
}

fn parse_html(raw: &str) -> (String, String) {
    use scraper::{Html, Selector};
    let doc = Html::parse_document(raw);

    let title = ["h1", "title"].iter().find_map(|s| {
        Selector::parse(s).ok().and_then(|sel| {
            doc.select(&sel).next()
                .map(|el| el.text().collect::<String>().trim().to_string())
        }).filter(|t| !t.is_empty())
    }).unwrap_or_else(|| "未知标题".to_string());

    let body = ["#b_con", ".article-content", ".content-detail", "article"]
        .iter()
        .find_map(|s| {
            Selector::parse(s).ok().and_then(|sel| {
                doc.select(&sel).next().map(|el| el.inner_html())
            }).filter(|h| h.trim().len() > 50)
        })
        .or_else(|| {
            Selector::parse("body").ok().and_then(|sel| {
                doc.select(&sel).next().map(|el| el.inner_html())
            })
        })
        .unwrap_or_default();

    (title, body)
}

fn extract_img_srcs(html: &str) -> Vec<String> {
    use scraper::{Html, Selector};
    let doc = Html::parse_fragment(html);
    let sel = match Selector::parse("img") { Ok(s) => s, Err(_) => return vec![] };
    doc.select(&sel)
        .filter_map(|el| el.value().attr("src"))
        .map(|s| s.to_string())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect()
}

// ─────────────────────────────────────────────
//  图片下载 & Base64
// ─────────────────────────────────────────────

async fn collect_and_download_images(
    client: &Client,
    html: &str,
    base_url: &str,
) -> HashMap<String, String> {
    let srcs = extract_img_srcs(html);
    if srcs.is_empty() { return HashMap::new(); }
    info!("发现图片 {} 张，并发下载...", srcs.len());

    let tasks: Vec<_> = srcs.into_iter().map(|src| {
        let client = client.clone();
        let abs = to_absolute(&src, base_url);
        tokio::spawn(async move {
            match download_as_base64(&client, &abs).await {
                Ok(d)  => { info!("  ✓ {}", abs); Some((src, d)) }
                Err(e) => { warn!("  ✗ {} => {}", abs, e); None }
            }
        })
    }).collect();

    let mut map = HashMap::new();
    for t in tasks {
        if let Ok(Some((src, d))) = t.await { map.insert(src, d); }
    }
    map
}

async fn download_as_base64(client: &Client, url: &str) -> Result<String, String> {
    let resp = client.get(url).send().await
        .map_err(|e| format!("下载失败: {}", e))?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    let mime = resp.headers()
        .get("content-type")
        .and_then(|v: &reqwest::header::HeaderValue| v.to_str().ok())
        .map(|ct: &str| {
            if ct.contains("png")       { "image/png" }
            else if ct.contains("gif")  { "image/gif" }
            else if ct.contains("webp") { "image/webp" }
            else if ct.contains("svg")  { "image/svg+xml" }
            else                        { "image/jpeg" }
        })
        .unwrap_or("image/jpeg")
        .to_string();
    let bytes = resp.bytes().await.map_err(|e| format!("读取字节失败: {}", e))?;
    Ok(format!("data:{};base64,{}", mime, B64.encode(&bytes)))
}

fn replace_img_src(html: &str, map: &HashMap<String, String>) -> String {
    let mut result = html.to_string();
    for (src, data_uri) in map {
        result = result
            .replace(&format!("src=\"{}\"", src), &format!("src=\"{}\"", data_uri))
            .replace(&format!("src='{}'", src),   &format!("src='{}'", data_uri));
    }
    result
}

// ─────────────────────────────────────────────
//  工具函数
// ─────────────────────────────────────────────

fn is_valid_url(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

fn extract_base_url(url: &str) -> String {
    if let Some(idx) = url.find("://") {
        let after = &url[idx + 3..];
        let end = after.find('/').unwrap_or(after.len());
        return format!("{}://{}", &url[..idx], &after[..end]);
    }
    url.to_string()
}

fn to_absolute(src: &str, base: &str) -> String {
    if src.starts_with("http://") || src.starts_with("https://") { src.to_string() }
    else if src.starts_with("//") { format!("https:{}", src) }
    else if src.starts_with('/') { format!("{}{}", base, src) }
    else { format!("{}/{}", base, src) }
}

fn bad_request(msg: &str) -> Response {
    (StatusCode::BAD_REQUEST, Json(ScrapeResponse {
        success: false, html: None, title: None,
        error: Some(msg.to_string()),
    })).into_response()
}