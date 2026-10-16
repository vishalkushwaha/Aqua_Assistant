
use aqua_native_tools::http::HttpProvider;
use aqua_tool_host::{ToolProvider, ExecCtx, FsConfig};
use std::sync::atomic::AtomicBool;
use serde_json::json;

#[tokio::main]
async fn main() {
    let provider = HttpProvider::new(FsConfig::default(), std::path::PathBuf::from("."));
    let ctx = ExecCtx {
        cancel: std::sync::Arc::new(AtomicBool::new(false)),
        fs_config: FsConfig::default(),
        app_data: std::path::PathBuf::from("."),
        ssh_known_hosts: std::path::PathBuf::from("."),
        ssh_identity: None,
        auth_profiles: Default::default(),
    };
    
    let args = json!({
        "url": "https://wttr.in/Delhi?format=3",
        "css_selector": ""
    });
    
    let result = provider.call("web.scrape", args, &ctx).await.unwrap();
    println!("Weather Data: {}", result.text);
}

