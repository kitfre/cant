//! Network capabilities: `http/request` (always on by default) and `web/search` + `web/fetch`
//! (configurable via `--disable web` for harnesses like Antigravity that provide native web search).

use crate::capability::{
    ArgumentMode, CallContext, Capability, CapabilityCatalog, CapabilityError, CapabilityMeta,
};
use async_trait::async_trait;
use regex::Regex;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::redirect::Policy;
use serde_json::{json, Map, Value};
use std::net::{IpAddr, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;
use url::Url;

const DEFAULT_MAX_RESPONSE_BYTES: usize = 512 * 1024;
const MAX_RESPONSE_BYTES: usize = 768 * 1024;
const DEFAULT_HTTP_TIMEOUT_MS: u64 = 30_000;
const MAX_HTTP_TIMEOUT_MS: u64 = 120_000;
const MAX_WEB_FETCH_BYTES: usize = 50 * 1024;
const MAX_WEB_FETCH_LINES: usize = 2_000;

fn is_sensitive_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "set-cookie" | "set-cookie2" | "proxy-authenticate"
    )
}

fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_unspecified()
                || v4.octets()[0] == 100 && (v4.octets()[1] & 0xc0) == 64 // CGNAT 100.64.0.0/10
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || (v6.segments()[0] & 0xfe00) == 0xfc00 // ULA fc00::/7
                || (v6.segments()[0] & 0xffc0) == 0xfe80 // link-local fe80::/10
        }
    }
}

fn assert_public_url(url: &Url) -> Result<(), CapabilityError> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(CapabilityError::invalid_args(
            "web.fetch",
            "Only http and https URLs are supported.",
        ));
    }
    let host = url
        .host_str()
        .ok_or_else(|| CapabilityError::invalid_args("web.fetch", "URL has no host."))?;
    let port = url.port_or_known_default().unwrap_or(80);
    if host.eq_ignore_ascii_case("localhost") || host.ends_with(".local") {
        return Err(CapabilityError::exec(
            "NetworkPolicyError",
            "web/fetch refuses loopback and local hostnames.",
        ));
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        if is_private_ip(ip) {
            return Err(CapabilityError::exec(
                "NetworkPolicyError",
                "web/fetch refuses private/loopback IP addresses.",
            ));
        }
        return Ok(());
    }
    let addrs = (host, port).to_socket_addrs().map_err(|e| {
        CapabilityError::exec("NetworkError", format!("DNS resolution failed: {e}"))
    })?;
    for addr in addrs {
        if is_private_ip(addr.ip()) {
            return Err(CapabilityError::exec(
                "NetworkPolicyError",
                "web/fetch refuses hostnames resolving to private/loopback addresses.",
            ));
        }
    }
    Ok(())
}

async fn read_bounded_bytes(
    mut response: reqwest::Response,
    max_bytes: usize,
) -> Result<Vec<u8>, CapabilityError> {
    let mut buf = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| CapabilityError::exec("HttpError", e.to_string()))?
    {
        if buf.len() + chunk.len() > max_bytes {
            return Err(CapabilityError::exec(
                "ResponseTooLargeError",
                format!("HTTP response exceeded {max_bytes}-byte limit."),
            ));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

fn html_to_text(html: &str) -> String {
    let re_script = Regex::new(r"(?is)<script(?:\s[^>]*)?>.*?</script\s*>|<style(?:\s[^>]*)?>.*?</style\s*>|<noscript(?:\s[^>]*)?>.*?</noscript\s*>").unwrap();
    let stripped = re_script.replace_all(html, " ");
    let re_block = Regex::new(
        r"(?i)</?(p|div|br|li|tr|h[1-6]|section|article|header|footer|pre|blockquote)[^>]*>",
    )
    .unwrap();
    let with_breaks = re_block.replace_all(&stripped, "\n");
    let re_tags = Regex::new(r"<[^>]+>").unwrap();
    let no_tags = re_tags.replace_all(&with_breaks, "");
    let decoded = decode_basic_entities(&no_tags);
    let mut lines = Vec::new();
    let mut prev_blank = false;
    for raw in decoded.lines() {
        let line = raw.split_whitespace().collect::<Vec<_>>().join(" ");
        if line.is_empty() {
            if !prev_blank && !lines.is_empty() {
                lines.push(String::new());
                prev_blank = true;
            }
        } else {
            lines.push(line);
            prev_blank = false;
        }
    }
    lines.join("\n").trim().to_string()
}

fn decode_basic_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ")
}

fn extract_title(html: &str) -> Option<String> {
    let re = Regex::new(r"(?is)<title[^>]*>(.*?)</title>").ok()?;
    let cap = re.captures(html)?;
    let raw = cap.get(1)?.as_str();
    let cleaned = decode_basic_entities(raw)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned)
    }
}

struct HttpRequest(CapabilityMeta);

#[async_trait]
impl Capability for HttpRequest {
    fn meta(&self) -> &CapabilityMeta {
        &self.0
    }

    async fn call(&self, _ctx: &CallContext, args: Value) -> Result<Value, CapabilityError> {
        let raw_url = args
            .get("url")
            .and_then(Value::as_str)
            .ok_or_else(|| CapabilityError::invalid_args("http.request", "Missing 'url'."))?;
        let mut url = Url::parse(raw_url).map_err(|e| {
            CapabilityError::invalid_args("http.request", format!("Invalid URL: {e}"))
        })?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(CapabilityError::invalid_args(
                "http.request",
                "HTTP requests support only http and https URLs.",
            ));
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(CapabilityError::invalid_args(
                "http.request",
                "URL userinfo is not allowed; use an explicit Authorization header.",
            ));
        }

        if let Some(params) = args.get("searchParams").and_then(Value::as_object) {
            let mut pairs = url.query_pairs_mut();
            for (k, v) in params {
                match v {
                    Value::Array(arr) => {
                        for item in arr {
                            let s = match item {
                                Value::String(s) => s.clone(),
                                other => other.to_string(),
                            };
                            pairs.append_pair(k, &s);
                        }
                    }
                    Value::String(s) => {
                        pairs.append_pair(k, s);
                    }
                    other => {
                        pairs.append_pair(k, &other.to_string());
                    }
                }
            }
        }

        let method_str = args
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("GET")
            .to_ascii_uppercase();
        let method = reqwest::Method::from_bytes(method_str.as_bytes()).map_err(|_| {
            CapabilityError::invalid_args(
                "http.request",
                format!("Unsupported HTTP method: {method_str}"),
            )
        })?;
        let has_json = args.get("json").is_some();
        let has_body = args.get("body").is_some();
        if has_json && has_body {
            return Err(CapabilityError::invalid_args(
                "http.request",
                "'json' and 'body' are mutually exclusive.",
            ));
        }
        if (has_json || has_body) && matches!(method, reqwest::Method::GET | reqwest::Method::HEAD)
        {
            return Err(CapabilityError::invalid_args(
                "http.request",
                "GET and HEAD requests cannot include a request body.",
            ));
        }

        let timeout_ms = args
            .get("timeoutMs")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_HTTP_TIMEOUT_MS)
            .clamp(1, MAX_HTTP_TIMEOUT_MS);
        let max_bytes = args
            .get("maxResponseBytes")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_MAX_RESPONSE_BYTES as u64)
            .clamp(1, MAX_RESPONSE_BYTES as u64) as usize;
        let parse_as = args
            .get("parseAs")
            .and_then(Value::as_str)
            .unwrap_or("auto");
        let throw_http_errors = args
            .get("throwHttpErrors")
            .and_then(Value::as_bool)
            .unwrap_or(true);

        let origin = url.origin();
        let redirect_policy = Policy::custom(move |attempt| {
            if attempt.previous().len() >= 5 {
                return attempt.error("Too many redirects");
            }
            if attempt.url().origin() != origin {
                return attempt.stop();
            }
            attempt.follow()
        });

        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(timeout_ms))
            .redirect(redirect_policy)
            .build()
            .map_err(|e| CapabilityError::exec("HttpError", e.to_string()))?;

        let mut headers = HeaderMap::new();
        if let Some(hdrs) = args.get("headers").and_then(Value::as_object) {
            for (k, v) in hdrs {
                let s = v.as_str().ok_or_else(|| {
                    CapabilityError::invalid_args(
                        "http.request",
                        format!("Header '{k}' must be a string."),
                    )
                })?;
                let name = HeaderName::from_bytes(k.as_bytes()).map_err(|e| {
                    CapabilityError::invalid_args(
                        "http.request",
                        format!("Invalid header name '{k}': {e}"),
                    )
                })?;
                let val = HeaderValue::from_str(s).map_err(|e| {
                    CapabilityError::invalid_args(
                        "http.request",
                        format!("Invalid header value for '{k}': {e}"),
                    )
                })?;
                headers.insert(name, val);
            }
        }

        let mut req = client.request(method, url).headers(headers);
        if let Some(j) = args.get("json") {
            req = req.json(j);
        } else if let Some(b) = args.get("body").and_then(Value::as_str) {
            req = req.body(b.to_string());
        }

        let resp = req
            .send()
            .await
            .map_err(|e| CapabilityError::exec("HttpError", e.to_string()))?;

        let status = resp.status();
        let final_url = resp.url().to_string();
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        let mut visible_headers = Map::new();
        let mut redacted = Vec::new();
        for (k, v) in resp.headers() {
            let key = k.as_str().to_string();
            if is_sensitive_header(&key) {
                redacted.push(Value::String(key));
            } else if let Ok(val) = v.to_str() {
                visible_headers.insert(key, Value::String(val.to_string()));
            }
        }

        let bytes = read_bounded_bytes(resp, max_bytes).await?;
        let text = String::from_utf8_lossy(&bytes).into_owned();

        if throw_http_errors && !status.is_success() {
            return Err(CapabilityError::exec(
                "HttpStatusError",
                format!(
                    "HTTP {} {}: {}",
                    status.as_u16(),
                    status.canonical_reason().unwrap_or(""),
                    &text[..text.len().min(500)]
                ),
            ));
        }

        let body = match parse_as {
            "json" => serde_json::from_str::<Value>(&text)
                .map_err(|e| CapabilityError::exec("JsonParseError", e.to_string()))?,
            "text" => Value::String(text),
            _ => {
                if content_type.contains("json") {
                    serde_json::from_str::<Value>(&text).unwrap_or(Value::String(text))
                } else {
                    Value::String(text)
                }
            }
        };

        Ok(json!({
            "status": status.as_u16(),
            "statusText": status.canonical_reason().unwrap_or(""),
            "ok": status.is_success(),
            "url": final_url,
            "headers": visible_headers,
            "redactedHeaders": redacted,
            "body": body,
        }))
    }
}

struct WebSearch(CapabilityMeta);

#[async_trait]
impl Capability for WebSearch {
    fn meta(&self) -> &CapabilityMeta {
        &self.0
    }

    async fn call(&self, _ctx: &CallContext, args: Value) -> Result<Value, CapabilityError> {
        let query = args
            .get("query")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                CapabilityError::invalid_args("web.search", "Missing non-empty 'query'.")
            })?;
        let max_results = args
            .get("maxResults")
            .or_else(|| args.get("max_results"))
            .and_then(Value::as_u64)
            .unwrap_or(5)
            .clamp(1, 20) as usize;

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .user_agent("Mozilla/5.0 (compatible; CantBot/0.1)")
            .build()
            .map_err(|e| CapabilityError::exec("WebSearchError", e.to_string()))?;

        let resp = client
            .get("https://html.duckduckgo.com/html/")
            .query(&[("q", query)])
            .send()
            .await
            .map_err(|e| CapabilityError::exec("WebSearchError", e.to_string()))?;

        let bytes = read_bounded_bytes(resp, MAX_RESPONSE_BYTES).await?;
        let html = String::from_utf8_lossy(&bytes);

        let re_link =
            Regex::new(r#"(?is)<a[^>]*class="result__a"[^>]*href="([^"]+)"[^>]*>(.*?)</a>"#)
                .unwrap();
        let re_snip =
            Regex::new(r#"(?is)<a[^>]*class="result__snippet"[^>]*>(.*?)</a>|<div[^>]*class="result__snippet"[^>]*>(.*?)</div>"#)
                .unwrap();
        let re_tags = Regex::new(r"<[^>]+>").unwrap();

        let snippets: Vec<String> = re_snip
            .captures_iter(&html)
            .map(|c| {
                let raw = c
                    .get(1)
                    .or_else(|| c.get(2))
                    .map(|m| m.as_str())
                    .unwrap_or("");
                decode_basic_entities(&re_tags.replace_all(raw, ""))
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect();

        let mut results = Vec::new();
        for (idx, cap) in re_link.captures_iter(&html).enumerate() {
            if results.len() >= max_results {
                break;
            }
            let raw_href = decode_basic_entities(cap.get(1).map(|m| m.as_str()).unwrap_or(""));
            let url = unwrap_ddg_redirect(&raw_href);
            let title = decode_basic_entities(
                &re_tags.replace_all(cap.get(2).map(|m| m.as_str()).unwrap_or(""), ""),
            )
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
            let snippet = snippets.get(idx).cloned().unwrap_or_default();
            if !url.is_empty() && !title.is_empty() {
                results.push(json!({
                    "title": title,
                    "url": url,
                    "snippet": snippet,
                }));
            }
        }
        Ok(Value::Array(results))
    }
}

fn unwrap_ddg_redirect(href: &str) -> String {
    let full = if href.starts_with("//") {
        format!("https:{href}")
    } else {
        href.to_string()
    };
    if let Ok(parsed) = Url::parse(&full) {
        if parsed
            .host_str()
            .is_some_and(|h| h.contains("duckduckgo.com"))
        {
            for (k, v) in parsed.query_pairs() {
                if k == "uddg" {
                    return v.into_owned();
                }
            }
        }
    }
    full
}

struct WebFetch(CapabilityMeta);

#[async_trait]
impl Capability for WebFetch {
    fn meta(&self) -> &CapabilityMeta {
        &self.0
    }

    async fn call(&self, _ctx: &CallContext, args: Value) -> Result<Value, CapabilityError> {
        let raw_url = args
            .get("url")
            .and_then(Value::as_str)
            .ok_or_else(|| CapabilityError::invalid_args("web.fetch", "Missing 'url'."))?;
        let url = Url::parse(raw_url)
            .map_err(|e| CapabilityError::invalid_args("web.fetch", format!("Invalid URL: {e}")))?;
        assert_public_url(&url)?;

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent("Mozilla/5.0 (compatible; CantBot/0.1)")
            .redirect(Policy::limited(5))
            .build()
            .map_err(|e| CapabilityError::exec("WebFetchError", e.to_string()))?;

        let mut req = client.get(url.clone());
        if let Some(cookie) = args.get("cookie").and_then(Value::as_str) {
            req = req.header(reqwest::header::COOKIE, cookie);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| CapabilityError::exec("WebFetchError", e.to_string()))?;

        let final_url = resp.url().clone();
        assert_public_url(&final_url)?;

        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("text/plain")
            .to_string();
        let bytes = read_bounded_bytes(resp, MAX_RESPONSE_BYTES).await?;
        let raw_text = String::from_utf8_lossy(&bytes).into_owned();

        let (title, extracted, extractor) = if content_type.contains("html") {
            (
                extract_title(&raw_text),
                html_to_text(&raw_text),
                "basic-html",
            )
        } else {
            (None, raw_text, "plain")
        };

        let mut out_lines = Vec::new();
        let mut out_bytes = 0usize;
        let mut truncated = false;
        for (idx, line) in extracted.lines().enumerate() {
            if idx >= MAX_WEB_FETCH_LINES {
                truncated = true;
                break;
            }
            let extra = line.len() + usize::from(!out_lines.is_empty());
            if out_bytes + extra > MAX_WEB_FETCH_BYTES {
                truncated = true;
                break;
            }
            out_bytes += extra;
            out_lines.push(line);
        }

        Ok(json!({
            "url": raw_url,
            "finalUrl": final_url.to_string(),
            "title": title,
            "contentType": content_type,
            "contentLength": bytes.len(),
            "extractor": extractor,
            "text": out_lines.join("\n"),
            "truncation": {
                "truncated": truncated,
                "returnedBytes": out_bytes,
                "returnedLines": out_lines.len(),
            }
        }))
    }
}

fn steel_pos(fields: &[&str]) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("steel".into(), json!({ "positional": fields }));
    m
}

pub fn register(catalog: &CapabilityCatalog) {
    catalog.register(Arc::new(HttpRequest(CapabilityMeta {
        tool_name: "__cant_http_request".into(),
        api_path: vec!["http".into(), "request".into()],
        description: "Perform a bounded HTTP(S) request with optional searchParams, headers, json/body, parseAs (auto|json|text), and throwHttpErrors.".into(),
        params_schema: json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["url"],
            "properties": {
                "url": { "type": "string" },
                "method": { "type": "string", "enum": ["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE"] },
                "searchParams": { "type": "object" },
                "headers": { "type": "object" },
                "json": {},
                "body": { "type": "string" },
                "parseAs": { "type": "string", "enum": ["auto", "json", "text"] },
                "timeoutMs": { "type": "integer", "minimum": 1, "maximum": 120000 },
                "maxResponseBytes": { "type": "integer", "minimum": 1, "maximum": 786432 },
                "throwHttpErrors": { "type": "boolean" }
            }
        }),
        result_schema: json!({ "type": "object" }),
        argument_mode: ArgumentMode::Single,
        language_hints: steel_pos(&["url"]),
    })));

    catalog.register(Arc::new(WebSearch(CapabilityMeta {
        tool_name: "web_search".into(),
        api_path: vec!["web".into(), "search".into()],
        description: "Search DuckDuckGo public HTML results. Returns [{title, url, snippet}]. Configurable/disableable per harness.".into(),
        params_schema: json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["query"],
            "properties": {
                "query": { "type": "string" },
                "maxResults": { "type": "integer", "minimum": 1, "maximum": 20 }
            }
        }),
        result_schema: json!({ "type": "array" }),
        argument_mode: ArgumentMode::Single,
        language_hints: steel_pos(&["query"]),
    })));

    catalog.register(Arc::new(WebFetch(CapabilityMeta {
        tool_name: "web_fetch".into(),
        api_path: vec!["web".into(), "fetch".into()],
        description: "Fetch a public HTTP/HTTPS page as bounded extracted text (50 KiB / 2000 lines) with truncation metadata. Configurable/disableable per harness.".into(),
        params_schema: json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["url"],
            "properties": {
                "url": { "type": "string" },
                "cookie": { "type": "string" }
            }
        }),
        result_schema: json!({ "type": "object" }),
        argument_mode: ArgumentMode::Single,
        language_hints: steel_pos(&["url"]),
    })));
}

#[cfg(test)]
mod tests {
    use super::html_to_text;

    #[test]
    fn html_to_text_skips_hidden_elements() {
        let html = "before<SCRIPT type='module'>\n'</style>'; hidden</SCRIPT>middle<style>x</style><noscript>y</noscript>after";
        assert_eq!(html_to_text(html), "before middle after");
    }
}
