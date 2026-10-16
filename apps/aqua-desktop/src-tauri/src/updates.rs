//! Updating the app, with the person's say-so at every step.
//!
//! Nothing here installs anything on its own. The page asks `check_for_update` (a short request to the update address, a small
//! JSON file); if a newer version exists the page shows a prompt, and only when the person clicks "Install and restart" does
//! `install_update` download the installer, check its signature against the public key built into the app, stop Aqua's core
//! and everything it started, and run the installer. An installer whose signature does not match is thrown away.
//!
//! Where updates come from is configuration, not code: `AQUA_UPDATE_URL`, or `<data folder>\update.json`
//! (`{"endpoint": "https://example.com/aqua/latest.json"}`). Without either, updates are simply not set up and the page says so.
//!
//! Before an update is offered, and again before it is installed, `update_policy` judges the update information: it must not
//! have expired, must not be older than the highest version this installation has accepted (`<data folder>\update_state.json`),
//! must be newer than what is installed, and the installed version must be recent enough to update by itself. After the
//! download, the version the installer was signed as must be the version announced. A refusal is a sentence the page can show.

use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Manager, Url};
use tauri_plugin_updater::{Update, UpdaterExt};

use crate::{stop_core, Core, Quitting};

#[path = "update_policy.rs"]
mod update_policy;
use update_policy::{Context, Manifest, Verdict};

static DOWNLOADED: AtomicU64 = AtomicU64::new(0);
static TOTAL: AtomicU64 = AtomicU64::new(0);
static INSTALLING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// What the last check refused, as a sentence for the page (nothing when the last check was fine or only found nothing newer).
static LAST_REFUSAL: Mutex<Option<String>> = Mutex::new(None);

fn note_refusal(sentence: Option<String>) {
    *LAST_REFUSAL.lock().unwrap_or_else(|p| p.into_inner()) = sentence;
}

/// The update address from the environment value and/or the text of `update.json`. The environment wins. Only `https` is
/// accepted (plain `http` only for this computer, which is how a new release is tried out before it is published), and an
/// address with a user name or password in it is refused: a secret does not belong in a URL that is logged and sent along.
pub fn endpoint_from(env_value: Option<&str>, file_text: Option<&str>) -> Result<Option<Url>, String> {
    let raw = match env_value.map(str::trim).filter(|v| !v.is_empty()) {
        Some(v) => Some(v.to_string()),
        None => match file_text {
            Some(text) => {
                let v: serde_json::Value = serde_json::from_str(text).map_err(|e| format!("update.json is not valid JSON: {e}"))?;
                v.get("endpoint").and_then(|e| e.as_str()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
            }
            None => None,
        },
    };
    let Some(raw) = raw else { return Ok(None) };
    let url = Url::parse(&raw).map_err(|e| format!("the update address is not a valid URL: {e}"))?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err("the update address must not contain a user name or password".into());
    }
    let local = update_policy::host_is_this_computer(url.host_str());
    match url.scheme() {
        "https" => {}
        "http" if local => {}
        _ => return Err("the update address must be https (plain http is accepted only for this computer)".into()),
    }
    Ok(Some(url))
}

fn configured_endpoint() -> Result<Option<Url>, String> {
    let file = crate::workspace_root().map(|r| r.join("data").join("update.json")).and_then(|p| std::fs::read_to_string(p).ok());
    endpoint_from(std::env::var("AQUA_UPDATE_URL").ok().as_deref(), file.as_deref())
}

#[derive(Serialize)]
pub struct UpdateStatus {
    /// An update address is set up.
    pub configured: bool,
    /// The host the app asks (not the whole address), for display.
    pub host: Option<String>,
    pub current: String,
    /// What is wrong with the configuration, if anything.
    pub problem: Option<String>,
    /// Why the last check did not use the update information (expired, an older version put back, ...), if it did not.
    pub refusal: Option<String>,
}

#[derive(Serialize)]
pub struct UpdateInfo {
    pub available: bool,
    pub current: String,
    pub version: Option<String>,
    /// The release notes the publisher wrote, as plain text (the page shows them as text, never as markup).
    pub notes: Option<String>,
    pub date: Option<String>,
    /// Why the update information was not used, as a plain sentence. Not set when there is simply nothing newer.
    pub refusal: Option<String>,
}

fn no_update(current: String, refusal: Option<String>) -> UpdateInfo {
    UpdateInfo { available: false, current, version: None, notes: None, date: None, refusal }
}

#[derive(Serialize)]
pub struct UpdateProgress {
    pub downloaded: u64,
    pub total: u64,
    pub installing: bool,
}

fn current_version(app: &AppHandle) -> String {
    app.package_info().version.to_string()
}

#[tauri::command]
pub fn update_status(app: AppHandle) -> UpdateStatus {
    let current = current_version(&app);
    let refusal = LAST_REFUSAL.lock().unwrap_or_else(|p| p.into_inner()).clone();
    match configured_endpoint() {
        Ok(Some(url)) => UpdateStatus { configured: true, host: url.host_str().map(String::from), current, problem: None, refusal },
        Ok(None) => UpdateStatus { configured: false, host: None, current, problem: None, refusal: None },
        Err(e) => UpdateStatus { configured: false, host: None, current, problem: Some(e), refusal: None },
    }
}

/// A field of the update information as text. A number or a list is returned as its JSON text, which no date or version
/// reads as, so it is refused by the policy and not mistaken for "missing". `null` and an absent field are missing.
fn manifest_text(json: &serde_json::Value, key: &str) -> Option<String> {
    match json.get(key)? {
        serde_json::Value::Null => None,
        serde_json::Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

/// The update information as the policy judged it.
struct Judged {
    update: Update,
    verdict: Verdict,
    /// The highest version accepted before, as read from `update_state.json` (the state to put back if an install fails).
    saved: Option<String>,
}

/// The highest version this installation has accepted. `Ok(None)` before the first accepted update; a file that is there
/// and cannot be read is an error, not "nothing".
fn read_saved() -> Result<Option<String>, update_policy::Reason> {
    let Some(path) = crate::data_dir().map(|d| d.join("update_state.json")) else { return Ok(None) };
    match std::fs::read_to_string(&path) {
        Ok(text) => update_policy::read_state(&text).map(Some),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(update_policy::Reason::StateDamaged),
    }
}

/// Record the version that is about to be installed. Written to a side file and renamed, so a crash leaves the old record whole.
fn write_saved(version: &str) -> Result<(), String> {
    let path = crate::data_dir().map(|d| d.join("update_state.json")).ok_or("there is no data folder to keep the record of the accepted update in")?;
    let text = update_policy::state_text(version).ok_or("the version of the update cannot be recorded")?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("the record of the accepted update could not be saved ({e}), so nothing was installed"))?;
    }
    let side = path.with_extension("json.tmp");
    std::fs::write(&side, text)
        .and_then(|_| std::fs::rename(&side, &path))
        .map_err(|e| format!("the record of the accepted update could not be saved ({e}), so nothing was installed"))
}

/// Put the record back as it was when the installer did not start.
fn restore_saved(previous: Option<String>) {
    match previous {
        Some(version) => {
            let _ = write_saved(&version);
        }
        None => {
            if let Some(path) = crate::data_dir().map(|d| d.join("update_state.json")) {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

/// Ask the update address and let the policy judge what comes back. `Ok(None)`: the address has nothing to offer.
async fn judge(app: &AppHandle) -> Result<Option<Judged>, String> {
    let url = configured_endpoint()?.ok_or("updates are not set up (no update address is configured)")?;
    let local = update_policy::host_is_this_computer(url.host_str());
    // The plugin would hide a release that is not newer, and with it the expiry of a stale file. The policy decides here, so
    // the plugin is asked to hand every release over.
    let updater = app
        .updater_builder()
        .version_comparator(|_, _| true)
        .endpoints(vec![url])
        .map_err(|e| e.to_string())?
        .build()
        .map_err(|e| e.to_string())?;
    let Some(update) = updater.check().await.map_err(|e| format!("could not check for updates: {e}"))? else { return Ok(None) };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .map_err(|_| "this computer's clock is set before 1970, so Aqua cannot tell whether the update information is fresh".to_string())?;
    let (expires, min_version, pub_date) = (manifest_text(&update.raw_json, "expires"), manifest_text(&update.raw_json, "min_version"), manifest_text(&update.raw_json, "pub_date"));
    let (saved, verdict) = match read_saved() {
        Err(reason) => (None, Verdict::Refuse(reason)),
        Ok(saved) => {
            let manifest = Manifest { version: &update.version, expires: expires.as_deref(), min_version: min_version.as_deref(), pub_date: pub_date.as_deref() };
            let context = Context { installed: &update.current_version, highest_accepted: saved.as_deref(), endpoint_is_local: local, now };
            let verdict = update_policy::decide(&manifest, &context);
            (saved, verdict)
        }
    };
    Ok(Some(Judged { update, verdict, saved }))
}

/// Ask the update address whether a newer version exists. Downloads nothing and changes nothing.
#[tauri::command]
pub async fn check_for_update(app: AppHandle) -> Result<UpdateInfo, String> {
    let current = current_version(&app);
    let Some(judged) = judge(&app).await? else {
        note_refusal(None);
        return Ok(no_update(current, None));
    };
    match judged.verdict {
        Verdict::Accept => {
            note_refusal(None);
            let update = judged.update;
            Ok(UpdateInfo {
                available: true,
                current,
                version: Some(update.version.clone()),
                notes: update.body.clone().map(|b| b.chars().take(4000).collect()),
                date: update.date.map(|d| d.to_string()),
                refusal: None,
            })
        }
        Verdict::Refuse(reason) => {
            let warning = reason.is_warning().then(|| reason.sentence());
            note_refusal(warning.clone());
            Ok(no_update(current, warning))
        }
    }
}

#[tauri::command]
pub fn update_progress() -> UpdateProgress {
    UpdateProgress { downloaded: DOWNLOADED.load(Ordering::SeqCst), total: TOTAL.load(Ordering::SeqCst), installing: INSTALLING.load(Ordering::SeqCst) }
}

/// Download, verify and install the update the person agreed to. The order matters: the installer is downloaded and its
/// signature checked while Aqua keeps running, and only a verified installer causes the core to be stopped. If the
/// download or the check fails, nothing has been touched and Aqua carries on as it was.
#[tauri::command]
pub async fn install_update(app: AppHandle) -> Result<(), String> {
    if INSTALLING.swap(true, Ordering::SeqCst) {
        return Err("an update is already being installed".into());
    }
    let result = install_inner(&app).await;
    if result.is_err() {
        INSTALLING.store(false, Ordering::SeqCst);
    }
    result
}

async fn install_inner(app: &AppHandle) -> Result<(), String> {
    DOWNLOADED.store(0, Ordering::SeqCst);
    TOTAL.store(0, Ordering::SeqCst);
    // The same judgement as the page's check: this command can be called without it, and the information may have changed.
    let judged = judge(app).await?.ok_or("there is no newer version to install")?;
    if let Verdict::Refuse(reason) = judged.verdict {
        return Err(reason.sentence());
    }
    let (update, previous_record) = (judged.update, judged.saved);
    // Download (the plugin checks the signature of what it downloaded before it returns it).
    let bytes = update
        .download(
            |chunk, total| {
                DOWNLOADED.fetch_add(chunk as u64, Ordering::SeqCst);
                if let Some(t) = total {
                    TOTAL.store(t, Ordering::SeqCst);
                }
            },
            || {},
        )
        .await
        .map_err(|e| format!("the update could not be downloaded or its signature did not match, so it was not installed: {e}"))?;
    // The signature has been checked, so its signed comment can be believed: the installer must be the version that was announced.
    update_policy::check_signed_release(update_policy::trusted_comment(&update.signature).as_deref(), &update.version)
        .map_err(|reason| format!("{} Nothing was installed.", reason.sentence()))?;
    // Record the version before the installer starts: on Windows it ends this process, so nothing runs after it. Put back below if it did not start.
    write_saved(&update.version)?;
    // Verified. Now stop the core and what it started, so no file is in use when the installer replaces it.
    app.state::<Quitting>().0.store(true, Ordering::SeqCst);
    if let Some(child) = app.state::<Core>().0.lock().unwrap_or_else(|p| p.into_inner()).take() {
        stop_core(child);
    }
    update.install(bytes).map_err(|e| {
        // The installer did not run: bring the core back so the app still works, and forget the version that was not installed.
        restore_saved(previous_record);
        app.state::<Quitting>().0.store(false, Ordering::SeqCst);
        crate::start_core(app.clone(), 0);
        format!("the update could not be installed: {e}")
    })?;
    // On Windows the installer takes over and ends this process; elsewhere the app restarts itself.
    app.restart();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(env: Option<&str>, file: Option<&str>) -> Option<String> {
        endpoint_from(env, file).unwrap().map(|u| u.to_string())
    }

    #[test]
    fn no_address_means_updates_are_not_set_up() {
        assert_eq!(ok(None, None), None);
        assert_eq!(ok(Some("  "), None), None);
        assert_eq!(ok(None, Some(r#"{"other": 1}"#)), None);
        assert_eq!(ok(None, Some(r#"{"endpoint": ""}"#)), None);
    }

    #[test]
    fn the_environment_wins_over_the_file_and_both_are_read() {
        assert_eq!(ok(Some("https://updates.example.com/latest.json"), Some(r#"{"endpoint": "https://other.example/x.json"}"#)).unwrap(), "https://updates.example.com/latest.json");
        assert_eq!(ok(None, Some(r#"{"endpoint": " https://other.example/x.json "}"#)).unwrap(), "https://other.example/x.json");
    }

    #[test]
    fn only_https_is_accepted_and_plain_http_only_for_this_computer() {
        for good in ["https://example.com/a.json", "http://127.0.0.1:9000/latest.json", "http://localhost/latest.json"] {
            assert!(endpoint_from(Some(good), None).is_ok(), "{good}");
        }
        for bad in ["http://example.com/a.json", "ftp://example.com/a", "file:///C:/x.json", "javascript:alert(1)", "example.com/a.json", "http://127.0.0.1.evil.com/a.json", "http://localhost.evil.com/a.json"] {
            assert!(endpoint_from(Some(bad), None).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_field_of_the_update_information_is_text_and_only_an_absent_or_null_field_is_missing() {
        let json: serde_json::Value = serde_json::from_str(r#"{"expires": "2026-11-07T00:00:00Z", "min_version": null, "pub_date": 1790000000, "list": ["a"]}"#).unwrap();
        assert_eq!(manifest_text(&json, "expires").as_deref(), Some("2026-11-07T00:00:00Z"));
        assert_eq!(manifest_text(&json, "min_version"), None);
        assert_eq!(manifest_text(&json, "absent"), None);
        // A number is passed on as its text, which the policy cannot read as a date: it is refused, not treated as missing.
        assert_eq!(manifest_text(&json, "pub_date").as_deref(), Some("1790000000"));
        assert_eq!(manifest_text(&json, "list").as_deref(), Some(r#"["a"]"#));
        assert!(update_policy::parse_utc(&manifest_text(&json, "pub_date").unwrap()).is_none());
    }

    #[test]
    fn a_secret_in_the_address_and_a_broken_file_are_refused_with_a_reason() {
        assert!(endpoint_from(Some("https://user:token@example.com/a.json"), None).unwrap_err().contains("user name or password"));
        assert!(endpoint_from(Some("https://token@example.com/a.json"), None).is_err());
        assert!(endpoint_from(None, Some("not json")).unwrap_err().contains("update.json"));
    }
}
