//! The gateway itself (card OA3-06, moved out of `server.rs`): `run_api_gateway` and `run_api_gateway_on` start what runs beside it and put the gate over the routes (from the inside out: the deadline, the token check, CORS, the contract, the span of the request), and `api_routes` is the table of every route.

use super::*;

pub async fn run_api_gateway(port: u16, state: AppState) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], port))).await?;
    run_api_gateway_on(listener, state).await
}

/// The gateway on a socket that is already bound: the port `main` took at the start and answered "starting" on (see `startup_health::early`), handed over with nothing
/// refused in between. [`run_api_gateway`] binds the port itself and calls this.
pub async fn run_api_gateway_on(listener: tokio::net::TcpListener, state: AppState) -> Result<()> {
    let port = listener.local_addr()?.port();
    // The routes the gate gives and the answers on the feedback buttons are kept from here on (`data/learning/routing_*`); before this they are in memory only.
    crate::laya_validator::ledger().attach(crate::runtime::data_dir().join("learning"));
    // Practice tasks on its own while idle (does nothing unless switched on in Agent Learning).
    crate::modules::practice_runner::spawn(state.llm.clone(), state.browser.clone());
    let (workflow_llm, workflow_browser) = (state.llm.clone(), state.browser.clone());
    let (flow_llm, flow_browser) = (state.llm.clone(), state.browser.clone());
    let flow_page: std::sync::Arc<dyn crate::flows::browser_exec::Driver> = state.browser.clone();
    let flow_drafts_llm = state.llm.clone();
    crate::flows::comments_browser::install(state.browser.clone());
    let flow_social: std::sync::Arc<dyn crate::flows::browser_exec::Driver> = state.browser.clone();
    let flow_uploads = state.browser.dirs().uploads.clone();
    // Spreadsheet steps reach files through the same guard as the file tools (allowed folders, protected paths).
    let flow_sheets: Option<std::sync::Arc<dyn crate::flows::run::exec::NodeExecutor>> = {
        let data_dir = crate::runtime::workspace_root().join("data");
        crate::tool_host::load_fs_config(&data_dir).and_then(|cfg| aqua_native_tools::fs::PathGuard::new(cfg, crate::tool_host::protected_paths(&data_dir)).ok()).map(|g| crate::flows::sheet_exec::SheetNodeExecutor::new(std::sync::Arc::new(g)) as std::sync::Arc<dyn crate::flows::run::exec::NodeExecutor>)
    };
    // The AI flow builder falls back to the local model for requests its rules do not understand.
    crate::flows::planner::install(crate::flows::live::LlamaCompleter::new(state.llm.clone()));
    start_warmup(state.laya.clone(), state.llm.clone());
    let data_dir = crate::runtime::data_dir();
    let api_token = load_or_create_token(&data_dir)?;
    let ui_token = load_or_create_ui_token(&data_dir)?;
    // Aqua's own credentials are masked wherever they might appear: results, logs and the audit record.
    state.tools.register_secret(&api_token);
    state.tools.register_secret(&ui_token);
    // How much Aqua asks, and how much one run may do (card R7-18): the saved level goes to the tool host's policy, the caps to the counters of the runs.
    crate::autonomy::restore(&state.tools, &data_dir);
    let auth = ApiAuth::new(api_token, port).with_ui_token(ui_token);
    info!("API token required; read it from {}", token_path(&data_dir).display());
    let cors = auth.cors_layer();

    let app = api_routes()
        // Order matters: CORS is outermost so preflights are answered before auth runs.
        .layer(axum::middleware::from_fn(request_deadline))
        .layer(axum::middleware::from_fn_with_state(auth, require_local_auth))
        .layer(cors)
        // Every answer says which contract it follows (`x-aqua-contract`), and an error body that has no `code` gets the one of its status (card OA10-02).
        .layer(axum::middleware::from_fn(super::super::contract::contract_layer))
        // Outermost: every request but the health probe is numbered, and what the core logs while it is handled says which request it was for.
        .layer(axum::middleware::from_fn(request_span))
        .with_state(state);

    info!("Starting Aqua API Gateway Server on http://127.0.0.1:{}", port);

    // The record of what Aqua could not do (data/self_repair.db): from here on, the places that notice a gap write it down.
    crate::self_repair::install();
    // What needs the person (a question that waits, a run that ended) is put in the queue of spoken announcements when `AQUA_ATTENTION` is on (card OA10-03, UX-13).
    super::super::attention::install();
    // Where a spoken "read the diff" finds what a run changed: the diff route's own reader (card OA10-06, UX-16).
    super::super::coding_api::install();

    // Workflows start only once this process owns the port: a second Aqua on the same data folder
    // fails to bind and exits, and must not run anyone's workflows or mark their runs interrupted.
    crate::workflows::runner::spawn(crate::workflows::live::LiveExecutor::new(workflow_llm, workflow_browser));
    // AI flows: runs the ones a person starts, wakes the ones waiting on a time, marks the ones a crash cut off.
    let flow_runner = crate::flows::run::manager::spawn(
        super::super::flow_api::store(),
        super::super::flow_api::run_store(),
        std::sync::Arc::new({
            let mut e = crate::flows::run::exec::Executors::new().with(crate::flows::live::LiveNodeExecutor::new(flow_llm, flow_browser)).with(crate::flows::browser_exec::BrowserNodeExecutor::new(flow_page)).with(crate::flows::image_exec::ImageNodeExecutor::system()).with(crate::flows::social::SocialNodeExecutor::new(flow_social, flow_uploads.clone(), vec![flow_uploads, crate::flows::image::output_dir()]));
            e = e.with(crate::flows::n8n::N8nNodeExecutor::from_env(std::sync::Arc::new(aqua_native_tools::secrets::CredentialManager)));
            e = e.with(crate::flows::comment_drafts::CommentDraftExecutor::with_inbox(crate::flows::live::LlamaCompleter::new(flow_drafts_llm), super::super::flow_comments_api::inbox()));
            if let Some(sheets) = flow_sheets {
                e = e.with(sheets);
            }
            e
        }),
    );
    // A reply that was being sent when Aqua last stopped may or may not have gone out: it becomes "unknown", never retried.
    if let Ok(n) = super::super::flow_comments_api::inbox().recover_crashed() {
        if n > 0 {
            tracing::warn!("{n} comment repl{} were being sent when Aqua stopped; they are marked unknown for you to check", if n == 1 { "y" } else { "ies" });
        }
    }
    // Learns from finished runs whether the fixes that were applied worked.
    flow_runner.observe(std::sync::Arc::new(crate::flows::supervisor::recovery::RecoveryLearner { runs: super::super::flow_api::run_store() }));
    // Schedules live in the database; this looks at them every few seconds, decides what to do about times
    // missed while Aqua was closed, and tells the person how runs ended.
    crate::flows::sched::spawn(super::super::flow_sched_api::sched_store(), flow_runner, super::super::flow_api::store(), super::super::flow_sched_api::notifier());
    // Old runs, read notifications and decided scheduled times are forgotten once an hour; nothing that is still needed is.
    crate::flows::retention::spawn(super::super::flow_api::run_store(), super::super::flow_sched_api::notify_store(), super::super::flow_sched_api::sched_store());
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    info!("Shutdown signal received; stopping API gateway.");
}

/// Every route of the gateway, with no layer over it and no state (card OA3-01, AQ-04). `run_api_gateway_on` puts the gate over it (the deadline, the token check, CORS) and gives it
/// the state; a test of the routes does the same with a state of its own, so that what it asks is the route table that is served and not a copy of it.
pub(crate) fn api_routes() -> Router<AppState> {
    Router::new()
        .merge(super::status_api::routes())
        .merge(super::index_api::routes())
        .merge(super::ws::routes())
        .merge(super::approvals_api::routes())
        .merge(super::pipeline_api::routes())
        .merge(super::stream_api::routes())
        .merge(super::turn_control_api::routes())
        .merge(super::listen_api::routes())
        .merge(super::speech_api::routes())
        .merge(super::voice_settings_api::routes())
        .merge(super::llm_api::routes())
        .merge(super::mcp_api::routes())
        .merge(super::tools_api::routes())
        .merge(super::browser_api::routes())
        .merge(super::agent_api::routes())
        .merge(super::agent_stream_api::routes())
        .merge(super::super::route_api::routes())
        .merge(super::super::run_api::routes())
        .merge(super::super::run_control_api::routes())
        .merge(super::super::sessions_api::routes())
        .merge(super::super::preapproval_api::routes())
        .merge(super::super::coding_api::routes())
        .merge(super::super::learning_api::routes())
        .merge(super::super::devices_api::routes())
        .merge(super::super::dictation_api::routes())
        .merge(super::super::recipes_api::routes())
        .merge(super::super::announce_api::routes())
        .merge(super::super::workflow_api::routes())
        .merge(super::super::flow_api::routes())
        .merge(super::super::flow_sched_api::routes())
        .merge(super::super::flow_comments_api::routes())
        .merge(super::super::self_repair_api::routes())
        .merge(super::super::autonomy_api::routes())
        .merge(super::super::eval_api::routes())
        .merge(super::super::models_api::routes())
        .merge(super::super::contract::routes())
        .merge(super::super::routing_feedback_api::routes())
}
