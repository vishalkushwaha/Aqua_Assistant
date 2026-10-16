//! Shared test scaffolding (card OA2-01; tasks QA-01, QA-04 and CX-15 of the OpenCode audit).
//!
//! Before this module the same doubles and the same habits were copied into every test file that needed them: a scripted planner in
//! one place, a scripted model in four, a scripted approver in two, a hand-made temp folder and an environment variable set under a
//! lock of the file's own. One place now holds them:
//!
//! * [`ScriptedPlanner`] says what it is told to, in order, to the agent loop (`Reply::Text`, `Error`, `Yield`, `After`), and remembers
//!   what it was asked, what it was told to warm, and what window it claims. [`ScriptedCompleter`] does the same for the flows'
//!   `Completer` (the model that reads and writes flow commands, triage and comment drafts). [`ScriptedApprover`] answers every
//!   approval the same way and remembers the questions.
//! * [`EnvGuard`] sets or removes environment variables for the length of a test and puts them back, also when the test panics. It
//!   holds one process-wide lock (re-entrant for the same thread), so two tests that use it never see each other's variables.
//! * [`TempWorld`] is a throw-away folder that is removed when it is dropped without ever following a junction or a link, and that
//!   refuses to be made inside Aqua's own `data` or `logs` folder.
//! * [`wait_until`] and [`wait_for`] poll a condition every 10 ms and fail with a sentence that names what was waited for. **Wait on
//!   a signal, never sleep:** a `thread::sleep` is either too short (a flaky test) or too long (a slow suite).
//! * The data-folder tripwire (`runtime::data_tripwire`) lists a test that reached the real `data/` folder of the checkout (QA-04).
//! * [`BrokerAnswerer`] answers the browser's own questions (the old routes put theirs to its broker, not to the host's approver) with no, from a thread, and keeps them.
//! * [`AgentWorld`] is the agent loop run end to end with no model: the real tool host over a workspace made of the files a scenario names, a run record of its own, a person who
//!   answers from a script, a process that can be killed in the middle of a call and started again over the same folder (card OA2-04, QA-03 and QA-08; the scenarios are in `core/tests/agent_scenarios_test.rs`).
//! * [`model_server::FakeModelServer`] is a model server on a loopback port that says what a test tells it to, for the provider of an Ollama or an LM Studio (card OA9-04).
//! * [`app_state`] is the gateway's state over a world and a tool host of the test's own, with no model or engine started: a test asks a route of `api_routes()` through the real
//!   token check instead of searching the server's source for the call it expects (card OA3-01, AQ-04).
//!
//! The module is compiled for this crate's own tests (`cfg(test)`) and, for integration tests, with the `test-support` feature
//! (`cargo test -p aqua-core --features test-support --test <name>`). It is not a separate crate on purpose: a crate of its own would
//! have to depend on `aqua-core` for the planner and so could not be a dev-dependency of it without two copies of the core types.

pub mod agent;
pub mod app;
pub mod approver;
pub mod broker;
pub mod completer;
pub mod env;
pub mod model_server;
pub mod planner;
pub mod wait;
pub mod world;

pub use agent::{yes_for_the_session, yes_once, AgentWorld, KillAt, Killed, Person, StopAt};
pub use app::app_state;
pub use approver::ScriptedApprover;
pub use broker::BrokerAnswerer;
pub use completer::ScriptedCompleter;
pub use env::EnvGuard;
pub use planner::{Reply, ScriptedPlanner, Seen};
pub use wait::{wait_for, wait_for_file, wait_for_port, wait_until};
pub use world::{remove_tree, TempWorld};
