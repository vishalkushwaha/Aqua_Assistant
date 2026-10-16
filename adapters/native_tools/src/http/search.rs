//! Web search results: what each search service sends back, in one shape, and how they are shown to the model.
//!
//! No search service is free of conditions, so `web.search` asks them one after the other (the person's own choice first: SearXNG, Brave or Tavily
//! with a key kept in the credential store) and ends with Wikipedia, which needs no key. A result is a [`Hit`]: address, title, a snippet and, when the
//! service says, a date. The model reads a short numbered list, `[n] domain, date: snippet`, so that it can cite `[n]` in its answer.
//!
//! Results are cached for a short time (a repeated question does not ask the service again). What may be kept is the service's rule, not ours:
//! Brave's terms allow only transient use, so its results are held for a minute and never written to disk; nothing here is written to disk at all.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// One search result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub title: String,
    pub url: String,
    pub snippet: String,
    /// When the page says it was made or changed, as the service wrote it (`2025-03-14`, `3 days ago`).
    pub date: Option<String>,
    /// Which service found it (`brave`, `tavily`, `searxng`, `duckduckgo`, `wikipedia`, `parallel`, `ollama`).
    pub provider: &'static str,
}

/// The longest a snippet is shown (about 60 tokens).
pub const SNIPPET_CHARS: usize = 240;

fn plain(html: &str) -> String {
    super::html::to_text(html).text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn cut(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>().trim_end())
    }
}

fn text(v: &Value, key: &str) -> String {
    v.get(key).and_then(|x| x.as_str()).unwrap_or("").to_string()
}

fn hit(provider: &'static str, title: &str, url: &str, snippet: &str, date: Option<String>) -> Option<Hit> {
    let url = url.trim();
    // Only web addresses: a result is something the model may be told to read.
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return None;
    }
    Some(Hit { title: cut(&plain(title), 160), url: url.to_string(), snippet: cut(&plain(snippet), SNIPPET_CHARS), date: date.filter(|d| !d.trim().is_empty()).map(|d| cut(d.trim(), 40)), provider })
}

pub fn parse_brave(v: &Value) -> Vec<Hit> {
    v["web"]["results"].as_array().into_iter().flatten().filter_map(|r| hit("brave", &text(r, "title"), &text(r, "url"), &text(r, "description"), r.get("page_age").and_then(|x| x.as_str()).or_else(|| r.get("age").and_then(|x| x.as_str())).map(str::to_string))).collect()
}

pub fn parse_searxng(v: &Value) -> Vec<Hit> {
    v["results"].as_array().into_iter().flatten().filter_map(|r| hit("searxng", &text(r, "title"), &text(r, "url"), &text(r, "content"), r.get("publishedDate").and_then(|x| x.as_str()).map(str::to_string))).collect()
}

pub fn parse_tavily(v: &Value) -> Vec<Hit> {
    v["results"].as_array().into_iter().flatten().filter_map(|r| hit("tavily", &text(r, "title"), &text(r, "url"), &text(r, "content"), r.get("published_date").and_then(|x| x.as_str()).map(str::to_string))).collect()
}

/// Parallel's search through its MCP server. The answer is a JSON-RPC message, plain or as one server-sent event; its result holds text that is itself JSON
/// (`{"results": [{"url", "title", "publish_date", "excerpts": [...]}]}`).
pub fn parse_parallel_body(body: &[u8]) -> Vec<Hit> {
    let text = String::from_utf8_lossy(body);
    let trimmed = text.trim_start();
    let message: Option<Value> = if trimmed.starts_with('{') {
        serde_json::from_str(trimmed).ok()
    } else {
        trimmed.lines().filter_map(|l| l.strip_prefix("data:")).find_map(|d| serde_json::from_str::<Value>(d.trim()).ok().filter(|v| v.get("result").is_some() || v.get("error").is_some()))
    };
    message.map(|v| parse_parallel(&v)).unwrap_or_default()
}

pub fn parse_parallel(v: &Value) -> Vec<Hit> {
    if v.pointer("/result/isError").and_then(Value::as_bool) == Some(true) {
        return Vec::new();
    }
    v.pointer("/result/content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|c| c.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|c| serde_json::from_str::<Value>(c.get("text")?.as_str()?).ok())
        .flat_map(|inner| inner["results"].as_array().cloned().unwrap_or_default())
        .filter_map(|r| {
            let excerpt = r["excerpts"].as_array().and_then(|e| e.iter().filter_map(Value::as_str).find(|t| !t.trim().is_empty())).unwrap_or("");
            hit("parallel", &text(&r, "title"), &text(&r, "url"), excerpt, r.get("publish_date").and_then(Value::as_str).map(str::to_string))
        })
        .collect()
}

/// Ollama's web search: `{"results": [{"title", "url", "content"}]}`.
pub fn parse_ollama(v: &Value) -> Vec<Hit> {
    v["results"].as_array().into_iter().flatten().filter_map(|r| hit("ollama", &text(r, "title"), &text(r, "url"), &text(r, "content"), None)).collect()
}

/// Wikipedia's search: the page's address is built from its title.
/// DuckDuckGo's results page (`html.duckduckgo.com/html/`): the links of the results, their titles and snippets. The page is parsed, not a service's answer, so a
/// change of its markup shows as no results (and the next service is asked). Advertisements are left out, and a link that goes through DuckDuckGo's redirect
/// (`/l/?uddg=...`) is given as the address it points to.
pub fn parse_duckduckgo(page: &str) -> Vec<Hit> {
    use regex::Regex;
    use std::sync::OnceLock;
    static TITLE: OnceLock<Regex> = OnceLock::new();
    static SNIPPET: OnceLock<Regex> = OnceLock::new();
    static TAGS: OnceLock<Regex> = OnceLock::new();
    let title_re = TITLE.get_or_init(|| Regex::new(r#"(?s)<a[^>]*class="result__a"[^>]*href="([^"]+)"[^>]*>(.*?)</a>"#).unwrap());
    let snippet_re = SNIPPET.get_or_init(|| Regex::new(r#"(?s)class="result__snippet"[^>]*>(.*?)</a>"#).unwrap());
    let tags = TAGS.get_or_init(|| Regex::new(r"(?s)<[^>]*>").unwrap());
    let plain = |fragment: &str| -> String { super::html::decode_entities(&tags.replace_all(fragment, "")).split_whitespace().collect::<Vec<_>>().join(" ") };
    let found: Vec<regex::Captures> = title_re.captures_iter(page).collect();
    let mut hits = Vec::new();
    for (i, c) in found.iter().enumerate() {
        let whole = c.get(0).unwrap();
        let next = found.get(i + 1).map_or(page.len(), |n| n.get(0).unwrap().start());
        let Some(url) = duckduckgo_target(&c[1]) else { continue };
        let snippet = snippet_re.captures(&page[whole.end()..next]).map(|m| plain(&m[1])).unwrap_or_default();
        hits.push(Hit { title: plain(&c[2]), url, snippet, date: None, provider: "duckduckgo" });
    }
    hits
}

/// The address a DuckDuckGo result points to: a redirect link carries it in `uddg`; an advertisement (`/y.js`) and anything that is not http or https is none.
fn duckduckgo_target(href: &str) -> Option<String> {
    let href = super::html::decode_entities(href.trim());
    let full = match href.strip_prefix("//") {
        Some(rest) => format!("https://{rest}"),
        None => href,
    };
    let u = url::Url::parse(&full).ok()?;
    if u.host_str().is_some_and(|h| h == "duckduckgo.com" || h.ends_with(".duckduckgo.com")) {
        if u.path() == "/y.js" {
            return None;
        }
        let target = u.query_pairs().find(|(k, _)| k == "uddg").map(|(_, v)| v.into_owned())?;
        return matches!(url::Url::parse(&target).ok()?.scheme(), "http" | "https").then_some(target);
    }
    matches!(u.scheme(), "http" | "https").then(|| u.to_string())
}

pub fn parse_wikipedia(v: &Value) -> Vec<Hit> {
    v["query"]["search"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| {
            let title = text(r, "title");
            if title.is_empty() {
                return None;
            }
            let path: String = url::form_urlencoded::byte_serialize(title.replace(' ', "_").as_bytes()).collect::<String>().replace("%2F", "/").replace("%3A", ":");
            hit("wikipedia", &title, &format!("https://en.wikipedia.org/wiki/{path}"), &text(r, "snippet"), r.get("timestamp").and_then(|x| x.as_str()).map(|t| t.chars().take(10).collect()))
        })
        .collect()
}

/// The host of an address, without `www.`.
pub fn domain(url: &str) -> String {
    url::Url::parse(url).ok().and_then(|u| u.host_str().map(|h| h.trim_start_matches("www.").to_string())).unwrap_or_default()
}

/// Drops results that point at the same page (the same address without its fragment or a trailing slash) and keeps the first.
pub fn dedupe(hits: Vec<Hit>) -> Vec<Hit> {
    let mut seen: Vec<String> = Vec::new();
    hits.into_iter()
        .filter(|h| {
            let key = h.url.split('#').next().unwrap_or("").trim_end_matches('/').to_lowercase();
            if seen.contains(&key) {
                false
            } else {
                seen.push(key);
                true
            }
        })
        .collect()
}

/// The list the model reads, and the data a program reads. `[n] domain, date: snippet` and the address on the next line.
pub fn render(query: &str, hits: &[Hit]) -> (String, Value) {
    render_from(query, hits, 1)
}

/// Like [`render`], numbering from `first` (a run that searches twice keeps counting, so a number names one page).
pub fn render_from(query: &str, hits: &[Hit], first: usize) -> (String, Value) {
    let lines: Vec<String> = hits
        .iter()
        .enumerate()
        .map(|(i, h)| {
            let d = domain(&h.url);
            let head = match &h.date {
                Some(date) => format!("[{}] {d}, {date}", first + i),
                None => format!("[{}] {d}", first + i),
            };
            let title = if h.title.is_empty() { String::new() } else { format!(" {}.", h.title.trim_end_matches('.')) };
            format!("{head}:{title} {}\n    {}", h.snippet, h.url)
        })
        .collect();
    let data = json!({
        "query": query,
        "results": hits.iter().map(|h| json!({"title": h.title, "url": h.url, "snippet": h.snippet, "date": h.date, "provider": h.provider})).collect::<Vec<_>>(),
    });
    (if lines.is_empty() { "No results.".into() } else { lines.join("\n") }, data)
}

// ---- the short-lived cache ---------------------------------------------------------------------------------------------------------

/// Words that say the answer changes by the hour: such a search is cached for minutes, not hours.
const FRESH_WORDS: &[&str] = &["news", "latest", "today", "tonight", "now", "current", "currently", "price", "prices", "score", "scores", "live", "breaking", "weather", "stock", "stocks", "rate", "rates", "update", "updates", "this week", "yesterday"];

/// How long a search for `query` may be reused. `transient_only` is for a service whose terms allow no more than that (Brave): a minute.
pub fn ttl_for(query: &str, transient_only: bool) -> Duration {
    if transient_only {
        return Duration::from_secs(60);
    }
    let q = query.to_lowercase();
    if FRESH_WORDS.iter().any(|w| if w.contains(' ') { q.contains(w) } else { q.split(|c: char| !c.is_alphanumeric()).any(|t| t == *w) }) {
        Duration::from_secs(10 * 60)
    } else {
        Duration::from_secs(6 * 3600)
    }
}

#[derive(Default)]
pub struct Cache {
    entries: Mutex<HashMap<String, (Instant, Vec<Hit>)>>,
}

impl Cache {
    fn key(provider: &str, query: &str, limit: usize) -> String {
        format!("{provider}\u{1f}{}\u{1f}{limit}", query.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase())
    }

    pub fn get(&self, provider: &str, query: &str, limit: usize) -> Option<Vec<Hit>> {
        let mut e = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        let key = Self::key(provider, query, limit);
        match e.get(&key) {
            Some((until, hits)) if Instant::now() < *until => Some(hits.clone()),
            Some(_) => {
                e.remove(&key);
                None
            }
            None => None,
        }
    }

    pub fn put(&self, provider: &str, query: &str, limit: usize, hits: &[Hit], ttl: Duration) {
        if hits.is_empty() {
            return;
        }
        let mut e = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        if e.len() > 200 {
            let now = Instant::now();
            e.retain(|_, (until, _)| now < *until);
        }
        e.insert(Self::key(provider, query, limit), (Instant::now() + ttl, hits.to_vec()));
    }
}

#[cfg(test)]
mod tests {

    const DUCKDUCKGO_PAGE: &str = r#"<html><body><div id="links" class="results">
<div class="result results_links results_links_deep web-result "><div class="links_main links_deep result__body"><h2 class="result__title"><a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fwww.quicklrc.com%2Flrc%2Dgenerator&amp;rut=abc">LRC Generator - AI Auto-Sync Lyrics to Audio | QuickLRC</a></h2><div class="result__extras"><div class="result__extras__url"><a class="result__url" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fwww.quicklrc.com%2Flrc%2Dgenerator&amp;rut=abc">www.quicklrc.com/lrc-generator</a></div></div><a class="result__snippet" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fwww.quicklrc.com%2Flrc%2Dgenerator&amp;rut=abc">Upload your song and lyrics &amp; <b>QuickLRC</b> syncs every line in under 30 seconds.</a></div></div>
<div class="result results_links results_links_deep result--ad "><h2 class="result__title"><a rel="nofollow" class="result__a" href="https://duckduckgo.com/y.js?ad_domain=ads.example&amp;ad_provider=bingv7aa">An advertisement</a></h2><a class="result__snippet" href="https://duckduckgo.com/y.js?ad_domain=ads.example">Buy now</a></div>
<div class="result results_links web-result "><h2 class="result__title"><a rel="nofollow" class="result__a" href="https://example.org/direct?x=1&amp;y=2">A direct link &amp; more</a></h2><a class="result__snippet" href="https://example.org/direct">The second
   snippet.</a></div>
<div class="result results_links web-result "><h2 class="result__title"><a rel="nofollow" class="result__a" href="javascript:alert(1)">Not a web address</a></h2></div>
</div></body></html>"#;

    #[test]
    fn a_duckduckgo_page_gives_its_results_without_the_advertisement_and_with_the_real_addresses() {
        let hits = parse_duckduckgo(DUCKDUCKGO_PAGE);
        assert_eq!(hits.len(), 2, "{hits:?}");
        assert_eq!(hits[0].url, "https://www.quicklrc.com/lrc-generator", "the redirect is undone");
        assert_eq!(hits[0].title, "LRC Generator - AI Auto-Sync Lyrics to Audio | QuickLRC");
        assert_eq!(hits[0].snippet, "Upload your song and lyrics & QuickLRC syncs every line in under 30 seconds.", "tags are dropped, entities decoded");
        assert_eq!((hits[1].url.as_str(), hits[1].title.as_str(), hits[1].snippet.as_str()), ("https://example.org/direct?x=1&y=2", "A direct link & more", "The second snippet."));
        assert!(hits.iter().all(|h| h.provider == "duckduckgo"));
        // A page that is not a results page (a check, a change of markup) is no results, not a failure of the parser.
        assert!(parse_duckduckgo("<html><body>Please complete the check</body></html>").is_empty());
        assert!(parse_duckduckgo("").is_empty());
    }
    use super::*;

    #[test]
    fn each_service_s_answer_becomes_the_same_shape_with_its_date() {
        let brave = json!({"web": {"results": [
            {"title": "Rust 1.90 released", "url": "https://blog.rust-lang.org/2025/09/14/Rust-1.90.0.html", "description": "The Rust team is <strong>happy</strong> to announce 1.90.", "page_age": "2025-09-14T00:00:00"},
            {"title": "No scheme", "url": "javascript:alert(1)", "description": "x"},
        ]}});
        let b = parse_brave(&brave);
        assert_eq!(b.len(), 1, "an address that is not http(s) is not a result");
        assert_eq!((b[0].provider, b[0].snippet.as_str(), b[0].date.as_deref()), ("brave", "The Rust team is happy to announce 1.90.", Some("2025-09-14T00:00:00")));
        let searx = parse_searxng(&json!({"results": [{"title": "A", "url": "https://a.example/x", "content": "about a", "publishedDate": "2025-03-01"}]}));
        assert_eq!((searx[0].provider, searx[0].date.as_deref()), ("searxng", Some("2025-03-01")));
        let tav = parse_tavily(&json!({"results": [{"title": "T", "url": "https://t.example/y", "content": "tavily text", "published_date": "Mon, 01 Sep 2025"}]}));
        assert_eq!((tav[0].provider, tav[0].snippet.as_str()), ("tavily", "tavily text"));
        let wiki = parse_wikipedia(&json!({"query": {"search": [{"title": "Pune", "snippet": "<span class=\"searchmatch\">Pune</span> is a city in Maharashtra", "timestamp": "2025-08-30T10:00:00Z"}, {"title": "A/B testing (software)", "snippet": "x", "timestamp": "2025-01-01T00:00:00Z"}]}}));
        assert_eq!(wiki[0].url, "https://en.wikipedia.org/wiki/Pune");
        assert_eq!((wiki[0].snippet.as_str(), wiki[0].date.as_deref()), ("Pune is a city in Maharashtra", Some("2025-08-30")));
        assert!(wiki[1].url.starts_with("https://en.wikipedia.org/wiki/A/B_testing_%28software%29") || wiki[1].url.contains("A/B_testing"), "{}", wiki[1].url);
        for none in [json!({}), json!({"web": {}}), json!({"results": "no"}), json!(null)] {
            assert!(parse_brave(&none).is_empty() && parse_searxng(&none).is_empty() && parse_tavily(&none).is_empty() && parse_wikipedia(&none).is_empty());
        }
    }

    #[test]
    fn a_long_snippet_is_cut_and_the_markup_in_a_title_is_gone() {
        let long = "word ".repeat(200);
        let h = hit("brave", "<b>Bold</b> &amp; title", "https://example.com/a", &long, None).unwrap();
        assert_eq!(h.title, "Bold & title");
        assert!(h.snippet.chars().count() <= SNIPPET_CHARS + 1 && h.snippet.ends_with('…'), "{}", h.snippet);
    }

    #[test]
    fn the_list_is_numbered_with_the_domain_and_date_and_the_address_for_a_citation() {
        let hits = dedupe(vec![
            Hit { title: "Rust 1.90".into(), url: "https://www.blog.rust-lang.org/a".into(), snippet: "Released.".into(), date: Some("2025-09-14".into()), provider: "brave" },
            Hit { title: "Same page".into(), url: "https://www.blog.rust-lang.org/a/#top".into(), snippet: "dup".into(), date: None, provider: "brave" },
            Hit { title: "Other".into(), url: "https://docs.rs/x".into(), snippet: "Docs.".into(), date: None, provider: "brave" },
        ]);
        assert_eq!(hits.len(), 2, "the same page twice is one result");
        let (text, data) = render("rust release", &hits);
        assert_eq!(text, "[1] blog.rust-lang.org, 2025-09-14: Rust 1.90. Released.\n    https://www.blog.rust-lang.org/a\n[2] docs.rs: Other. Docs.\n    https://docs.rs/x");
        assert_eq!(data["results"][0]["provider"], "brave");
        assert_eq!(render("q", &[]).0, "No results.");
    }

    #[test]
    fn a_search_about_something_that_changes_by_the_hour_is_kept_minutes_and_a_services_transient_rule_is_a_minute() {
        assert_eq!(ttl_for("latest rust release", false), Duration::from_secs(600));
        assert_eq!(ttl_for("bitcoin price today", false), Duration::from_secs(600));
        assert_eq!(ttl_for("history of the roman empire", false), Duration::from_secs(6 * 3600));
        assert_eq!(ttl_for("history of the roman empire", true), Duration::from_secs(60), "Brave's terms allow only transient use");
        assert_eq!(ttl_for("rates of exchange", false), Duration::from_secs(600), "a word, not a piece of one: 'rates' is fresh, 'pirate' is not");
        assert_eq!(ttl_for("pirate history", false), Duration::from_secs(6 * 3600));
    }

    #[test]
    fn the_cache_returns_what_is_fresh_ignores_spacing_and_case_and_keeps_nothing_empty() {
        let c = Cache::default();
        let h = vec![Hit { title: "t".into(), url: "https://a.example/".into(), snippet: "s".into(), date: None, provider: "wikipedia" }];
        assert!(c.get("wikipedia", "Pune weather", 5).is_none());
        c.put("wikipedia", "Pune   weather", 5, &h, Duration::from_secs(60));
        assert_eq!(c.get("wikipedia", "pune weather", 5), Some(h.clone()));
        assert!(c.get("brave", "pune weather", 5).is_none(), "per provider");
        assert!(c.get("wikipedia", "pune weather", 3).is_none(), "per size");
        c.put("wikipedia", "gone", 5, &h, Duration::from_millis(0));
        assert!(c.get("wikipedia", "gone", 5).is_none(), "expired");
        c.put("wikipedia", "nothing", 5, &[], Duration::from_secs(60));
        assert!(c.get("wikipedia", "nothing", 5).is_none(), "an empty answer is not worth keeping");
    }

    #[test]
    fn parallels_answer_is_read_plain_or_as_an_event_and_its_text_is_json_inside_json() {
        let inner = json!({"search_id": "s1", "results": [
            {"url": "https://canberra.com.au/other/where-is-canberra", "title": "Where is Canberra | Canberra", "publish_date": "2024-05-01", "excerpts": ["", "Canberra is Australia\u{2019}s capital city. It is in the ACT."]},
            {"url": "ftp://not-a-web-page.example/x", "title": "skipped", "excerpts": ["x"]},
            {"url": "https://www.aph.gov.au/a", "title": "Parliament of Australia", "publish_date": null, "excerpts": ["Canberra is the National Capital of Australia."]}
        ]});
        let message = json!({"jsonrpc": "2.0", "id": 2, "result": {"_meta": {"parallel/usage": [{"name": "sku_search", "count": 1}]}, "content": [{"type": "text", "text": inner.to_string()}]}});
        let plain = parse_parallel_body(message.to_string().as_bytes());
        assert_eq!(plain.len(), 2, "{plain:?}");
        assert_eq!((plain[0].provider, plain[0].title.as_str(), plain[0].date.as_deref()), ("parallel", "Where is Canberra | Canberra", Some("2024-05-01")));
        assert!(plain[0].snippet.starts_with("Canberra is Australia\u{2019}s capital city"), "the first excerpt that says something: {}", plain[0].snippet);
        assert_eq!(plain[1].date, None);
        // The same message as one server-sent event, after a comment line.
        let event = format!(": ping\nevent: message\ndata: {}\n\n", message);
        assert_eq!(parse_parallel_body(event.as_bytes()), plain);
        // An error, a message without results, and what is not a message at all.
        assert!(parse_parallel_body(json!({"jsonrpc": "2.0", "id": 2, "result": {"isError": true, "content": [{"type": "text", "text": inner.to_string()}]}}).to_string().as_bytes()).is_empty());
        assert!(parse_parallel_body(json!({"jsonrpc": "2.0", "id": 2, "error": {"code": -32000, "message": "rate limited"}}).to_string().as_bytes()).is_empty());
        assert!(parse_parallel_body(b"<html>blocked</html>").is_empty() && parse_parallel_body(b"").is_empty());
    }

    #[test]
    fn ollamas_answer_is_title_address_and_content() {
        let v = json!({"results": [{"title": "Rust 1.90", "url": "https://blog.rust-lang.org/a", "content": "Released <b>today</b>."}, {"title": "no address", "url": "", "content": "x"}]});
        let hits = parse_ollama(&v);
        assert_eq!(hits.len(), 1);
        assert_eq!((hits[0].provider, hits[0].snippet.as_str(), hits[0].date.clone()), ("ollama", "Released today.", None));
    }
}
