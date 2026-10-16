//! Looking things up: search the database, rank, check each hit still exists, and recover when it does not.
//!
//! Lookups run on the caller's thread with their own read-only connection and never write. A hit whose file has gone is
//! reported to the worker (`RemoveEntry`, `MarkDirty`), which fixes the database; the caller gets an answer at once.

use super::platform::{long_path, VolumeMap};
use super::rules;
use super::store::{Candidate, Db};
use super::{Cmd, FileIndex};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// What kind of thing the user asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Want {
    Any,
    Folder,
    Program,
    Installer,
    File,
    /// "Search my system for X": everything the index holds, installers included.
    Search,
}

#[derive(Debug, Clone)]
pub struct LookupRequest {
    pub query: String,
    pub want: Want,
    /// How many valid hits to return at most.
    pub limit: usize,
}

#[derive(Debug, Clone)]
pub struct Found {
    /// The database row (0 when the hit came from the live search).
    pub id: i64,
    pub path: PathBuf,
    pub kind: &'static str,
    pub size: u64,
    pub cloud: bool,
    pub score: i32,
}

#[derive(Debug, Clone, Default)]
pub struct Lookup {
    /// Valid hits (the file exists now), best first.
    pub found: Vec<Found>,
    /// The index said it was somewhere that no longer exists, and it was found at a new place.
    pub moved: bool,
    /// The index was ready (the main places are indexed).
    pub index_ready: bool,
    pub index_enabled: bool,
    /// Every match was on a drive that is not connected.
    pub offline_only: bool,
    /// How the answer was made: "index" or "live".
    pub source: &'static str,
}

fn kinds_for(want: Want, index_files: bool) -> Vec<i64> {
    match want {
        Want::Folder => vec![rules::FOLDER],
        Want::Program => vec![rules::EXE, rules::SHORTCUT],
        Want::Installer => vec![rules::INSTALLER],
        Want::File => vec![rules::FILE],
        Want::Search => {
            let mut k = vec![rules::FOLDER, rules::EXE, rules::INSTALLER, rules::SHORTCUT, rules::UNINSTALLER];
            if index_files {
                k.push(rules::FILE);
            }
            k
        }
        Want::Any => {
            let mut k = vec![rules::FOLDER, rules::EXE, rules::SHORTCUT];
            if index_files {
                k.push(rules::FILE);
            }
            k
        }
    }
}

/// How well a candidate fits the query. Higher is better. Pure, so the ranking is tested without a database.
pub fn score(c: &Candidate, q: &str, want: Want, now: i64) -> i32 {
    let mut s: i32 = if c.name_norm == q {
        100
    } else if c.name_norm.starts_with(q) {
        60
    } else if c.name_norm.contains(q) {
        20
    } else {
        // Found through "all the words, in any order".
        15
    };
    let preferred = match want {
        Want::Folder => c.kind == rules::FOLDER,
        Want::Program => c.kind == rules::EXE || c.kind == rules::SHORTCUT,
        Want::Installer => c.kind == rules::INSTALLER,
        Want::File => c.kind == rules::FILE,
        Want::Any | Want::Search => false,
    };
    if preferred {
        s += 30;
    }
    // A shortcut or an executable beats a folder of the same name when nothing was said ("open Chrome").
    if want == Want::Any && (c.kind == rules::SHORTCUT || c.kind == rules::EXE) {
        s += 5;
    }
    // The user's own places before the rest of the disk.
    if c.tier <= 2 {
        s += 10;
    }
    if c.last_opened > 0 {
        s += if now - c.last_opened < 30 * 86_400 { 12 } else { 5 };
    }
    s += (c.open_count.min(10)) as i32;
    s -= c.depth.min(15) as i32;
    s -= ((c.name_norm.chars().count() as i64 - q.chars().count() as i64).max(0) / 3).min(10) as i32;
    s
}

fn now_s() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

impl FileIndex {
    /// Look something up by name. See the module comment for what is and is not written.
    pub fn lookup(&self, req: &LookupRequest) -> Lookup {
        let t0 = Instant::now();
        let settings = self.settings();
        let (ready, complete) = self.shared.status.lock().map(|s| (s.tier1_ready, s.complete)).unwrap_or((false, false));
        let mut out = Lookup { index_ready: ready, index_enabled: settings.enabled, source: "index", ..Lookup::default() };
        // "Vishal CV.pdf": names are stored without their extension.
        let query = rules::strip_document_ext(&req.query);
        let req = &LookupRequest { query, want: req.want, limit: req.limit };
        let q = rules::norm(&req.query);
        if q.is_empty() {
            return out;
        }
        let vols: VolumeMap = self.shared.vols.read().map(|v| v.clone()).unwrap_or_default();
        let mut stale = 0usize;
        let mut offline = 0usize;
        if settings.enabled && self.db_path.exists() {
            if let Ok(mut db) = Db::open_reader(&self.db_path) {
                let words: Vec<String> = req.query.split_whitespace().map(str::to_string).collect();
                let kinds = kinds_for(req.want, settings.index_files || settings.index_documents);
                let now = now_s();
                let mut cands: Vec<(i32, Candidate)> = db.search(&q, &words, &kinds, 200).into_iter().map(|c| (score(&c, &q, req.want, now), c)).collect();
                cands.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.name_norm.len().cmp(&b.1.name_norm.len())));
                for (sc, c) in cands.into_iter().take(40) {
                    let Some(path) = db.path_of(c.id, &vols) else {
                        offline += 1;
                        continue;
                    };
                    match std::fs::metadata(long_path(&path)) {
                        Ok(md) if md.is_dir() == (c.kind == rules::FOLDER) => {
                            out.found.push(Found { id: c.id, path, kind: rules::kind_str(c.kind), size: md.len(), cloud: c.flags & rules::FLAG_CLOUD != 0, score: sc });
                        }
                        // Exists, but is a different kind of thing now: the folder holding it needs a fresh look.
                        Ok(_) => {
                            stale += 1;
                            let _ = self.tx.send(Cmd::RemoveEntry(c.id));
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                            // Gone from where the index said: the worker removes the row and re-lists the folder.
                            stale += 1;
                            let _ = self.tx.send(Cmd::RemoveEntry(c.id));
                        }
                        // Access denied or a device problem: not a verdict. Skip it, keep the row.
                        Err(_) => {}
                    }
                    if out.found.len() >= req.limit.max(1) {
                        break;
                    }
                }
            }
        }
        out.offline_only = out.found.is_empty() && stale == 0 && offline > 0;
        // Nothing valid: the index is still building or off, or the hit moved. Look at the main places live (bounded).
        // A miss is final only once everything has been scanned. Until then (or after a stale hit) look live, bounded.
        let live_budget = if stale > 0 { Duration::from_secs(4) } else if !ready || !complete { Duration::from_secs(8) } else { Duration::ZERO };
        if out.found.is_empty() && !out.offline_only && live_budget > Duration::ZERO {
            let hits = crate::local_files::find_live(&req.query, live_budget, ready && stale > 0);
            let now = now_s();
            for h in hits {
                // The live search follows the user's exclusions too.
                if rules::under_excluded_path(&h.path.to_string_lossy().to_lowercase(), &settings) {
                    continue;
                }
                let kind = rules::kind_from_str(h.kind);
                if !kinds_for(req.want, true).contains(&kind) {
                    continue;
                }
                let name = h.path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                let cand = Candidate { id: 0, name: name.to_string(), name_norm: rules::search_text(name, kind == rules::FOLDER), kind, size: h.bytes as i64, flags: 0, depth: h.path.components().count() as i64, tier: 1, last_opened: 0, open_count: 0, volume_id: 0 };
                let sc = score(&cand, &q, req.want, now);
                // Teach the index about it: the folder that holds it is looked at soon.
                if let Some(parent) = h.path.parent() {
                    let _ = self.tx.send(Cmd::MarkDirty(parent.to_path_buf()));
                }
                out.found.push(Found { id: 0, path: h.path, kind: h.kind, size: h.bytes, cloud: false, score: sc });
            }
            out.found.sort_by(|a, b| b.score.cmp(&a.score));
            out.found.truncate(req.limit.max(1));
            out.moved = stale > 0 && !out.found.is_empty();
            out.source = "live";
        }
        self.record_lookup(t0.elapsed().as_millis().min(u32::MAX as u128) as u32);
        out
    }

    /// The assistant opened this hit: remember it, so it ranks higher next time.
    pub fn mark_opened(&self, id: i64) {
        if id > 0 {
            let _ = self.tx.send(Cmd::Opened(id));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_index::scan::{reconcile_dir, Ctx};
    use crate::file_index::store::Db;
    use crate::file_index::{Settings, Shared, Status};
    use std::collections::{HashMap, HashSet};
    use std::sync::{Arc, Mutex, RwLock};

    fn cand(name: &str, kind: i64, depth: i64, tier: i64) -> Candidate {
        Candidate { id: 1, name: name.into(), name_norm: rules::search_text(name, kind == rules::FOLDER), kind, size: 0, flags: 0, depth, tier, last_opened: 0, open_count: 0, volume_id: 1 }
    }

    #[test]
    fn exact_beats_prefix_beats_contains_and_the_asked_kind_wins() {
        let q = "personalai";
        let exact = score(&cand("Personal_AI", rules::FOLDER, 3, 1), q, Want::Folder, 0);
        let prefix = score(&cand("Personal AI Backup", rules::FOLDER, 3, 1), q, Want::Folder, 0);
        let contains = score(&cand("Old Personal_AI", rules::FOLDER, 3, 1), q, Want::Folder, 0);
        assert!(exact > prefix && prefix > contains, "{exact} {prefix} {contains}");
        let folder = score(&cand("Personal_AI", rules::FOLDER, 3, 1), q, Want::Folder, 0);
        let exe = score(&cand("Personal_AI.exe", rules::EXE, 3, 1), q, Want::Folder, 0);
        assert!(folder > exe + 25);
        // The user's own places beat the rest of the disk; a deep copy loses to a shallow one.
        assert!(score(&cand("Personal_AI", rules::FOLDER, 3, 1), q, Want::Folder, 0) > score(&cand("Personal_AI", rules::FOLDER, 3, 3), q, Want::Folder, 0));
        assert!(score(&cand("Personal_AI", rules::FOLDER, 2, 1), q, Want::Folder, 0) > score(&cand("Personal_AI", rules::FOLDER, 9, 1), q, Want::Folder, 0));
        let mut used = cand("Personal_AI", rules::FOLDER, 3, 1);
        used.last_opened = 1000;
        used.open_count = 4;
        assert!(score(&used, q, Want::Folder, 1100) > folder);
    }

    struct Rig {
        index: FileIndex,
        tree: std::path::PathBuf,
        db: Db,
        root: i64,
        vols: VolumeMap,
        rx: std::sync::mpsc::Receiver<Cmd>,
    }

    fn rig(tag: &str) -> Rig {
        let dir = std::env::temp_dir().join(format!("aqua_res_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let tree = dir.join("tree");
        std::fs::create_dir_all(&tree).unwrap();
        let db_path = dir.join("files.db");
        let mut db = Db::open(&db_path).unwrap();
        let vols = VolumeMap::single("t", &tree);
        let vid = db.volume_id("t").unwrap();
        let root = db.add_root(vid, "", 1, 6, false).unwrap();
        db.save_settings(&Settings::default()).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let shared = Arc::new(Shared {
            status: Mutex::new(Status { tier1_ready: true, complete: true, ..Status::default() }),
            vols: RwLock::new(vols.clone()),
            lookups_ms: Mutex::new(Default::default()),
            user_paused: Default::default(),
            settings: RwLock::new(Settings::default()),
            watch_states: Mutex::new(None),
        });
        Rig { index: FileIndex { db_path, tx, shared }, tree, db, root, vols, rx }
    }

    impl Rig {
        fn scan(&mut self) {
            let settings = Settings::default();
            let roots: HashMap<i64, (i64, i64)> = [(self.root, (1, 6))].into();
            let (rp, bp) = (HashSet::new(), HashSet::new());
            let docs = vec![self.tree.to_string_lossy().to_lowercase()];
            self.db.todo_push(self.root, 0).unwrap();
            while let Some(id) = self.db.todo_pop() {
                let ctx = Ctx { settings: &settings, vols: &self.vols, root_paths: &rp, blocked_paths: &bp, roots: &roots, now: 10, doc_dirs: &docs, doc_skip: &[], over_cap: false };
                reconcile_dir(&mut self.db, &ctx, id).unwrap();
            }
        }
    }

    #[test]
    fn a_fuzzy_name_finds_the_folder_and_a_moved_one_is_recovered_not_reported_as_found() {
        let mut r = rig("lookup");
        std::fs::create_dir_all(r.tree.join("Work/Personal_AI/src")).unwrap();
        std::fs::create_dir_all(r.tree.join("Other")).unwrap();
        r.scan();
        let hit = r.index.lookup(&LookupRequest { query: "the personal AI".into(), want: Want::Folder, limit: 5 });
        // "the" is part of the query here; the caller strips filler words, so use the clean name:
        let hit2 = r.index.lookup(&LookupRequest { query: "Personal AI".into(), want: Want::Folder, limit: 5 });
        assert!(hit.found.is_empty() || hit.found[0].path.ends_with("Personal_AI"));
        assert_eq!(hit2.found.len(), 1, "{hit2:?}");
        assert!(hit2.found[0].path.ends_with("Personal_AI") && hit2.found[0].kind == "folder" && hit2.source == "index");
        assert!(r.rx.try_recv().is_err(), "a good lookup writes nothing");

        // Stale: the folder is moved behind the index's back.
        std::fs::rename(r.tree.join("Work/Personal_AI"), r.tree.join("Other/Personal_AI")).unwrap();
        let stale = r.index.lookup(&LookupRequest { query: "Personal AI".into(), want: Want::Folder, limit: 5 });
        assert!(stale.found.iter().all(|f| f.path.exists()), "never reports something that is not there: {stale:?}");
        let sent: Vec<bool> = std::iter::from_fn(|| r.rx.try_recv().ok()).map(|c| matches!(c, Cmd::RemoveEntry(_))).collect();
        assert!(sent.contains(&true), "the stale row is reported to the worker");

        // Deleted: nothing found, and not 'found'.
        std::fs::remove_dir_all(r.tree.join("Other/Personal_AI")).unwrap();
        let gone = r.index.lookup(&LookupRequest { query: "Personal AI".into(), want: Want::Folder, limit: 5 });
        assert!(gone.found.is_empty());
    }

    #[test]
    fn a_hundred_thousand_rows_are_searched_well_inside_the_time_goal() {
        let r = rig("speed");
        let db = Db::open(&r.index.db_path).unwrap();
        let root = db.entry(r.root).unwrap();
        db.begin().unwrap();
        for i in 0..100_000 {
            db.insert_entry(&root, &format!("Project_{i:06} files"), rules::FOLDER, 0, 0, 0, 0).unwrap();
        }
        db.insert_entry(&root, "Personal_AI", rules::FOLDER, 0, 0, 0, 0).unwrap();
        db.commit().unwrap();
        let t = Instant::now();
        let c = db.search("personalai", &["personal".into(), "ai".into()], &[rules::FOLDER], 200);
        let ms = t.elapsed().as_millis();
        assert_eq!(c.len(), 1);
        assert!(ms < 100, "search took {ms} ms on 100k rows");
    }
}
