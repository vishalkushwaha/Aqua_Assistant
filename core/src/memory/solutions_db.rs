//! Solutions DB & Fast-Path Vector Memory Store
//!
//! Persists verified MCP tool recipes with 384-d intent embeddings so repeated
//! goals can skip slow LLM planning.

use crate::storage::sqlite_store::{self, Migration, Opts};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Stored solution recipe record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SolutionRecord {
    pub id: i64,
    pub intent_pattern: String,
    pub payload_recipe_json: String,
    pub execution_count: u64,
    pub success_rate: f32,
    pub last_executed_timestamp: String,
}

/// Vector search match result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub solution: SolutionRecord,
    pub similarity_score: f32,
    pub is_fast_path_eligible: bool,
}

/// Result of [`SolutionsDb::purge_unsafe_recipes`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PurgeReport {
    pub scanned: usize,
    /// Recipes deleted because they use a tool Aqua no longer exposes.
    pub deleted: usize,
    /// Recipes kept after their typed text and URL credentials were redacted.
    pub redacted: usize,
}

/// Tools that used to be reachable and no longer are. A recipe using one is deleted.
pub const RETIRED_TOOLS: &[&str] = &["PowerShell", "FileSystem", "Registry", "Process", "Scrape", "Clipboard", "Shell"];

/// True for text that is a raw dump of a page or a desktop snapshot (an accessibility tree, a list
/// of links and buttons) rather than an answer someone can read. Such text must never be saved as
/// a recipe's final answer: replaying it hands the user yesterday's page source.
pub fn looks_like_raw_dump(text: &str) -> bool {
    let t = text.trim_start();
    let lines: Vec<&str> = text.lines().collect();
    let tree_lines = lines.iter().filter(|l| {
        let l = l.trim_start();
        let l = l.trim_start_matches(|c: char| c.is_ascii_digit() || c == '.' || c == ' ');
        l.starts_with("- link") || l.starts_with("- button") || l.starts_with("- heading") || l.starts_with("- img") || l.starts_with("- tab")
    }).count();
    t.starts_with("Cursor Position:")
        || t.contains("Visible Displays:")
        || (t.contains("Page URL:") && tree_lines >= 5)
        || tree_lines >= 12
        || text.chars().count() > 3000
}

/// Text a small model sometimes copies out of the instructions instead of answering ("done answer").
pub fn is_placeholder_answer(answer: &str) -> bool {
    // Letters and digits of any script count as words: an answer in Hindi, Tamil, Arabic or Chinese is an answer. (It used to keep ASCII only, so an answer with no Latin letter in it
    // came out empty and was taken for a placeholder, refused by the loop's check of a final and never learned: card OA2-04, found by the scenario `non_ascii_goal_and_file`.)
    let a: String = answer.to_lowercase().chars().filter(|c| c.is_alphanumeric() || *c == ' ').collect();
    let a = a.trim();
    a.is_empty()
        || matches!(a, "done answer" | "done" | "answer" | "final" | "the goal is done" | "short confirmation" | "your answer" | "ok" | "yes")
        || a.starts_with("short confirmation that")
        || a.starts_with("write your real answer")
        || a.contains("your answer with the facts")
}

/// The schema, one step per change; never edit a step that has shipped, add one at the end (see `storage::sqlite_store`). A file made before versions existed holds this table
/// already: the step does nothing to it and the version is stamped.
const STEPS: &[Migration] = &[Migration {
    id: 1,
    sql: "CREATE TABLE IF NOT EXISTS solutions (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 intent_pattern TEXT NOT NULL,
                 payload_recipe_json TEXT NOT NULL,
                 vector_embedding_blob BLOB NOT NULL,
                 execution_count INTEGER DEFAULT 1,
                 success_rate REAL DEFAULT 1.0,
                 last_executed_timestamp TEXT NOT NULL
             );",
}];

/// `secure_delete`: a deleted or rewritten row is overwritten on disk, not just unlinked, so text that was redacted does not stay readable in the freed pages. It belongs to the
/// connection, so every connection sets it.
fn opts() -> Opts {
    Opts::standard().secure_delete(true)
}

/// Solutions DB Engine.
pub struct SolutionsDb {
    db_path: PathBuf,
}

impl SolutionsDb {
    /// A connection that overwrites deleted text. `secure_delete` belongs to the connection, so it is set on every
    /// one (it was only set on the first), and a recipe that is rewritten or deleted does not stay readable on disk.
    fn open_conn(&self) -> Result<Connection, String> {
        sqlite_store::connect_checked(&self.db_path, &opts(), sqlite_store::newest(STEPS)).map_err(|e| format!("Failed to open DB: {e}"))
    }

    /// Creates a new SolutionsDb instance.
    pub fn new(base_dir: impl AsRef<Path>) -> Self {
        let db_path = base_dir.as_ref().join("data").join("solutions.db");
        let engine = Self { db_path };
        // A failure here used to be dropped, and every later call then failed with "no such table" and no word of why.
        if let Err(e) = engine.init_db() {
            tracing::warn!("solutions db: {e}");
        }
        engine
    }

    /// The folder, the check (a damaged file is moved aside and a new one made), the write-ahead log and the schema version.
    fn init_db(&self) -> Result<(), String> {
        sqlite_store::open_migrated(&self.db_path, &opts(), STEPS).map(|_| ()).map_err(|e| format!("Failed to initialize solutions DB: {e}"))
    }

    /// One-time cleanup for recipes stored before the tool host existed: delete recipes that call
    /// a retired tool (PowerShell, FileSystem, Registry, Process, Scrape, Clipboard) and redact
    /// typed text and credential-looking URL parameters in the rest.
    pub fn purge_unsafe_recipes(&self) -> Result<PurgeReport, String> {
        let conn = self.open_conn()?;
        let rows: Vec<(i64, String)> = {
            let mut stmt = conn
                .prepare("SELECT id, payload_recipe_json FROM solutions")
                .map_err(|e| e.to_string())?;
            let mapped = stmt
                .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
                .map_err(|e| e.to_string())?;
            mapped.filter_map(Result::ok).collect()
        };
        let mut report = PurgeReport { scanned: rows.len(), ..Default::default() };
        for (id, json) in rows {
            let Ok(mut recipe) = serde_json::from_str::<serde_json::Value>(&json) else {
                conn.execute("DELETE FROM solutions WHERE id = ?1", params![id]).map_err(|e| e.to_string())?;
                report.deleted += 1;
                continue;
            };
            let tools = recipe.get("tools").and_then(|t| t.as_array()).cloned().unwrap_or_default();
            let uses_retired = tools.iter().any(|t| {
                t.get("tool")
                    .and_then(|n| n.as_str())
                    .is_some_and(|n| RETIRED_TOOLS.iter().any(|r| r.eq_ignore_ascii_case(n)))
            });
            if uses_retired {
                conn.execute("DELETE FROM solutions WHERE id = ?1", params![id]).map_err(|e| e.to_string())?;
                report.deleted += 1;
                continue;
            }
            // A recipe whose saved answer is a raw page dump is worse than none. An empty answer is not a placeholder: a run that
            // read a page stores its steps without the answer (the page could have shaped it), and a replay answers from a fresh reading.
            if recipe.get("final_answer").and_then(|f| f.as_str()).is_some_and(|f| looks_like_raw_dump(f) || (!f.trim().is_empty() && is_placeholder_answer(f))) {
                conn.execute("DELETE FROM solutions WHERE id = ?1", params![id]).map_err(|e| e.to_string())?;
                report.deleted += 1;
                continue;
            }
            let mut changed = false;
            // A version 2 recipe keeps a placeholder (`{{text1}}`) in a field that redaction hides whatever it holds: the placeholder holds nothing.
            let v2 = recipe.get("version").and_then(|v| v.as_u64()).is_some_and(|v| v >= 2);
            if let Some(arr) = recipe.get_mut("tools").and_then(|t| t.as_array_mut()) {
                for call in arr {
                    let name = call.get("tool").and_then(|n| n.as_str()).unwrap_or("").to_string();
                    if let Some(args) = call.get("arguments").cloned() {
                        let mut clean = crate::modules::browser_agent::redact_recipe_args(&name, &args);
                        if v2 {
                            crate::memory::agent_learning::keep_markers(&args, &mut clean);
                        }
                        if clean != args {
                            call["arguments"] = clean;
                            changed = true;
                        }
                    }
                }
            }
            // The saved answer can name the same people and numbers as the goal did.
            if let Some(answer) = recipe.get("final_answer").and_then(|f| f.as_str()).map(str::to_string) {
                let (clean, removed) = crate::memory::experience::sanitize::redact(&answer);
                if removed > 0 {
                    recipe["final_answer"] = serde_json::Value::String(clean);
                    changed = true;
                }
            }
            if changed {
                conn.execute(
                    "UPDATE solutions SET payload_recipe_json = ?1 WHERE id = ?2",
                    params![recipe.to_string(), id],
                )
                .map_err(|e| e.to_string())?;
                report.redacted += 1;
            }
        }
        // The goal text itself ("email bob@x.com saying ...") is stored too: redact it, and recompute the embedding
        // from the redacted text so a replay never depends on words that were removed.
        let patterns: Vec<(i64, String)> = {
            let mut stmt = conn.prepare("SELECT id, intent_pattern FROM solutions").map_err(|e| e.to_string())?;
            let mapped = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))).map_err(|e| e.to_string())?;
            mapped.filter_map(Result::ok).collect()
        };
        for (id, pattern) in patterns {
            let (clean, removed) = crate::memory::experience::sanitize::redact(&pattern);
            if removed > 0 {
                let emb = serialize_embedding(&crate::memory::intent_embed::intent_embedding(&clean));
                conn.execute(
                    "UPDATE solutions SET intent_pattern = ?1, vector_embedding_blob = ?2 WHERE id = ?3",
                    params![clean, emb, id],
                )
                .map_err(|e| e.to_string())?;
                report.redacted += 1;
            }
        }
        if report.deleted > 0 || report.redacted > 0 {
            // Rewrite the file so the removed text is not left behind in free pages.
            let _ = conn.execute_batch("PRAGMA secure_delete = ON; VACUUM;");
        }
        Ok(report)
    }

    /// Saves a verified solution recipe into the database.
    pub fn save_solution(
        &self,
        pattern: &str,
        recipe_json: &str,
        embedding: &[f32],
    ) -> Result<i64, String> {
        let conn =
            self.open_conn()?;
        let blob = serialize_embedding(embedding);
        let now = utc_now();

        conn.execute(
            "INSERT INTO solutions (intent_pattern, payload_recipe_json, vector_embedding_blob, execution_count, success_rate, last_executed_timestamp)
             VALUES (?1, ?2, ?3, 1, 1.0, ?4);",
            params![pattern, recipe_json, blob, now],
        )
        .map_err(|e| format!("Failed to insert solution: {e}"))?;

        Ok(conn.last_insert_rowid())
    }

    /// Upsert: reinforce a near-identical intent, otherwise insert a new recipe.
    pub fn save_or_reinforce(
        &self,
        pattern: &str,
        recipe_json: &str,
        embedding: &[f32],
    ) -> Result<i64, String> {
        if let Some(existing) = self.query_by_intent_text(pattern)? {
            self.record_reuse(existing.id, true, Some(recipe_json))?;
            return Ok(existing.id);
        }
        // Only a goal that says the same thing may overwrite a stored recipe: "weather in Pune" is close to "weather
        // in Delhi" in the embedding and must not replace it.
        if let Some(hit) = self.query_fast_path_for(pattern, embedding, 0.95, true)? {
            self.record_reuse(hit.solution.id, true, Some(recipe_json))?;
            return Ok(hit.solution.id);
        }
        self.save_solution(pattern, recipe_json, embedding)
    }

    /// Bump reuse counters after a recipe is replayed (or fails).
    pub fn record_reuse(
        &self,
        id: i64,
        success: bool,
        new_recipe: Option<&str>,
    ) -> Result<(), String> {
        let conn =
            self.open_conn()?;

        let row: Option<(i64, f64)> = conn
            .query_row(
                "SELECT execution_count, success_rate FROM solutions WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(|e| format!("Failed to load solution {id}: {e}"))?;

        let Some((count, rate)) = row else {
            return Err(format!("solution id {id} not found"));
        };

        let count_f = count as f64;
        let outcome = if success { 1.0 } else { 0.0 };
        let new_rate = ((rate * count_f) + outcome) / (count_f + 1.0);
        let now = utc_now();

        if let Some(recipe) = new_recipe {
            conn.execute(
                "UPDATE solutions SET execution_count = ?1, success_rate = ?2,
                 last_executed_timestamp = ?3, payload_recipe_json = ?4 WHERE id = ?5",
                params![count + 1, new_rate, now, recipe, id],
            )
            .map_err(|e| format!("Failed to update solution: {e}"))?;
        } else {
            conn.execute(
                "UPDATE solutions SET execution_count = ?1, success_rate = ?2,
                 last_executed_timestamp = ?3 WHERE id = ?4",
                params![count + 1, new_rate, now, id],
            )
            .map_err(|e| format!("Failed to update solution: {e}"))?;
        }
        Ok(())
    }

    /// Every stored recipe, the one used most recently first (and the newest first among those used at the same second). Nothing is changed by listing.
    pub fn list_solutions(&self) -> Result<Vec<SolutionRecord>, String> {
        let conn = self.open_conn()?;
        let mut stmt = conn
            .prepare(
                "SELECT id, intent_pattern, payload_recipe_json, execution_count, success_rate, last_executed_timestamp
                 FROM solutions ORDER BY CAST(last_executed_timestamp AS INTEGER) DESC, id DESC;",
            )
            .map_err(|e| format!("Failed to prepare list query: {e}"))?;
        let rows = stmt
            .query_map([], |row| {
                Ok(SolutionRecord {
                    id: row.get(0)?,
                    intent_pattern: row.get(1)?,
                    payload_recipe_json: row.get(2)?,
                    execution_count: row.get::<_, i64>(3)? as u64,
                    success_rate: row.get::<_, f64>(4)? as f32,
                    last_executed_timestamp: row.get(5)?,
                })
            })
            .map_err(|e| format!("List query failed: {e}"))?;
        Ok(rows.flatten().collect())
    }

    /// Forgets one recipe. `Ok(false)` when there is none with that id. The connection overwrites what it deletes, so the steps do not stay readable on disk.
    pub fn delete_solution(&self, id: i64) -> Result<bool, String> {
        let conn = self.open_conn()?;
        let removed = conn.execute("DELETE FROM solutions WHERE id = ?1", params![id]).map_err(|e| format!("Failed to delete solution {id}: {e}"))?;
        Ok(removed > 0)
    }

    /// Exact / normalized intent lookup (stronger than embedding for identical tasks).
    pub fn query_by_intent_text(&self, pattern: &str) -> Result<Option<SolutionRecord>, String> {
        let conn =
            self.open_conn()?;
        let needle = normalize_intent(pattern);
        // Nothing to compare (the goal had no letters or digits): an empty text is not "the same as" an empty text.
        if needle.is_empty() {
            return Ok(None);
        }
        let mut stmt = conn
            .prepare(
                "SELECT id, intent_pattern, payload_recipe_json, execution_count, success_rate, last_executed_timestamp
                 FROM solutions ORDER BY execution_count DESC;",
            )
            .map_err(|e| format!("Failed to prepare text query: {e}"))?;

        let rows = stmt
            .query_map([], |row| {
                Ok(SolutionRecord {
                    id: row.get(0)?,
                    intent_pattern: row.get(1)?,
                    payload_recipe_json: row.get(2)?,
                    execution_count: row.get::<_, i64>(3)? as u64,
                    success_rate: row.get::<_, f64>(4)? as f32,
                    last_executed_timestamp: row.get(5)?,
                })
            })
            .map_err(|e| format!("Text query failed: {e}"))?;

        for row in rows.flatten() {
            if normalize_intent(&row.intent_pattern) == needle {
                return Ok(Some(row));
            }
        }
        Ok(None)
    }

    /// Queries vector memory for fast-path macro re-use.
    /// Like [`query_fast_path`](Self::query_fast_path), but a stored goal only counts as the same task when it says
    /// the same thing: the same content words (see [`same_content`]). The embedding is a hashed bag of words, so it
    /// scores "weather in Delhi" and "weather in Pune" as nearly identical, and a replay would then open the wrong
    /// page or hand back the wrong city's saved answer. With `require_same`, a near miss is no match at all;
    /// without it, the hit is returned as a hint but is never eligible for replay.
    pub fn query_fast_path_for(
        &self,
        goal: &str,
        query_embedding: &[f32],
        min_threshold: f32,
        require_same: bool,
    ) -> Result<Option<SearchResult>, String> {
        Ok(self.query_fast_path(query_embedding, min_threshold)?.and_then(|mut hit| {
            let same = same_content(goal, &hit.solution.intent_pattern);
            if require_same && !same {
                return None;
            }
            hit.is_fast_path_eligible = hit.is_fast_path_eligible && same;
            Some(hit)
        }))
    }

    pub fn query_fast_path(
        &self,
        query_embedding: &[f32],
        min_threshold: f32,
    ) -> Result<Option<SearchResult>, String> {
        let conn =
            self.open_conn()?;
        let mut stmt = conn
            .prepare(
                "SELECT id, intent_pattern, payload_recipe_json, vector_embedding_blob,
                        execution_count, success_rate, last_executed_timestamp FROM solutions;",
            )
            .map_err(|e| format!("Failed to prepare query statement: {e}"))?;

        let rows = stmt
            .query_map([], |row| {
                let blob: Vec<u8> = row.get(3)?;
                Ok((
                    SolutionRecord {
                        id: row.get(0)?,
                        intent_pattern: row.get(1)?,
                        payload_recipe_json: row.get(2)?,
                        execution_count: row.get::<_, i64>(4)? as u64,
                        success_rate: row.get::<_, f64>(5)? as f32,
                        last_executed_timestamp: row.get(6)?,
                    },
                    blob,
                ))
            })
            .map_err(|e| format!("Query failed: {e}"))?;

        let mut best_match: Option<SearchResult> = None;
        let mut max_similarity = 0.0f32;

        for item in rows {
            if let Ok((solution, blob)) = item {
                let stored_embedding = deserialize_embedding(&blob);
                let similarity = cosine_similarity(query_embedding, &stored_embedding);

                if similarity > max_similarity && similarity >= min_threshold {
                    max_similarity = similarity;
                    best_match = Some(SearchResult {
                        solution,
                        similarity_score: similarity,
                        is_fast_path_eligible: similarity >= 0.92,
                    });
                }
            }
        }

        Ok(best_match)
    }
}

pub fn normalize_intent(text: &str) -> String {
    // Letters of every script count: with ASCII only, "東京の天気" and "北京の天気" both normalised to nothing and were
    // "the same task", so a recipe learned for one city was replayed for the other.
    text.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.is_empty() || b.is_empty() || a.len() != b.len() {
        return 0.0;
    }

    let mut dot = 0.0f32;
    let mut norm_a = 0.0f32;
    let mut norm_b = 0.0f32;

    for i in 0..a.len() {
        dot += a[i] * b[i];
        norm_a += a[i] * a[i];
        norm_b += b[i] * b[i];
    }

    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }

    dot / (norm_a.sqrt() * norm_b.sqrt())
}

/// Words that carry no meaning of their own in a request.
const STOP_WORDS: &[&str] = &[
    "a", "an", "the", "me", "my", "i", "you", "your", "to", "for", "of", "in", "on", "at", "is", "are", "was", "be", "it", "its", "and", "or", "please",
    "can", "could", "would", "will", "tell", "show", "give", "get", "what", "whats", "what's", "how", "now", "right", "today", "todays", "just", "kindly",
    "hey", "aqua", "okay", "ok", "thanks", "thank", "do", "does", "this", "that", "with", "about",
];

/// Is this a word that carries no meaning of its own in a request (the wake phrase, "please", "okay", ...)?
pub(crate) fn is_filler_word(word: &str) -> bool {
    STOP_WORDS.contains(&word)
}

fn content_words(goal: &str) -> std::collections::BTreeSet<String> {
    content_word_sequence(goal).into_iter().collect()
}

/// The content words of a goal in the order they were said, with the filler words left out (`content_words` is the same as a set). A recipe with placeholders
/// is matched on this: "copy {{file1}} to {{file2}}" and "copy {{file2}} to {{file1}}" have the same words and are not the same task.
pub fn content_word_sequence(goal: &str) -> Vec<String> {
    let (clean, removed) = crate::memory::experience::sanitize::redact(goal);
    // A goal with an email address, a number that looked like a secret and the like is never "the same" as another.
    // "today's" and "todays", "what's" and "whats" are the same word (the stop words are listed without the apostrophe).
    let mut words: Vec<String> = clean
        .to_lowercase()
        .replace(['\'', '\u{2019}'], "")
        .split(|c: char| !(c.is_alphanumeric() || c == '@' || c == '.' || c == '\'' || c == '-'))
        .map(|w| w.trim_matches(|c: char| c == '.' || c == '\'' || c == '-').to_string())
        .filter(|w| !w.is_empty() && !STOP_WORDS.contains(&w.as_str()))
        .collect();
    if removed > 0 {
        words.push(format!("<redacted:{removed}>"));
    }
    words
}

/// Do two goals say the same thing? Equal once filler words are removed ("tell me the weather in Delhi please" and
/// "weather in delhi" are; "weather in Pune" is not). Goals that contained redacted text are never the same, because
/// what was removed is exactly what made them different.
pub fn same_content(a: &str, b: &str) -> bool {
    let (wa, wb) = (content_words(a), content_words(b));
    !wa.iter().any(|w| w.starts_with("<redacted")) && !wb.iter().any(|w| w.starts_with("<redacted")) && !wa.is_empty() && wa == wb
}

fn serialize_embedding(embedding: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(embedding.len() * 4);
    for &val in embedding {
        bytes.extend_from_slice(&val.to_le_bytes());
    }
    bytes
}

fn deserialize_embedding(blob: &[u8]) -> Vec<f32> {
    blob.chunks_exact(4)
        .map(|chunk| f32::from_le_bytes(chunk.try_into().unwrap_or([0; 4])))
        .collect()
}

fn utc_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{secs}")
}

#[cfg(test)]
mod unicode_intent_tests {
    use super::*;

    #[test]
    fn goals_in_other_scripts_are_told_apart_and_an_empty_goal_matches_nothing() {
        assert_ne!(normalize_intent("東京の天気"), normalize_intent("北京の天気"));
        assert_ne!(normalize_intent("weather in Zürich"), normalize_intent("weather in Zurich"));
        assert_eq!(normalize_intent("Weather,  IN   Delhi!"), "weather in delhi");
        assert_eq!(normalize_intent("!!! ???"), "");
        {
            // Every connection overwrites what it deletes.
            let probe = std::env::temp_dir().join(format!("aqua_sd_{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&probe);
            std::fs::create_dir_all(&probe).unwrap();
            let c = SolutionsDb::new(&probe).open_conn().unwrap();
            let on: i64 = c.query_row("PRAGMA secure_delete", [], |r| r.get(0)).unwrap();
            assert_eq!(on, 1);
            drop(c);
            let _ = std::fs::remove_dir_all(&probe);
        }
        let dir = std::env::temp_dir().join(format!("aqua_uni_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = SolutionsDb::new(&dir);
        db.save_or_reinforce("東京の天気", r#"{"tools":[],"final_answer":"x"}"#, &crate::memory::intent_embed::intent_embedding("東京の天気")).unwrap();
        assert!(db.query_by_intent_text("北京の天気").unwrap().is_none(), "another city is not the same task");
        assert!(db.query_by_intent_text("東京の天気").unwrap().is_some());
        assert!(db.query_by_intent_text("???").unwrap().is_none(), "an empty needle matches nothing");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod purge_tests {
    use super::*;

    #[test]
    fn raw_page_and_desktop_dumps_are_recognised_but_ordinary_answers_are_not() {
        let page = format!("1. Page URL: https://x/search\n2. Page Title: t\n{}", (3..40).map(|i| format!("{i}. - link \"L{i}\" ref{i}")).collect::<Vec<_>>().join("\n"));
        assert!(looks_like_raw_dump(&page));
        assert!(looks_like_raw_dump("Cursor Position: (1, 2)\nVisible Displays: 0:\\\\.\\DISPLAY1"));
        assert!(looks_like_raw_dump(&"x".repeat(4000)));
        for ok in [
            "Bikaner: sunny, 36 degrees, humidity 14%.",
            "Top 3 headlines: 1. Solar farm powers 10,000 homes 2. Rain expected 3. Match won",
            "The launch code word is PINEAPPLE-42.",
            "Products with stock under 20:\n1. Gadget - Stock: 15\n2. Gizmo - Stock: 0",
        ] {
            assert!(!looks_like_raw_dump(ok), "{ok}");
        }
    }

    #[test]
    fn purge_deletes_recipes_whose_saved_answer_is_a_raw_page_dump() {
        let dir = std::env::temp_dir().join(format!("aqua_purge_dump_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let db = SolutionsDb::new(&dir);
        let dump = format!("Page URL: https://x\n{}", (0..30).map(|i| format!("- link \"L{i}\" ref{i}")).collect::<Vec<_>>().join("\n"));
        let bad = serde_json::json!({"tools": [{"tool": "browser_navigate", "arguments": {"url": "https://x"}, "source": "browser"}], "final_answer": dump}).to_string();
        let good = r#"{"tools":[{"tool":"browser_navigate","arguments":{"url":"https://y"},"source":"browser"}],"final_answer":"Delhi: hot, 38 degrees."}"#;
        let (mut e1, mut e2) = (vec![0.0f32; 8], vec![0.0f32; 8]);
        e1[0] = 1.0;
        e2[5] = 1.0;
        db.save_or_reinforce("weather bad", &bad, &e1).unwrap();
        db.save_or_reinforce("weather good", good, &e2).unwrap();
        let mut e3 = vec![0.0f32; 8];
        e3[3] = 1.0;
        let placeholder = r#"{"tools":[{"tool":"browser_navigate","arguments":{"url":"https://z"},"source":"browser"}],"final_answer":"done answer"}"#;
        db.save_or_reinforce("weather placeholder", placeholder, &e3).unwrap();
        let r = db.purge_unsafe_recipes().unwrap();
        assert_eq!((r.scanned, r.deleted), (3, 2), "the raw dump and the placeholder go, the real answer stays: {r:?}");
    }

    /// A run that read a web page is "tainted": its tool steps are kept for replay but its answer, which the page could have
    /// shaped, is stored empty, and a replay answers from what it reads fresh. The purge used to take that empty answer for a
    /// placeholder copied out of the instructions and delete the recipe at the next task: 11 were stored in the log, none was ever
    /// replayed.
    #[test]
    fn purge_keeps_a_recipe_stored_with_an_empty_answer() {
        let dir = std::env::temp_dir().join(format!("aqua_purge_empty_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let db = SolutionsDb::new(&dir);
        let tainted = r#"{"tools":[{"tool":"browser_navigate","arguments":{"url":"https://weather.example/pune"},"source":"browser"},{"tool":"browser_snapshot","arguments":{},"source":"browser"}],"final_answer":""}"#;
        let spaces = r#"{"tools":[{"tool":"browser_navigate","arguments":{"url":"https://weather.example/delhi"},"source":"browser"}],"final_answer":"   "}"#;
        db.save_or_reinforce("weather in pune", tainted, &[0.1f32; 8]).unwrap();
        db.save_or_reinforce("weather in delhi", spaces, &[0.9f32; 8]).unwrap();
        let r = db.purge_unsafe_recipes().unwrap();
        assert_eq!((r.scanned, r.deleted), (2, 0), "an empty answer is not a placeholder: {r:?}");
        let conn = Connection::open(dir.join("data").join("solutions.db")).unwrap();
        let left: i64 = conn.query_row("SELECT COUNT(*) FROM solutions", [], |r| r.get(0)).unwrap();
        assert_eq!(left, 2);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Card OA2-04: the check kept ASCII letters only, so an answer in Hindi (or any script without a Latin letter in it) came out empty and was a "placeholder": the loop sent a correct final
    /// back and a recipe with such an answer was never learned. A sentence of any script is an answer; the words copied out of the instructions, and a bare symbol, still are not.
    #[test]
    fn an_answer_in_any_script_is_an_answer_and_the_words_of_the_instructions_are_not() {
        for answer in ["पहली पंक्ति है: नमस्ते दुनिया 🙂", "答案是四十二", "Ответ: сорок два", "الإجابة هي اثنان وأربعون", "42", "Delhi: hot, 38 degrees."] {
            assert!(!is_placeholder_answer(answer), "{answer:?} is an answer");
        }
        for placeholder in ["", "   ", "done", "Done.", "ANSWER", "  ok ", "Yes!", "done answer", "short confirmation that it worked", "write your real answer here", "🙂", "..."] {
            assert!(is_placeholder_answer(placeholder), "{placeholder:?} is not an answer");
        }
    }

    #[test]
    fn purge_deletes_retired_tools_and_redacts_the_rest() {
        let dir = std::env::temp_dir().join(format!("aqua_purge_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let db = SolutionsDb::new(&dir);
        let emb = vec![0.1f32; 384];
        let bad = r#"{"tools":[{"tool":"PowerShell","arguments":{"command":"whoami"},"source":"windows"}],"final_answer":"x"}"#;
        let typed = r#"{"tools":[{"tool":"Type","arguments":{"text":"hunter2"},"source":"windows"},{"tool":"browser_navigate","arguments":{"url":"https://a.com/x?token=abc&q=news"},"source":"browser"}],"final_answer":"y"}"#;
        let clean = r#"{"tools":[{"tool":"browser_navigate","arguments":{"url":"https://a.com/news"},"source":"browser"}],"final_answer":"z"}"#;
        db.save_solution("open a shell", bad, &emb).unwrap();
        db.save_solution("type a password", typed, &[0.2f32; 384]).unwrap();
        db.save_solution("read news", clean, &[0.3f32; 384]).unwrap();
        let report = db.purge_unsafe_recipes().unwrap();
        assert_eq!(report, PurgeReport { scanned: 3, deleted: 1, redacted: 1 });
        let left = Connection::open(dir.join("data").join("solutions.db")).unwrap();
        let all: Vec<String> = left
            .prepare("SELECT payload_recipe_json FROM solutions").unwrap()
            .query_map([], |r| r.get(0)).unwrap().filter_map(Result::ok).collect();
        assert_eq!(all.len(), 2);
        let joined = all.join("\n");
        assert!(!joined.contains("hunter2") && !joined.contains("abc") && !joined.contains("PowerShell"));
        assert!(joined.contains("q=news"));
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(test)]
mod same_content_tests {
    use super::same_content;

    #[test]
    fn two_goals_are_the_same_only_when_they_say_the_same_thing() {
        assert!(same_content("Tell me the weather in Delhi please", "weather in delhi"));
        assert!(same_content("What's the weather in Delhi?", "weather delhi"));
        assert!(!same_content("weather in Delhi", "weather in Pune"), "a different city is a different task");
        assert!(!same_content("search wikipedia for apples", "search wikipedia for oranges"));
        assert!(!same_content("open the calendar", "open the camera"));
        assert!(!same_content("", ""), "nothing is not the same as nothing");
    }

    /// "today's" kept its apostrophe and so was a word of its own, while "todays" was dropped as a filler: the same request, spoken
    /// or typed two ways, was two different goals and never matched a stored recipe.
    #[test]
    fn an_apostrophe_does_not_make_a_different_goal() {
        assert!(same_content("Tell me today's weather in Pune", "weather in pune"));
        assert!(same_content("Tell me today\u{2019}s weather in Pune", "tell me todays weather in pune"));
        assert!(same_content("What's the weather in Pune", "whats the weather in pune"));
        assert!(same_content("what\u{2019}s the weather in Pune?", "weather pune"));
        assert!(!same_content("Tell me today's weather in Pune", "weather in Delhi"), "the place still matters");
        assert!(!same_content("don't open the calendar", "do open the calendar"), "dont is not do");
    }

    #[test]
    fn a_goal_with_an_email_address_is_never_the_same_as_another_so_it_is_never_replayed() {
        assert!(!same_content("email bob@example.com saying hi", "email bob@example.com saying hi"));
        assert!(!same_content("email bob@example.com saying hi", "email carol@example.com saying hi"));
    }

    /// A recipe with placeholders is matched on the words in the order they were said: the set of words alone cannot tell "copy a to b" from "copy b to a".
    #[test]
    fn the_content_words_in_order_tell_apart_what_the_set_of_words_cannot() {
        use super::{content_word_sequence, content_words};
        assert_eq!(content_word_sequence("Copy {{file1}} to {{file2}} please"), vec!["copy", "file1", "file2"]);
        assert_ne!(content_word_sequence("copy {{file1}} to {{file2}}"), content_word_sequence("copy {{file2}} to {{file1}}"));
        assert_eq!(content_words("copy {{file1}} to {{file2}}"), content_words("copy {{file2}} to {{file1}}"), "as a set they are the same, as they always were");
        // The address becomes "[email]" and its marker closes the list.
        assert_eq!(content_word_sequence("email bob@example.com"), vec!["email", "email", "<redacted:1>"], "a redaction is still part of the words, at the end");
    }
}

#[cfg(test)]
mod listing_tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("aqua_sol_list_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn stamp(dir: &Path, id: i64, secs: &str) {
        let conn = Connection::open(dir.join("data").join("solutions.db")).unwrap();
        conn.execute("UPDATE solutions SET last_executed_timestamp = ?1 WHERE id = ?2", params![secs, id]).unwrap();
    }

    #[test]
    fn recipes_are_listed_with_the_one_used_last_first_and_one_is_forgotten_by_its_id() {
        let dir = temp("list");
        let db = SolutionsDb::new(&dir);
        let a = db.save_solution("weather in pune", r#"{"tools":[],"final_answer":"a"}"#, &[0.1f32; 8]).unwrap();
        let b = db.save_solution("news headlines", r#"{"tools":[],"final_answer":"b"}"#, &[0.9f32; 8]).unwrap();
        stamp(&dir, a, "100");
        stamp(&dir, b, "200");
        let ids = |db: &SolutionsDb| db.list_solutions().unwrap().into_iter().map(|r| r.id).collect::<Vec<_>>();
        assert_eq!(ids(&db), vec![b, a]);
        stamp(&dir, a, "300");
        assert_eq!(ids(&db), vec![a, b], "the stamp is compared as a number, not as text");
        let all = db.list_solutions().unwrap();
        assert_eq!((all[0].intent_pattern.as_str(), all[0].execution_count, all[0].payload_recipe_json.contains("\"a\"")), ("weather in pune", 1, true));

        assert!(db.delete_solution(a).unwrap());
        assert_eq!(ids(&db), vec![b], "only that one is gone");
        assert!(!db.delete_solution(a).unwrap(), "it is not there any more");
        assert!(!db.delete_solution(9_999).unwrap(), "nor is an id that never was");
        assert_eq!(ids(&db), vec![b]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod file_tests {
    use super::*;
    use crate::test_support::TempWorld;

    fn version(path: &Path) -> i64 {
        Connection::open(path).unwrap().query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn a_new_file_has_a_version_and_a_file_without_one_is_stamped_and_keeps_its_recipes() {
        let w = TempWorld::new("solutions_version");
        let db = SolutionsDb::new(&w.root);
        let path = w.path("data/solutions.db");
        assert_eq!(version(&path), 1);
        db.save_solution("read news", r#"{"tools":[],"final_answer":"z"}"#, &[0.3f32; 8]).unwrap();
        // The file as the build before this one left it: the table, the row, no version.
        Connection::open(&path).unwrap().execute_batch("PRAGMA user_version = 0;").unwrap();
        let again = SolutionsDb::new(&w.root);
        assert_eq!(version(&path), 1);
        assert_eq!(again.list_solutions().unwrap().len(), 1);
    }

    #[test]
    fn a_damaged_file_is_moved_aside_and_the_store_keeps_working_where_it_used_to_fail_on_every_call() {
        let w = TempWorld::new("solutions_damaged");
        w.write("data/solutions.db", "not a database, only text. ".repeat(300));
        let db = SolutionsDb::new(&w.root);
        db.save_solution("read news", r#"{"tools":[],"final_answer":"z"}"#, &[0.3f32; 8]).unwrap();
        assert_eq!(db.list_solutions().unwrap().len(), 1);
        let aside = crate::storage::sqlite_store::leftovers(&w.root);
        assert_eq!(aside.len(), 1, "{aside:?}");
        assert!(std::fs::read_to_string(&aside[0]).unwrap().starts_with("not a database"));
    }

    #[test]
    fn a_file_from_a_newer_build_is_refused_on_every_call_and_not_written() {
        let w = TempWorld::new("solutions_newer");
        let db = SolutionsDb::new(&w.root);
        db.save_solution("read news", r#"{"tools":[],"final_answer":"z"}"#, &[0.3f32; 8]).unwrap();
        let path = w.path("data/solutions.db");
        {
            let c = Connection::open(&path).unwrap();
            let _: String = c.query_row("PRAGMA journal_mode = DELETE", [], |r| r.get(0)).unwrap();
            c.execute_batch("PRAGMA user_version = 9;").unwrap();
        }
        let before = std::fs::read(&path).unwrap();
        let newer = SolutionsDb::new(&w.root);
        let err = newer.list_solutions().unwrap_err();
        assert!(err.contains("newer than this build"), "{err}");
        assert!(newer.save_solution("write", r#"{"tools":[],"final_answer":"z"}"#, &[0.1f32; 8]).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn four_threads_save_and_read_at_once() {
        let w = TempWorld::new("solutions_threads");
        let db = SolutionsDb::new(&w.root);
        let handles: Vec<_> = (0..4)
            .map(|t| {
                let root = w.root.clone();
                std::thread::spawn(move || {
                    let db = SolutionsDb::new(&root);
                    for i in 0..10 {
                        db.save_solution(&format!("goal {t} {i}"), r#"{"tools":[],"final_answer":"z"}"#, &[0.1f32 * (t + 1) as f32; 8]).unwrap();
                        assert!(!db.list_solutions().unwrap().is_empty());
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(db.list_solutions().unwrap().len(), 40);
    }
}
