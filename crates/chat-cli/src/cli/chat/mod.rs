pub mod cli;
mod consts;
pub mod context;
mod conversation;
mod input_source;
mod message;
mod parse;
pub mod stream_json;
use std::path::MAIN_SEPARATOR;
pub mod checkpoint;
mod line_tracker;
mod parser;
mod prompt;
mod prompt_parser;
pub mod server_messenger;
use crate::cli::chat::checkpoint::CHECKPOINT_MESSAGE_MAX_LENGTH;
use crate::constants::ui_text::{
    LIMIT_REACHED_TEXT,
    POPULAR_SHORTCUTS,
    RESUME_TEXT,
    SMALL_SCREEN_POPULAR_SHORTCUTS,
    SMALL_SCREEN_WELCOME,
    WELCOME_TEXT,
};
#[cfg(unix)]
mod skim_integration;
mod token_counter;
pub mod tool_manager;
pub mod tools;
pub mod util;
use std::borrow::Cow;
use std::collections::{
    HashMap,
    VecDeque,
};
use std::io::{
    IsTerminal,
    Read,
    Write,
};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{
    Duration,
    Instant,
};

use amzn_codewhisperer_client::types::SubscriptionStatus;
use clap::{
    Args,
    CommandFactory,
    Parser,
    ValueEnum,
};
use cli::compact::CompactStrategy;
use cli::hooks::ToolContext;
use cli::model::{
    find_model,
    get_available_models,
    select_model,
};
pub use conversation::ConversationState;
use conversation::TokenWarningLevel;
use crossterm::style::{
    Attribute,
    Color,
    Stylize,
};
use crossterm::{
    cursor,
    execute,
    queue,
    style,
    terminal,
};
use eyre::{
    Report,
    Result,
    bail,
    eyre,
};
use input_source::InputSource;
use message::{
    AssistantMessage,
    AssistantToolUse,
    ToolUseResult,
    ToolUseResultBlock,
};
use parse::{
    ParseState,
    interpret_markdown,
};
use parser::{
    RecvErrorKind,
    RequestMetadata,
    SendMessageStream,
};
use regex::Regex;
use rmcp::model::PromptMessage;
use spinners::{
    Spinner,
    Spinners,
};
use thiserror::Error;
use time::OffsetDateTime;
use token_counter::TokenCounter;
use tokio::signal::ctrl_c;
use tokio::sync::{
    Mutex,
    broadcast,
};
use tool_manager::{
    PromptQuery,
    PromptQueryResult,
    ToolManager,
    ToolManagerBuilder,
};
use tools::delegate::status_all_agents;
use tools::gh_issue::GhIssueContext;
use tools::{
    NATIVE_TOOLS,
    OutputKind,
    QueuedTool,
    Tool,
    ToolSpec,
};
use tracing::{
    debug,
    error,
    info,
    trace,
    warn,
};
use util::images::RichImageBlock;
use util::ui::draw_box;
use util::{
    animate_output,
    play_notification_bell,
};
use winnow::Partial;
use winnow::stream::Offset;

use super::agent::{
    Agent,
    DEFAULT_AGENT_NAME,
    PermissionEvalResult,
};
use crate::api_client::model::ToolResultStatus;
use crate::api_client::{
    self,
    ApiClientError,
};
use crate::auth::AuthError;
use crate::auth::builder_id::is_idc_user;
use crate::cli::TodoListState;
use crate::cli::agent::Agents;
use crate::cli::chat::checkpoint::{
    CheckpointManager,
    truncate_message,
};
use crate::cli::chat::cli::SlashCommand;
use crate::cli::chat::cli::editor::open_editor;
use crate::cli::chat::cli::prompts::{
    GetPromptError,
    PromptsSubcommand,
};
use crate::cli::chat::message::UserMessage;
use crate::cli::chat::util::sanitize_unicode_tags;
use crate::cli::experiment::experiment_manager::{
    ExperimentManager,
    ExperimentName,
};
use crate::constants::{
    error_messages,
    tips,
    ui_text,
};
use crate::database::settings::Setting;
use crate::os::Os;
use crate::telemetry::core::{
    AgentConfigInitArgs,
    ChatAddedMessageParams,
    ChatConversationType,
    MessageMetaTag,
    RecordUserTurnCompletionArgs,
    ToolUseEventBuilder,
};
use crate::telemetry::{
    ReasonCode,
    TelemetryResult,
    get_error_reason,
};
use crate::util::directories::get_shadow_repo_dir;
use crate::util::{
    MCP_SERVER_TOOL_DELIMITER,
    directories,
    ui,
};

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum WrapMode {
    /// Always wrap at terminal width
    Always,
    /// Never wrap (raw output)
    Never,
    /// Auto-detect based on output target (default)
    Auto,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    /// Plain text output with markdown rendering (default)
    Plain,
    /// JSON output (newline-delimited JSON events)
    Json,
}

impl Default for OutputFormat {
    fn default() -> Self {
        Self::Plain
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Args)]
pub struct ChatArgs {
    /// Resumes the previous conversation from this directory.
    #[arg(short, long)]
    pub resume: bool,
    /// Context profile to use
    #[arg(long = "agent", alias = "profile")]
    pub agent: Option<String>,
    /// Current model to use
    #[arg(long = "model")]
    pub model: Option<String>,
    /// Allows the model to use any tool to run commands without asking for confirmation.
    #[arg(short = 'a', long)]
    pub trust_all_tools: bool,
    /// Trust only this set of tools. Example: trust some tools:
    /// '--trust-tools=fs_read,fs_write', trust no tools: '--trust-tools='
    #[arg(long, value_delimiter = ',', value_name = "TOOL_NAMES")]
    pub trust_tools: Option<Vec<String>>,
    /// Whether the command should run without expecting user input
    #[arg(long, alias = "non-interactive")]
    pub no_interactive: bool,
    /// The first question to ask
    pub input: Option<String>,
    /// Control line wrapping behavior (default: auto-detect)
    #[arg(short = 'w', long, value_enum)]
    pub wrap: Option<WrapMode>,

    /// Output format (default: plain)
    #[arg(short = 'f', long = "output-format", value_enum)]
    pub output_format: Option<OutputFormat>,
}

impl ChatArgs {
    pub async fn execute(mut self, os: &mut Os) -> Result<ExitCode> {
        let mut input = self.input;
        let output_format = self.output_format.unwrap_or_default();
        let is_non_interactive = self.no_interactive;

        if is_non_interactive && input.is_none() {
            if !std::io::stdin().is_terminal() {
                let mut buffer = String::new();
                match std::io::stdin().read_to_string(&mut buffer) {
                    Ok(_) => {
                        if !buffer.trim().is_empty() {
                            input = Some(buffer.trim().to_string());
                        }
                    },
                    Err(e) => {
                        eprintln!("Error reading from stdin: {}", e);
                    },
                }
            }

            if input.is_none() {
                bail!("Input must be supplied when running in non-interactive mode");
            }
        }

        let stdout = std::io::stdout();
        let mut stderr = std::io::stderr();

        let args: Vec<String> = std::env::args().collect();
        if args
            .iter()
            .any(|arg| arg == "--profile" || arg.starts_with("--profile="))
        {
            execute!(
                stderr,
                style::SetForegroundColor(Color::Yellow),
                style::Print("WARNING: "),
                style::SetForegroundColor(Color::Reset),
                style::Print("--profile is deprecated, use "),
                style::SetForegroundColor(Color::Green),
                style::Print("--agent"),
                style::SetForegroundColor(Color::Reset),
                style::Print(" instead\n")
            )?;
        }

        let conversation_id = uuid::Uuid::new_v4().to_string();
        info!(?conversation_id, "Generated new conversation id");

        // Check MCP status once at the beginning of the session
        let mcp_enabled = match os.client.is_mcp_enabled().await {
            Ok(enabled) => enabled,
            Err(err) => {
                tracing::warn!(?err, "Failed to check MCP configuration, defaulting to enabled");
                true
            },
        };

        let agents = {
            let skip_migration = self.no_interactive;
            let (mut agents, md) =
                Agents::load(os, self.agent.as_deref(), skip_migration, &mut stderr, mcp_enabled).await;
            agents.trust_all_tools = self.trust_all_tools;

            os.telemetry
                .send_agent_config_init(&os.database, conversation_id.clone(), AgentConfigInitArgs {
                    agents_loaded_count: md.load_count as i64,
                    agents_loaded_failed_count: md.load_failed_count as i64,
                    legacy_profile_migration_executed: md.migration_performed,
                    legacy_profile_migrated_count: md.migrated_count as i64,
                    launched_agent: md.launched_agent,
                })
                .await
                .map_err(|err| error!(?err, "failed to send agent config init telemetry"))
                .ok();

            // Only show MCP safety message if MCP is enabled and has servers
            if mcp_enabled
                && agents
                    .get_active()
                    .is_some_and(|a| !a.mcp_servers.mcp_servers.is_empty())
            {
                if !self.no_interactive && !os.database.settings.get_bool(Setting::McpLoadedBefore).unwrap_or(false) {
                    execute!(
                        stderr,
                        style::Print(
                            "To learn more about MCP safety, see https://docs.aws.amazon.com/amazonq/latest/qdeveloper-ug/command-line-mcp-security.html\n\n"
                        )
                    )?;
                }
                os.database.settings.set(Setting::McpLoadedBefore, true).await?;
            }

            if let Some(trust_tools) = self.trust_tools.take() {
                for tool in &trust_tools {
                    if !tool.starts_with("@") && !NATIVE_TOOLS.contains(&tool.as_str()) {
                        let _ = queue!(
                            stderr,
                            style::SetForegroundColor(Color::Yellow),
                            style::Print("WARNING: "),
                            style::SetForegroundColor(Color::Reset),
                            style::Print("--trust-tools arg for custom tool "),
                            style::SetForegroundColor(Color::Cyan),
                            style::Print(tool),
                            style::SetForegroundColor(Color::Reset),
                            style::Print(" needs to be prepended with "),
                            style::SetForegroundColor(Color::Green),
                            style::Print("@{MCPSERVERNAME}/"),
                            style::SetForegroundColor(Color::Reset),
                            style::Print("\n"),
                        );
                    }
                }

                let _ = stderr.flush();

                if let Some(a) = agents.get_active_mut() {
                    a.allowed_tools.extend(trust_tools);
                }
            }

            agents
        };

        // If modelId is specified, verify it exists before starting the chat
        // Otherwise, CLI will use a default model when starting chat
        let (models, default_model_opt) = get_available_models(os).await?;
        // Fallback logic: try user's saved default, then system default
        let fallback_model_id = || {
            if let Some(saved) = os.database.settings.get_string(Setting::ChatDefaultModel) {
                find_model(&models, &saved)
                    .map(|m| m.model_id.clone())
                    .or(Some(default_model_opt.model_id.clone()))
            } else {
                Some(default_model_opt.model_id.clone())
            }
        };

        let model_id: Option<String> = if let Some(requested) = self.model.as_ref() {
            // CLI argument takes highest priority
            if let Some(m) = find_model(&models, requested) {
                Some(m.model_id.clone())
            } else {
                let available = models
                    .iter()
                    .map(|m| m.model_name.as_deref().unwrap_or(&m.model_id))
                    .collect::<Vec<_>>()
                    .join(", ");
                bail!("Model '{}' does not exist. Available models: {}", requested, available);
            }
        } else if let Some(agent_model) = agents.get_active().and_then(|a| a.model.as_ref()) {
            // Agent model takes second priority
            if let Some(m) = find_model(&models, agent_model) {
                Some(m.model_id.clone())
            } else {
                let _ = execute!(
                    stderr,
                    style::SetForegroundColor(Color::Yellow),
                    style::Print("WARNING: "),
                    style::SetForegroundColor(Color::Reset),
                    style::Print("Agent specifies model '"),
                    style::SetForegroundColor(Color::Cyan),
                    style::Print(agent_model),
                    style::SetForegroundColor(Color::Reset),
                    style::Print("' which is not available. Falling back to configured defaults.\n"),
                );
                fallback_model_id()
            }
        } else {
            fallback_model_id()
        };

        let (prompt_request_sender, prompt_request_receiver) = tokio::sync::broadcast::channel::<PromptQuery>(5);
        let (prompt_response_sender, prompt_response_receiver) =
            tokio::sync::broadcast::channel::<PromptQueryResult>(5);
        let mut tool_manager = ToolManagerBuilder::default()
            .prompt_query_result_sender(prompt_response_sender)
            .prompt_query_receiver(prompt_request_receiver)
            .prompt_query_sender(prompt_request_sender.clone())
            .prompt_query_result_receiver(prompt_response_receiver.resubscribe())
            .conversation_id(&conversation_id)
            .agent(agents.get_active().cloned().unwrap_or_default())
            .build(os, Box::new(std::io::stderr()), !self.no_interactive)
            .await?;
        let tool_config = tool_manager.load_tools(os, &mut stderr).await?;

        let interactive = !is_non_interactive;

        ChatSession::new(
            os,
            stdout,
            stderr,
            &conversation_id,
            agents,
            input,
            InputSource::new(os, prompt_request_sender, prompt_response_receiver)?,
            self.resume,
            || terminal::window_size().map(|s| s.columns.into()).ok(),
            tool_manager,
            model_id,
            tool_config,
            interactive,
            mcp_enabled,
            self.wrap,
            output_format,
        )
        .await?
        .spawn(os)
        .await
        .map(|_| ExitCode::SUCCESS)
    }
}

// Maximum number of times to show the changelog announcement per version
const CHANGELOG_MAX_SHOW_COUNT: i64 = 2;

// Only show the model-related tip for now to make users aware of this feature.
const ROTATING_TIPS: [&str; 20] = tips::ROTATING_TIPS;

const GREETING_BREAK_POINT: usize = 80;

const RESPONSE_TIMEOUT_CONTENT: &str = "Response timed out - message took too long to generate";
fn trust_all_text() -> String {
    ui_text::trust_all_warning()
}

const TOOL_BULLET: &str = " ● ";
const CONTINUATION_LINE: &str = " ⋮ ";
const PURPOSE_ARROW: &str = " ↳ ";
const SUCCESS_TICK: &str = " ✓ ";
const ERROR_EXCLAMATION: &str = " ❗ ";
const DELEGATE_NOTIFIER: &str = "[BACKGROUND TASK READY]";

/// Enum used to denote the origin of a tool use event
enum ToolUseStatus {
    /// Variant denotes that the tool use event associated with chat context is a direct result of
    /// a user request
    Idle,
    /// Variant denotes that the tool use event associated with the chat context is a result of a
    /// retry for one or more previously attempted tool use. The tuple is the utterance id
    /// associated with the original user request that necessitated the tool use
    RetryInProgress(String),
}

#[derive(Debug, Error)]
pub enum ChatError {
    #[error("{0}")]
    Client(Box<crate::api_client::ApiClientError>),
    #[error("{0}")]
    Auth(#[from] AuthError),
    #[error("{0}")]
    SendMessage(Box<parser::SendMessageError>),
    #[error("{0}")]
    ResponseStream(Box<parser::RecvError>),
    #[error("{0}")]
    Std(#[from] std::io::Error),
    #[error("{0}")]
    Readline(#[from] rustyline::error::ReadlineError),
    #[error("{0}")]
    Custom(Cow<'static, str>),
    #[error("interrupted")]
    Interrupted { tool_uses: Option<Vec<QueuedTool>> },
    #[error(transparent)]
    GetPromptError(#[from] GetPromptError),
    #[error(
        "Tool approval required but --no-interactive was specified. Use --trust-all-tools to automatically approve tools."
    )]
    NonInteractiveToolApproval,
    #[error("The conversation history is too large to compact")]
    CompactHistoryFailure,
    #[error("Failed to swap to agent: {0}")]
    AgentSwapError(eyre::Report),
}

impl ChatError {
    fn status_code(&self) -> Option<u16> {
        match self {
            ChatError::Client(e) => e.status_code(),
            ChatError::Auth(_) => None,
            ChatError::SendMessage(e) => e.status_code(),
            ChatError::ResponseStream(_) => None,
            ChatError::Std(_) => None,
            ChatError::Readline(_) => None,
            ChatError::Custom(_) => None,
            ChatError::Interrupted { .. } => None,
            ChatError::GetPromptError(_) => None,
            ChatError::NonInteractiveToolApproval => None,
            ChatError::CompactHistoryFailure => None,
            ChatError::AgentSwapError(_) => None,
        }
    }
}

impl ReasonCode for ChatError {
    fn reason_code(&self) -> String {
        match self {
            ChatError::Client(e) => e.reason_code(),
            ChatError::SendMessage(e) => e.reason_code(),
            ChatError::ResponseStream(e) => e.reason_code(),
            ChatError::Std(_) => "StdIoError".to_string(),
            ChatError::Readline(_) => "ReadlineError".to_string(),
            ChatError::Custom(_) => "GenericError".to_string(),
            ChatError::Interrupted { .. } => "Interrupted".to_string(),
            ChatError::GetPromptError(_) => "GetPromptError".to_string(),
            ChatError::Auth(_) => "AuthError".to_string(),
            ChatError::NonInteractiveToolApproval => "NonInteractiveToolApproval".to_string(),
            ChatError::CompactHistoryFailure => "CompactHistoryFailure".to_string(),
            ChatError::AgentSwapError(_) => "AgentSwapError".to_string(),
        }
    }
}

impl From<ApiClientError> for ChatError {
    fn from(value: ApiClientError) -> Self {
        Self::Client(Box::new(value))
    }
}

impl From<parser::SendMessageError> for ChatError {
    fn from(value: parser::SendMessageError) -> Self {
        Self::SendMessage(Box::new(value))
    }
}

impl From<parser::RecvError> for ChatError {
    fn from(value: parser::RecvError) -> Self {
        Self::ResponseStream(Box::new(value))
    }
}

pub struct ChatSession {
    /// For output read by humans and machine
    pub stdout: std::io::Stdout,
    /// For display output, only read by humans
    pub stderr: std::io::Stderr,
    initial_input: Option<String>,
    /// Whether we're starting a new conversation or continuing an old one.
    existing_conversation: bool,
    input_source: InputSource,
    /// Width of the terminal, required for [ParseState].
    terminal_width_provider: fn() -> Option<usize>,
    spinner: Option<Spinner>,
    /// [ConversationState].
    conversation: ConversationState,
    /// Tool uses requested by the model that are actively being handled.
    tool_uses: Vec<QueuedTool>,
    /// An index into [Self::tool_uses] to represent the current tool use being handled.
    pending_tool_index: Option<usize>,
    /// The time immediately after having received valid tool uses from the model.
    ///
    /// Used to track the time taken from initially prompting the user to tool execute
    /// completion.
    tool_turn_start_time: Option<Instant>,
    /// [RequestMetadata] about the ongoing operation.
    user_turn_request_metadata: Vec<RequestMetadata>,
    /// Telemetry events to be sent as part of the conversation. The HashMap key is tool_use_id.
    tool_use_telemetry_events: HashMap<String, ToolUseEventBuilder>,
    /// State used to keep track of tool use relation
    tool_use_status: ToolUseStatus,
    /// Any failed requests that could be useful for error report/debugging
    failed_request_ids: Vec<String>,
    /// Pending prompts to be sent
    pending_prompts: VecDeque<PromptMessage>,
    interactive: bool,
    inner: Option<ChatState>,
    ctrlc_rx: broadcast::Receiver<()>,
    wrap: Option<WrapMode>,
    output_format: OutputFormat,
}

impl ChatSession {
    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        os: &mut Os,
        stdout: std::io::Stdout,
        mut stderr: std::io::Stderr,
        conversation_id: &str,
        mut agents: Agents,
        mut input: Option<String>,
        input_source: InputSource,
        resume_conversation: bool,
        terminal_width_provider: fn() -> Option<usize>,
        tool_manager: ToolManager,
        model_id: Option<String>,
        tool_config: HashMap<String, ToolSpec>,
        interactive: bool,
        mcp_enabled: bool,
        wrap: Option<WrapMode>,
        output_format: OutputFormat,
    ) -> Result<Self> {
        // Reload prior conversation
        let mut existing_conversation = false;
        let previous_conversation = std::env::current_dir()
            .ok()
            .and_then(|cwd| os.database.get_conversation_by_path(cwd).ok())
            .flatten();

        // Only restore conversations where there were actual messages.
        // Prevents edge case where user clears conversation then exits without chatting.
        let conversation = match resume_conversation
            && previous_conversation
                .as_ref()
                .is_some_and(|cs| !cs.history().is_empty())
        {
            true => {
                let mut cs = previous_conversation.unwrap();
                existing_conversation = true;
                input = Some(input.unwrap_or("In a few words, summarize our conversation so far.".to_owned()));
                cs.tool_manager = tool_manager;
                if let Some(profile) = cs.current_profile() {
                    if agents.switch(profile).is_err() {
                        execute!(
                            stderr,
                            style::SetForegroundColor(Color::Red),
                            style::Print("Error"),
                            style::ResetColor,
                            style::Print(format!(
                                ": cannot resume conversation with {profile} because it no longer exists. Using default.\n"
                            ))
                        )?;
                        let _ = agents.switch(DEFAULT_AGENT_NAME);
                    }
                }
                cs.agents = agents;
                cs.mcp_enabled = mcp_enabled;
                cs.update_state(true).await;
                cs.enforce_tool_use_history_invariants();
                cs
            },
            false => {
                ConversationState::new(
                    conversation_id,
                    agents,
                    tool_config,
                    tool_manager,
                    model_id,
                    os,
                    mcp_enabled,
                )
                .await
            },
        };

        // Spawn a task for listening and broadcasting sigints.
        let (ctrlc_tx, ctrlc_rx) = tokio::sync::broadcast::channel(4);
        tokio::spawn(async move {
            loop {
                match ctrl_c().await {
                    Ok(_) => {
                        let _ = ctrlc_tx
                            .send(())
                            .map_err(|err| error!(?err, "failed to send ctrlc to broadcast channel"));
                    },
                    Err(err) => {
                        error!(?err, "Encountered an error while receiving a ctrl+c");
                    },
                }
            }
        });

        Ok(Self {
            stdout,
            stderr,
            initial_input: input,
            existing_conversation,
            input_source,
            terminal_width_provider,
            spinner: None,
            conversation,
            tool_uses: vec![],
            user_turn_request_metadata: vec![],
            pending_tool_index: None,
            tool_turn_start_time: None,
            tool_use_telemetry_events: HashMap::new(),
            tool_use_status: ToolUseStatus::Idle,
            failed_request_ids: Vec::new(),
            pending_prompts: VecDeque::new(),
            interactive,
            inner: Some(ChatState::default()),
            ctrlc_rx,
            wrap,
            output_format,
        })
    }

    pub async fn next(&mut self, os: &mut Os) -> Result<(), ChatError> {
        // Update conversation state with new tool information
        self.conversation.update_state(false).await;

        let mut ctrl_c_stream = self.ctrlc_rx.resubscribe();
        let result = match self.inner.take().expect("state must always be Some") {
            ChatState::PromptUser { skip_printing_tools } => {
                match (self.interactive, self.tool_uses.is_empty()) {
                    (false, true) => {
                        self.inner = Some(ChatState::Exit);
                        return Ok(());
                    },
                    (false, false) => {
                        return Err(ChatError::NonInteractiveToolApproval);
                    },
                    _ => (),
                };

                self.prompt_user(os, skip_printing_tools).await
            },
            ChatState::HandleInput { input } => {
                tokio::select! {
                    res = self.handle_input(os, input) => res,
                    Ok(_) = ctrl_c_stream.recv() => Err(ChatError::Interrupted { tool_uses: Some(self.tool_uses.clone()) })
                }
            },
            ChatState::CompactHistory {
                prompt,
                show_summary,
                strategy,
            } => {
                // compact_history manages ctrl+c handling
                self.compact_history(os, prompt, show_summary, strategy).await
            },
            ChatState::ExecuteTools => {
                let tool_uses_clone = self.tool_uses.clone();
                tokio::select! {
                    res = self.tool_use_execute(os) => res,
                    Ok(_) = ctrl_c_stream.recv() => Err(ChatError::Interrupted { tool_uses: Some(tool_uses_clone) })
                }
            },
            ChatState::ValidateTools { tool_uses } => {
                tokio::select! {
                    res = self.validate_tools(os, tool_uses) => res,
                    Ok(_) = ctrl_c_stream.recv() => Err(ChatError::Interrupted { tool_uses: None })
                }
            },
            ChatState::HandleResponseStream(conversation_state) => {
                let request_metadata: Arc<Mutex<Option<RequestMetadata>>> = Arc::new(Mutex::new(None));
                let request_metadata_clone = Arc::clone(&request_metadata);

                tokio::select! {
                    res = self.handle_response(os, conversation_state, request_metadata_clone) => res,
                    Ok(_) = ctrl_c_stream.recv() => {
                        debug!(?request_metadata, "ctrlc received");
                        // Wait for handle_response to finish handling the ctrlc.
                        tokio::time::sleep(Duration::from_millis(5)).await;
                        if let Some(request_metadata) = request_metadata.lock().await.take() {
                            self.user_turn_request_metadata.push(request_metadata);
                        }
                        self.send_chat_telemetry(os, TelemetryResult::Cancelled, None, None, None, true).await;
                        Err(ChatError::Interrupted { tool_uses: None })
                    }
                }
            },
            ChatState::RetryModelOverload => tokio::select! {
                res = self.retry_model_overload(os) => res,
                Ok(_) = ctrl_c_stream.recv() => {
                    Err(ChatError::Interrupted { tool_uses: None })
                }
            },
            ChatState::Exit => return Ok(()),
        };

        let err = match result {
            Ok(state) => {
                self.inner = Some(state);
                return Ok(());
            },
            Err(err) => err,
        };

        // We encountered an error. Handle it.
        error!(?err, "An error occurred processing the current state");
        
        if self.output_format == OutputFormat::Json {
            use crate::cli::chat::stream_json::{StreamEvent, ResultEvent, ResultSubtype};
            let event = StreamEvent::Result(ResultEvent {
                subtype: ResultSubtype::Error,
                duration_ms: None,
                total_cost_usd: None,
                usage: None,
                error: Some(format!("{:?}", err)),
            });
            let _ = stream_json::emit_event(&event);
        }
        
        let (reason, reason_desc) = get_error_reason(&err);
        self.send_error_telemetry(os, reason, Some(reason_desc), err.status_code())
            .await;

        if self.spinner.is_some() {
            drop(self.spinner.take());
            queue!(
                self.stderr,
                terminal::Clear(terminal::ClearType::CurrentLine),
                cursor::MoveToColumn(0),
            )?;
        }

        let (context, report, display_err_message) = match err {
            ChatError::Interrupted { tool_uses: ref inter } => {
                execute!(self.stderr, style::Print("\n\n"))?;

                // If there was an interrupt during tool execution, then we add fake
                // messages to "reset" the chat state.
                match inter {
                    Some(tool_uses) if !tool_uses.is_empty() => {
                        self.conversation
                            .abandon_tool_use(tool_uses, "The user interrupted the tool execution.".to_string());
                        let _ = self
                            .conversation
                            .as_sendable_conversation_state(os, &mut self.stderr, false)
                            .await?;
                        self.conversation.push_assistant_message(
                            os,
                            AssistantMessage::new_response(
                                None,
                                "Tool uses were interrupted, waiting for the next user prompt".to_string(),
                            ),
                            None,
                        );
                    },
                    _ => (),
                }

                ("Tool use was interrupted", Report::from(err), false)
            },
            ChatError::CompactHistoryFailure => {
                // This error is not retryable - the user must take manual intervention to manage
                // their context.
                execute!(
                    self.stderr,
                    style::SetForegroundColor(Color::Red),
                    style::Print("Your conversation is too large to continue.\n"),
                    style::SetForegroundColor(Color::Reset),
                    style::Print(format!(
                        "• Run {} to compact your conversation. See {} for compaction options\n",
                        "/compact".green(),
                        "/compact --help".green()
                    )),
                    style::Print(format!("• Run {} to analyze your context usage\n", "/usage".green())),
                    style::Print(format!("• Run {} to reset your conversation state\n", "/clear".green())),
                    style::SetAttribute(Attribute::Reset),
                    style::Print("\n\n"),
                )?;
                ("Unable to compact the conversation history", eyre!(err), true)
            },
            ChatError::SendMessage(err) => match err.source {
                // Errors from attempting to send too large of a conversation history. In
                // this case, attempt to automatically compact the history for the user.
                ApiClientError::ContextWindowOverflow { .. } => {
                    if os
                        .database
                        .settings
                        .get_bool(Setting::ChatDisableAutoCompaction)
                        .unwrap_or(false)
                    {
                        execute!(
                            self.stderr,
                            style::SetForegroundColor(Color::Red),
                            style::Print("The conversation history has overflowed.\n"),
                            style::SetForegroundColor(Color::Reset),
                            style::Print(format!("• Run {} to compact your conversation\n", "/compact".green())),
                            style::SetAttribute(Attribute::Reset),
                            style::Print("\n\n"),
                        )?;
                        ("The conversation history has overflowed", eyre!(err), false)
                    } else {
                        self.inner = Some(ChatState::CompactHistory {
                            prompt: None,
                            show_summary: false,
                            strategy: CompactStrategy {
                                truncate_large_messages: self.conversation.history().len() <= 2,
                                max_message_length: if self.conversation.history().len() <= 2 {
                                    25_000
                                } else {
                                    Default::default()
                                },
                                ..Default::default()
                            },
                        });

                        execute!(
                            self.stdout,
                            style::SetForegroundColor(Color::Yellow),
                            style::Print("The context window has overflowed, summarizing the history..."),
                            style::SetAttribute(Attribute::Reset),
                            style::Print("\n\n"),
                        )?;

                        return Ok(());
                    }
                },
                ApiClientError::QuotaBreach {
                    message: _,
                    status_code: _,
                } => {
                    let err = "Request quota exceeded. Please wait a moment and try again.".to_string();
                    self.conversation.append_transcript(err.clone());
                    execute!(
                        self.stderr,
                        style::SetAttribute(Attribute::Bold),
                        style::SetForegroundColor(Color::Red),
                        style::Print(error_messages::RATE_LIMIT_PREFIX),
                        style::Print("\n"),
                        style::Print(format!("    {}\n\n", err.clone())),
                        style::SetAttribute(Attribute::Reset),
                        style::SetForegroundColor(Color::Reset),
                    )?;
                    (error_messages::TROUBLE_RESPONDING, eyre!(err), false)
                },
                ApiClientError::ModelOverloadedError { request_id, .. } => {
                    if self.interactive {
                        execute!(
                            self.stderr,
                            style::SetAttribute(Attribute::Bold),
                            style::SetForegroundColor(Color::Red),
                            style::Print(
                                "\nThe model you've selected is temporarily unavailable. Please select a different model.\n"
                            ),
                            style::SetAttribute(Attribute::Reset),
                            style::SetForegroundColor(Color::Reset),
                        )?;

                        if let Some(id) = request_id {
                            self.conversation
                                .append_transcript(format!("Model unavailable (Request ID: {})", id));
                        }

                        self.inner = Some(ChatState::RetryModelOverload);

                        return Ok(());
                    }

                    // non-interactive throws this error
                    let model_instruction = "Please relaunch with '--model <model_id>' to use a different model.";
                    let err = format!(
                        "The model you've selected is temporarily unavailable. {}{}\n\n",
                        model_instruction,
                        match request_id {
                            Some(id) => format!("\n    Request ID: {}", id),
                            None => "".to_owned(),
                        }
                    );
                    self.conversation.append_transcript(err.clone());
                    execute!(
                        self.stderr,
                        style::SetAttribute(Attribute::Bold),
                        style::SetForegroundColor(Color::Red),
                        style::Print(format!("{}:\n", error_messages::TROUBLE_RESPONDING)),
                        style::Print(format!("    {}\n", err.clone())),
                        style::SetAttribute(Attribute::Reset),
                        style::SetForegroundColor(Color::Reset),
                    )?;
                    (error_messages::TROUBLE_RESPONDING, eyre!(err), false)
                },
                ApiClientError::MonthlyLimitReached { .. } => {
                    let subscription_status = get_subscription_status(os).await;
                    if subscription_status.is_err() {
                        execute!(
                            self.stderr,
                            style::SetForegroundColor(Color::Red),
                            style::Print(format!(
                                "Unable to verify subscription status: {}\n\n",
                                subscription_status.as_ref().err().unwrap()
                            )),
                            style::SetForegroundColor(Color::Reset),
                        )?;
                    }

                    execute!(
                        self.stderr,
                        style::SetForegroundColor(Color::Yellow),
                        style::Print("Monthly request limit reached"),
                        style::SetForegroundColor(Color::Reset),
                    )?;

                    let limits_text = format!(
                        "The limits reset on {:02}/01.",
                        OffsetDateTime::now_utc().month().next() as u8
                    );

                    if subscription_status.is_err()
                        || subscription_status.is_ok_and(|s| s == ActualSubscriptionStatus::None)
                    {
                        execute!(
                            self.stderr,
                            style::Print(format!("\n\n{LIMIT_REACHED_TEXT} {limits_text}")),
                            style::SetForegroundColor(Color::DarkGrey),
                            style::Print("\n\nUse "),
                            style::SetForegroundColor(Color::Green),
                            style::Print("/subscribe"),
                            style::SetForegroundColor(Color::DarkGrey),
                            style::Print(" to upgrade your subscription.\n\n"),
                            style::SetForegroundColor(Color::Reset),
                        )?;
                    } else {
                        execute!(
                            self.stderr,
                            style::SetForegroundColor(Color::Yellow),
                            style::Print(format!(" - {limits_text}\n\n")),
                            style::SetForegroundColor(Color::Reset),
                        )?;
                    }

                    self.inner = Some(ChatState::PromptUser {
                        skip_printing_tools: false,
                    });

                    return Ok(());
                },
                _ => (error_messages::TROUBLE_RESPONDING, Report::from(err), true),
            },
            _ => (error_messages::TROUBLE_RESPONDING, Report::from(err), true),
        };

        if display_err_message {
            // Remove non-ASCII and ANSI characters.
            let re = Regex::new(r"((\x9B|\x1B\[)[0-?]*[ -\/]*[@-~])|([^\x00-\x7F]+)").unwrap();

            queue!(
                self.stderr,
                style::SetAttribute(Attribute::Bold),
                style::SetForegroundColor(Color::Red),
            )?;

            let text = re.replace_all(&format!("{}: {:?}\n", context, report), "").into_owned();

            queue!(self.stderr, style::Print(&text),)?;
            self.conversation.append_transcript(text);

            execute!(
                self.stderr,
                style::SetAttribute(Attribute::Reset),
                style::SetForegroundColor(Color::Reset),
            )?;
        }

        self.conversation.enforce_conversation_invariants();
        self.conversation.reset_next_user_message();
        self.pending_tool_index = None;
        self.tool_turn_start_time = None;
        self.reset_user_turn();

        self.inner = Some(ChatState::PromptUser {
            skip_printing_tools: false,
        });

        Ok(())
    }

    async fn show_changelog_announcement(&mut self, os: &mut Os) -> Result<()> {
        let current_version = env!("CARGO_PKG_VERSION");
        let last_version = os.database.get_changelog_last_version()?;
        let show_count = os.database.get_changelog_show_count()?.unwrap_or(0);

        // Check if version changed or if we haven't shown it max times yet
        let should_show = match &last_version {
            Some(last) if last == current_version => show_count < CHANGELOG_MAX_SHOW_COUNT,
            _ => true, // New version or no previous version
        };

        if should_show {
            // Use the shared rendering function
            ui::render_changelog_content(&mut self.stderr)?;

            // Update the database entries
            os.database.set_changelog_last_version(current_version)?;
            let new_count = if last_version.as_deref() == Some(current_version) {
                show_count + 1
            } else {
                1
            };
            os.database.set_changelog_show_count(new_count)?;
        }

        Ok(())
    }

    /// Reload built-in tools to reflect experiment changes while preserving MCP tools
    pub async fn reload_builtin_tools(&mut self, os: &mut Os) -> Result<(), ChatError> {
        self.conversation
            .reload_builtin_tools(os, &mut self.stderr)
            .await
            .map_err(|e| ChatError::Custom(format!("Failed to update tool spec: {e}").into()))
    }
}

impl Drop for ChatSession {
    fn drop(&mut self) {
        if let Some(spinner) = &mut self.spinner {
            spinner.stop();
        }

        execute!(
            self.stderr,
            cursor::MoveToColumn(0),
            style::SetAttribute(Attribute::Reset),
            style::ResetColor,
            cursor::Show
        )
        .ok();
    }
}

/// The chat execution state.
///
/// Intended to provide more robust handling around state transitions while dealing with, e.g.,
/// tool validation, execution, response stream handling, etc.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum ChatState {
    /// Prompt the user with `tool_uses`, if available.
    PromptUser {
        /// Used to avoid displaying the tool info at inappropriate times, e.g. after clear or help
        /// commands.
        skip_printing_tools: bool,
    },
    /// Handle the user input, depending on if any tools require execution.
    HandleInput { input: String },
    /// Validate the list of tool uses provided by the model.
    ValidateTools { tool_uses: Vec<AssistantToolUse> },
    /// Execute the list of tools.
    ExecuteTools,
    /// Consume the response stream and display to the user.
    HandleResponseStream(crate::api_client::model::ConversationState),
    /// Compact the chat history.
    CompactHistory {
        /// Custom prompt to include as part of history compaction.
        prompt: Option<String>,
        /// Whether or not the summary should be shown on compact success.
        show_summary: bool,
        /// Parameters for how to perform the compaction request.
        strategy: CompactStrategy,
    },
    /// Retry the current request if we encounter a model overloaded error.
    RetryModelOverload,
    /// Exit the chat.
    Exit,
}

impl Default for ChatState {
    fn default() -> Self {
        Self::PromptUser {
            skip_printing_tools: false,
        }
    }
}

impl ChatSession {
    /// Sends a request to the SendMessage API. Emits error telemetry on failure.
    async fn send_message(
        &mut self,
        os: &mut Os,
        conversation_state: api_client::model::ConversationState,
        request_metadata_lock: Arc<Mutex<Option<RequestMetadata>>>,
        message_meta_tags: Option<Vec<MessageMetaTag>>,
    ) -> Result<SendMessageStream, ChatError> {
        match SendMessageStream::send_message(&os.client, conversation_state, request_metadata_lock, message_meta_tags)
            .await
        {
            Ok(res) => Ok(res),
            Err(err) => {
                let (reason, reason_desc) = get_error_reason(&err);
                self.send_chat_telemetry(
                    os,
                    TelemetryResult::Failed,
                    Some(reason),
                    Some(reason_desc),
                    err.status_code(),
                    true, // We never retry failed requests, so this always ends the current turn.
                )
                .await;
                Err(err.into())
            },
        }
    }

    async fn spawn(&mut self, os: &mut Os) -> Result<()> {
        if self.output_format == OutputFormat::Json {
            use crate::cli::chat::stream_json::{StreamEvent, SystemEvent};
            let event = StreamEvent::System(SystemEvent {
                subtype: "init".to_string(),
                session_id: Some(self.conversation.conversation_id().to_string()),
                model: self.conversation.model_info.as_ref().map(|m| m.model_id.clone()),
            });
            stream_json::emit_event(&event).ok();
        } else {
            let is_small_screen = self.terminal_width() < GREETING_BREAK_POINT;
            if os
                .database
                .settings
                .get_bool(Setting::ChatGreetingEnabled)
                .unwrap_or(true)
            {
            let welcome_text = match self.existing_conversation {
                true => RESUME_TEXT,
                false => match is_small_screen {
                    true => SMALL_SCREEN_WELCOME,
                    false => WELCOME_TEXT,
                },
            };

            execute!(self.stderr, style::Print(welcome_text), style::Print("\n\n"),)?;

            let tip = ROTATING_TIPS[usize::try_from(rand::random::<u32>()).unwrap_or(0) % ROTATING_TIPS.len()];
            if is_small_screen {
                // If the screen is small, print the tip in a single line
                execute!(
                    self.stderr,
                    style::Print("💡 ".to_string()),
                    style::Print(tip),
                    style::Print("\n")
                )?;
            } else {
                draw_box(
                    &mut self.stderr,
                    "Did you know?",
                    tip,
                    GREETING_BREAK_POINT,
                    Color::DarkGrey,
                )?;
            }

            execute!(
                self.stderr,
                style::Print("\n"),
                style::Print(match is_small_screen {
                    true => SMALL_SCREEN_POPULAR_SHORTCUTS,
                    false => POPULAR_SHORTCUTS,
                }),
                style::Print("\n"),
                style::Print(
                    "━"
                        .repeat(if is_small_screen { 0 } else { GREETING_BREAK_POINT })
                        .dark_grey()
                )
            )?;
            execute!(self.stderr, style::Print("\n"), style::SetForegroundColor(Color::Reset))?;
        }

        // Check if we should show the whats-new announcement
        self.show_changelog_announcement(os).await?;

        if self.all_tools_trusted() {
            let is_small_screen = self.terminal_width() < GREETING_BREAK_POINT;
            queue!(
                self.stderr,
                style::Print(format!(
                    "{}{}\n\n",
                    trust_all_text(),
                    if !is_small_screen { "\n" } else { "" }
                ))
            )?;
        }
        }
        if let Some(agent) = self.conversation.agents.get_active() {
            agent.print_overridden_permissions(&mut self.stderr)?;
        }

        self.stderr.flush()?;

        if self.output_format == OutputFormat::Plain {
            if let Some(ref model_info) = self.conversation.model_info {
                let (models, _default_model) = get_available_models(os).await?;
                if let Some(model_option) = models.iter().find(|option| option.model_id == model_info.model_id) {
                    let display_name = model_option.model_name.as_deref().unwrap_or(&model_option.model_id);
                    execute!(
                        self.stderr,
                        style::SetForegroundColor(Color::Cyan),
                        style::Print(format!("🤖 You are chatting with {}\n", display_name)),
                        style::SetForegroundColor(Color::Reset),
                        style::Print("\n")
                    )?;
                }
            }
        }

        // Initialize capturing if possible
        if ExperimentManager::is_enabled(os, ExperimentName::Checkpoint) {
            let path = get_shadow_repo_dir(os, self.conversation.conversation_id().to_string())?;
            let start = std::time::Instant::now();
            let checkpoint_manager = match CheckpointManager::auto_init(os, &path, self.conversation.history()).await {
                Ok(manager) => {
                    execute!(
                        self.stderr,
                        style::Print(
                            format!(
                                "📷 Checkpoints are enabled! (took {:.2}s)\n\n",
                                start.elapsed().as_secs_f32()
                            )
                            .blue()
                            .bold()
                        )
                    )?;
                    Some(manager)
                },
                Err(e) => {
                    execute!(self.stderr, style::Print(format!("{e}\n\n").blue()))?;
                    None
                },
            };
            self.conversation.checkpoint_manager = checkpoint_manager;
        }

        if let Some(user_input) = self.initial_input.take() {
            self.inner = Some(ChatState::HandleInput { input: user_input });
        }

        while !matches!(self.inner, Some(ChatState::Exit)) {
            self.next(os).await?;
        }

        Ok(())
    }

    /// Compacts the conversation history using the strategy specified by [CompactStrategy],
    /// replacing the history with a summary generated by the model.
    ///
    /// If the compact request itself fails, it will be retried depending on [CompactStrategy]
    ///
    /// If [CompactStrategy::messages_to_exclude] is greater than 0, and
    /// [CompactStrategy::truncate_large_messages] is true, then compaction will not be retried and
    /// will fail with [ChatError::CompactHistoryFailure].
    async fn compact_history(
        &mut self,
        os: &mut Os,
        custom_prompt: Option<String>,
        show_summary: bool,
        strategy: CompactStrategy,
    ) -> Result<ChatState, ChatError> {
        // Same pattern as is done for handle_response for getting request metadata on sigint.
        let request_metadata: Arc<Mutex<Option<RequestMetadata>>> = Arc::new(Mutex::new(None));
        let request_metadata_clone = Arc::clone(&request_metadata);
        let mut ctrl_c_stream = self.ctrlc_rx.resubscribe();

        tokio::select! {
            res = self.compact_history_impl(os, custom_prompt, show_summary, strategy, request_metadata_clone) => res,
            Ok(_) = ctrl_c_stream.recv() => {
                debug!(?request_metadata, "ctrlc received in compact history");
                // Wait for handle_response to finish handling the ctrlc.
                tokio::time::sleep(Duration::from_millis(5)).await;
                if let Some(request_metadata) = request_metadata.lock().await.take() {
                    self.user_turn_request_metadata.push(request_metadata);
                }
                self.send_chat_telemetry(
                    os,
                    TelemetryResult::Cancelled,
                    None,
                    None,
                    None,
                    true,
                )
                .await;
                Err(ChatError::Interrupted { tool_uses: Some(self.tool_uses.clone()) })
            }
        }
    }

    async fn compact_history_impl(
        &mut self,
        os: &mut Os,
        custom_prompt: Option<String>,
        show_summary: bool,
        strategy: CompactStrategy,
        request_metadata_lock: Arc<Mutex<Option<RequestMetadata>>>,
    ) -> Result<ChatState, ChatError> {
        let hist = self.conversation.history();
        debug!(?strategy, ?hist, "compacting history");

        if self.conversation.history().is_empty() {
            execute!(
                self.stderr,
                style::SetForegroundColor(Color::Yellow),
                style::Print("\nConversation too short to compact.\n\n"),
                style::SetForegroundColor(Color::Reset)
            )?;

            return Ok(ChatState::PromptUser {
                skip_printing_tools: true,
            });
        }

        if strategy.truncate_large_messages {
            info!("truncating large messages");
            execute!(
                self.stderr,
                terminal::Clear(terminal::ClearType::CurrentLine),
                cursor::MoveToColumn(0),
                style::SetForegroundColor(Color::Yellow),
                style::Print("Truncating large messages..."),
                style::SetAttribute(Attribute::Reset),
                style::Print("\n\n"),
            )?;
        }

        let summary_state = self
            .conversation
            .create_summary_request(os, custom_prompt.as_ref(), strategy)
            .await?;

        if self.interactive {
            execute!(self.stderr, cursor::Hide, style::Print("\n"))?;
            self.spinner = Some(Spinner::new(Spinners::Dots, "Creating summary...".to_string()));
        }

        let mut response = match self
            .send_message(
                os,
                summary_state,
                request_metadata_lock,
                Some(vec![MessageMetaTag::Compact]),
            )
            .await
        {
            Ok(res) => res,
            Err(err) => {
                if self.interactive {
                    self.spinner.take();
                    execute!(
                        self.stderr,
                        terminal::Clear(terminal::ClearType::CurrentLine),
                        cursor::MoveToColumn(0),
                        style::SetAttribute(Attribute::Reset)
                    )?;
                }

                // If the request fails due to context window overflow, then we'll see if it's
                // retryable according to the passed strategy.
                let history_len = self.conversation.history().len();
                match err {
                    ChatError::SendMessage(err)
                        if matches!(err.source, ApiClientError::ContextWindowOverflow { .. }) =>
                    {
                        error!(?strategy, "failed to send compaction request");
                        // If there's only two messages in the history, we have no choice but to
                        // truncate it. We use two messages since it's almost guaranteed to contain:
                        // 1. A small user prompt
                        // 2. A large user tool use result
                        if history_len <= 2 && !strategy.truncate_large_messages {
                            return Ok(ChatState::CompactHistory {
                                prompt: custom_prompt,
                                show_summary,
                                strategy: CompactStrategy {
                                    truncate_large_messages: true,
                                    max_message_length: 25_000,
                                    messages_to_exclude: 0,
                                },
                            });
                        }

                        // Otherwise, we will first exclude the most recent message, and only then
                        // truncate. If both of these have already been set, then return an error.
                        if history_len > 2 && strategy.messages_to_exclude < 1 {
                            return Ok(ChatState::CompactHistory {
                                prompt: custom_prompt,
                                show_summary,
                                strategy: CompactStrategy {
                                    messages_to_exclude: 1,
                                    ..strategy
                                },
                            });
                        } else if !strategy.truncate_large_messages {
                            return Ok(ChatState::CompactHistory {
                                prompt: custom_prompt,
                                show_summary,
                                strategy: CompactStrategy {
                                    truncate_large_messages: true,
                                    max_message_length: 25_000,
                                    ..strategy
                                },
                            });
                        } else {
                            return Err(ChatError::CompactHistoryFailure);
                        }
                    },
                    err => return Err(err),
                }
            },
        };

        let (summary, request_metadata) = {
            loop {
                match response.recv().await {
                    Some(Ok(parser::ResponseEvent::EndStream {
                        message,
                        request_metadata,
                    })) => {
                        self.user_turn_request_metadata.push(request_metadata.clone());
                        break (message.content().to_string(), request_metadata);
                    },
                    Some(Ok(_)) => (),
                    Some(Err(err)) => {
                        if let Some(request_id) = &err.request_metadata.request_id {
                            self.failed_request_ids.push(request_id.clone());
                        };

                        self.user_turn_request_metadata.push(err.request_metadata.clone());

                        let (reason, reason_desc) = get_error_reason(&err);
                        self.send_chat_telemetry(
                            os,
                            TelemetryResult::Failed,
                            Some(reason),
                            Some(reason_desc),
                            err.status_code(),
                            true,
                        )
                        .await;

                        return Err(err.into());
                    },
                    None => {
                        error!("response stream receiver closed before receiving a stop event");
                        return Err(ChatError::Custom("Stream failed during compaction".into()));
                    },
                }
            }
        };

        if self.spinner.is_some() {
            drop(self.spinner.take());
            queue!(
                self.stderr,
                terminal::Clear(terminal::ClearType::CurrentLine),
                cursor::MoveToColumn(0),
                cursor::Show
            )?;
        }

        self.conversation
            .replace_history_with_summary(summary.clone(), strategy, request_metadata);

        // If a next message is set, then retry the request.
        let should_retry = self.conversation.next_user_message().is_some();

        // If we retry, then don't end the current turn.
        self.send_chat_telemetry(os, TelemetryResult::Succeeded, None, None, None, !should_retry)
            .await;

        // Print output to the user.
        {
            execute!(
                self.stderr,
                style::SetForegroundColor(Color::Green),
                style::Print("✔ Conversation history has been compacted successfully!\n\n"),
                style::SetForegroundColor(Color::DarkGrey)
            )?;

            let mut output = Vec::new();
            if let Some(custom_prompt) = &custom_prompt {
                execute!(
                    output,
                    style::Print(format!("• Custom prompt applied: {}\n", custom_prompt))
                )?;
            }
            animate_output(&mut self.stderr, &output)?;

            // Display the summary if the show_summary flag is set
            if show_summary {
                // Add a border around the summary for better visual separation
                let terminal_width = self.terminal_width();
                let border = "═".repeat(terminal_width.min(80));
                execute!(
                    self.stderr,
                    style::Print("\n"),
                    style::SetForegroundColor(Color::Cyan),
                    style::Print(&border),
                    style::Print("\n"),
                    style::SetAttribute(Attribute::Bold),
                    style::Print("                       CONVERSATION SUMMARY"),
                    style::Print("\n"),
                    style::Print(&border),
                    style::SetAttribute(Attribute::Reset),
                    style::Print("\n\n"),
                )?;

                execute!(
                    output,
                    style::Print(&summary),
                    style::Print("\n\n"),
                    style::SetForegroundColor(Color::Cyan),
                    style::Print("The conversation history has been replaced with this summary.\n"),
                    style::Print("It contains all important details from previous interactions.\n"),
                )?;
                animate_output(&mut self.stderr, &output)?;

                execute!(
                    self.stderr,
                    style::Print(&border),
                    style::Print("\n\n"),
                    style::SetForegroundColor(Color::Reset)
                )?;
            }
        }

        if should_retry {
            Ok(ChatState::HandleResponseStream(
                self.conversation
                    .as_sendable_conversation_state(os, &mut self.stderr, false)
                    .await?,
            ))
        } else {
            // Otherwise, return back to the prompt for any pending tool uses.
            Ok(ChatState::PromptUser {
                skip_printing_tools: true,
            })
        }
    }

    /// Generates a custom agent configuration (system prompt and tool config) based on user input.
    /// Uses an LLM to create the agent specifications from the provided name and description.
    async fn generate_agent_config(
        &mut self,
        os: &mut Os,
        agent_name: &str,
        agent_description: &str,
        selected_servers: &str,
        schema: &str,
        is_global: bool,
    ) -> Result<ChatState, ChatError> {
        // Same pattern as compact_history for handling ctrl+c interruption
        let request_metadata: Arc<Mutex<Option<RequestMetadata>>> = Arc::new(Mutex::new(None));
        let request_metadata_clone = Arc::clone(&request_metadata);
        let mut ctrl_c_stream = self.ctrlc_rx.resubscribe();

        tokio::select! {
            res = self.generate_agent_config_impl(os, agent_name, agent_description, selected_servers, schema, is_global, request_metadata_clone) => res,
            Ok(_) = ctrl_c_stream.recv() => {
                debug!(?request_metadata, "ctrlc received in generate agent config");
                // Wait for handle_response to finish handling the ctrlc.
                tokio::time::sleep(Duration::from_millis(5)).await;
                if let Some(request_metadata) = request_metadata.lock().await.take() {
                    self.user_turn_request_metadata.push(request_metadata);
                }
                self.send_chat_telemetry(
                    os,
                    TelemetryResult::Cancelled,
                    None,
                    None,
                    None,
                    true,
                )
                .await;
                Err(ChatError::Interrupted { tool_uses: Some(self.tool_uses.clone()) })
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn generate_agent_config_impl(
        &mut self,
        os: &mut Os,
        agent_name: &str,
        agent_description: &str,
        selected_servers: &str,
        schema: &str,
        is_global: bool,
        request_metadata_lock: Arc<Mutex<Option<RequestMetadata>>>,
    ) -> Result<ChatState, ChatError> {
        debug!(?agent_name, ?agent_description, "generating agent config");

        if agent_name.trim().is_empty() || agent_description.trim().is_empty() {
            execute!(
                self.stderr,
                style::SetForegroundColor(Color::Yellow),
                style::Print("\nAgent name and description cannot be empty.\n\n"),
                style::SetForegroundColor(Color::Reset)
            )?;

            return Ok(ChatState::PromptUser {
                skip_printing_tools: true,
            });
        }

        let prepopulated_agent = Agent {
            name: agent_name.to_string(),
            description: Some(agent_description.to_string()),
            ..Default::default()
        };
        let prepopulated_content = prepopulated_agent
            .to_str_pretty()
            .map_err(|e| ChatError::Custom(format!("Error prepopulating agent fields: {}", e).into()))?;

        // Create the agent generation request - this now works!
        let generation_state = self
            .conversation
            .create_agent_generation_request(
                agent_name,
                agent_description,
                selected_servers,
                schema,
                prepopulated_content.as_str(),
            )
            .await?;

        if self.interactive {
            execute!(self.stderr, cursor::Hide, style::Print("\n"))?;
            self.spinner = Some(Spinner::new(
                Spinners::Dots,
                format!("Generating agent config for '{}'...", agent_name),
            ));
        }

        let mut response = match self
            .send_message(
                os,
                generation_state,
                request_metadata_lock,
                Some(vec![MessageMetaTag::GenerateAgent]),
            )
            .await
        {
            Ok(res) => res,
            Err(err) => {
                if self.interactive {
                    self.spinner.take();
                    execute!(
                        self.stderr,
                        terminal::Clear(terminal::ClearType::CurrentLine),
                        cursor::MoveToColumn(0),
                        style::SetAttribute(Attribute::Reset)
                    )?;
                }
                return Err(err);
            },
        };

        let (agent_config_json, _request_metadata) = {
            loop {
                match response.recv().await {
                    Some(Ok(parser::ResponseEvent::EndStream {
                        message,
                        request_metadata,
                    })) => {
                        self.user_turn_request_metadata.push(request_metadata.clone());
                        break (message.content().to_string(), request_metadata);
                    },
                    Some(Ok(_)) => (),
                    Some(Err(err)) => {
                        if let Some(request_id) = &err.request_metadata.request_id {
                            self.failed_request_ids.push(request_id.clone());
                        }

                        self.user_turn_request_metadata.push(err.request_metadata.clone());

                        let (reason, reason_desc) = get_error_reason(&err);
                        self.send_chat_telemetry(
                            os,
                            TelemetryResult::Failed,
                            Some(reason),
                            Some(reason_desc),
                            err.status_code(),
                            true,
                        )
                        .await;

                        return Err(err.into());
                    },
                    None => {
                        error!("response stream receiver closed before receiving a stop event");
                        return Err(ChatError::Custom("Stream failed during agent generation".into()));
                    },
                }
            }
        };

        if self.spinner.is_some() {
            drop(self.spinner.take());
            queue!(
                self.stderr,
                terminal::Clear(terminal::ClearType::CurrentLine),
                cursor::MoveToColumn(0),
                cursor::Show
            )?;
        }
        // Parse and validate the initial generated config
        let initial_agent_config = match serde_json::from_str::<Agent>(&agent_config_json) {
            Ok(config) => config,
            Err(_) => {
                execute!(
                    self.stderr,
                    style::SetForegroundColor(Color::Red),
                    style::Print("✗ The LLM did not generate a valid agent configuration. Please try again.\n\n"),
                    style::SetForegroundColor(Color::Reset)
                )?;
                return Ok(ChatState::PromptUser {
                    skip_printing_tools: true,
                });
            },
        };

        let formatted_json = serde_json::to_string_pretty(&initial_agent_config)
            .map_err(|e| ChatError::Custom(format!("Failed to format JSON: {}", e).into()))?;

        let edited_content = open_editor(Some(formatted_json))?;

        // Parse and validate the edited config
        let final_agent_config = match serde_json::from_str::<Agent>(&edited_content) {
            Ok(config) => config,
            Err(err) => {
                execute!(
                    self.stderr,
                    style::SetForegroundColor(Color::Red),
                    style::Print(format!("✗ Invalid edited configuration: {}\n\n", err)),
                    style::SetForegroundColor(Color::Reset)
                )?;
                return Ok(ChatState::PromptUser {
                    skip_printing_tools: true,
                });
            },
        };

        // Save the final agent config to file
        if let Err(err) = save_agent_config(os, &final_agent_config, agent_name, is_global).await {
            execute!(
                self.stderr,
                style::SetForegroundColor(Color::Red),
                style::Print(format!("✗ Failed to save agent config: {}\n\n", err)),
                style::SetForegroundColor(Color::Reset)
            )?;
            return Err(err);
        }

        execute!(
            self.stderr,
            style::SetForegroundColor(Color::Green),
            style::Print(format!(
                "✓ Agent '{}' has been created and saved successfully!\n",
                agent_name
            )),
            style::SetForegroundColor(Color::Reset)
        )?;

        Ok(ChatState::PromptUser {
            skip_printing_tools: true,
        })
    }

    /// Read input from the user.
    async fn prompt_user(&mut self, os: &Os, skip_printing_tools: bool) -> Result<ChatState, ChatError> {
        execute!(self.stderr, cursor::Show)?;

        // Check token usage and display warnings if needed
        if self.pending_tool_index.is_none() {
            // Only display warnings when not waiting for tool approval
            if let Err(err) = self.display_char_warnings(os).await {
                warn!("Failed to display character limit warnings: {}", err);
            }
        }

        let show_tool_use_confirmation_dialog = !skip_printing_tools && self.pending_tool_index.is_some();
        if show_tool_use_confirmation_dialog {
            execute!(
                self.stderr,
                style::SetForegroundColor(Color::DarkGrey),
                style::Print("\nAllow this action? Use '"),
                style::SetForegroundColor(Color::Green),
                style::Print("t"),
                style::SetForegroundColor(Color::DarkGrey),
                style::Print("' to trust (always allow) this tool for the session. ["),
                style::SetForegroundColor(Color::Green),
                style::Print("y"),
                style::SetForegroundColor(Color::DarkGrey),
                style::Print("/"),
                style::SetForegroundColor(Color::Green),
                style::Print("n"),
                style::SetForegroundColor(Color::DarkGrey),
                style::Print("/"),
                style::SetForegroundColor(Color::Green),
                style::Print("t"),
                style::SetForegroundColor(Color::DarkGrey),
                style::Print("]:\n\n"),
                style::SetForegroundColor(Color::Reset),
            )?;
        }

        // Do this here so that the skim integration sees an updated view of the context *during the current
        // q session*. (e.g., if I add files to context, that won't show up for skim for the current
        // q session unless we do this in prompt_user... unless you can find a better way)
        #[cfg(unix)]
        if let Some(ref context_manager) = self.conversation.context_manager {
            use std::sync::Arc;

            use crate::cli::chat::consts::DUMMY_TOOL_NAME;

            let tool_names = self
                .conversation
                .tool_manager
                .tn_map
                .keys()
                .filter(|name| *name != DUMMY_TOOL_NAME)
                .cloned()
                .collect::<Vec<_>>();
            self.input_source
                .put_skim_command_selector(os, Arc::new(context_manager.clone()), tool_names);
        }

        execute!(
            self.stderr,
            style::SetForegroundColor(Color::Reset),
            style::SetAttribute(Attribute::Reset)
        )?;
        let prompt = self.generate_tool_trust_prompt(os).await;
        let user_input = match self.read_user_input(&prompt, false) {
            Some(input) => input,
            None => return Ok(ChatState::Exit),
        };

        self.conversation.append_user_transcript(&user_input);
        Ok(ChatState::HandleInput { input: user_input })
    }

    async fn handle_input(&mut self, os: &mut Os, mut user_input: String) -> Result<ChatState, ChatError> {
        queue!(self.stderr, style::Print('\n'))?;
        user_input = sanitize_unicode_tags(&user_input);
        let input = user_input.trim();

        if self.output_format == OutputFormat::Json {
            use crate::cli::chat::stream_json::{StreamEvent, UserEvent};
            let event = StreamEvent::User(UserEvent {
                content: input.to_string(),
            });
            stream_json::emit_event(&event).ok();
        }

        // handle image path
        if let Some(chat_state) = does_input_reference_file(input) {
            return Ok(chat_state);
        }
        if let Some(mut args) = input.strip_prefix("/").and_then(shlex::split) {
            // Required for printing errors correctly.
            let orig_args = args.clone();

            // We set the binary name as a dummy name "slash_command" which we
            // replace anytime we error out and print a usage statement.
            args.insert(0, "slash_command".to_owned());

            match SlashCommand::try_parse_from(args) {
                Ok(command) => {
                    let command_name = command.command_name().to_string();
                    let subcommand_name = command.subcommand_name().map(|s| s.to_string());

                    match command.execute(os, self).await {
                        Ok(chat_state) => {
                            let _ = self
                                .send_slash_command_telemetry(
                                    os,
                                    command_name,
                                    subcommand_name,
                                    TelemetryResult::Succeeded,
                                    None,
                                )
                                .await;

                            if matches!(chat_state, ChatState::Exit)
                                || matches!(chat_state, ChatState::HandleResponseStream(_))
                                || matches!(chat_state, ChatState::HandleInput { input: _ })
                                // TODO(bskiser): this is just a hotfix for handling state changes
                                // from manually running /compact, without impacting behavior of
                                // other slash commands.
                                || matches!(chat_state, ChatState::CompactHistory { .. })
                            {
                                return Ok(chat_state);
                            }
                        },
                        Err(err) => {
                            queue!(
                                self.stderr,
                                style::SetForegroundColor(Color::Red),
                                style::Print(format!("\nFailed to execute command: {}\n", err)),
                                style::SetForegroundColor(Color::Reset)
                            )?;
                            let _ = self
                                .send_slash_command_telemetry(
                                    os,
                                    command_name,
                                    subcommand_name,
                                    TelemetryResult::Failed,
                                    Some(err.to_string()),
                                )
                                .await;
                        },
                    }

                    writeln!(self.stderr)?;
                },
                Err(err) => {
                    // Replace the dummy name with a slash. Also have to check for an ansi sequence
                    // for invalid slash commands (e.g. on a "/doesntexist" input).
                    let ansi_output = err
                        .render()
                        .ansi()
                        .to_string()
                        .replace("slash_command ", "/")
                        .replace("slash_command\u{1b}[0m ", "/");

                    writeln!(self.stderr, "{}", ansi_output)?;

                    // Print the subcommand help, if available. Required since by default we won't
                    // show what the actual arguments are, requiring an unnecessary --help call.
                    if let clap::error::ErrorKind::InvalidValue
                    | clap::error::ErrorKind::UnknownArgument
                    | clap::error::ErrorKind::InvalidSubcommand
                    | clap::error::ErrorKind::MissingRequiredArgument = err.kind()
                    {
                        let mut cmd = SlashCommand::command();
                        for arg in &orig_args {
                            match cmd.find_subcommand(arg) {
                                Some(subcmd) => cmd = subcmd.clone(),
                                None => break,
                            }
                        }
                        let help = cmd.help_template("{all-args}").render_help();
                        writeln!(self.stderr, "{}", help.ansi())?;
                    }
                },
            }

            Ok(ChatState::PromptUser {
                skip_printing_tools: false,
            })
        } else if let Some(command) = input.strip_prefix("@") {
            let input_parts =
                shlex::split(command).ok_or(ChatError::Custom("Error splitting prompt command".into()))?;

            let mut iter = input_parts.into_iter();
            let prompt_name = iter
                .next()
                .ok_or(ChatError::Custom("Prompt name needs to be specified".into()))?;

            let args: Vec<String> = iter.collect();
            let arguments = if args.is_empty() { None } else { Some(args) };

            let subcommand = PromptsSubcommand::Get {
                orig_input: Some(command.to_string()),
                name: prompt_name,
                arguments,
            };
            return subcommand.execute(os, self).await;
        } else if let Some(command) = input.strip_prefix("!") {
            // Use platform-appropriate shell
            let result = if cfg!(target_os = "windows") {
                std::process::Command::new("cmd").args(["/C", command]).status()
            } else {
                std::process::Command::new("bash").args(["-c", command]).status()
            };

            // Handle the result and provide appropriate feedback
            match result {
                Ok(status) => {
                    if !status.success() {
                        queue!(
                            self.stderr,
                            style::SetForegroundColor(Color::Yellow),
                            style::Print(format!("Self exited with status: {}\n", status)),
                            style::SetForegroundColor(Color::Reset)
                        )?;
                    }
                },
                Err(e) => {
                    queue!(
                        self.stderr,
                        style::SetForegroundColor(Color::Red),
                        style::Print(format!("\nFailed to execute command: {}\n", e)),
                        style::SetForegroundColor(Color::Reset)
                    )?;
                },
            }

            Ok(ChatState::PromptUser {
                skip_printing_tools: false,
            })
        } else {
            // Track the message for checkpoint descriptions, but only if not already set
            // This prevents tool approval responses (y/n/t) from overwriting the original message
            if ExperimentManager::is_enabled(os, ExperimentName::Checkpoint) && !self.conversation.is_in_tangent_mode()
            {
                if let Some(manager) = self.conversation.checkpoint_manager.as_mut() {
                    if !manager.message_locked && self.pending_tool_index.is_none() {
                        manager.pending_user_message = Some(user_input.clone());
                        manager.message_locked = true;
                    }
                }
            }

            // Check for a pending tool approval
            if let Some(index) = self.pending_tool_index {
                let is_trust = ["t", "T"].contains(&input);
                let tool_use = &mut self.tool_uses[index];
                if ["y", "Y"].contains(&input) || is_trust {
                    if is_trust {
                        let formatted_tool_name = self
                            .conversation
                            .tool_manager
                            .tn_map
                            .get(&tool_use.name)
                            .map(|info| {
                                format!(
                                    "@{}{MCP_SERVER_TOOL_DELIMITER}{}",
                                    info.server_name, info.host_tool_name
                                )
                            })
                            .clone()
                            .unwrap_or(tool_use.name.clone());
                        self.conversation.agents.trust_tools(vec![formatted_tool_name]);

                        if let Some(agent) = self.conversation.agents.get_active() {
                            agent
                                .print_overridden_permissions(&mut self.stderr)
                                .map_err(|_e| ChatError::Custom("Failed to validate agent tool settings".into()))?;
                        }
                    }
                    tool_use.accepted = true;

                    return Ok(ChatState::ExecuteTools);
                }
            } else if !self.pending_prompts.is_empty() {
                let prompts = self.pending_prompts.drain(0..).collect();
                user_input = self
                    .conversation
                    .append_prompts(prompts)
                    .ok_or(ChatError::Custom("Prompt append failed".into()))?;
            }

            // Otherwise continue with normal chat on 'n' or other responses
            self.tool_use_status = ToolUseStatus::Idle;

            if self.pending_tool_index.is_some() {
                // If the user just enters "n", replace the message we send to the model with
                // something more substantial.
                // TODO: Update this flow to something that does *not* require two requests just to
                // get a meaningful response from the user - this is a short term solution before
                // we decide on a better flow.
                let user_input = if ["n", "N"].contains(&user_input.trim()) {
                    "I deny this tool request. Ask a follow up question clarifying the expected action".to_string()
                } else {
                    user_input
                };
                self.conversation.abandon_tool_use(&self.tool_uses, user_input);
            } else {
                self.conversation.set_next_user_message(user_input).await;
            }

            self.reset_user_turn();

            let conv_state = self
                .conversation
                .as_sendable_conversation_state(os, &mut self.stderr, true)
                .await?;
            self.send_tool_use_telemetry(os).await;

            queue!(self.stderr, style::SetForegroundColor(Color::Magenta))?;
            queue!(self.stderr, style::SetForegroundColor(Color::Reset))?;
            queue!(self.stderr, cursor::Hide)?;

            if self.interactive && self.output_format == OutputFormat::Plain {
                self.spinner = Some(Spinner::new(Spinners::Dots, "Thinking...".to_owned()));
            }

            Ok(ChatState::HandleResponseStream(conv_state))
        }
    }

    async fn tool_use_execute(&mut self, os: &mut Os) -> Result<ChatState, ChatError> {
        // Check if we should auto-enter tangent mode for introspect tool
        if ExperimentManager::is_enabled(os, ExperimentName::TangentMode)
            && os
                .database
                .settings
                .get_bool(Setting::IntrospectTangentMode)
                .unwrap_or(false)
            && !self.conversation.is_in_tangent_mode()
            && self
                .tool_uses
                .iter()
                .any(|tool| matches!(tool.tool, Tool::Introspect(_)))
        {
            self.conversation.enter_tangent_mode();
        }

        // Cache UI display flag to avoid borrow checker issues
        let show_ui = self.should_show_ui();

        // Verify tools have permissions.
        for i in 0..self.tool_uses.len() {
            let tool = &mut self.tool_uses[i];

            // Manually accepted by the user or otherwise verified already.
            if tool.accepted {
                continue;
            }

            let mut denied_match_set = None::<Vec<String>>;
            let allowed =
                self.conversation
                    .agents
                    .get_active()
                    .is_some_and(|a| match tool.tool.requires_acceptance(os, a) {
                        PermissionEvalResult::Allow => true,
                        PermissionEvalResult::Ask => false,
                        PermissionEvalResult::Deny(matches) => {
                            denied_match_set.replace(matches);
                            false
                        },
                    })
                    || self.conversation.agents.trust_all_tools;

            if let Some(match_set) = denied_match_set {
                let formatted_set = match_set.into_iter().fold(String::new(), |mut acc, rule| {
                    acc.push_str(&format!("\n  - {rule}"));
                    acc
                });

                if show_ui {
                    execute!(
                        self.stderr,
                        style::SetForegroundColor(Color::Red),
                        style::Print("Command "),
                        style::SetForegroundColor(Color::Yellow),
                        style::Print(&tool.name),
                        style::SetForegroundColor(Color::Red),
                        style::Print(" is rejected because it matches one or more rules on the denied list:"),
                        style::Print(formatted_set),
                        style::Print("\n"),
                        style::SetForegroundColor(Color::Reset),
                    )?;
                }

                return Ok(ChatState::HandleInput {
                    input: format!(
                        "Tool use with {} was rejected because the arguments supplied were forbidden",
                        tool.name
                    ),
                });
            }

            if os
                .database
                .settings
                .get_bool(Setting::ChatEnableNotifications)
                .unwrap_or(false)
            {
                play_notification_bell(!allowed);
            }

            // TODO: Control flow is hacky here because of borrow rules
            let _ = tool;
            if show_ui {
                self.print_tool_description(os, i, allowed).await?;
            }
            let tool = &mut self.tool_uses[i];

            if allowed {
                tool.accepted = true;
                self.tool_use_telemetry_events
                    .entry(tool.id.clone())
                    .and_modify(|ev| ev.is_trusted = true);
                continue;
            }

            self.pending_tool_index = Some(i);

            return Ok(ChatState::PromptUser {
                skip_printing_tools: false,
            });
        }

        // All tools are allowed now
        // Execute the requested tools.
        let mut tool_results = vec![];
        let mut image_blocks: Vec<RichImageBlock> = Vec::new();

        for tool in &self.tool_uses {
            let tool_start = std::time::Instant::now();
            let mut tool_telemetry = self.tool_use_telemetry_events.entry(tool.id.clone());
            tool_telemetry = tool_telemetry.and_modify(|ev| {
                ev.is_accepted = true;
            });

            // Extract AWS service name and operation name if available
            if let Some(additional_info) = tool.tool.get_additional_info() {
                if let Some(aws_service_name) = additional_info.get("aws_service_name").and_then(|v| v.as_str()) {
                    tool_telemetry =
                        tool_telemetry.and_modify(|ev| ev.aws_service_name = Some(aws_service_name.to_string()));
                }
                if let Some(aws_operation_name) = additional_info.get("aws_operation_name").and_then(|v| v.as_str()) {
                    tool_telemetry =
                        tool_telemetry.and_modify(|ev| ev.aws_operation_name = Some(aws_operation_name.to_string()));
                }
            }

            let invoke_result = tool
                .tool
                .invoke(
                    os,
                    &mut self.stdout,
                    &mut self.conversation.file_line_tracker,
                    &self.conversation.agents,
                )
                .await;

            if show_ui {
                if self.spinner.is_some() {
                    queue!(
                        self.stderr,
                        terminal::Clear(terminal::ClearType::CurrentLine),
                        cursor::MoveToColumn(0),
                        cursor::Show
                    )?;
                }
                execute!(self.stdout, style::Print("\n"))?;
            }

            // Handle checkpoint after tool execution - store tag for later display
            let checkpoint_tag: Option<String> = {
                if invoke_result.is_err()
                    || !ExperimentManager::is_enabled(os, ExperimentName::Checkpoint)
                    || self.conversation.is_in_tangent_mode()
                {
                    None
                }
                // Take manager out temporarily to avoid borrow conflicts
                else if let Some(mut manager) = self.conversation.checkpoint_manager.take() {
                    // Check if there are uncommitted changes
                    let has_changes = match manager.has_changes() {
                        Ok(b) => b,
                        Err(e) => {
                            execute!(
                                self.stderr,
                                style::SetForegroundColor(Color::Yellow),
                                style::Print(format!("Could not check if uncommitted changes exist: {e}\n")),
                                style::Print("Saving anyways...\n"),
                                style::SetForegroundColor(Color::Reset),
                            )?;
                            true
                        },
                    };
                    let tag = if has_changes {
                        // Generate tag for this tool use
                        let tool_tag = format!("{}.{}", manager.current_turn + 1, manager.tools_in_turn + 1);

                        // Get tool summary for commit message
                        let is_fs_read = matches!(&tool.tool, Tool::FsRead(_));
                        let description = if is_fs_read {
                            "External edits detected (likely manual change)".to_string()
                        } else {
                            match tool.tool.get_summary() {
                                Some(summary) => summary,
                                None => tool.tool.display_name(),
                            }
                        };

                        // Create tool checkpoint
                        if let Err(e) = manager.create_checkpoint(
                            &tool_tag,
                            &description,
                            &self.conversation.history().clone(),
                            false,
                            Some(tool.name.clone()),
                        ) {
                            debug!("Failed to create tool checkpoint: {}", e);
                            None
                        } else {
                            manager.tools_in_turn += 1;

                            // Also update/create the turn checkpoint to point to latest state
                            // This is important so that we create turn-checkpoints even when tools are aborted
                            let turn_tag = format!("{}", manager.current_turn + 1);
                            let turn_description = "Turn in progress".to_string();

                            if let Err(e) = manager.create_checkpoint(
                                &turn_tag,
                                &turn_description,
                                &self.conversation.history().clone(),
                                true,
                                None,
                            ) {
                                debug!("Failed to update turn checkpoint: {}", e);
                            }

                            Some(tool_tag)
                        }
                    } else {
                        None
                    };

                    // Put manager back
                    self.conversation.checkpoint_manager = Some(manager);
                    tag
                } else {
                    None
                }
            };

            let tool_end_time = Instant::now();
            let tool_time = tool_end_time.duration_since(tool_start);
            tool_telemetry = tool_telemetry.and_modify(|ev| {
                ev.execution_duration = Some(tool_time);
                ev.turn_duration = self.tool_turn_start_time.map(|t| tool_end_time.duration_since(t));
            });
            if let Tool::Custom(ct) = &tool.tool {
                tool_telemetry = tool_telemetry.and_modify(|ev| {
                    ev.is_custom_tool = true;
                    // legacy fields previously implemented for only MCP tools
                    ev.custom_tool_call_latency = Some(tool_time.as_secs() as usize);
                    ev.input_token_size = Some(ct.get_input_token_size());
                });
            }
            let tool_time = format!("{}.{}", tool_time.as_secs(), tool_time.subsec_millis());
            match invoke_result {
                Ok(result) => {
                    match result.output {
                        OutputKind::Text(ref text) => {
                            debug!("Output is Text: {}", text);
                        },
                        OutputKind::Json(ref json) => {
                            debug!("Output is JSON: {}", json);
                        },
                        OutputKind::Images(ref image) => {
                            image_blocks.extend(image.clone());
                        },
                        OutputKind::Mixed { ref text, ref images } => {
                            debug!("Output is Mixed: text = {:?}, images = {}", text, images.len());
                            image_blocks.extend(images.clone());
                        },
                    }

                    debug!("tool result output: {:#?}", result);
                    if show_ui {
                        execute!(
                            self.stdout,
                            style::Print(CONTINUATION_LINE),
                            style::Print("\n"),
                            style::SetForegroundColor(Color::Green),
                            style::SetAttribute(Attribute::Bold),
                            style::Print(format!(" ● Completed in {}s", tool_time)),
                            style::SetForegroundColor(Color::Reset),
                        )?;
                        if let Some(tag) = checkpoint_tag {
                            execute!(
                                self.stdout,
                                style::SetForegroundColor(Color::Blue),
                                style::SetAttribute(Attribute::Bold),
                                style::Print(format!(" [{tag}]")),
                                style::SetForegroundColor(Color::Reset),
                                style::SetAttribute(Attribute::Reset),
                            )?;
                        }
                        execute!(self.stdout, style::Print("\n\n"))?;
                    }

                    tool_telemetry = tool_telemetry.and_modify(|ev| ev.is_success = Some(true));
                    if let Tool::Custom(_) = &tool.tool {
                        tool_telemetry
                            .and_modify(|ev| ev.output_token_size = Some(TokenCounter::count_tokens(&result.as_str())));
                    }

                    // Send telemetry for agent contribution
                    if let Tool::FsWrite(w) = &tool.tool {
                        let sanitized_path_str = w.path(os).to_string_lossy().to_string();
                        let conversation_id = self.conversation.conversation_id().to_string();
                        let message_id = self.conversation.message_id().map(|s| s.to_string());
                        if let Some(tracker) = self.conversation.file_line_tracker.get_mut(&sanitized_path_str) {
                            let lines_by_agent = tracker.lines_by_agent();
                            let lines_by_user = tracker.lines_by_user();

                            os.telemetry
                                .send_agent_contribution_metric(
                                    &os.database,
                                    conversation_id,
                                    message_id,
                                    Some(tool.id.clone()),   // Already a String
                                    Some(tool.name.clone()), // Already a String
                                    Some(lines_by_agent),
                                    Some(lines_by_user),
                                )
                                .await
                                .ok();

                            tracker.prev_fswrite_lines = tracker.after_fswrite_lines;
                        }
                    }

                    tool_results.push(ToolUseResult {
                        tool_use_id: tool.id.clone(),
                        content: vec![result.into()],
                        status: ToolResultStatus::Success,
                    });
                },
                Err(err) => {
                    error!(?err, "An error occurred processing the tool");
                    if show_ui {
                        execute!(
                            self.stderr,
                            style::Print(CONTINUATION_LINE),
                            style::Print("\n"),
                            style::SetAttribute(Attribute::Bold),
                            style::SetForegroundColor(Color::Red),
                            style::Print(format!(" ● Execution failed after {}s:\n", tool_time)),
                            style::SetAttribute(Attribute::Reset),
                            style::SetForegroundColor(Color::Red),
                            style::Print(&err),
                            style::SetAttribute(Attribute::Reset),
                            style::Print("\n\n"),
                        )?;
                    }

                    tool_telemetry.and_modify(|ev| {
                        ev.is_success = Some(false);
                        ev.reason_desc = Some(err.to_string());
                    });
                    tool_results.push(ToolUseResult {
                        tool_use_id: tool.id.clone(),
                        content: vec![ToolUseResultBlock::Text(format!(
                            "An error occurred processing the tool: \n{}",
                            &err
                        ))],
                        status: ToolResultStatus::Error,
                    });
                    if let ToolUseStatus::Idle = self.tool_use_status {
                        self.tool_use_status = ToolUseStatus::RetryInProgress(
                            self.conversation
                                .message_id()
                                .map_or("No utterance id found".to_string(), |v| v.to_string()),
                        );
                    }
                },
            }
        }

        if self.output_format == OutputFormat::Json {
            use crate::cli::chat::stream_json::{StreamEvent, ToolResultEvent};
            for result in &tool_results {
                let content = result
                    .content
                    .iter()
                    .map(|block| match block {
                        ToolUseResultBlock::Text(text) => text.clone(),
                        ToolUseResultBlock::Json(json) => json.to_string(),
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                
                let event = StreamEvent::ToolResult(ToolResultEvent {
                    tool_use_id: result.tool_use_id.clone(),
                    content,
                    status: Some(match result.status {
                        ToolResultStatus::Success => "success".to_string(),
                        ToolResultStatus::Error => "error".to_string(),
                    }),
                });
                stream_json::emit_event(&event).ok();
            }
        }

        // Run PostToolUse hooks for all executed tools after we have the tool_results
        if let Some(cm) = self.conversation.context_manager.as_mut() {
            for result in &tool_results {
                if let Some(tool) = self.tool_uses.iter().find(|t| t.id == result.tool_use_id) {
                    let content: Vec<serde_json::Value> = result
                        .content
                        .iter()
                        .map(|block| match block {
                            ToolUseResultBlock::Text(text) => serde_json::Value::String(text.clone()),
                            ToolUseResultBlock::Json(json) => json.clone(),
                        })
                        .collect();

                    let tool_response = match result.status {
                        ToolResultStatus::Success => serde_json::json!({"success": true, "result": content}),
                        ToolResultStatus::Error => serde_json::json!({"success": false, "error": content}),
                    };

                    let tool_context = ToolContext {
                        tool_name: match &tool.tool {
                            Tool::Custom(custom_tool) => custom_tool.namespaced_tool_name(), /* for MCP tool, pass MCP name to the hook */
                            _ => tool.name.clone(),
                        },
                        tool_input: tool.tool_input.clone(),
                        tool_response: Some(tool_response),
                    };

                    // Here is how we handle postToolUse output:
                    // Exit code is 0: nothing. stdout is not shown to user. We don't support processing the PostToolUse
                    // hook output yet. Exit code is non-zero: display an error to user (already
                    // taken care of by the ContextManager.run_hooks)
                    let _ = cm
                        .run_hooks(
                            crate::cli::agent::hook::HookTrigger::PostToolUse,
                            &mut std::io::stderr(),
                            os,
                            None,
                            Some(tool_context),
                        )
                        .await;
                }
            }
        }

        if !image_blocks.is_empty() {
            let images = image_blocks.into_iter().map(|(block, _)| block).collect();
            self.conversation.add_tool_results_with_images(tool_results, images);
            execute!(
                self.stderr,
                style::SetAttribute(Attribute::Reset),
                style::SetForegroundColor(Color::Reset),
                style::Print("\n")
            )?;
        } else {
            self.conversation.add_tool_results(tool_results);
        }

        execute!(self.stderr, cursor::Hide)?;
        execute!(self.stderr, style::Print("\n"), style::SetAttribute(Attribute::Reset))?;
        if self.interactive && self.output_format == OutputFormat::Plain {
            self.spinner = Some(Spinner::new(Spinners::Dots, "Thinking...".to_string()));
        }

        self.send_chat_telemetry(os, TelemetryResult::Succeeded, None, None, None, false)
            .await;
        self.send_tool_use_telemetry(os).await;
        return Ok(ChatState::HandleResponseStream(
            self.conversation
                .as_sendable_conversation_state(os, &mut self.stderr, false)
                .await?,
        ));
    }

    /// Sends a [crate::api_client::ApiClient::send_message] request to the backend and consumes
    /// the response stream.
    ///
    /// In order to handle sigints while also keeping track of metadata about how the
    /// response stream was handled, we need an extra parameter:
    /// * `request_metadata_lock` - Updated with the [RequestMetadata] once it has been received
    ///   (either though a successful request, or on an error).
    async fn handle_response(
        &mut self,
        os: &mut Os,
        state: crate::api_client::model::ConversationState,
        request_metadata_lock: Arc<Mutex<Option<RequestMetadata>>>,
    ) -> Result<ChatState, ChatError> {
        let mut rx = self.send_message(os, state, request_metadata_lock, None).await?;

        let request_id = rx.request_id().map(String::from);

        let mut buf = String::new();
        let mut offset = 0;
        let mut ended = false;
        let terminal_width = match self.wrap {
            Some(WrapMode::Never) => None,
            Some(WrapMode::Always) => Some(self.terminal_width()),
            Some(WrapMode::Auto) | None => {
                if std::io::stdout().is_terminal() {
                    Some(self.terminal_width())
                } else {
                    None
                }
            },
        };

        let mut state = ParseState::new(
            terminal_width,
            os.database.settings.get_bool(Setting::ChatDisableMarkdownRendering),
        );
        let mut response_prefix_printed = false;

        let mut tool_uses = Vec::new();
        let mut tool_name_being_recvd: Option<String> = None;

        if self.spinner.is_some() {
            drop(self.spinner.take());
            queue!(
                self.stderr,
                style::SetForegroundColor(Color::Reset),
                cursor::MoveToColumn(0),
                cursor::Show,
                terminal::Clear(terminal::ClearType::CurrentLine),
            )?;
        }

        loop {
            match rx.recv().await {
                Some(Ok(msg_event)) => {
                    trace!("Consumed: {:?}", msg_event);
                    match msg_event {
                        parser::ResponseEvent::ToolUseStart { name } => {
                            // We need to flush the buffer here, otherwise text will not be
                            // printed while we are receiving tool use events.
                            buf.push('\n');
                            tool_name_being_recvd = Some(name);
                        },
                        parser::ResponseEvent::AssistantText(text) => {
                            match self.output_format {
                                OutputFormat::Plain => {
                                    // Add Q response prefix before the first assistant text.
                                    if !response_prefix_printed && !text.trim().is_empty() {
                                        queue!(
                                            self.stdout,
                                            style::SetForegroundColor(Color::Green),
                                            style::Print("> "),
                                            style::SetForegroundColor(Color::Reset)
                                        )?;
                                        response_prefix_printed = true;
                                    }
                                    buf.push_str(&text);
                                }
                                OutputFormat::Json => {
                                    buf.push_str(&text);
                                }
                            }
                        },
                        parser::ResponseEvent::ToolUse(tool_use) => {
                            if self.spinner.is_some() {
                                drop(self.spinner.take());
                                queue!(
                                    self.stderr,
                                    terminal::Clear(terminal::ClearType::CurrentLine),
                                    cursor::MoveToColumn(0),
                                    cursor::Show
                                )?;
                            }
                            
                            if self.output_format == OutputFormat::Json {
                                use crate::cli::chat::stream_json::{StreamEvent, ToolUseEvent};
                                let event = StreamEvent::ToolUse(ToolUseEvent {
                                    tool_use_id: tool_use.id.clone(),
                                    name: tool_use.name.clone(),
                                    input: Some(tool_use.args.clone()),
                                });
                                stream_json::emit_event(&event).ok();
                            }
                            
                            tool_uses.push(tool_use);
                            tool_name_being_recvd = None;
                        },
                        parser::ResponseEvent::EndStream {
                            message,
                            request_metadata: rm,
                        } => {
                            // This log is attempting to help debug instances where users encounter
                            // the response timeout message.
                            if message.content() == RESPONSE_TIMEOUT_CONTENT {
                                error!(?request_id, ?message, "Encountered an unexpected model response");
                            }
                            
                            if self.output_format == OutputFormat::Json {
                                use crate::cli::chat::stream_json::{StreamEvent, AssistantEvent, ResultEvent, ResultSubtype};
                                
                                if !buf.is_empty() {
                                    let event = StreamEvent::Assistant(AssistantEvent {
                                        content: buf.clone(),
                                    });
                                    stream_json::emit_event(&event).ok();
                                }
                                
                                let duration_ms = rm.stream_end_timestamp_ms
                                    .saturating_sub(rm.request_start_timestamp_ms);
                                let event = StreamEvent::Result(ResultEvent {
                                    subtype: ResultSubtype::Success,
                                    duration_ms: Some(duration_ms),
                                    total_cost_usd: None,
                                    usage: None,
                                    error: None,
                                });
                                stream_json::emit_event(&event).ok();
                            }
                            
                            self.conversation.push_assistant_message(os, message, Some(rm.clone()));
                            self.user_turn_request_metadata.push(rm);
                            ended = true;
                        },
                    }
                },
                Some(Err(recv_error)) => {
                    if let Some(request_id) = &recv_error.request_metadata.request_id {
                        self.failed_request_ids.push(request_id.clone());
                    };

                    self.user_turn_request_metadata
                        .push(recv_error.request_metadata.clone());
                    let (reason, reason_desc) = get_error_reason(&recv_error);
                    let status_code = recv_error.status_code();

                    if self.output_format == OutputFormat::Json {
                        use crate::cli::chat::stream_json::{StreamEvent, ResultEvent, ResultSubtype};
                        let error_message = reason_desc.to_string();
                        let event = StreamEvent::Result(ResultEvent {
                            subtype: ResultSubtype::Error,
                            duration_ms: None,
                            total_cost_usd: None,
                            usage: None,
                            error: Some(error_message),
                        });
                        let _ = stream_json::emit_event(&event);
                    }

                    match recv_error.source {
                        RecvErrorKind::StreamTimeout { source, duration } => {
                            self.send_chat_telemetry(
                                os,
                                TelemetryResult::Failed,
                                Some(reason),
                                Some(reason_desc),
                                status_code,
                                false, // We retry the request, so don't end the current turn yet.
                            )
                            .await;

                            error!(
                                recv_error.request_metadata.request_id,
                                ?source,
                                "Encountered a stream timeout after waiting for {}s",
                                duration.as_secs()
                            );

                            execute!(self.stderr, cursor::Hide)?;
                            self.spinner = Some(Spinner::new(Spinners::Dots, "Dividing up the work...".to_string()));

                            // For stream timeouts, we'll tell the model to try and split its response into
                            // smaller chunks.
                            self.conversation.push_assistant_message(
                                os,
                                AssistantMessage::new_response(None, RESPONSE_TIMEOUT_CONTENT.to_string()),
                                None,
                            );
                            self.conversation
                                .set_next_user_message(
                                    "You took too long to respond - try to split up the work into smaller steps."
                                        .to_string(),
                                )
                                .await;
                            self.send_tool_use_telemetry(os).await;
                            return Ok(ChatState::HandleResponseStream(
                                self.conversation
                                    .as_sendable_conversation_state(os, &mut self.stderr, false)
                                    .await?,
                            ));
                        },
                        RecvErrorKind::UnexpectedToolUseEos {
                            tool_use_id,
                            name,
                            message,
                            ..
                        } => {
                            self.send_chat_telemetry(
                                os,
                                TelemetryResult::Failed,
                                Some(reason),
                                Some(reason_desc),
                                status_code,
                                false, // We retry the request, so don't end the current turn yet.
                            )
                            .await;

                            error!(
                                recv_error.request_metadata.request_id,
                                tool_use_id, name, "The response stream ended before the entire tool use was received"
                            );
                            self.conversation
                                .push_assistant_message(os, *message, Some(recv_error.request_metadata));
                            let tool_results = vec![ToolUseResult {
                                    tool_use_id,
                                    content: vec![ToolUseResultBlock::Text(
                                        "The generated tool was too large, try again but this time split up the work between multiple tool uses".to_string(),
                                    )],
                                    status: ToolResultStatus::Error,
                                }];
                            self.conversation.add_tool_results(tool_results);
                            self.send_tool_use_telemetry(os).await;
                            return Ok(ChatState::HandleResponseStream(
                                self.conversation
                                    .as_sendable_conversation_state(os, &mut self.stderr, false)
                                    .await?,
                            ));
                        },
                        RecvErrorKind::ToolValidationError {
                            tool_use_id,
                            name,
                            message,
                            error_message,
                        } => {
                            self.send_chat_telemetry(
                                os,
                                TelemetryResult::Failed,
                                Some(reason),
                                Some(reason_desc),
                                status_code,
                                false, // We retry the request, so don't end the current turn yet.
                            )
                            .await;

                            error!(
                                recv_error.request_metadata.request_id,
                                tool_use_id, name, error_message, "Tool validation failed"
                            );
                            self.conversation
                                .push_assistant_message(os, *message, Some(recv_error.request_metadata));
                            let tool_results = vec![ToolUseResult {
                                tool_use_id,
                                content: vec![ToolUseResultBlock::Text(format!(
                                    "Tool validation failed: {}. Please ensure tool arguments are provided as a valid JSON object.",
                                    error_message
                                ))],
                                status: ToolResultStatus::Error,
                            }];
                            // User hint of what happened
                            let _ = queue!(
                                self.stdout,
                                style::Print("\n\n"),
                                style::SetForegroundColor(Color::Yellow),
                                style::Print(format!(
                                    "Tool validation failed: {}\n Retrying the request...",
                                    error_message
                                )),
                                style::ResetColor,
                                style::Print("\n"),
                            );
                            self.conversation.add_tool_results(tool_results);
                            self.send_tool_use_telemetry(os).await;
                            return Ok(ChatState::HandleResponseStream(
                                self.conversation
                                    .as_sendable_conversation_state(os, &mut self.stderr, false)
                                    .await?,
                            ));
                        },
                        _ => {
                            self.send_chat_telemetry(
                                os,
                                TelemetryResult::Failed,
                                Some(reason),
                                Some(reason_desc),
                                status_code,
                                true, // Hard fail -> end the current user turn.
                            )
                            .await;

                            return Err(recv_error.into());
                        },
                    }
                },
                None => {
                    warn!("response stream receiver closed before receiving a stop event");
                    ended = true;
                },
            }

            // Fix for the markdown parser copied over from q chat:
            // this is a hack since otherwise the parser might report Incomplete with useful data
            // still left in the buffer. I'm not sure how this is intended to be handled.
            if ended {
                buf.push('\n');
            }

            if tool_name_being_recvd.is_none() && !buf.is_empty() && self.spinner.is_some() {
                drop(self.spinner.take());
                queue!(
                    self.stderr,
                    terminal::Clear(terminal::ClearType::CurrentLine),
                    cursor::MoveToColumn(0),
                    cursor::Show
                )?;
            }

            // Print the response for normal cases
            if self.output_format == OutputFormat::Plain {
                loop {
                    let input = Partial::new(&buf[offset..]);
                    match interpret_markdown(input, &mut self.stdout, &mut state) {
                        Ok(parsed) => {
                            offset += parsed.offset_from(&input);
                            self.stdout.flush()?;
                            state.newline = state.set_newline;
                            state.set_newline = false;
                        },
                        Err(err) => match err.into_inner() {
                            Some(err) => return Err(ChatError::Custom(err.to_string().into())),
                            None => break, // Data was incomplete
                        },
                    }

                    // TODO: We should buffer output based on how much we have to parse, not as a constant
                    // Do not remove unless you are nabochay :)
                    tokio::time::sleep(Duration::from_millis(8)).await;
                }
            }

            // Set spinner after showing all of the assistant text content so far.
            if tool_name_being_recvd.is_some() {
                queue!(self.stderr, cursor::Hide)?;
                if self.interactive && self.output_format == OutputFormat::Plain {
                    self.spinner = Some(Spinner::new(Spinners::Dots, "Thinking...".to_string()));
                }
            }

            if ended {
                if os
                    .database
                    .settings
                    .get_bool(Setting::ChatEnableNotifications)
                    .unwrap_or(false)
                {
                    // For final responses (no tools suggested), always play the bell
                    play_notification_bell(tool_uses.is_empty());
                }

                queue!(self.stderr, style::ResetColor, style::SetAttribute(Attribute::Reset))?;
                execute!(self.stdout, style::Print("\n"))?;

                for (i, citation) in &state.citations {
                    queue!(
                        self.stdout,
                        style::Print("\n"),
                        style::SetForegroundColor(Color::Blue),
                        style::Print(format!("[^{i}]: ")),
                        style::SetForegroundColor(Color::DarkGrey),
                        style::Print(format!("{citation}\n")),
                        style::SetForegroundColor(Color::Reset)
                    )?;
                }

                break;
            }
        }

        if !tool_uses.is_empty() {
            Ok(ChatState::ValidateTools { tool_uses })
        } else {
            self.tool_uses.clear();
            self.pending_tool_index = None;
            self.tool_turn_start_time = None;

            // Create turn checkpoint if tools were used
            if ExperimentManager::is_enabled(os, ExperimentName::Checkpoint) && !self.conversation.is_in_tangent_mode()
            {
                if let Some(mut manager) = self.conversation.checkpoint_manager.take() {
                    if manager.tools_in_turn > 0 {
                        // Increment turn counter
                        manager.current_turn += 1;

                        // Get user message for description
                        let description = manager.pending_user_message.take().map_or_else(
                            || "Turn completed".to_string(),
                            |msg| truncate_message(&msg, CHECKPOINT_MESSAGE_MAX_LENGTH),
                        );

                        // Create turn checkpoint
                        let tag = manager.current_turn.to_string();
                        if let Err(e) = manager.create_checkpoint(
                            &tag,
                            &description,
                            &self.conversation.history().clone(),
                            true,
                            None,
                        ) {
                            execute!(
                                self.stderr,
                                style::SetForegroundColor(Color::Yellow),
                                style::Print(format!("⚠️ Could not create automatic checkpoint: {}\n\n", e)),
                                style::SetForegroundColor(Color::Reset),
                            )?;
                        } else {
                            execute!(
                                self.stderr,
                                style::SetForegroundColor(Color::Blue),
                                style::SetAttribute(Attribute::Bold),
                                style::Print(format!("✓ Created checkpoint {}\n\n", tag)),
                                style::SetForegroundColor(Color::Reset),
                                style::SetAttribute(Attribute::Reset),
                            )?;
                        }

                        // Reset for next turn
                        manager.tools_in_turn = 0;
                    } else {
                        // Clear pending message even if no tools were used
                        manager.pending_user_message = None;
                    }
                    manager.message_locked = false; // Unlock for next turn

                    // Put manager back
                    self.conversation.checkpoint_manager = Some(manager);
                }
            }

            self.send_chat_telemetry(os, TelemetryResult::Succeeded, None, None, None, true)
                .await;

            // Run Stop hooks when the assistant finishes responding
            if let Some(cm) = self.conversation.context_manager.as_mut() {
                let _ = cm
                    .run_hooks(
                        crate::cli::agent::hook::HookTrigger::Stop,
                        &mut std::io::stderr(),
                        os,
                        None,
                        None,
                    )
                    .await;
            }

            Ok(ChatState::PromptUser {
                skip_printing_tools: false,
            })
        }
    }

    // Validate the tool use request from LLM, including basic checks like fs_read file should exist, as
    // well as user-defined preToolUse hook check.
    async fn validate_tools(&mut self, os: &Os, tool_uses: Vec<AssistantToolUse>) -> Result<ChatState, ChatError> {
        let conv_id = self.conversation.conversation_id().to_owned();
        debug!(?tool_uses, "Validating tool uses");
        let mut queued_tools: Vec<QueuedTool> = Vec::new();
        let mut tool_results: Vec<ToolUseResult> = Vec::new();

        for tool_use in tool_uses {
            let tool_use_id = tool_use.id.clone();
            let tool_use_name = tool_use.name.clone();
            let tool_input = tool_use.args.clone();
            let mut tool_telemetry = ToolUseEventBuilder::new(
                conv_id.clone(),
                tool_use.id.clone(),
                self.conversation.model_info.as_ref().map(|m| m.model_id.clone()),
            )
            .set_tool_use_id(tool_use_id.clone())
            .set_tool_name(tool_use.name.clone())
            .utterance_id(self.conversation.message_id().map(|s| s.to_string()));
            match self.conversation.tool_manager.get_tool_from_tool_use(tool_use).await {
                Ok(mut tool) => {
                    // Apply non-Q-generated context to tools
                    self.contextualize_tool(&mut tool);

                    match tool.validate(os).await {
                        Ok(()) => {
                            tool_telemetry.is_valid = Some(true);
                            queued_tools.push(QueuedTool {
                                id: tool_use_id.clone(),
                                name: tool_use_name,
                                tool,
                                accepted: false,
                                tool_input,
                            });
                        },
                        Err(err) => {
                            tool_telemetry.is_valid = Some(false);
                            tool_results.push(ToolUseResult {
                                tool_use_id: tool_use_id.clone(),
                                content: vec![ToolUseResultBlock::Text(format!(
                                    "Failed to validate tool parameters: {err}"
                                ))],
                                status: ToolResultStatus::Error,
                            });
                        },
                    };
                },
                Err(err) => {
                    tool_telemetry.is_valid = Some(false);
                    tool_results.push(err.into());
                },
            }
            self.tool_use_telemetry_events.insert(tool_use_id, tool_telemetry);
        }

        // If we have any validation errors, then return them immediately to the model.
        if !tool_results.is_empty() {
            debug!(?tool_results, "Error found in the model tools");
            queue!(
                self.stderr,
                style::SetAttribute(Attribute::Bold),
                style::Print("Tool validation failed: "),
                style::SetAttribute(Attribute::Reset),
            )?;
            for tool_result in &tool_results {
                for block in &tool_result.content {
                    let content: Option<Cow<'_, str>> = match block {
                        ToolUseResultBlock::Text(t) => Some(t.as_str().into()),
                        ToolUseResultBlock::Json(d) => serde_json::to_string(d)
                            .map_err(|err| error!(?err, "failed to serialize tool result content"))
                            .map(Into::into)
                            .ok(),
                    };
                    if let Some(content) = content {
                        queue!(
                            self.stderr,
                            style::Print("\n"),
                            style::SetForegroundColor(Color::Red),
                            style::Print(format!("{}\n", content)),
                            style::SetForegroundColor(Color::Reset),
                        )?;
                    }
                }
            }

            self.conversation.add_tool_results(tool_results);
            self.send_chat_telemetry(os, TelemetryResult::Succeeded, None, None, None, false)
                .await;
            self.send_tool_use_telemetry(os).await;
            if let ToolUseStatus::Idle = self.tool_use_status {
                self.tool_use_status = ToolUseStatus::RetryInProgress(
                    self.conversation
                        .message_id()
                        .map_or("No utterance id found".to_string(), |v| v.to_string()),
                );
            }

            return Ok(ChatState::HandleResponseStream(
                self.conversation
                    .as_sendable_conversation_state(os, &mut self.stderr, false)
                    .await?,
            ));
        }

        // Execute PreToolUse hooks for all validated tools
        // The mental model is preToolHook is like validate tools, but its behavior can be customized by
        // user Note that after preTookUse hook, user can still reject the took run
        if let Some(cm) = self.conversation.context_manager.as_mut() {
            for tool in &queued_tools {
                let tool_context = ToolContext {
                    tool_name: match &tool.tool {
                        Tool::Custom(custom_tool) => custom_tool.namespaced_tool_name(), // for MCP tool, pass MCP
                        // name to the hook
                        _ => tool.name.clone(),
                    },
                    tool_input: tool.tool_input.clone(),
                    tool_response: None,
                };

                let hook_results = cm
                    .run_hooks(
                        crate::cli::agent::hook::HookTrigger::PreToolUse,
                        &mut std::io::stderr(),
                        os,
                        None, // prompt
                        Some(tool_context),
                    )
                    .await?;

                // Here is how we handle the preToolUse hook output:
                // Exit code is 0: nothing. stdout is not shown to user.
                // Exit code is 2: block the tool use. return stderr to LLM. show warning to user
                // Other error: show warning to user.

                // Check for exit code 2 and add to tool_results
                for (_, (exit_code, output)) in &hook_results {
                    if *exit_code == 2 {
                        tool_results.push(ToolUseResult {
                            tool_use_id: tool.id.clone(),
                            content: vec![ToolUseResultBlock::Text(format!(
                                "PreToolHook blocked the tool execution: {}",
                                output
                            ))],
                            status: ToolResultStatus::Error,
                        });
                    }
                }
            }
        }

        // If we have any hook validation errors, return them immediately to the model
        if !tool_results.is_empty() {
            debug!(?tool_results, "Error found in PreToolUse hooks");
            for tool_result in &tool_results {
                for block in &tool_result.content {
                    if let ToolUseResultBlock::Text(content) = block {
                        queue!(
                            self.stderr,
                            style::Print("\n"),
                            style::SetForegroundColor(Color::Red),
                            style::Print(format!("{}\n", content)),
                            style::SetForegroundColor(Color::Reset),
                        )?;
                    }
                }
            }

            self.conversation.add_tool_results(tool_results);
            return Ok(ChatState::HandleResponseStream(
                self.conversation
                    .as_sendable_conversation_state(os, &mut self.stderr, false)
                    .await?,
            ));
        }

        self.tool_uses = queued_tools;
        self.pending_tool_index = Some(0);
        self.tool_turn_start_time = Some(Instant::now());

        Ok(ChatState::ExecuteTools)
    }

    async fn retry_model_overload(&mut self, os: &mut Os) -> Result<ChatState, ChatError> {
        os.client.invalidate_model_cache().await;
        match select_model(os, self).await {
            Ok(Some(_)) => (),
            Ok(None) => {
                // User did not select a model, so reset the current request state.
                self.conversation.enforce_conversation_invariants();
                self.conversation.reset_next_user_message();
                self.pending_tool_index = None;
                self.tool_turn_start_time = None;
                return Ok(ChatState::PromptUser {
                    skip_printing_tools: false,
                });
            },
            Err(err) => return Err(err),
        }

        if self.interactive && self.output_format == OutputFormat::Plain {
            self.spinner = Some(Spinner::new(Spinners::Dots, "Thinking...".to_owned()));
        }

        Ok(ChatState::HandleResponseStream(
            self.conversation
                .as_sendable_conversation_state(os, &mut self.stderr, true)
                .await?,
        ))
    }

    /// Apply program context to tools that Q may not have.
    // We cannot attach this any other way because Tools are constructed by deserializing
    // output from Amazon Q.
    // TODO: Is there a better way?
    fn contextualize_tool(&self, tool: &mut Tool) {
        if let Tool::GhIssue(gh_issue) = tool {
            let allowed_tools = self
                .conversation
                .agents
                .get_active()
                .map(|a| a.allowed_tools.iter().cloned().collect::<Vec<_>>())
                .unwrap_or_default();
            gh_issue.set_context(GhIssueContext {
                // Ideally we avoid cloning, but this function is not called very often.
                // Using references with lifetimes requires a large refactor, and Arc<Mutex<T>>
                // seems like overkill and may incur some performance cost anyway.
                context_manager: self.conversation.context_manager.clone(),
                transcript: self.conversation.transcript.clone(),
                failed_request_ids: self.failed_request_ids.clone(),
                tool_permissions: allowed_tools,
            });
        }
    }

    /// Check if UI elements should be displayed based on output format.
    /// Returns true for Plain format, false for JSON format.
    fn should_show_ui(&self) -> bool {
        self.output_format == OutputFormat::Plain
    }

    async fn print_tool_description(&mut self, os: &Os, tool_index: usize, trusted: bool) -> Result<(), ChatError> {
        let tool_use = &self.tool_uses[tool_index];

        queue!(
            self.stdout,
            style::SetForegroundColor(Color::Magenta),
            style::Print(format!(
                "🛠️  Using tool: {}{}",
                tool_use.tool.display_name(),
                if trusted { " (trusted)".dark_green() } else { "".reset() }
            )),
            style::SetForegroundColor(Color::Reset)
        )?;
        if let Tool::Custom(ref tool) = tool_use.tool {
            queue!(
                self.stdout,
                style::SetForegroundColor(Color::Reset),
                style::Print(" from mcp server "),
                style::SetForegroundColor(Color::Magenta),
                style::Print(&tool.server_name),
                style::SetForegroundColor(Color::Reset),
            )?;
        }

        execute!(
            self.stdout,
            style::Print("\n"),
            style::Print(CONTINUATION_LINE),
            style::Print("\n"),
            style::Print(TOOL_BULLET)
        )?;

        tool_use
            .tool
            .queue_description(os, &mut self.stdout)
            .await
            .map_err(|e| ChatError::Custom(format!("failed to print tool, `{}`: {}", tool_use.name, e).into()))?;

        Ok(())
    }

    /// Helper function to read user input with a prompt and Ctrl+C handling
    fn read_user_input(&mut self, prompt: &str, exit_on_single_ctrl_c: bool) -> Option<String> {
        let mut ctrl_c = false;
        loop {
            match (self.input_source.read_line(Some(prompt)), ctrl_c) {
                (Ok(Some(line)), _) => {
                    if line.trim().is_empty() {
                        continue; // Reprompt if the input is empty
                    }
                    return Some(line);
                },
                (Ok(None), false) => {
                    if exit_on_single_ctrl_c {
                        return None;
                    }
                    execute!(
                        self.stderr,
                        style::Print(format!(
                            "\n(To exit the CLI, press Ctrl+C or Ctrl+D again or type {})\n\n",
                            "/quit".green()
                        ))
                    )
                    .unwrap_or_default();
                    ctrl_c = true;
                },
                (Ok(None), true) => return None, // Exit if Ctrl+C was pressed twice
                (Err(_), _) => return None,
            }
        }
    }

    /// Helper function to generate a prompt based on the current context
    async fn generate_tool_trust_prompt(&mut self, os: &Os) -> String {
        let profile = self.conversation.current_profile().map(|s| s.to_string());
        let all_trusted = self.all_tools_trusted();
        let tangent_mode = self.conversation.is_in_tangent_mode();

        // Check if context usage indicator is enabled
        let usage_percentage = if ExperimentManager::is_enabled(os, ExperimentName::ContextUsageIndicator) {
            use crate::cli::chat::cli::usage::get_total_usage_percentage;
            get_total_usage_percentage(self, os).await.ok()
        } else {
            None
        };

        let mut generated_prompt =
            prompt::generate_prompt(profile.as_deref(), all_trusted, tangent_mode, usage_percentage);

        if ExperimentManager::is_enabled(os, ExperimentName::Delegate) && status_all_agents(os).await.is_ok() {
            generated_prompt = format!("{DELEGATE_NOTIFIER}\n{generated_prompt}");
        }

        generated_prompt
    }

    async fn send_tool_use_telemetry(&mut self, os: &Os) {
        for (_, mut event) in self.tool_use_telemetry_events.drain() {
            event.user_input_id = match self.tool_use_status {
                ToolUseStatus::Idle => self.conversation.message_id(),
                ToolUseStatus::RetryInProgress(ref id) => Some(id.as_str()),
            }
            .map(|v| v.to_string());

            os.telemetry.send_tool_use_suggested(&os.database, event).await.ok();
        }
    }

    fn terminal_width(&self) -> usize {
        (self.terminal_width_provider)().unwrap_or(80)
    }

    fn all_tools_trusted(&self) -> bool {
        self.conversation.agents.trust_all_tools
    }

    /// Display character limit warnings based on current conversation size
    async fn display_char_warnings(&mut self, os: &Os) -> Result<(), ChatError> {
        let warning_level = self.conversation.get_token_warning_level(os).await?;

        match warning_level {
            TokenWarningLevel::Critical => {
                // Memory constraint warning with gentler wording
                execute!(
                    self.stderr,
                    style::SetForegroundColor(Color::Yellow),
                    style::SetAttribute(Attribute::Bold),
                    style::Print("\n⚠️ This conversation is getting lengthy.\n"),
                    style::SetAttribute(Attribute::Reset),
                    style::Print(
                        "To ensure continued smooth operation, please use /compact to summarize the conversation.\n\n"
                    ),
                    style::SetForegroundColor(Color::Reset)
                )?;
            },
            TokenWarningLevel::None => {
                // No warning needed
            },
        }

        Ok(())
    }

    /// Resets state associated with the active user turn.
    ///
    /// This should *always* be called whenever a new user prompt is sent to the backend. Note
    /// that includes tool use rejections.
    fn reset_user_turn(&mut self) {
        info!(?self.user_turn_request_metadata, "Resetting the current user turn");
        self.user_turn_request_metadata.clear();
    }

    /// Sends an "codewhispererterminal_addChatMessage" telemetry event.
    ///
    /// This *MUST* be called in the following cases:
    /// 1. After the end of a user turn
    /// 2. After tool use execution has completed
    /// 3. After an error was encountered during the handling of the response stream, tool use
    ///    validation, or tool use execution.
    ///
    /// [Self::user_turn_request_metadata] must contain the [RequestMetadata] associated with the
    /// current user turn.
    #[allow(clippy::too_many_arguments)]
    async fn send_chat_telemetry(
        &self,
        os: &Os,
        result: TelemetryResult,
        reason: Option<String>,
        reason_desc: Option<String>,
        status_code: Option<u16>,
        is_end_turn: bool,
    ) {
        // Get metadata for the most recent request.
        let md = self.user_turn_request_metadata.last();

        let conversation_id = self.conversation.conversation_id().to_owned();
        let data = ChatAddedMessageParams {
            request_id: md.and_then(|md| md.request_id.clone()),
            message_id: md.map(|md| md.message_id.clone()),
            context_file_length: self.conversation.context_message_length(),
            model: md.and_then(|m| m.model_id.clone()),
            reason: reason.clone(),
            reason_desc: reason_desc.clone(),
            status_code,
            time_to_first_chunk_ms: md.and_then(|md| md.time_to_first_chunk.map(|d| d.as_secs_f64() * 1000.0)),
            time_between_chunks_ms: md.map(|md| {
                md.time_between_chunks
                    .iter()
                    .map(|d| d.as_secs_f64() * 1000.0)
                    .collect::<Vec<_>>()
            }),
            chat_conversation_type: md.and_then(|md| md.chat_conversation_type),
            tool_use_id: self.conversation.latest_tool_use_ids(),
            tool_name: self.conversation.latest_tool_use_names(),
            assistant_response_length: md.map(|md| md.response_size as i32),
            message_meta_tags: {
                let mut tags = md.map(|md| md.message_meta_tags.clone()).unwrap_or_default();
                if self.conversation.is_in_tangent_mode() {
                    tags.push(crate::telemetry::core::MessageMetaTag::TangentMode);
                }
                tags
            },
        };
        os.telemetry
            .send_chat_added_message(&os.database, conversation_id.clone(), result, data)
            .await
            .ok();

        if is_end_turn {
            let mds = &self.user_turn_request_metadata;

            // Get the user turn duration.
            let start_time = mds.first().map(|md| md.request_start_timestamp_ms);
            let end_time = mds.last().map(|md| md.stream_end_timestamp_ms);
            let user_turn_duration_seconds = match (start_time, end_time) {
                // Convert ms back to seconds
                (Some(start), Some(end)) => end.saturating_sub(start) as i64 / 1000,
                _ => 0,
            };

            os.telemetry
                .send_record_user_turn_completion(&os.database, conversation_id, result, RecordUserTurnCompletionArgs {
                    message_ids: mds.iter().map(|md| md.message_id.clone()).collect::<_>(),
                    request_ids: mds.iter().map(|md| md.request_id.clone()).collect::<_>(),
                    reason,
                    reason_desc,
                    status_code,
                    time_to_first_chunks_ms: mds
                        .iter()
                        .map(|md| md.time_to_first_chunk.map(|d| d.as_secs_f64() * 1000.0))
                        .collect::<_>(),
                    chat_conversation_type: md.and_then(|md| md.chat_conversation_type),
                    assistant_response_length: mds.iter().map(|md| md.response_size as i64).sum(),
                    message_meta_tags: mds.last().map(|md| md.message_meta_tags.clone()).unwrap_or_default(),
                    user_prompt_length: mds.first().map(|md| md.user_prompt_length).unwrap_or_default() as i64,
                    user_turn_duration_seconds,
                    follow_up_count: mds
                        .iter()
                        .filter(|md| matches!(md.chat_conversation_type, Some(ChatConversationType::ToolUse)))
                        .count() as i64,
                })
                .await
                .ok();
        }
    }

    async fn send_error_telemetry(
        &self,
        os: &Os,
        reason: String,
        reason_desc: Option<String>,
        status_code: Option<u16>,
    ) {
        let md = self.user_turn_request_metadata.last();
        os.telemetry
            .send_response_error(
                &os.database,
                self.conversation.conversation_id().to_owned(),
                self.conversation.context_message_length(),
                TelemetryResult::Failed,
                Some(reason),
                reason_desc,
                status_code,
                md.and_then(|md| md.request_id.clone()),
                md.map(|md| md.message_id.clone()),
            )
            .await
            .ok();
    }

    pub async fn send_slash_command_telemetry(
        &self,
        os: &Os,
        command: String,
        subcommand: Option<String>,
        result: TelemetryResult,
        reason: Option<String>,
    ) {
        let conversation_id = self.conversation.conversation_id().to_owned();
        if let Err(e) = os
            .telemetry
            .send_chat_slash_command_executed(&os.database, conversation_id, command, subcommand, result, reason)
            .await
        {
            tracing::warn!("Failed to send slash command telemetry: {}", e);
        }
    }

    /// Prompts Q to resume a to-do list with the given id by calling the load
    /// command of the todo_list tool
    pub async fn resume_todo_request(&mut self, os: &mut Os, id: &str) -> Result<ChatState, ChatError> {
        // Have to unpack each value separately since Reports can't be converted to
        // ChatError
        let todo_list = match TodoListState::load(os, id).await {
            Ok(todo) => todo,
            Err(e) => {
                return Err(ChatError::Custom(format!("Error getting todo list: {e}").into()));
            },
        };
        let contents = match serde_json::to_string(&todo_list) {
            Ok(s) => s,
            Err(e) => return Err(ChatError::Custom(format!("Error deserializing todo list: {e}").into())),
        };
        let request_content = format!(
            "[SYSTEM NOTE: This is an automated request, not from the user]\n
            Read the TODO list contents below and understand the task description, completed tasks, and provided context.\n 
            Call the `load` command of the todo_list tool with the given ID as an argument to display the TODO list to the user and officially resume execution of the TODO list tasks.\n
            You do not need to display the tasks to the user yourself. You can begin completing the tasks after calling the `load` command.\n
            TODO LIST CONTENTS: {}\n
            ID: {}\n",
            contents,
            id
        );

        let summary_message = UserMessage::new_prompt(request_content.clone(), None);

        ChatSession::reset_user_turn(self);

        Ok(ChatState::HandleInput {
            input: summary_message
                .into_user_input_message(self.conversation.model.clone(), &self.conversation.tools)
                .content,
        })
    }
}

/// Replaces amzn_codewhisperer_client::types::SubscriptionStatus with a more descriptive type.
/// See response expectations in [`get_subscription_status`] for reasoning.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ActualSubscriptionStatus {
    Active,   // User has paid for this month
    Expiring, // User has paid for this month but cancelled
    None,     // User has not paid for this month
}

// NOTE: The subscription API behaves in a non-intuitive way. We expect the following responses:
//
// 1. SubscriptionStatus::Active:
//    - The user *has* a subscription, but it is set to *not auto-renew* (i.e., cancelled).
//    - We return ActualSubscriptionStatus::Expiring to indicate they are eligible to re-subscribe
//
// 2. SubscriptionStatus::Inactive:
//    - The user has no subscription at all (no Pro access).
//    - We return ActualSubscriptionStatus::None to indicate they are eligible to subscribe.
//
// 3. ConflictException (as an error):
//    - The user already has an active subscription *with auto-renewal enabled*.
//    - We return ActualSubscriptionStatus::Active since they don’t need to subscribe again.
//
// Also, it is currently not possible to subscribe or re-subscribe via console, only IDE/CLI.
async fn get_subscription_status(os: &mut Os) -> Result<ActualSubscriptionStatus> {
    if is_idc_user(&os.database).await? {
        return Ok(ActualSubscriptionStatus::Active);
    }

    match os.client.create_subscription_token().await {
        Ok(response) => match response.status() {
            SubscriptionStatus::Active => Ok(ActualSubscriptionStatus::Expiring),
            SubscriptionStatus::Inactive => Ok(ActualSubscriptionStatus::None),
            _ => Ok(ActualSubscriptionStatus::None),
        },
        Err(ApiClientError::CreateSubscriptionToken(e)) => {
            let sdk_error_code = e.as_service_error().and_then(|err| err.meta().code());

            if sdk_error_code.is_some_and(|c| c.contains("ConflictException")) {
                Ok(ActualSubscriptionStatus::Active)
            } else {
                Err(e.into())
            }
        },
        Err(e) => Err(e.into()),
    }
}

async fn get_subscription_status_with_spinner(
    os: &mut Os,
    output: &mut impl Write,
) -> Result<ActualSubscriptionStatus> {
    return with_spinner(output, "Checking subscription status...", || async {
        get_subscription_status(os).await
    })
    .await;
}

pub async fn with_spinner<T, E, F, Fut>(output: &mut impl std::io::Write, spinner_text: &str, f: F) -> Result<T, E>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<T, E>>,
{
    queue!(output, cursor::Hide,).ok();
    let spinner = Some(Spinner::new(Spinners::Dots, spinner_text.to_owned()));

    let result = f().await;

    if let Some(mut s) = spinner {
        s.stop();
        let _ = queue!(
            output,
            terminal::Clear(terminal::ClearType::CurrentLine),
            cursor::MoveToColumn(0),
        );
    }

    result
}

/// Checks if an input may be referencing a file and should not be handled as a typical slash
/// command. If true, then return [Option::Some<ChatState>], otherwise [Option::None].
fn does_input_reference_file(input: &str) -> Option<ChatState> {
    let after_slash = input.strip_prefix("/")?;

    if let Some(first) = shlex::split(after_slash).unwrap_or_default().first() {
        let looks_like_path =
            first.contains(MAIN_SEPARATOR) || first.contains('/') || first.contains('\\') || first.contains('.');

        if looks_like_path {
            return Some(ChatState::HandleInput {
                input: after_slash.to_string(),
            });
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::cli::agent::Agent;

    async fn get_test_agents(os: &Os) -> Agents {
        const AGENT_PATH: &str = "/persona/TestAgent.json";
        let mut agents = Agents::default();
        let agent = Agent {
            path: Some(PathBuf::from(AGENT_PATH)),
            ..Default::default()
        };
        if let Ok(false) = os.fs.try_exists(AGENT_PATH).await {
            let content = agent.to_str_pretty().expect("Failed to serialize test agent to file");
            let agent_path = PathBuf::from(AGENT_PATH);
            os.fs
                .create_dir_all(
                    agent_path
                        .parent()
                        .expect("Failed to obtain parent path for agent config"),
                )
                .await
                .expect("Failed to create test agent dir");
            os.fs
                .write(agent_path, &content)
                .await
                .expect("Failed to write test agent to file");
        }
        agents.agents.insert("TestAgent".to_string(), agent);
        agents.switch("TestAgent").expect("Failed to switch agent");
        agents
    }

    #[tokio::test]
    async fn test_flow() {
        let mut os = Os::new().await.unwrap();
        os.client.set_mock_output(serde_json::json!([
            [
                "Sure, I'll create a file for you",
                {
                    "tool_use_id": "1",
                    "name": "fs_write",
                    "args": {
                        "command": "create",
                        "file_text": "Hello, world!",
                        "path": "/file.txt",
                    }
                }
            ],
            [
                "Hope that looks good to you!",
            ],
        ]));

        let agents = get_test_agents(&os).await;
        let tool_manager = ToolManager::default();
        let tool_config = serde_json::from_str::<HashMap<String, ToolSpec>>(include_str!("tools/tool_index.json"))
            .expect("Tools failed to load");
        ChatSession::new(
            &mut os,
            std::io::stdout(),
            std::io::stderr(),
            "fake_conv_id",
            agents,
            None,
            InputSource::new_mock(vec![
                "create a new file".to_string(),
                "y".to_string(),
                "exit".to_string(),
            ]),
            false,
            || Some(80),
            tool_manager,
            None,
            tool_config,
            true,
            false,
            None,
            OutputFormat::Plain,
        )
        .await
        .unwrap()
        .spawn(&mut os)
        .await
        .unwrap();

        assert_eq!(os.fs.read_to_string("/file.txt").await.unwrap(), "Hello, world!\n");
    }

    #[tokio::test]
    async fn test_flow_tool_permissions() {
        let mut os = Os::new().await.unwrap();
        os.client.set_mock_output(serde_json::json!([
            [
                "Ok",
                {
                    "tool_use_id": "1",
                    "name": "fs_write",
                    "args": {
                        "command": "create",
                        "file_text": "Hello, world!",
                        "path": "/file1.txt",
                    }
                }
            ],
            [
                "Done",
            ],
            [
                "Ok",
                {
                    "tool_use_id": "1",
                    "name": "fs_write",
                    "args": {
                        "command": "create",
                        "file_text": "Hello, world!",
                        "path": "/file2.txt",
                    }
                }
            ],
            [
                "Done",
            ],
            [
                "Ok",
                {
                    "tool_use_id": "1",
                    "name": "fs_write",
                    "args": {
                        "command": "create",
                        "file_text": "Hello, world!",
                        "path": "/file3.txt",
                    }
                }
            ],
            [
                "Done",
            ],
            [
                "Ok",
                {
                    "tool_use_id": "1",
                    "name": "fs_write",
                    "args": {
                        "command": "create",
                        "file_text": "Hello, world!",
                        "path": "/file4.txt",
                    }
                }
            ],
            [
                "Ok, I won't make it.",
            ],
            [
                "Ok",
                {
                    "tool_use_id": "1",
                    "name": "fs_write",
                    "args": {
                        "command": "create",
                        "file_text": "Hello, world!",
                        "path": "/file5.txt",
                    }
                }
            ],
            [
                "Done",
            ],
            [
                "Ok",
                {
                    "tool_use_id": "1",
                    "name": "fs_write",
                    "args": {
                        "command": "create",
                        "file_text": "Hello, world!",
                        "path": "/file6.txt",
                    }
                }
            ],
            [
                "Ok, I won't make it.",
            ],
        ]));

        let agents = get_test_agents(&os).await;
        let tool_manager = ToolManager::default();
        let tool_config = serde_json::from_str::<HashMap<String, ToolSpec>>(include_str!("tools/tool_index.json"))
            .expect("Tools failed to load");
        ChatSession::new(
            &mut os,
            std::io::stdout(),
            std::io::stderr(),
            "fake_conv_id",
            agents,
            None,
            InputSource::new_mock(vec![
                "/tools".to_string(),
                "/tools help".to_string(),
                "create a new file".to_string(),
                "y".to_string(),
                "create a new file".to_string(),
                "t".to_string(),
                "create a new file".to_string(), // should make without prompting due to 't'
                "/tools untrust fs_write".to_string(),
                "create a file".to_string(), // prompt again due to untrust
                "n".to_string(),             // cancel
                "/tools trust fs_write".to_string(),
                "create a file".to_string(), // again without prompting due to '/tools trust'
                "/tools reset".to_string(),
                "create a file".to_string(), // prompt again due to reset
                "n".to_string(),             // cancel
                "exit".to_string(),
            ]),
            false,
            || Some(80),
            tool_manager,
            None,
            tool_config,
            true,
            false,
            None,
            OutputFormat::Plain,
        )
        .await
        .unwrap()
        .spawn(&mut os)
        .await
        .unwrap();

        assert_eq!(os.fs.read_to_string("/file2.txt").await.unwrap(), "Hello, world!\n");
        assert_eq!(os.fs.read_to_string("/file3.txt").await.unwrap(), "Hello, world!\n");
        assert!(!os.fs.exists("/file4.txt"));
        assert_eq!(os.fs.read_to_string("/file5.txt").await.unwrap(), "Hello, world!\n");
        // TODO: fix this with agent change (dingfeli)
        // assert!(!ctx.fs.exists("/file6.txt"));
    }

    #[tokio::test]
    async fn test_flow_multiple_tools() {
        // let _ = tracing_subscriber::fmt::try_init();
        let mut os = Os::new().await.unwrap();
        os.client.set_mock_output(serde_json::json!([
            [
                "Sure, I'll create a file for you",
                {
                    "tool_use_id": "1",
                    "name": "fs_write",
                    "args": {
                        "command": "create",
                        "file_text": "Hello, world!",
                        "path": "/file1.txt",
                    }
                },
                {
                    "tool_use_id": "2",
                    "name": "fs_write",
                    "args": {
                        "command": "create",
                        "file_text": "Hello, world!",
                        "path": "/file2.txt",
                    }
                }
            ],
            [
                "Done",
            ],
            [
                "Sure, I'll create a file for you",
                {
                    "tool_use_id": "1",
                    "name": "fs_write",
                    "args": {
                        "command": "create",
                        "file_text": "Hello, world!",
                        "path": "/file3.txt",
                    }
                },
                {
                    "tool_use_id": "2",
                    "name": "fs_write",
                    "args": {
                        "command": "create",
                        "file_text": "Hello, world!",
                        "path": "/file4.txt",
                    }
                }
            ],
            [
                "Done",
            ],
        ]));

        let agents = get_test_agents(&os).await;
        let tool_manager = ToolManager::default();
        let tool_config = serde_json::from_str::<HashMap<String, ToolSpec>>(include_str!("tools/tool_index.json"))
            .expect("Tools failed to load");
        ChatSession::new(
            &mut os,
            std::io::stdout(),
            std::io::stderr(),
            "fake_conv_id",
            agents,
            None,
            InputSource::new_mock(vec![
                "create 2 new files parallel".to_string(),
                "t".to_string(),
                "/tools reset".to_string(),
                "create 2 new files parallel".to_string(),
                "y".to_string(),
                "y".to_string(),
                "exit".to_string(),
            ]),
            false,
            || Some(80),
            tool_manager,
            None,
            tool_config,
            true,
            false,
            None,
            OutputFormat::Plain,
        )
        .await
        .unwrap()
        .spawn(&mut os)
        .await
        .unwrap();

        assert_eq!(os.fs.read_to_string("/file1.txt").await.unwrap(), "Hello, world!\n");
        assert_eq!(os.fs.read_to_string("/file2.txt").await.unwrap(), "Hello, world!\n");
        assert_eq!(os.fs.read_to_string("/file3.txt").await.unwrap(), "Hello, world!\n");
        assert_eq!(os.fs.read_to_string("/file4.txt").await.unwrap(), "Hello, world!\n");
    }

    #[tokio::test]
    async fn test_flow_tools_trust_all() {
        // let _ = tracing_subscriber::fmt::try_init();
        let mut os = Os::new().await.unwrap();
        os.client.set_mock_output(serde_json::json!([
            [
                "Sure, I'll create a file for you",
                {
                    "tool_use_id": "1",
                    "name": "fs_write",
                    "args": {
                        "command": "create",
                        "file_text": "Hello, world!",
                        "path": "/file1.txt",
                    }
                }
            ],
            [
                "Done",
            ],
            [
                "Sure, I'll create a file for you",
                {
                    "tool_use_id": "1",
                    "name": "fs_write",
                    "args": {
                        "command": "create",
                        "file_text": "Hello, world!",
                        "path": "/file3.txt",
                    }
                }
            ],
            [
                "Ok I won't.",
            ],
        ]));

        let agents = get_test_agents(&os).await;
        let tool_manager = ToolManager::default();
        let tool_config = serde_json::from_str::<HashMap<String, ToolSpec>>(include_str!("tools/tool_index.json"))
            .expect("Tools failed to load");
        ChatSession::new(
            &mut os,
            std::io::stdout(),
            std::io::stderr(),
            "fake_conv_id",
            agents,
            None,
            InputSource::new_mock(vec![
                "/tools trust-all".to_string(),
                "create a new file".to_string(),
                "/tools reset".to_string(),
                "create a new file".to_string(),
                "exit".to_string(),
            ]),
            false,
            || Some(80),
            tool_manager,
            None,
            tool_config,
            true,
            false,
            None,
            OutputFormat::Plain,
        )
        .await
        .unwrap()
        .spawn(&mut os)
        .await
        .unwrap();

        assert_eq!(os.fs.read_to_string("/file1.txt").await.unwrap(), "Hello, world!\n");
        assert!(!os.fs.exists("/file2.txt"));
    }

    #[test]
    fn test_editor_content_processing() {
        // Since we no longer have template replacement, this test is simplified
        let cases = vec![
            ("My content", "My content"),
            ("My content with newline\n", "My content with newline"),
            ("", ""),
        ];

        for (input, expected) in cases {
            let processed = input.trim().to_string();
            assert_eq!(processed, expected.trim().to_string(), "Failed for input: {}", input);
        }
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn test_subscribe_flow() {
        let mut os = Os::new().await.unwrap();
        os.client.set_mock_output(serde_json::Value::Array(vec![]));
        let agents = get_test_agents(&os).await;

        let tool_manager = ToolManager::default();
        let tool_config = serde_json::from_str::<HashMap<String, ToolSpec>>(include_str!("tools/tool_index.json"))
            .expect("Tools failed to load");
        ChatSession::new(
            &mut os,
            std::io::stdout(),
            std::io::stderr(),
            "fake_conv_id",
            agents,
            None,
            InputSource::new_mock(vec!["/subscribe".to_string(), "y".to_string(), "/quit".to_string()]),
            false,
            || Some(80),
            tool_manager,
            None,
            tool_config,
            true,
            false,
            None,
            OutputFormat::Plain,
        )
        .await
        .unwrap()
        .spawn(&mut os)
        .await
        .unwrap();
    }

    // Integration test for PreToolUse hook functionality.
    //
    // In this integration test we create a preToolUse hook that logs tool info into a file
    // and we run fs_read and verify the log is generated with the correct ToolContext data.
    #[tokio::test]
    async fn test_tool_hook_integration() {
        use std::collections::HashMap;

        use crate::cli::agent::hook::{
            Hook,
            HookTrigger,
        };

        let mut os = Os::new().await.unwrap();
        os.client.set_mock_output(serde_json::json!([
            [
                "I'll read that file for you",
                {
                    "tool_use_id": "1",
                    "name": "fs_read",
                    "args": {
                        "operations": [
                            {
                                "mode": "Line",
                                "path": "/test.txt",
                                "start_line": 1,
                                "end_line": 3
                            }
                        ]
                    }
                }
            ],
            [
                "Here's the file content!",
            ],
        ]));

        // Create test file
        os.fs.write("/test.txt", "line1\nline2\nline3\n").await.unwrap();

        // Create agent with PreToolUse and PostToolUse hooks
        let mut agents = Agents::default();
        let mut hooks = HashMap::new();

        // Get the real path in the temp directory for the hooks to write to
        let pre_hook_log_path = os.fs.chroot_path_str("/pre-hook-test.log");
        let post_hook_log_path = os.fs.chroot_path_str("/post-hook-test.log");
        let pre_hook_command = format!("cat > {}", pre_hook_log_path);
        let post_hook_command = format!("cat > {}", post_hook_log_path);

        hooks.insert(HookTrigger::PreToolUse, vec![Hook {
            command: pre_hook_command,
            timeout_ms: 5000,
            max_output_size: 1024,
            cache_ttl_seconds: 0,
            matcher: Some("fs_*".to_string()), // Match fs_read, fs_write, etc.
            source: crate::cli::agent::hook::Source::Agent,
        }]);

        hooks.insert(HookTrigger::PostToolUse, vec![Hook {
            command: post_hook_command,
            timeout_ms: 5000,
            max_output_size: 1024,
            cache_ttl_seconds: 0,
            matcher: Some("fs_*".to_string()), // Match fs_read, fs_write, etc.
            source: crate::cli::agent::hook::Source::Agent,
        }]);

        let agent = Agent {
            name: "TestAgent".to_string(),
            hooks,
            ..Default::default()
        };
        agents.agents.insert("TestAgent".to_string(), agent);
        agents.switch("TestAgent").expect("Failed to switch agent");

        let tool_manager = ToolManager::default();
        let tool_config = serde_json::from_str::<HashMap<String, ToolSpec>>(include_str!("tools/tool_index.json"))
            .expect("Tools failed to load");

        // Test that PreToolUse hook runs
        ChatSession::new(
            &mut os,
            std::io::stdout(),
            std::io::stderr(),
            "fake_conv_id",
            agents,
            None, // No initial input
            InputSource::new_mock(vec![
                "read /test.txt".to_string(),
                "y".to_string(), // Accept tool execution
                "exit".to_string(),
            ]),
            false,
            || Some(80),
            tool_manager,
            None,
            tool_config,
            true,
            false,
            None,
            OutputFormat::Plain,
        )
        .await
        .unwrap()
        .spawn(&mut os)
        .await
        .unwrap();

        // Verify the PreToolUse hook was called
        if let Ok(pre_log_content) = os.fs.read_to_string("/pre-hook-test.log").await {
            let pre_hook_data: serde_json::Value =
                serde_json::from_str(&pre_log_content).expect("PreToolUse hook output should be valid JSON");

            assert_eq!(pre_hook_data["hook_event_name"], "preToolUse");
            assert_eq!(pre_hook_data["tool_name"], "fs_read");
            assert_eq!(pre_hook_data["tool_response"], serde_json::Value::Null);

            let tool_input = &pre_hook_data["tool_input"];
            assert!(tool_input["operations"].is_array());

            println!("✓ PreToolUse hook validation passed: {}", pre_log_content);
        } else {
            panic!("PreToolUse hook log file not found - hook may not have been called");
        }

        // Verify the PostToolUse hook was called
        if let Ok(post_log_content) = os.fs.read_to_string("/post-hook-test.log").await {
            let post_hook_data: serde_json::Value =
                serde_json::from_str(&post_log_content).expect("PostToolUse hook output should be valid JSON");

            assert_eq!(post_hook_data["hook_event_name"], "postToolUse");
            assert_eq!(post_hook_data["tool_name"], "fs_read");

            // Validate tool_response structure for successful execution
            let tool_response = &post_hook_data["tool_response"];
            assert_eq!(tool_response["success"], true);
            assert!(tool_response["result"].is_array());

            let result_blocks = tool_response["result"].as_array().unwrap();
            assert!(!result_blocks.is_empty());
            let content = result_blocks[0].as_str().unwrap();
            assert!(content.contains("line1\nline2\nline3"));

            println!("✓ PostToolUse hook validation passed: {}", post_log_content);
        } else {
            panic!("PostToolUse hook log file not found - hook may not have been called");
        }
    }

    #[tokio::test]
    async fn test_pretool_hook_blocking_integration() {
        use std::collections::HashMap;

        use crate::cli::agent::hook::{
            Hook,
            HookTrigger,
        };

        let mut os = Os::new().await.unwrap();

        // Create a test file to read
        os.fs.write("/sensitive.txt", "classified information").await.unwrap();

        // Mock LLM responses: first tries fs_read, gets blocked, then responds to error
        os.client.set_mock_output(serde_json::json!([
            [
                "I'll read that file for you",
                {
                    "tool_use_id": "1",
                    "name": "fs_read",
                    "args": {
                        "operations": [
                            {
                                "mode": "Line",
                                "path": "/sensitive.txt"
                            }
                        ]
                    }
                }
            ],
            [
                "I understand the security policy blocked access to that file.",
            ],
        ]));

        // Create agent with blocking PreToolUse hook
        let mut agents = Agents::default();
        let mut hooks = HashMap::new();

        // Create a hook that blocks fs_read of sensitive files with exit code 2
        #[cfg(unix)]
        let hook_command = "echo 'Security policy violation: cannot read sensitive files' >&2; exit 2";
        #[cfg(windows)]
        let hook_command = "echo Security policy violation: cannot read sensitive files 1>&2 & exit /b 2";

        hooks.insert(HookTrigger::PreToolUse, vec![Hook {
            command: hook_command.to_string(),
            timeout_ms: 5000,
            max_output_size: 1024,
            cache_ttl_seconds: 0,
            matcher: Some("fs_read".to_string()),
            source: crate::cli::agent::hook::Source::Agent,
        }]);

        let agent = Agent {
            name: "SecurityAgent".to_string(),
            hooks,
            ..Default::default()
        };
        agents.agents.insert("SecurityAgent".to_string(), agent);
        agents.switch("SecurityAgent").expect("Failed to switch agent");

        let tool_manager = ToolManager::default();
        let tool_config = serde_json::from_str::<HashMap<String, ToolSpec>>(include_str!("tools/tool_index.json"))
            .expect("Tools failed to load");

        // Run chat session - hook should block tool execution
        let result = ChatSession::new(
            &mut os,
            std::io::stdout(),
            std::io::stderr(),
            "test_conv_id",
            agents,
            None,
            InputSource::new_mock(vec!["read /sensitive.txt".to_string(), "exit".to_string()]),
            false,
            || Some(80),
            tool_manager,
            None,
            tool_config,
            true,
            false,
            None,
            OutputFormat::Plain,
        )
        .await
        .unwrap()
        .spawn(&mut os)
        .await;

        // The session should complete successfully (hook blocks tool but doesn't crash)
        assert!(
            result.is_ok(),
            "Chat session should complete successfully even when hook blocks tool"
        );
    }

    #[test]
    fn test_does_input_reference_file() {
        let tests = &[
            (
                r"/Users/user/Desktop/Screenshot\ 2025-06-30\ at\ 2.13.34 PM.png read this image for me",
                true,
            ),
            ("/path/to/file.json", true),
            ("/save output.json", false),
            ("~/does/not/start/with/slash", false),
        ];
        for (input, expected) in tests {
            let actual = does_input_reference_file(input).is_some();
            assert_eq!(actual, *expected, "expected {} for input {}", expected, input);
        }
    }

    #[test]
    fn test_output_format_default() {
        let args = ChatArgs::default();
        assert_eq!(args.output_format, None);
    }

    #[test]
    fn test_output_format_plain() {
        use clap::Parser;
        #[derive(Parser)]
        struct TestCli {
            #[command(flatten)]
            chat_args: ChatArgs,
        }
        let cli = TestCli::parse_from(&["test", "--output-format", "plain"]);
        assert_eq!(cli.chat_args.output_format, Some(OutputFormat::Plain));
    }

    #[test]
    fn test_output_format_json() {
        use clap::Parser;
        #[derive(Parser)]
        struct TestCli {
            #[command(flatten)]
            chat_args: ChatArgs,
        }
        let cli = TestCli::parse_from(&["test", "--output-format", "json"]);
        assert_eq!(cli.chat_args.output_format, Some(OutputFormat::Json));
    }

    #[test]
    fn test_output_format_short_flag() {
        use clap::Parser;
        #[derive(Parser)]
        struct TestCli {
            #[command(flatten)]
            chat_args: ChatArgs,
        }
        let cli = TestCli::parse_from(&["test", "-f", "json"]);
        assert_eq!(cli.chat_args.output_format, Some(OutputFormat::Json));
    }
}

// Helper method to save the agent config to file
async fn save_agent_config(os: &mut Os, config: &Agent, agent_name: &str, is_global: bool) -> Result<(), ChatError> {
    let config_dir = if is_global {
        directories::chat_global_agent_path(os)
            .map_err(|e| ChatError::Custom(format!("Could not find global agent directory: {}", e).into()))?
    } else {
        directories::chat_local_agent_dir(os)
            .map_err(|e| ChatError::Custom(format!("Could not find local agent directory: {}", e).into()))?
    };

    tokio::fs::create_dir_all(&config_dir)
        .await
        .map_err(|e| ChatError::Custom(format!("Failed to create config directory: {}", e).into()))?;

    let config_file = config_dir.join(format!("{}.json", agent_name));
    let config_json = serde_json::to_string_pretty(config)
        .map_err(|e| ChatError::Custom(format!("Failed to serialize agent config: {}", e).into()))?;

    tokio::fs::write(&config_file, config_json)
        .await
        .map_err(|e| ChatError::Custom(format!("Failed to write agent config file: {}", e).into()))?;

    Ok(())
}
