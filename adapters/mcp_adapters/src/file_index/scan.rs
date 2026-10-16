//! `reconcile_dir`: list one folder and make the database agree with the disk. The first scan, event-driven refresh,
//! startup catch-up, the weekly verify and stale-hit recovery are all just "put this folder on the queue".

use super::platform::{long_path, VolumeMap};
use super::rules::{self, Settings, EXE, FILE, FOLDER, INSTALLER, SHORTCUT};
use super::store::{Db, Row};
use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Everything a reconcile needs besides the database. Built by the worker; built by hand in tests.
pub struct Ctx<'a> {
    pub settings: &'a Settings,
    pub vols: &'a VolumeMap,
    /// Lower-case absolute paths of every root: a folder that is itself a root is skipped by the folder above it.
    pub root_paths: &'a HashSet<String>,
    /// Lower-case absolute paths never entered (the folder that holds all user profiles, for example).
    pub blocked_paths: &'a HashSet<String>,
    /// root entry id -> (tier, max depth)
    pub roots: &'a HashMap<i64, (i64, i64)>,
    pub now: i64,
    /// Lower-case folders (the profile, other drives) where document files are indexed, and folders inside them where they
    /// are not (Program Files, Windows).
    pub doc_dirs: &'a [String],
    pub doc_skip: &'a [String],
    /// The row limit was reached: the biggest tier (other drives) stops adding folders.
    pub over_cap: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Listed { added: usize, removed: usize, changed: usize },
    /// The volume is not connected: nothing was touched.
    Offline,
    /// The folder no longer exists: it (and what was below it) was removed from the index.
    Gone,
    /// Could not be read now (access denied, device not ready): nothing was removed.
    Unreadable(String),
}

struct Disk {
    name: String,
    key: String,
    is_dir: bool,
    kind: i64,
    size: i64,
    mtime: i64,
    flags: i64,
    dir_mtime: i64,
}

fn secs(t: std::io::Result<std::time::SystemTime>) -> i64 {
    t.ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64).unwrap_or(0)
}

#[cfg(windows)]
fn attrs(md: &std::fs::Metadata) -> u32 {
    use std::os::windows::fs::MetadataExt;
    md.file_attributes()
}
#[cfg(not(windows))]
fn attrs(_md: &std::fs::Metadata) -> u32 {
    0
}

fn is_gone(e: &std::io::Error) -> bool {
    // NotFound, or ERROR_DIRECTORY (267): the path is now something that is not a folder.
    e.kind() == std::io::ErrorKind::NotFound || e.raw_os_error() == Some(267)
}

fn ext_lower(name: &str) -> String {
    Path::new(name).extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase()
}

/// The entries of `path` that the index wants, with the skip rules applied.
fn list_disk(path: &Path, ctx: &Ctx, tier: i64) -> std::io::Result<Vec<Disk>> {
    let mut out = Vec::new();
    let here = path.to_string_lossy().to_lowercase();
    let under = |p: &String| here == *p || here.starts_with(&format!("{p}\\"));
    let documents_here = ctx.settings.index_documents && ctx.doc_dirs.iter().any(under) && !ctx.doc_skip.iter().any(under);
    for entry in std::fs::read_dir(long_path(path))?.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        // Symlinks and junctions are not followed (no loops, no double counting). Cloud placeholders are not symlinks.
        if ft.is_symlink() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_string) else { continue };
        let Ok(md) = entry.metadata() else { continue };
        let a = attrs(&md);
        if a & (rules::ATTR_HIDDEN | rules::ATTR_SYSTEM) != 0 {
            continue;
        }
        let lower = name.to_lowercase();
        let is_dir = ft.is_dir();
        if is_dir {
            if rules::excluded_dir(&lower) || rules::sensitive_dir(&lower) {
                continue;
            }
            let child = path.join(&name).to_string_lossy().to_lowercase();
            if ctx.root_paths.contains(&child) || ctx.blocked_paths.contains(&child) || rules::under_excluded_path(&child, ctx.settings) {
                continue;
            }
            out.push(Disk { key: lower, name, is_dir: true, kind: FOLDER, size: 0, mtime: secs(md.modified()), flags: rules::cloud_flag(a), dir_mtime: secs(md.modified()) });
        } else if ft.is_file() {
            let ext = ext_lower(&name);
            let kind = rules::kind_from_str(crate::local_files::classify(Path::new(&name), false));
            if kind == FILE {
                // Ordinary files only when asked for; documents (by type) in the user's own places and on other drives.
                let all_files = ctx.settings.index_files && tier < 3;
                if !all_files && !(documents_here && rules::is_document_ext(&ext)) {
                    continue;
                }
            }
            if rules::temp_file(&lower, &ext) || rules::sensitive_file(&lower, &ext) {
                continue;
            }
            out.push(Disk { key: lower, name, is_dir: false, kind, size: md.len() as i64, mtime: secs(md.modified()), flags: rules::cloud_flag(a), dir_mtime: 0 });
        }
    }
    Ok(out)
}

pub fn reconcile_dir(db: &mut Db, ctx: &Ctx, dir_id: i64) -> Result<Outcome> {
    let Some(dir) = db.entry(dir_id) else { return Ok(Outcome::Gone) };
    if dir.kind != FOLDER {
        return Ok(Outcome::Gone);
    }
    let Some(path) = db.path_of(dir_id, ctx.vols) else { return Ok(Outcome::Offline) };
    let (tier, max_depth) = ctx.roots.get(&dir.root_id).copied().unwrap_or((3, 6));

    let disk = match list_disk(&path, ctx, tier) {
        Ok(d) => d,
        Err(e) if is_gone(&e) => {
            db.begin()?;
            let r = (|| -> Result<()> {
                if dir.parent_id.is_none() {
                    // A root that is gone: keep the root, drop what was below it.
                    for c in db.children(dir_id) {
                        db.delete_subtree(c.id)?;
                    }
                    db.set_root_status(dir_id, "missing")?;
                } else {
                    db.delete_subtree(dir_id)?;
                }
                Ok(())
            })();
            return match r {
                Ok(()) => {
                    db.commit()?;
                    Ok(Outcome::Gone)
                }
                Err(e) => {
                    db.rollback();
                    Err(e)
                }
            };
        }
        // Access denied, device not ready, network timeout: never delete on an error we do not understand.
        Err(e) => return Ok(Outcome::Unreadable(e.to_string())),
    };
    let own_mtime = secs(std::fs::metadata(long_path(&path)).and_then(|m| m.modified()));

    db.begin()?;
    let result = apply(db, ctx, &dir, disk, tier, max_depth, own_mtime);
    match result {
        Ok(o) => {
            db.commit()?;
            Ok(o)
        }
        Err(e) => {
            db.rollback();
            Err(e)
        }
    }
}

fn apply(db: &mut Db, ctx: &Ctx, dir: &Row, disk: Vec<Disk>, tier: i64, max_depth: i64, own_mtime: i64) -> Result<Outcome> {
    if dir.parent_id.is_none() {
        db.set_root_status(dir.id, "ok")?;
    }
    let stored = db.children(dir.id);
    let mut by_key: HashMap<String, Vec<&Row>> = HashMap::new();
    for r in &stored {
        by_key.entry(r.name.to_lowercase()).or_default().push(r);
    }
    let (mut added, mut changed) = (0usize, 0usize);
    let mut matched: HashSet<i64> = HashSet::new();
    let mut fresh: Vec<Disk> = Vec::new();
    let child_depth = dir.depth + 1;
    for d in disk {
        let found = by_key.get(&d.key).and_then(|list| list.iter().find(|r| r.name == d.name).or(list.first()).copied());
        match found {
            // A file that became a folder (or the other way round) is a different thing: delete and create.
            Some(r) if (r.kind == FOLDER) == d.is_dir && !matched.contains(&r.id) => {
                matched.insert(r.id);
                if r.name != d.name {
                    db.rename_entry(r.id, &d.name, d.is_dir)?;
                    changed += 1;
                }
                if r.kind != d.kind || r.size != d.size || r.mtime != d.mtime || r.flags != d.flags {
                    db.update_entry(r.id, d.kind, d.size, d.mtime, d.flags, None)?;
                    changed += 1;
                }
                // A folder whose own time differs from when it was last listed has changed inside: list it again.
                if d.is_dir && r.dir_mtime != d.dir_mtime && child_depth < max_depth {
                    db.todo_push(r.id, 4)?;
                }
            }
            _ => fresh.push(d),
        }
    }
    let mut missing: Vec<&Row> = stored.iter().filter(|r| !matched.contains(&r.id)).collect();

    // A rename in this folder: one missing row and one new entry that match unambiguously keep the row (and, for a
    // folder, everything below it). Anything vaguer is treated as a delete plus a create.
    let mut renamed_new: HashSet<usize> = HashSet::new();
    let mut renamed_old: HashSet<i64> = HashSet::new();
    for m in &missing {
        let cands: Vec<usize> = fresh
            .iter()
            .enumerate()
            .filter(|(_, d)| match m.kind {
                FOLDER => d.is_dir && m.dir_mtime > 0 && d.dir_mtime == m.dir_mtime,
                EXE | INSTALLER | SHORTCUT => !d.is_dir && d.kind == m.kind && m.size > 0 && d.size == m.size && d.mtime == m.mtime && ext_lower(&d.name) == ext_lower(&m.name),
                _ => false,
            })
            .map(|(i, _)| i)
            .collect();
        if cands.len() != 1 || renamed_new.contains(&cands[0]) {
            continue;
        }
        // The same new entry must not also fit another missing row.
        let rivals = missing
            .iter()
            .filter(|o| o.id != m.id && o.kind == m.kind && if m.kind == FOLDER { o.dir_mtime == m.dir_mtime } else { o.size == m.size && o.mtime == m.mtime })
            .count();
        if rivals > 0 {
            continue;
        }
        let d = &fresh[cands[0]];
        db.rename_entry(m.id, &d.name, d.is_dir)?;
        db.update_entry(m.id, d.kind, d.size, d.mtime, d.flags, None)?;
        if d.is_dir && child_depth < max_depth {
            db.todo_push(m.id, 4)?;
        }
        renamed_new.insert(cands[0]);
        renamed_old.insert(m.id);
        changed += 1;
    }
    missing.retain(|m| !renamed_old.contains(&m.id));

    let removed = missing.len();
    for m in missing {
        db.delete_subtree(m.id)?;
    }
    for (i, d) in fresh.iter().enumerate() {
        if renamed_new.contains(&i) || (ctx.over_cap && tier >= 3) {
            continue;
        }
        let id = db.insert_entry(dir, &d.name, d.kind, d.size, d.mtime, d.flags, 0)?;
        added += 1;
        if d.is_dir && child_depth < max_depth {
            db.todo_push(id, tier)?;
        }
    }
    db.set_dir_state(dir.id, own_mtime, ctx.now)?;
    Ok(Outcome::Listed { added, removed, changed })
}

// ---------------------------------------------------------------------------------------------------------------------
// Which folders are roots

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Desired {
    pub path: PathBuf,
    pub tier: i64,
    pub max_depth: i64,
    pub user_added: bool,
}

fn env_path(var: &str) -> Option<PathBuf> {
    std::env::var_os(var).map(PathBuf::from).filter(|p| p.is_dir())
}

/// The folders this machine's index should cover, in tiers. Pure given the environment and the volume list.
pub fn desired_roots(settings: &Settings, vols: &VolumeMap) -> Vec<Desired> {
    let mut out: Vec<Desired> = Vec::new();
    let mut add = |p: Option<PathBuf>, tier: i64, max_depth: i64, user: bool| {
        let Some(p) = p else { return };
        if !super::platform::is_local_drive_path(&p.to_string_lossy()) || !p.is_dir() {
            return;
        }
        let key = p.to_string_lossy().trim_end_matches('\\').to_lowercase();
        if out.iter().any(|d| d.path.to_string_lossy().trim_end_matches('\\').to_lowercase() == key) {
            return;
        }
        out.push(Desired { path: p, tier, max_depth, user_added: user });
    };
    let home = env_path("USERPROFILE");
    add(env_path("APPDATA").map(|a| a.join(r"Microsoft\Windows\Start Menu\Programs")), 1, 5, false);
    add(env_path("ProgramData").map(|a| a.join(r"Microsoft\Windows\Start Menu\Programs")), 1, 5, false);
    for sub in ["Desktop", "Downloads", "Documents", "Pictures", "Videos", "Music"] {
        add(home.as_ref().map(|h| h.join(sub)), 1, 6, false);
    }
    add(env_path("PUBLIC").map(|p| p.join("Desktop")), 1, 3, false);
    add(env_path("LOCALAPPDATA").map(|a| a.join("Programs")), 1, 4, false);
    add(env_path("ProgramFiles"), 1, 4, false);
    add(env_path("ProgramFiles(x86)"), 1, 4, false);
    add(home, 2, 6, false);
    if settings.other_drives {
        for v in vols.list.iter().filter(|v| v.fixed) {
            add(v.mount.clone(), 3, 6, false);
        }
    }
    for e in &settings.extra_roots {
        if super::platform::is_local_drive_path(e) {
            add(Some(PathBuf::from(e.trim_end_matches(['\\', '/']))), 2, 8, true);
        }
    }
    out
}

/// Folders that are never entered, whatever root reaches them: the folder that holds every user's profile (only the
/// current user's own profile is a root).
pub fn blocked_paths() -> HashSet<String> {
    let mut s = HashSet::new();
    if let Some(h) = env_path("USERPROFILE") {
        if let Some(parent) = h.parent() {
            s.insert(parent.to_string_lossy().to_lowercase());
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct Rig {
        dir: PathBuf,
        db: Db,
        vols: VolumeMap,
        settings: Settings,
        root: i64,
        roots: HashMap<i64, (i64, i64)>,
        root_paths: HashSet<String>,
        blocked: HashSet<String>,
        clock: i64,
        doc_dirs: Vec<String>,
    }

    fn rig(tag: &str) -> Rig {
        let dir = std::env::temp_dir().join(format!("aqua_idx_{tag}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("tree")).unwrap();
        let mut db = Db::open(&dir.join("idx.db")).unwrap();
        let vols = VolumeMap::single("test-vol", &dir.join("tree"));
        let vid = db.volume_id("test-vol").unwrap();
        // The root is the mount itself (empty relative path).
        let root = db.add_root(vid, "", 1, 6, false).unwrap();
        let mut roots = HashMap::new();
        roots.insert(root, (1, 6));
        let doc_dirs = vec![dir.join("tree").to_string_lossy().to_lowercase()];
        Rig { dir, db, vols, settings: Settings::default(), root, roots, root_paths: HashSet::new(), blocked: HashSet::new(), clock: 1000, doc_dirs }
    }

    impl Rig {
        fn tree(&self) -> PathBuf {
            self.dir.join("tree")
        }
        fn run(&mut self) -> Vec<Outcome> {
            self.clock += 10;
            let mut outs = Vec::new();
            for _ in 0..10_000 {
                let Some(id) = self.db.todo_pop() else { break };
                let ctx = Ctx { settings: &self.settings, vols: &self.vols, root_paths: &self.root_paths, blocked_paths: &self.blocked, roots: &self.roots, now: self.clock, doc_dirs: &self.doc_dirs, doc_skip: &[], over_cap: false };
                outs.push(reconcile_dir(&mut self.db, &ctx, id).unwrap());
            }
            outs
        }
        /// Every (relative path, kind) in the database, built from the parent chain.
        fn db_paths(&mut self) -> Vec<String> {
            let ids: Vec<i64> = {
                let mut st = self.db.conn.prepare("SELECT id FROM entries WHERE parent_id IS NOT NULL").unwrap();
                st.query_map([], |r| r.get(0)).unwrap().flatten().collect()
            };
            let tree = self.tree().to_string_lossy().to_string();
            let mut v: Vec<String> = ids
                .into_iter()
                .filter_map(|id| self.db.path_of(id, &self.vols))
                .map(|p| p.to_string_lossy().strip_prefix(&tree).unwrap().trim_start_matches('\\').to_lowercase())
                .collect();
            v.sort();
            v
        }
    }

    fn touch(p: &Path, bytes: usize) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, vec![b'x'; bytes]).unwrap();
    }

    #[test]
    fn a_scan_indexes_folders_programs_and_shortcuts_and_skips_the_rest() {
        let mut r = rig("scan");
        r.settings.index_documents = false; // this test is about folders, programs and shortcuts only
        let t = r.tree();
        touch(&t.join("Games/Need For Speed/nfs.exe"), 10);
        touch(&t.join("Games/Need For Speed/readme.txt"), 5);
        touch(&t.join("Downloads/nfs_setup.exe"), 9);
        touch(&t.join("Start/Need for Speed.lnk"), 3);
        touch(&t.join("node_modules/x/y.exe"), 3);
        touch(&t.join(".ssh/id_rsa"), 3);
        touch(&t.join("Games/a.crdownload"), 3);
        r.run();
        let p = r.db_paths();
        assert!(p.contains(&"games\\need for speed\\nfs.exe".to_string()), "{p:?}");
        assert!(p.contains(&"downloads\\nfs_setup.exe".to_string()) && p.contains(&"start\\need for speed.lnk".to_string()));
        assert!(p.contains(&"games\\need for speed".to_string()));
        assert!(!p.iter().any(|x| x.contains("readme") || x.contains("node_modules") || x.contains(".ssh") || x.contains("crdownload")), "{p:?}");
        let (folders, programs, files) = r.db.counts();
        // the root folder itself is a row too
        assert_eq!((folders, programs, files), (5, 3, 0), "{p:?}");
    }

    #[test]
    fn documents_are_indexed_by_type_in_the_user_places_but_not_photos_or_program_files() {
        let mut r = rig("docs");
        let t = r.tree();
        touch(&t.join("Documents/Vishal CV.pdf"), 20);
        touch(&t.join("Documents/Budget 2024.xlsx"), 20);
        touch(&t.join("Documents/Holiday.jpg"), 20);
        touch(&t.join("Documents/passwords.docx"), 20);
        touch(&t.join("Documents/Work/report final.docx"), 20);
        touch(&t.join("Program Files/App/license.pdf"), 20);
        r.run();
        let before = r.db_paths();
        assert!(before.contains(&"documents\\vishal cv.pdf".to_string()) && before.contains(&"documents\\work\\report final.docx".to_string()), "{before:?}");
        assert!(before.contains(&"program files\\app\\license.pdf".to_string()), "the rig lets everything under the tree count: {before:?}");
        assert!(!before.iter().any(|x| x.contains("holiday") || x.contains("passwords")), "photos and credential-looking names are not stored: {before:?}");
        // Program Files is a skip folder: the same file is not stored there.
        let mut r2 = rig("docs2");
        let t2 = r2.tree();
        touch(&t2.join("Program Files/App/license.pdf"), 20);
        let skip = vec![t2.join("Program Files").to_string_lossy().to_lowercase()];
        r2.db.todo_push(r2.root, 0).unwrap();
        while let Some(id) = r2.db.todo_pop() {
            let ctx = Ctx { settings: &r2.settings, vols: &r2.vols, root_paths: &r2.root_paths, blocked_paths: &r2.blocked, roots: &r2.roots, now: 5, doc_dirs: &r2.doc_dirs, doc_skip: &skip, over_cap: false };
            reconcile_dir(&mut r2.db, &ctx, id).unwrap();
        }
        assert!(!r2.db_paths().iter().any(|x| x.ends_with("license.pdf")), "{:?}", r2.db_paths());
        // Switched off: only folders and programs.
        let mut r3 = rig("docs3");
        r3.settings.index_documents = false;
        touch(&r3.tree().join("Documents/a.pdf"), 20);
        r3.run();
        assert_eq!(r3.db_paths(), vec!["documents"]);
    }

    #[test]
    fn create_delete_and_rename_are_followed_and_a_file_rename_keeps_its_row() {
        let mut r = rig("changes");
        let t = r.tree();
        touch(&t.join("A/app.exe"), 100);
        r.run();
        let id_before: i64 = r.db.conn.query_row("SELECT id FROM entries WHERE name='app.exe'", [], |x| x.get(0)).unwrap();
        r.db.mark_opened(id_before, 5).unwrap();
        // rename the file inside the same folder
        fs::rename(t.join("A/app.exe"), t.join("A/renamed.exe")).unwrap();
        let a: i64 = r.db.conn.query_row("SELECT id FROM entries WHERE name='A'", [], |x| x.get(0)).unwrap();
        r.db.todo_push(a, 0).unwrap();
        r.run();
        let (id_after, opened): (i64, i64) = r.db.conn.query_row("SELECT id, open_count FROM entries WHERE name='renamed.exe'", [], |x| Ok((x.get(0)?, x.get(1)?))).unwrap();
        assert_eq!((id_after, opened), (id_before, 1), "the row and its usage history survive a rename");
        // create and delete
        touch(&t.join("A/new.exe"), 7);
        fs::remove_file(t.join("A/renamed.exe")).unwrap();
        r.db.todo_push(a, 0).unwrap();
        r.run();
        assert_eq!(r.db_paths(), vec!["a", "a\\new.exe"]);
    }

    #[test]
    fn an_unrelated_new_file_is_not_mistaken_for_a_rename() {
        let mut r = rig("norename");
        let t = r.tree();
        touch(&t.join("A/old.exe"), 100);
        r.run();
        let id_old: i64 = r.db.conn.query_row("SELECT id FROM entries WHERE name='old.exe'", [], |x| x.get(0)).unwrap();
        fs::remove_file(t.join("A/old.exe")).unwrap();
        touch(&t.join("A/other.exe"), 55); // different size
        let a: i64 = r.db.conn.query_row("SELECT id FROM entries WHERE name='A'", [], |x| x.get(0)).unwrap();
        r.db.todo_push(a, 0).unwrap();
        r.run();
        let id_new: i64 = r.db.conn.query_row("SELECT id FROM entries WHERE name='other.exe'", [], |x| x.get(0)).unwrap();
        assert_ne!(id_old, id_new, "ids are never reused");
        assert!(r.db.entry(id_old).is_none());
        assert_eq!(r.db_paths(), vec!["a", "a\\other.exe"]);
    }

    #[test]
    fn a_folder_renamed_in_place_keeps_its_children_and_a_deleted_folder_takes_its_subtree() {
        let mut r = rig("folders");
        let t = r.tree();
        touch(&t.join("Proj/src/main.exe"), 40);
        touch(&t.join("Proj/Docs/x.lnk"), 4);
        r.run();
        let main_id: i64 = r.db.conn.query_row("SELECT id FROM entries WHERE name='main.exe'", [], |x| x.get(0)).unwrap();
        fs::rename(t.join("Proj"), t.join("Project2")).unwrap();
        r.db.todo_push(r.root, 0).unwrap();
        r.run();
        assert_eq!(r.db.entry(main_id).map(|e| e.name), Some("main.exe".to_string()), "children survive a folder rename");
        assert_eq!(r.db_paths(), vec!["project2", "project2\\docs", "project2\\docs\\x.lnk", "project2\\src", "project2\\src\\main.exe"]);
        fs::remove_dir_all(t.join("Project2")).unwrap();
        r.db.todo_push(r.root, 0).unwrap();
        r.run();
        assert!(r.db_paths().is_empty() && r.db.entry(main_id).is_none());
        let orphans: i64 = r.db.conn.query_row("SELECT COUNT(*) FROM entries_fts", [], |x| x.get(0)).unwrap();
        assert_eq!(orphans, 0, "the search table is emptied with the rows");
    }

    #[test]
    fn a_folder_that_is_itself_a_root_is_skipped_by_the_folder_above() {
        let mut r = rig("nested");
        let t = r.tree();
        touch(&t.join("Users/me/Documents/a.lnk"), 4);
        r.root_paths.insert(t.join("Users").join("me").join("Documents").to_string_lossy().to_lowercase());
        r.run();
        let p = r.db_paths();
        assert!(p.contains(&"users\\me".to_string()) && !p.iter().any(|x| x.contains("documents")), "{p:?}");
    }

    #[test]
    fn a_folder_that_cannot_be_read_is_never_deleted_but_a_missing_one_is() {
        let mut r = rig("gone");
        let t = r.tree();
        touch(&t.join("Keep/k.exe"), 5);
        r.run();
        assert_eq!(r.db_paths().len(), 2);
        // The volume goes away: nothing is touched.
        r.vols = VolumeMap { list: vec![super::super::platform::VolInfo { guid: "test-vol".into(), mount: None, fixed: true }] };
        r.db.todo_push(r.root, 0).unwrap();
        let outs = r.run();
        assert!(outs.contains(&Outcome::Offline));
        assert_eq!(r.db.counts().0, 2, "rows are kept while a drive is offline (root + Keep)");
        r.vols = VolumeMap::single("test-vol", &t);
        fs::remove_dir_all(t.join("Keep")).unwrap();
        r.db.todo_push(r.root, 0).unwrap();
        r.run();
        assert!(r.db_paths().is_empty());
    }

    #[test]
    fn a_random_series_of_changes_always_ends_with_the_index_equal_to_the_disk() {
        // A small deterministic generator: no extra crate, the same sequence every run.
        let mut seed: u64 = 0x9E3779B97F4A7C15;
        let mut next = move |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        let mut r = rig("random");
        let t = r.tree();
        let dirs = ["a", "b", "c", "a/x", "b/y", "c/z", "a/x/deep"];
        for round in 0..60 {
            for _ in 0..4 {
                let d = dirs[next(dirs.len() as u64) as usize];
                let f = format!("{}/p{}.exe", d, next(6));
                match next(5) {
                    0 | 1 => touch(&t.join(&f), (next(500) + 1) as usize),
                    2 => {
                        let _ = fs::remove_file(t.join(&f));
                    }
                    3 => {
                        let to = format!("{}/p{}.exe", dirs[next(dirs.len() as u64) as usize], next(6));
                        if t.join(&f).exists() && t.join(dirs[0]).is_dir() && t.join(to.rsplit_once('/').unwrap().0).is_dir() {
                            let _ = fs::rename(t.join(&f), t.join(&to));
                        }
                    }
                    _ => {
                        let _ = fs::remove_dir_all(t.join(d));
                    }
                }
            }
            // Reconcile every folder, as the weekly verify would.
            r.db.todo_push(r.root, 0).unwrap();
            let all: Vec<i64> = {
                let mut st = r.db.conn.prepare("SELECT id FROM entries WHERE kind=1").unwrap();
                st.query_map([], |x| x.get(0)).unwrap().flatten().collect()
            };
            for id in all {
                r.db.todo_push(id, 1).unwrap();
            }
            r.run();
            // and again, so folders discovered in the first pass are listed
            r.run();
            let mut disk: Vec<String> = Vec::new();
            fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) {
                for e in fs::read_dir(dir).unwrap().flatten() {
                    let p = e.path();
                    let rel = p.strip_prefix(base).unwrap().to_string_lossy().to_lowercase();
                    if e.file_type().unwrap().is_dir() {
                        out.push(rel);
                        walk(base, &p, out);
                    } else if rel.ends_with(".exe") {
                        out.push(rel);
                    }
                }
            }
            walk(&t, &t, &mut disk);
            disk.sort();
            assert_eq!(r.db_paths(), disk, "round {round}");
        }
    }
}
