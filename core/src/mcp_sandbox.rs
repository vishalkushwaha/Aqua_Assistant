//! Starting the MCP servers the person added inside the MXC container, when they chose that (card R6-35, decision D-5 of 2026-10-08).
//!
//! **Off by default:** nothing here does anything unless `data/tools/term.json` says `"mcp_tier": "mxc"`. Then the servers listed in `data/tools/mcp.json` (and only those: the
//! desktop and browser servers Aqua itself starts are never wrapped) are started through the container's launcher with no network, with read access to their working folder
//! and their program's folder (and `mxc.read_paths`), write access to a scratch folder, and Aqua's own files named as denied. A server that needs the network, or files
//! elsewhere, will not work that way until `mxc.network_allow` and `mxc.read_paths` name them (and an allowed address leaves name lookups open: see `aqua_native_tools::sandbox`).
//! If the container is not usable on this machine, such a server is **not started** and the reason is given; it does not run unprotected, and it does not fall back to Low
//! integrity (a stdio server cannot be started that way).

use aqua_mcp_adapters::stdio_mcp::{set_launch_wrapper, LaunchRequest, LaunchWrapper, Wrapped};
use aqua_native_tools::sandbox::{self, Launch, Mxc, Tier};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{info, warn};

/// Gives the closure the signature the hook expects (a closure passed straight to `Arc::new` would have its lifetimes inferred without it).
fn make<F>(f: F) -> LaunchWrapper
where
    F: for<'a, 'b> Fn(&'a LaunchRequest<'b>) -> Result<Option<Wrapped>, String> + Send + Sync + 'static,
{
    Arc::new(f)
}

/// The wrapper for the servers whose ids are `user_ids`. A server outside the list is started as it always was.
pub fn wrapper_for(mxc: Arc<Mxc>, user_ids: BTreeSet<String>, scratch_base: PathBuf, denied: Vec<PathBuf>) -> LaunchWrapper {
    make(move |req| {
        if !user_ids.contains(req.label) {
            return Ok(None);
        }
        mxc.available().map_err(|why| format!("it is set to run in an MXC container (mcp_tier in term.json), which is not usable here: {why}"))?;
        let name: String = req.label.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
        let scratch = scratch_base.join(name);
        std::fs::create_dir_all(&scratch).map_err(|e| format!("could not make the scratch folder {}: {e}", scratch.display()))?;
        let mut env: Vec<(String, String)> = req.env.iter().filter(|(k, _)| !k.eq_ignore_ascii_case("TEMP") && !k.eq_ignore_ascii_case("TMP")).cloned().collect();
        for k in ["TEMP", "TMP"] {
            env.push((k.to_string(), scratch.to_string_lossy().into_owned()));
        }
        // A working folder that holds Aqua's own data, or lies inside it, is not shared; the server then works in its scratch folder.
        let shared = req.cwd.filter(|c| !denied.iter().any(|p| c.starts_with(p) || p.starts_with(c)));
        let launch = Launch {
            exe: PathBuf::from(req.program),
            args: req.args.clone(),
            cwd: shared.map(Path::to_path_buf).unwrap_or_else(|| scratch.clone()),
            env,
            read_paths: shared.map(|c| vec![c.to_path_buf()]).unwrap_or_default(),
            write_paths: vec![scratch],
            denied_paths: denied.clone(),
            // A server stays up until Aqua stops it.
            timeout: None,
        };
        let sandbox::Wrapped { exe, args, env, cwd, cleanup, .. } = mxc.plan(&launch)?;
        Ok(Some(Wrapped { program: exe, args, cwd: Some(cwd), env, keep: Box::new(cleanup) }))
    })
}

/// At start-up, before any server is started: install the wrapper if the person chose `mcp_tier: "mxc"`. The container is probed here, once.
pub fn install(data_dir: &Path) {
    let Some(cfg) = crate::tool_host::load_term_config(data_dir) else { return };
    match cfg.mcp_tier {
        Tier::None => return,
        Tier::Low => {
            warn!("term.json: mcp_tier \"low\" is read as \"none\" (a server that talks over pipes cannot be started at Low integrity)");
            return;
        }
        Tier::Mxc => {}
    }
    let ids: BTreeSet<String> = std::fs::read_to_string(aqua_mcp_adapters::user_servers::ServerManager::config_path(&data_dir.join("tools")))
        .ok()
        .and_then(|text| aqua_mcp_adapters::user_servers::parse_file(crate::tool_host::strip_bom(&text)).ok())
        .map(|m| m.into_keys().collect())
        .unwrap_or_default();
    let mxc = Arc::new(Mxc::from_config(&cfg.mxc));
    match mxc.available() {
        Ok(()) => info!("MCP servers you added ({}) will start inside an MXC container", ids.len()),
        Err(why) => warn!("mcp_tier is mxc but the container is not usable ({why}): the MCP servers you added will not start"),
    }
    let denied = crate::tool_host::protected_paths(data_dir);
    if !set_launch_wrapper(wrapper_for(mxc, ids, std::env::temp_dir().join("aqua_mcp_scratch"), denied)) {
        warn!("a launch wrapper for MCP servers was already installed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aqua_native_tools::sandbox::{MxcConfig, Output, Runner};
    use std::time::Duration;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("aqua_mcp_sandbox_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn runner(probe: &'static str) -> Runner {
        Arc::new(move |_: &Path, args: &[String], _: Duration| -> Result<Output, String> {
            Ok(Output { code: Some(0), stdout: if args.first().map(String::as_str) == Some("--probe") { probe.to_string() } else { String::new() }, stderr: String::new() })
        })
    }

    const OK: &str = r#"{"tier":"base-container","needsDaclAugmentation":false,"warnings":[]}"#;

    fn request<'a>(label: &'a str, cwd: Option<&'a Path>) -> LaunchRequest<'a> {
        LaunchRequest { label, program: std::ffi::OsStr::new("node.exe"), args: vec!["server.js".into()], cwd, env: vec![("PATH".into(), "C:\\Windows".into()), ("TEMP".into(), "C:\\Users\\x\\Temp".into())] }
    }

    #[test]
    fn a_server_the_person_added_starts_through_the_launcher_with_no_network_and_aquas_data_denied_and_the_servers_aqua_starts_are_never_wrapped() {
        let d = temp("wrap");
        let project = d.join("project");
        let data = d.join("data");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&data).unwrap();
        let mxc = Arc::new(Mxc::with_runner(&MxcConfig::default(), PathBuf::from("wxc-exec.exe"), d.join("policies"), runner(OK)));
        let ids: BTreeSet<String> = ["notes".to_string()].into();
        let wrap = wrapper_for(mxc, ids, d.join("scratch"), vec![data.clone()]);
        assert!(wrap(&request("windows-mcp", Some(&project))).unwrap().is_none(), "the desktop server is not one the person added");
        let w = wrap(&request("notes", Some(&project))).unwrap().expect("a server of mcp.json is wrapped");
        assert_eq!(w.program, PathBuf::from("wxc-exec.exe"));
        let policy: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&w.args[0]).unwrap()).unwrap();
        assert_eq!(policy["network"]["egress"], serde_json::json!({ "default": "deny" }));
        assert_eq!(policy["network"]["ingress"]["hostLoopback"], "deny");
        let (data_s, project_s) = (data.to_string_lossy().into_owned(), project.to_string_lossy().into_owned());
        assert_eq!(policy["filesystem"]["deniedPaths"][0], data_s.as_str());
        assert!(policy["filesystem"]["readonlyPaths"].as_array().unwrap().iter().any(|p| p.as_str() == Some(project_s.as_str())));
        assert!(policy["process"].get("timeout").is_none(), "a server stays up");
        let temp_entries = policy["process"]["env"].as_array().unwrap().iter().filter_map(|e| e.as_str()).filter(|e| e.starts_with("TEMP=")).collect::<Vec<_>>();
        assert_eq!(temp_entries.len(), 1);
        assert!(temp_entries[0].contains("scratch"), "its temporary files go in the scratch folder: {temp_entries:?}");
        let file = PathBuf::from(&w.args[0]);
        drop(w);
        assert!(!file.exists(), "the policy file goes with the server");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_working_folder_that_holds_aquas_data_is_not_shared_with_the_server() {
        let d = temp("holds");
        let data = d.join("data");
        std::fs::create_dir_all(&data).unwrap();
        let mxc = Arc::new(Mxc::with_runner(&MxcConfig::default(), PathBuf::from("wxc-exec.exe"), d.join("policies"), runner(OK)));
        let wrap = wrapper_for(mxc, ["notes".to_string()].into(), d.join("scratch"), vec![data.clone()]);
        let w = wrap(&request("notes", Some(&d))).unwrap().unwrap();
        let policy: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&w.args[0]).unwrap()).unwrap();
        let reads = policy["filesystem"]["readonlyPaths"].as_array().unwrap().iter().filter_map(|p| p.as_str()).map(String::from).collect::<Vec<_>>();
        assert!(!reads.contains(&d.to_string_lossy().into_owned()), "the folder above the data is not granted: {reads:?}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn when_the_container_is_not_usable_a_server_the_person_added_is_not_started_at_all() {
        let d = temp("unusable");
        let mxc = Arc::new(Mxc::with_runner(&MxcConfig::default(), PathBuf::from("wxc-exec.exe"), d.join("policies"), runner(r#"{"tier":"appcontainer","needsDaclAugmentation":false}"#)));
        let wrap = wrapper_for(mxc, ["notes".to_string()].into(), d.join("scratch"), vec![]);
        let err = wrap(&request("notes", None)).err().expect("an error, not a server that runs unprotected");
        assert!(err.contains("not usable here"), "{err}");
        assert!(wrap(&request("windows-mcp", None)).unwrap().is_none(), "the others are unaffected");
        let _ = std::fs::remove_dir_all(&d);
    }
}
