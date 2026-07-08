//! Deterministic web tools (ADR-0017): keyless search + page fetch, so the foil can pull live
//! external facts into an interrogation on EITHER backend.
//!
//! claude-code brings its own `WebSearch`/`WebFetch` tools (the router just allows them,
//! `ai::backend::LlmBackend::claude`); the local Ollama backend has none, so this module supplies
//! the two tool *leaves* — [`web_search`] (DuckDuckGo's no-JS HTML endpoint, no API key) and
//! [`fetch_url`] (GET + tag-strip) — that the router's bounded tool-calling loop
//! (`ai::backend`) executes on the model's behalf.
//!
//! Design rules:
//! - **Tool errors are content, not errors.** [`execute_tool`] never fails the turn — a dead
//!   network or a 404 comes back as a short text the model can read and route around (D20:
//!   degrade, don't die).
//! - **Bounded output.** Search returns at most [`MAX_RESULTS`] hits; a fetched page is
//!   tag-stripped and truncated to [`FETCH_MAX_CHARS`] so one tool round can never blow the
//!   context budget.
//! - **The search endpoint is swappable** via `IDEA_VAULT_SEARCH_URL` (e.g. a self-hosted
//!   SearXNG instance with `/search?format=...`-compatible HTML) — read per call, so tests and
//!   privacy-minded owners can redirect it without a rebuild.

use std::time::Duration;

use serde_json::{json, Value};

/// DuckDuckGo's no-JS HTML frontend — returns server-rendered results, no API key, no JS.
pub const DEFAULT_SEARCH_BASE: &str = "https://html.duckduckgo.com/html/";
/// Per-request wall clock for search and fetch (well under the job's model timeout).
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);
/// Max search hits returned to the model.
pub const MAX_RESULTS: usize = 5;
/// Max characters of tag-stripped page text handed back from one fetch.
pub const FETCH_MAX_CHARS: usize = 12_000;

/// One search result, already unwrapped from DuckDuckGo's redirect link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .user_agent("idea-vault/0.1 (localhost ideation tool)")
        .build()
        .map_err(|e| format!("http client: {e}"))
}

/// Search the web: GET the (env-overridable) HTML search endpoint and parse the result anchors.
pub async fn web_search(query: &str) -> Result<Vec<SearchHit>, String> {
    let base =
        std::env::var("IDEA_VAULT_SEARCH_URL").unwrap_or_else(|_| DEFAULT_SEARCH_BASE.to_string());
    let url = format!("{}?q={}", base.trim_end_matches('?'), percent_encode(query));
    let resp = http_client()?
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("search request failed: {e}"))?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| format!("reading search response: {e}"))?;
    if !status.is_success() {
        return Err(format!("search endpoint returned {status}"));
    }
    Ok(parse_ddg_html(&body, MAX_RESULTS))
}

/// Fetch one page and return its tag-stripped text, truncated to [`FETCH_MAX_CHARS`].
pub async fn fetch_url(url: &str) -> Result<String, String> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(format!("only http(s) URLs can be fetched, got: {url}"));
    }
    let resp = http_client()?
        .get(url)
        .send()
        .await
        .map_err(|e| format!("fetch failed: {e}"))?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| format!("reading page: {e}"))?;
    if !status.is_success() {
        return Err(format!("page returned {status}"));
    }
    let text = html_to_text(&body);
    Ok(truncate_chars(&text, FETCH_MAX_CHARS))
}

/// The Ollama-shape tool definitions for the router's tool loop (`/api/chat` `tools` field).
pub fn tool_definitions() -> Value {
    json!([
        {
            "type": "function",
            "function": {
                "name": "web_search",
                "description": "Search the live web. Returns up to 5 results as title, URL and \
                                snippet. Use it when the discussion would benefit from current \
                                external facts (market numbers, prior art, competitors, news).",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "the search query" }
                    },
                    "required": ["query"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "fetch_url",
                "description": "Fetch one web page and return its readable text (truncated). \
                                Use it to read a promising search result in full.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "url": { "type": "string", "description": "the http(s) URL to fetch" }
                    },
                    "required": ["url"]
                }
            }
        }
    ])
}

/// Execute one model-requested tool call. Infallible by design: every failure mode returns a
/// short explanatory text the model can read (and cite or route around) — a tool hiccup must
/// never turn a whole turn into `mark_failed`.
pub async fn execute_tool(name: &str, args: &Value) -> String {
    // Ollama passes `function.arguments` as a JSON object; some models emit it as a string of
    // JSON instead — accept both.
    let args = match args {
        Value::String(s) => serde_json::from_str::<Value>(s).unwrap_or(Value::Null),
        other => other.clone(),
    };
    match name {
        "web_search" => {
            let query = args
                .get("query")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim();
            if query.is_empty() {
                return "web_search error: missing query".to_string();
            }
            match web_search(query).await {
                Ok(hits) if hits.is_empty() => "no results found".to_string(),
                Ok(hits) => hits
                    .iter()
                    .enumerate()
                    .map(|(i, h)| format!("{}. {}\n   {}\n   {}", i + 1, h.title, h.url, h.snippet))
                    .collect::<Vec<_>>()
                    .join("\n"),
                Err(e) => format!("web_search error: {e}"),
            }
        }
        "fetch_url" => {
            let url = args.get("url").and_then(Value::as_str).unwrap_or("").trim();
            if url.is_empty() {
                return "fetch_url error: missing url".to_string();
            }
            match fetch_url(url).await {
                Ok(text) if text.trim().is_empty() => "the page had no readable text".to_string(),
                Ok(text) => text,
                Err(e) => format!("fetch_url error: {e}"),
            }
        }
        other => format!("unknown tool: {other}"),
    }
}

/// Parse DuckDuckGo's HTML results page: each hit is an `<a class="result__a" href="…">title</a>`
/// followed by a `result__snippet` element. Pure string scanning — resilient to attribute noise,
/// and an unparsable page just yields fewer (or zero) hits, never an error.
pub(crate) fn parse_ddg_html(html: &str, max: usize) -> Vec<SearchHit> {
    let mut hits = Vec::new();
    let mut cursor = 0;
    while hits.len() < max {
        let Some(rel) = html[cursor..].find("result__a") else {
            break;
        };
        let anchor_at = cursor + rel;
        let after_class = &html[anchor_at..];
        // href="…" (DDG puts href after the class attribute).
        let Some(href_start) = after_class.find("href=\"") else {
            cursor = anchor_at + "result__a".len();
            continue;
        };
        let href_rest = &after_class[href_start + 6..];
        let Some(href_end) = href_rest.find('"') else {
            break;
        };
        let href = &href_rest[..href_end];
        // Inner text: from the tag's closing '>' to '</a>'.
        let Some(gt) = after_class.find('>') else {
            break;
        };
        let title_rest = &after_class[gt + 1..];
        let Some(a_end) = title_rest.find("</a>") else {
            break;
        };
        let title = clean_inline(&title_rest[..a_end]);
        // The snippet element between this hit and the next one (absent on some layouts).
        let tail = &after_class[gt + 1 + a_end..];
        let next_hit = tail.find("result__a").unwrap_or(tail.len());
        let snippet = tail[..next_hit]
            .find("result__snippet")
            .and_then(|s| {
                let rest = &tail[s..next_hit];
                let open = rest.find('>')?;
                let close = rest.find("</a>").or_else(|| rest.find("</div>"))?;
                (open < close).then(|| clean_inline(&rest[open + 1..close]))
            })
            .unwrap_or_default();

        let url = unwrap_ddg_href(href);
        if !url.is_empty() && !title.is_empty() {
            hits.push(SearchHit {
                title,
                url,
                snippet,
            });
        }
        cursor = anchor_at + "result__a".len() + gt + a_end;
    }
    hits
}

/// DDG result links are redirect-wrapped: `//duckduckgo.com/l/?uddg=<encoded-target>&rut=…`.
/// Unwrap to the real target; pass plain links through (with a scheme added to `//…`).
fn unwrap_ddg_href(href: &str) -> String {
    if let Some(pos) = href.find("uddg=") {
        let rest = &href[pos + 5..];
        let end = rest.find('&').unwrap_or(rest.len());
        return percent_decode(&rest[..end]);
    }
    if let Some(rest) = href.strip_prefix("//") {
        return format!("https://{rest}");
    }
    href.to_string()
}

/// Strip tags + decode entities + collapse whitespace for a short inline fragment (title/snippet).
fn clean_inline(fragment: &str) -> String {
    collapse_ws(&decode_entities(&strip_tags(fragment)))
}

/// Reduce an HTML document to readable text: drop `<script>`/`<style>` blocks, turn block-level
/// closers into newlines, strip the remaining tags, decode common entities, collapse whitespace.
pub(crate) fn html_to_text(html: &str) -> String {
    let html = strip_blocks(html, "script");
    let html = strip_blocks(&html, "style");
    // Preserve coarse structure: block-element boundaries become newlines before tag-stripping.
    let mut structured = html;
    for tag in [
        "</p>", "</div>", "</li>", "</tr>", "</h1>", "</h2>", "</h3>", "</h4>", "<br>", "<br/>",
        "<br />",
    ] {
        structured = case_insensitive_replace(&structured, tag, "\n");
    }
    let text = decode_entities(&strip_tags(&structured));
    // Collapse whitespace per line, drop empty runs.
    let mut out = String::new();
    let mut blank_run = 0;
    for line in text.lines() {
        let line = collapse_ws(line);
        if line.is_empty() {
            blank_run += 1;
            if blank_run > 1 {
                continue;
            }
        } else {
            blank_run = 0;
        }
        out.push_str(&line);
        out.push('\n');
    }
    out.trim().to_string()
}

/// Replace every case-insensitive occurrence of `needle` with `with` (block-tag → newline).
fn case_insensitive_replace(haystack: &str, needle: &str, with: &str) -> String {
    let lower_h = haystack.to_lowercase();
    let lower_n = needle.to_lowercase();
    let mut out = String::with_capacity(haystack.len());
    let mut cursor = 0;
    while let Some(rel) = lower_h[cursor..].find(&lower_n) {
        let at = cursor + rel;
        out.push_str(&haystack[cursor..at]);
        out.push_str(with);
        cursor = at + needle.len();
    }
    out.push_str(&haystack[cursor..]);
    out
}

/// Remove `<tag …>…</tag>` blocks (case-insensitive), e.g. script/style, content included.
fn strip_blocks(html: &str, tag: &str) -> String {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let lower = html.to_lowercase();
    let mut out = String::with_capacity(html.len());
    let mut cursor = 0;
    while let Some(rel) = lower[cursor..].find(&open) {
        let start = cursor + rel;
        out.push_str(&html[cursor..start]);
        match lower[start..].find(&close) {
            Some(rel_end) => cursor = start + rel_end + close.len(),
            None => {
                // Unclosed block: drop the rest (safer than leaking raw JS/CSS into "text").
                return out;
            }
        }
    }
    out.push_str(&html[cursor..]);
    out
}

/// Remove every `<…>` tag (state machine; no allocation per tag).
fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => {
                in_tag = false;
                out.push(' ');
            }
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

/// Decode the entities that actually occur in titles/snippets/pages worth reading.
fn decode_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Truncate to at most `max` characters on a char boundary (the fetch budget is characters of
/// readable text, not bytes of HTML).
fn truncate_chars(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((idx, _)) => format!("{}…", &s[..idx]),
        None => s.to_string(),
    }
}

/// Minimal query percent-encoding (space → `+`, unreserved kept, everything else `%XX`).
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Standard percent-decoding (used to unwrap DDG's `uddg=` redirect target).
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = &s[i + 1..i + 3];
                match u8::from_str_radix(hex, 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const DDG_FIXTURE: &str = r#"
    <div class="result results_links results_links_deep web-result ">
      <div class="links_main links_deep result__body">
        <h2 class="result__title">
          <a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fpage&amp;rut=abc">Example <b>Title</b></a>
        </h2>
        <a class="result__snippet" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fpage">A short &amp; useful snippet.</a>
      </div>
    </div>
    <div class="result">
      <a rel="nofollow" class="result__a" href="https://plain.example.org/direct">Plain Link</a>
    </div>
    "#;

    #[test]
    fn parse_ddg_unwraps_redirects_titles_and_snippets() {
        let hits = parse_ddg_html(DDG_FIXTURE, 5);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "Example Title");
        assert_eq!(hits[0].url, "https://example.com/page");
        assert_eq!(hits[0].snippet, "A short & useful snippet.");
        assert_eq!(hits[1].title, "Plain Link");
        assert_eq!(hits[1].url, "https://plain.example.org/direct");
    }

    #[test]
    fn parse_ddg_respects_max_and_survives_garbage() {
        assert_eq!(parse_ddg_html(DDG_FIXTURE, 1).len(), 1);
        assert!(parse_ddg_html("<html>no results markup</html>", 5).is_empty());
        assert!(parse_ddg_html("result__a with no href or anchor", 5).is_empty());
    }

    #[test]
    fn html_to_text_strips_scripts_styles_and_tags() {
        let page = "<html><head><title>T</title><style>.x{}</style>\
                    <script>alert('x')</script></head>\
                    <body><h1>Heading</h1><p>alpha &amp; beta</p><p></p><p></p>\
                    <div>gamma</div></body></html>";
        let text = html_to_text(page);
        assert!(text.contains("Heading"));
        assert!(text.contains("alpha & beta"));
        assert!(text.contains("gamma"));
        assert!(!text.contains("alert"), "script content stripped");
        assert!(!text.contains(".x{}"), "style content stripped");
        assert!(!text.contains('<'), "no tags survive");
        assert!(!text.contains("\n\n\n"), "blank runs collapsed");
    }

    #[test]
    fn truncate_chars_is_char_safe() {
        let s = "héllo wörld".repeat(100);
        let out = truncate_chars(&s, 10);
        assert_eq!(out.chars().count(), 11, "10 chars + ellipsis");
        assert!(s.starts_with(out.trim_end_matches('…')));
        assert_eq!(truncate_chars("short", 10), "short");
    }

    #[test]
    fn percent_roundtrip() {
        assert_eq!(percent_encode("a b&c/d"), "a+b%26c%2Fd");
        assert_eq!(percent_decode("a+b%26c%2Fd"), "a b&c/d");
        assert_eq!(
            percent_decode("https%3A%2F%2Fexample.com%2Fpage"),
            "https://example.com/page"
        );
        assert_eq!(percent_decode("bad%zz"), "bad%zz");
    }

    #[test]
    fn unwrap_href_variants() {
        assert_eq!(
            unwrap_ddg_href("//duckduckgo.com/l/?uddg=https%3A%2F%2Fx.org%2F&rut=1"),
            "https://x.org/"
        );
        assert_eq!(
            unwrap_ddg_href("//cdn.example.com/x"),
            "https://cdn.example.com/x"
        );
        assert_eq!(
            unwrap_ddg_href("https://direct.example"),
            "https://direct.example"
        );
    }

    #[tokio::test]
    async fn execute_tool_is_infallible_content() {
        // Unknown tool and malformed args come back as readable text, never Err.
        assert_eq!(execute_tool("nope", &json!({})).await, "unknown tool: nope");
        assert_eq!(
            execute_tool("web_search", &json!({})).await,
            "web_search error: missing query"
        );
        assert_eq!(
            execute_tool("fetch_url", &json!({})).await,
            "fetch_url error: missing url"
        );
        // Non-http scheme is refused without any network I/O.
        let out = execute_tool("fetch_url", &json!({"url": "file:///etc/passwd"})).await;
        assert!(out.contains("only http(s)"));
        // String-encoded arguments (some models) are accepted.
        let out = execute_tool("fetch_url", &json!("{\"url\": \"ftp://x\"}")).await;
        assert!(out.contains("only http(s)"));
    }
}
