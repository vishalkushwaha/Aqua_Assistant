//! The attribute names used on spans, kept in one place.
//!
//! They follow the OpenTelemetry GenAI semantic conventions, which are still in "Development" status, so a rename upstream is one edit
//! here. Only names and operation values go in this file; nothing in it reads data.

/// `gen_ai.operation.name` values.
pub const OP_INVOKE_AGENT: &str = "invoke_agent";
/// The planning phase of an agent (still "Development" in the conventions): its span is named `plan {agent}` and the model call that makes the plan is its child.
pub const OP_PLAN: &str = "plan";
pub const OP_CHAT: &str = "chat";
pub const OP_EXECUTE_TOOL: &str = "execute_tool";

/// The name of the agent in a span's name and in `gen_ai.agent.name`.
pub const AGENT: &str = "aqua";

pub const OPERATION_NAME: &str = "gen_ai.operation.name";
pub const AGENT_NAME: &str = "gen_ai.agent.name";
pub const REQUEST_MODEL: &str = "gen_ai.request.model";
pub const TOOL_NAME: &str = "gen_ai.tool.name";
pub const TOOL_CALL_ARGUMENTS: &str = "gen_ai.tool.call.arguments";
pub const TOOL_CALL_RESULT: &str = "gen_ai.tool.call.result";
pub const USAGE_INPUT_TOKENS: &str = "gen_ai.usage.input_tokens";
pub const USAGE_OUTPUT_TOKENS: &str = "gen_ai.usage.output_tokens";
pub const OUTPUT_TYPE: &str = "gen_ai.output.type";
pub const ERROR_TYPE: &str = "error.type";

/// Aqua's own attributes live under this prefix.
pub const AQUA_RUN_ID: &str = "aqua.run.id";
pub const AQUA_RUN_STATUS: &str = "aqua.run.status";
pub const AQUA_STOPPED_REASON: &str = "aqua.run.stopped_reason";
pub const AQUA_CALLER: &str = "aqua.run.caller";
pub const AQUA_ROUTE: &str = "aqua.run.route";
pub const AQUA_STEP: &str = "aqua.step";
pub const AQUA_DECISION: &str = "aqua.tool.decision";
pub const AQUA_APPROVAL: &str = "aqua.tool.approval";
pub const AQUA_CACHE_READ_TOKENS: &str = "aqua.usage.cache_read_tokens";
pub const AQUA_TTFT_MS: &str = "aqua.usage.time_to_first_token_ms";
pub const AQUA_DECODE_TPS: &str = "aqua.usage.decode_tokens_per_second";
