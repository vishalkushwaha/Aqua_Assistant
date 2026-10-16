//! `aqua_browser_extract`: structured data from the current page using fixed, reviewed,
//! read-only scripts. The model picks a `kind`; it never supplies JavaScript, so this works
//! while `browser_evaluate` is disabled by policy.

use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtractKind {
    Headlines,
    Links,
    Tables,
    Article,
    Metadata,
    SearchResults,
}

impl ExtractKind {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "headlines" | "news" => Self::Headlines,
            "links" => Self::Links,
            "tables" | "table" => Self::Tables,
            "article" | "text" => Self::Article,
            "metadata" | "meta" => Self::Metadata,
            "search_results" | "results" => Self::SearchResults,
            _ => return None,
        })
    }
}

// Shared helpers; `el` is the optional element from `target`, otherwise <main> or <body>.
const PRELUDE: &str = r#"
  const root = (el && el.nodeType === 1) ? el : (document.querySelector('main') || document.body);
  const clean = (s) => (s || '').replace(/\s+/g, ' ').trim();
  const visible = (n) => { const r = n.getBoundingClientRect(); const s = getComputedStyle(n);
    return r.width > 0 && r.height > 0 && s.visibility !== 'hidden' && s.display !== 'none' && parseFloat(s.fontSize || '12') >= 6; };
  const inChrome = (n) => !!n.closest('nav, header, footer, aside, [role=navigation], [role=banner], [role=contentinfo]');
  const abs = (a) => { try { const u = new URL(a.getAttribute('href'), location.href); return /^https?:$/.test(u.protocol) ? u.href : null; } catch { return null; } };
"#;

const HEADLINES: &str = r#"
  const out = []; const seen = new Set();
  const push = (node) => {
    if (out.length >= MAX || !visible(node) || inChrome(node)) return;
    const title = clean(node.textContent);
    if (title.length < 12 || title.length > 300 || seen.has(title)) return;
    seen.add(title);
    const a = node.closest('a[href]') || node.querySelector('a[href]');
    const url = a ? abs(a) : null;
    out.push(url ? { title, url } : { title });
  };
  root.querySelectorAll('article h1, article h2, article h3, article h4, [data-testid*=headline i], [class*=headline i]').forEach(push);
  if (out.length < MAX) root.querySelectorAll('h2, h3, h4').forEach(push);
  return { items: out };
"#;

const LINKS: &str = r#"
  const out = []; const seen = new Set();
  for (const a of root.querySelectorAll('a[href]')) {
    if (out.length >= MAX) break;
    const url = abs(a); const text = clean(a.textContent);
    if (!url || text.length < 3 || seen.has(url) || !visible(a)) continue;
    seen.add(url); out.push({ text, url });
  }
  return { items: out };
"#;

const TABLES: &str = r#"
  const tables = [];
  for (const t of [...root.querySelectorAll('table')].slice(0, 5)) {
    const headers = [...t.querySelectorAll('thead th, tr:first-child th')].map(th => clean(th.textContent));
    const rows = [];
    for (const tr of t.querySelectorAll('tbody tr, tr')) {
      if (rows.length >= MAX) break;
      const cells = [...tr.querySelectorAll('td')].map(td => clean(td.textContent));
      if (cells.length) rows.push(cells);
    }
    tables.push({ name: clean(t.getAttribute('aria-label') || (t.caption && t.caption.textContent) || ''), headers, rows });
  }
  return { tables };
"#;

const ARTICLE: &str = r#"
  const node = (el && el.nodeType === 1) ? el : (document.querySelector('article') || root);
  const text = clean(node.innerText).slice(0, 8000);
  return { title: clean(document.title), text, chars: text.length };
"#;

const METADATA: &str = r#"
  const meta = (sel) => { const m = document.querySelector(sel); return m ? clean(m.getAttribute('content') || m.getAttribute('href')) : null; };
  return {
    title: clean(document.title), url: location.href, lang: document.documentElement.lang || null,
    description: meta('meta[name=description]'), canonical: meta('link[rel=canonical]'),
    og_title: meta('meta[property="og:title"]'), og_description: meta('meta[property="og:description"]'),
    published: meta('meta[property="article:published_time"]'),
  };
"#;

const SEARCH_RESULTS: &str = r#"
  const out = []; const seen = new Set();
  for (const a of root.querySelectorAll('a[href]')) {
    if (out.length >= MAX) break;
    const url = abs(a); const title = clean(a.textContent);
    if (!url || title.length < 15 || seen.has(url) || inChrome(a) || !visible(a)) continue;
    if (new URL(url).host === location.host && !/[?&](q|u|url)=/.test(url)) continue;
    seen.add(url);
    const block = a.closest('li, article, div');
    const snippet = block ? clean(block.textContent).replace(title, '').slice(0, 220) : '';
    out.push({ title, url, snippet });
  }
  return { items: out };
"#;

/// Build the `function` argument for `browser_evaluate`.
pub fn script(kind: ExtractKind, max_items: u32) -> String {
    let body = match kind {
        ExtractKind::Headlines => HEADLINES,
        ExtractKind::Links => LINKS,
        ExtractKind::Tables => TABLES,
        ExtractKind::Article => ARTICLE,
        ExtractKind::Metadata => METADATA,
        ExtractKind::SearchResults => SEARCH_RESULTS,
    };
    let max = max_items.clamp(1, 50);
    format!("(el) => {{ const MAX = {max};{PRELUDE}{body}}}")
}

/// Human/model-readable rendering of extractor JSON.
pub fn render(kind: ExtractKind, data: &Value) -> String {
    let items = data.get("items").and_then(|v| v.as_array());
    match kind {
        ExtractKind::Headlines => {
            let Some(items) = items.filter(|i| !i.is_empty()) else {
                return "No matching items found on this page.".into();
            };
            items
                .iter()
                .enumerate()
                .map(|(i, it)| {
                    let title = it
                        .get("title")
                        .or_else(|| it.get("text"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    format!("{}. {title}", i + 1)
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        ExtractKind::Links => {
            let Some(items) = items.filter(|i| !i.is_empty()) else {
                return "No matching items found on this page.".into();
            };
            items
                .iter()
                .enumerate()
                .map(|(i, it)| {
                    let title = it
                        .get("title")
                        .or_else(|| it.get("text"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let url = it.get("url").and_then(|v| v.as_str()).unwrap_or("");
                    if url.is_empty() {
                        format!("{}. {title}", i + 1)
                    } else {
                        format!("{}. {title} — {url}", i + 1)
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        ExtractKind::SearchResults => {
            let Some(items) = items.filter(|i| !i.is_empty()) else {
                return "No matching items found on this page.".into();
            };
            items
                .iter()
                .enumerate()
                .map(|(i, it)| {
                    let title = it.get("title").and_then(|v| v.as_str()).unwrap_or("");
                    let url = it.get("url").and_then(|v| v.as_str()).unwrap_or("");
                    let snippet = it.get("snippet").and_then(|v| v.as_str()).unwrap_or("");
                    let mut line = format!("{}. {title} — {url}", i + 1);
                    if !snippet.is_empty() {
                        line.push('\n');
                        line.push_str(snippet);
                    }
                    line
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        ExtractKind::Tables => {
            let tables = data.get("tables").and_then(|v| v.as_array()).cloned().unwrap_or_default();
            if tables.is_empty() {
                return "No tables found on this page.".into();
            }
            tables
                .iter()
                .map(|t| {
                    let row = |v: &Value| {
                        v.as_array()
                            .map(|c| c.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(" | "))
                            .unwrap_or_default()
                    };
                    let mut s = String::new();
                    if let Some(n) = t.get("name").and_then(|v| v.as_str()).filter(|n| !n.is_empty()) {
                        s.push_str(&format!("Table: {n}\n"));
                    }
                    if let Some(h) = t.get("headers") {
                        let h = row(h);
                        if !h.is_empty() {
                            s.push_str(&format!("{h}\n"));
                        }
                    }
                    for r in t.get("rows").and_then(|v| v.as_array()).into_iter().flatten() {
                        s.push_str(&format!("{}\n", row(r)));
                    }
                    s.trim_end().to_string()
                })
                .collect::<Vec<_>>()
                .join("\n\n")
        }
        ExtractKind::Article => data.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        ExtractKind::Metadata => data
            .as_object()
            .map(|o| {
                o.iter()
                    .filter(|(_, v)| !v.is_null())
                    .map(|(k, v)| format!("{k}: {}", v.as_str().map(String::from).unwrap_or_else(|| v.to_string())))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn kinds_parse_and_scripts_embed_limit() {
        assert_eq!(ExtractKind::parse("headlines"), Some(ExtractKind::Headlines));
        assert_eq!(ExtractKind::parse("News"), Some(ExtractKind::Headlines));
        assert_eq!(ExtractKind::parse("javascript"), None);
        let s = script(ExtractKind::Headlines, 500);
        assert!(s.starts_with("(el) => {"));
        assert!(s.contains("const MAX = 50;"));
        assert!(!s.contains("fetch(") && !s.contains("XMLHttpRequest") && !s.contains("cookie"));
    }

    #[test]
    fn renders_items_and_tables() {
        let d = json!({"items": [{"title": "A story", "url": "https://x/1"}, {"title": "B story"}]});
        assert_eq!(render(ExtractKind::Headlines, &d), "1. A story\n2. B story");
        assert_eq!(render(ExtractKind::Headlines, &json!({"items": []})), "No matching items found on this page.");
        let t = json!({"tables": [{"name": "Weather", "headers": ["City", "High"], "rows": [["Bikaner", "38"]]}]});
        assert_eq!(render(ExtractKind::Tables, &t), "Table: Weather\nCity | High\nBikaner | 38");
    }
}
