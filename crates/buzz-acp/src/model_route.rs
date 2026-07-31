//! Owner-controlled, channel-scoped model routing for the Buzz ACP harness.
//!
//! This module is deliberately independent from ACP transport details. The
//! main loop owns one [`ModelRouteController`], consumes signed control events,
//! and attaches an immutable [`TurnRouteLease`] to each dispatched logical
//! turn. The Pinchpoint ACP adapter consumes the lease through its private
//! `_pinchpoint/route/arm` extension.

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::Arc;
use uuid::Uuid;

const CONTROL_FRESHNESS_SECS: i64 = 300;
const STATUS_DEDUP_CAPACITY: usize = 4096;
const MAX_DURATION_MINUTES: u32 = 1440;
const MAX_DURATION_HOURS: u32 = 24;

pub const DEFAULT_MODEL_ID: &str = "gpt-5.6-sol";
pub const OPUS_MODEL_ID: &str = "claude-opus-5";
pub const FABLE_MODEL_ID: &str = "claude-fable-5";
pub const KIMI_MODEL_ID: &str = "kimi-code/k3";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ModelTarget {
    Primary,
    Fable5,
    Opus5,
    KimiK3,
}

impl ModelTarget {
    pub fn parse(alias: &str) -> Option<Self> {
        match alias {
            "primary" => Some(Self::Primary),
            "fable5" => Some(Self::Fable5),
            "opus5" => Some(Self::Opus5),
            "kimi-k3" => Some(Self::KimiK3),
            _ => None,
        }
    }

    pub fn alias(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Fable5 => "fable5",
            Self::Opus5 => "opus5",
            Self::KimiK3 => "kimi-k3",
        }
    }

    pub fn model_id(self) -> &'static str {
        match self {
            Self::Primary => DEFAULT_MODEL_ID,
            Self::Fable5 => FABLE_MODEL_ID,
            Self::Opus5 => OPUS_MODEL_ID,
            Self::KimiK3 => KIMI_MODEL_ID,
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::Primary => "GPT-5.6 Sol",
            Self::Fable5 => "Fable 5",
            Self::Opus5 => "Opus 5",
            Self::KimiK3 => "Kimi K3",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverrideScope {
    Turn,
    Session,
    Duration {
        token: String,
        expires_at: DateTime<Utc>,
    },
}

impl OverrideScope {
    fn wire_name(&self) -> &'static str {
        match self {
            Self::Turn => "turn",
            Self::Session => "session",
            Self::Duration { .. } => "duration",
        }
    }

    fn display_name(&self) -> String {
        match self {
            Self::Turn => "next turn".to_string(),
            Self::Session => "session".to_string(),
            Self::Duration { token, .. } => token.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelCommand {
    Status,
    Reset,
    Use {
        target: ModelTarget,
        scope: ParsedScope,
        inline_prompt: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParsedScope {
    Turn,
    Session,
    Minutes(u32),
    Hours(u32),
}

impl ParsedScope {
    fn activate(self, accepted_at: DateTime<Utc>, token: &str) -> OverrideScope {
        match self {
            Self::Turn => OverrideScope::Turn,
            Self::Session => OverrideScope::Session,
            Self::Minutes(minutes) => OverrideScope::Duration {
                token: token.to_string(),
                expires_at: accepted_at + chrono::Duration::minutes(i64::from(minutes)),
            },
            Self::Hours(hours) => OverrideScope::Duration {
                token: token.to_string(),
                expires_at: accepted_at + chrono::Duration::hours(i64::from(hours)),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandParse {
    NotModelCommand,
    Valid(ModelCommand),
    Invalid,
}

/// Parse the already mention-stripped slash command.
///
/// Grammar:
/// - `/model status`
/// - `/model reset`
/// - `/model use <alias> --for turn|session|<duration>`
/// - `/model use <alias> --for turn -- <inline prompt>`
pub fn parse_command(command: &str) -> CommandParse {
    let trimmed = command.trim();
    let Some(first) = trimmed.split_whitespace().next() else {
        return CommandParse::NotModelCommand;
    };
    if first != "/model" {
        return CommandParse::NotModelCommand;
    }

    let (head, inline_prompt) = match trimmed.split_once(" -- ") {
        Some((head, prompt)) => {
            let prompt = prompt.trim();
            if prompt.is_empty() {
                return CommandParse::Invalid;
            }
            (head, Some(prompt.to_string()))
        }
        None => (trimmed, None),
    };
    let tokens: Vec<&str> = head.split_whitespace().collect();
    match tokens.as_slice() {
        ["/model", "status"] if inline_prompt.is_none() => {
            CommandParse::Valid(ModelCommand::Status)
        }
        ["/model", "reset"] if inline_prompt.is_none() => CommandParse::Valid(ModelCommand::Reset),
        ["/model", "use", alias, "--for", scope_token] => {
            let Some(target) = ModelTarget::parse(alias) else {
                return CommandParse::Invalid;
            };
            let Some(scope) = parse_scope(scope_token) else {
                return CommandParse::Invalid;
            };
            if inline_prompt.is_some() && scope != ParsedScope::Turn {
                return CommandParse::Invalid;
            }
            CommandParse::Valid(ModelCommand::Use {
                target,
                scope,
                inline_prompt,
            })
        }
        _ => CommandParse::Invalid,
    }
}

fn parse_scope(token: &str) -> Option<ParsedScope> {
    match token {
        "turn" => return Some(ParsedScope::Turn),
        "session" => return Some(ParsedScope::Session),
        _ => {}
    }
    if let Some(digits) = token.strip_suffix('m') {
        if digits.is_empty()
            || digits.len() > 4
            || digits.starts_with('0')
            || !digits.bytes().all(|b| b.is_ascii_digit())
        {
            return None;
        }
        let value: u32 = digits.parse().ok()?;
        return (1..=MAX_DURATION_MINUTES)
            .contains(&value)
            .then_some(ParsedScope::Minutes(value));
    }
    if let Some(digits) = token.strip_suffix('h') {
        if digits.is_empty()
            || digits.len() > 2
            || digits.starts_with('0')
            || !digits.bytes().all(|b| b.is_ascii_digit())
        {
            return None;
        }
        let value: u32 = digits.parse().ok()?;
        return (1..=MAX_DURATION_HOURS)
            .contains(&value)
            .then_some(ParsedScope::Hours(value));
    }
    None
}

#[derive(Debug, Clone)]
struct RoutingOverride {
    target: ModelTarget,
    scope: OverrideScope,
    generation_id: u64,
    source_event_id: String,
    inline_prompt: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct ChannelState {
    current: Option<RoutingOverride>,
    pending: Option<RoutingOverride>,
    active_lease: Option<TurnRouteLease>,
    admission_reserved: bool,
    retry_reserved: bool,
    closing: bool,
}

#[derive(Debug, Clone)]
pub struct TurnRouteLease {
    pub logical_turn_id: String,
    pub generation_id: u64,
    pub target: Option<ModelTarget>,
    pub explicit: bool,
    pub inline_prompt: Option<String>,
    pub source_event_id: Option<String>,
    attempt_started: Arc<AtomicBool>,
}

impl TurnRouteLease {
    pub fn default_chain(logical_turn_id: String) -> Self {
        Self {
            logical_turn_id,
            generation_id: 0,
            target: None,
            explicit: false,
            inline_prompt: None,
            source_event_id: None,
            attempt_started: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn model_id(&self) -> &str {
        self.target
            .map(ModelTarget::model_id)
            .unwrap_or(DEFAULT_MODEL_ID)
    }

    pub fn display_name(&self) -> &str {
        self.target
            .map(ModelTarget::display_name)
            .unwrap_or("Default")
    }

    pub fn mark_attempt_started(&self) {
        self.attempt_started.store(true, AtomicOrdering::Release);
    }

    pub fn attempt_started(&self) -> bool {
        self.attempt_started.load(AtomicOrdering::Acquire)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcquireResultKind {
    Acquired,
    Closing,
}

#[derive(Debug, Clone)]
pub struct AcquireResult {
    pub kind: AcquireResultKind,
    pub lease: Option<TurnRouteLease>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ControlResult {
    #[serde(rename = "type")]
    pub result_type: &'static str,
    pub action: &'static str,
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    pub route_state: &'static str,
    pub fallback_enabled: bool,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct ControlOutcome {
    pub result: ControlResult,
    pub consume: bool,
    pub queue_inline_turn: bool,
    pub invalidate_sessions: bool,
}

#[derive(Debug, Clone)]
pub struct LeaseCompletion {
    pub invalidate_sessions: bool,
    pub lifecycle: Vec<ControlResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct LastMutation {
    created_at: u64,
    event_id: String,
    accepted_at: String,
    expires_at: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct LedgerFile {
    version: u32,
    scopes: HashMap<String, LastMutation>,
}

struct MutationLedger {
    path: PathBuf,
    file: LedgerFile,
}

impl MutationLedger {
    fn open(path: PathBuf) -> Result<Self, String> {
        let file = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice::<LedgerFile>(&bytes)
                .map_err(|e| format!("model-route ledger is invalid: {e}"))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => LedgerFile {
                version: 1,
                scopes: HashMap::new(),
            },
            Err(error) => return Err(format!("model-route ledger read failed: {error}")),
        };
        Ok(Self { path, file })
    }

    fn is_strictly_newer(&self, scope_hash: &str, created_at: u64, event_id: &str) -> bool {
        self.file
            .scopes
            .get(scope_hash)
            .is_none_or(|last| (created_at, event_id) > (last.created_at, last.event_id.as_str()))
    }

    fn persist(
        &mut self,
        scope_hash: String,
        created_at: u64,
        event_id: String,
        accepted_at: DateTime<Utc>,
        expires_at: Option<DateTime<Utc>>,
    ) -> Result<(), String> {
        let next = LastMutation {
            created_at,
            event_id,
            accepted_at: rfc3339(accepted_at),
            expires_at: expires_at.map(rfc3339),
        };
        let previous = self.file.scopes.insert(scope_hash.clone(), next);
        if let Err(error) = atomic_write_ledger(&self.path, &self.file) {
            match previous {
                Some(value) => {
                    self.file.scopes.insert(scope_hash, value);
                }
                None => {
                    self.file.scopes.remove(&scope_hash);
                }
            }
            return Err(error);
        }
        Ok(())
    }
}

fn atomic_write_ledger(path: &Path, ledger: &LedgerFile) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "model-route ledger path has no parent".to_string())?;
    fs::create_dir_all(parent).map_err(|e| format!("model-route ledger mkdir failed: {e}"))?;
    let temp = path.with_extension(format!("tmp-{}-{}", std::process::id(), Uuid::new_v4()));
    let bytes = serde_json::to_vec(ledger)
        .map_err(|e| format!("model-route ledger serialization failed: {e}"))?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temp)
        .map_err(|e| format!("model-route ledger temp open failed: {e}"))?;
    let write_result = (|| {
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&temp, path)?;
        // Durability requires the containing directory entry, not only the
        // file bytes, to reach stable storage after the atomic rename.
        fs::File::open(parent)?.sync_all()?;
        Ok::<(), std::io::Error>(())
    })();
    if let Err(error) = write_result {
        let _ = fs::remove_file(&temp);
        return Err(format!("model-route ledger atomic write failed: {error}"));
    }
    Ok(())
}

fn default_ledger_path(agent_pubkey: &str, relay_url: &str) -> PathBuf {
    let mut hasher = Sha256::new();
    hasher.update(agent_pubkey.as_bytes());
    hasher.update(b"\0");
    hasher.update(relay_url.as_bytes());
    let identity_hash = hex::encode(hasher.finalize());
    if let Some(dir) = std::env::var_os("BUZZ_ACP_MODEL_ROUTE_LEDGER_DIR") {
        return PathBuf::from(dir).join(format!("{identity_hash}.json"));
    }
    #[cfg(target_os = "macos")]
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home)
            .join("Library/Application Support/xyz.block.buzz.app/agents/model-routing")
            .join(format!("{identity_hash}.json"));
    }
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("buzz/model-routing")
        .join(format!("{identity_hash}.json"))
}

fn scope_hash(agent_pubkey: &str, channel_id: Uuid) -> String {
    let mut hasher = Sha256::new();
    hasher.update(agent_pubkey.as_bytes());
    hasher.update(b"\0");
    hasher.update(channel_id.as_bytes());
    hex::encode(hasher.finalize())
}

fn rfc3339(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Secs, true)
}

pub struct ModelRouteController {
    agent_pubkey: String,
    channels: HashMap<Uuid, ChannelState>,
    next_generation: u64,
    ledger: Result<MutationLedger, String>,
    status_seen: HashSet<String>,
    status_order: VecDeque<String>,
    routing_enabled: bool,
    qualified_targets: HashSet<ModelTarget>,
    fallback_qualified: bool,
}

impl ModelRouteController {
    pub fn new(agent_pubkey: &str, relay_url: &str) -> Self {
        let routing_enabled = std::env::var("PINCHPOINT_ACP_ROUTING_ENABLED")
            .is_ok_and(|value| matches!(value.trim().to_ascii_lowercase().as_str(), "1" | "true"));
        let mut qualified_targets = if routing_enabled {
            HashSet::from([ModelTarget::Primary])
        } else {
            HashSet::new()
        };
        if routing_enabled {
            if let Ok(value) = std::env::var("PINCHPOINT_ACP_QUALIFIED_TARGETS") {
                for item in value.split(',').map(str::trim) {
                    let target = ModelTarget::parse(item).or(match item {
                        DEFAULT_MODEL_ID => Some(ModelTarget::Primary),
                        FABLE_MODEL_ID => Some(ModelTarget::Fable5),
                        OPUS_MODEL_ID => Some(ModelTarget::Opus5),
                        KIMI_MODEL_ID => Some(ModelTarget::KimiK3),
                        _ => None,
                    });
                    if let Some(target) = target {
                        qualified_targets.insert(target);
                    }
                }
            }
        }
        let fallback_qualified = qualified_targets.contains(&ModelTarget::Opus5)
            && qualified_targets.contains(&ModelTarget::KimiK3);
        Self::with_ledger_path_and_targets(
            agent_pubkey,
            default_ledger_path(agent_pubkey, relay_url),
            routing_enabled,
            qualified_targets,
            fallback_qualified,
        )
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn with_ledger_path(
        agent_pubkey: &str,
        ledger_path: PathBuf,
        fallback_qualified: bool,
    ) -> Self {
        let qualified_targets = if fallback_qualified {
            HashSet::from([
                ModelTarget::Primary,
                ModelTarget::Fable5,
                ModelTarget::Opus5,
                ModelTarget::KimiK3,
            ])
        } else {
            HashSet::from([ModelTarget::Primary])
        };
        Self::with_ledger_path_and_targets(
            agent_pubkey,
            ledger_path,
            true,
            qualified_targets,
            fallback_qualified,
        )
    }

    fn with_ledger_path_and_targets(
        agent_pubkey: &str,
        ledger_path: PathBuf,
        routing_enabled: bool,
        qualified_targets: HashSet<ModelTarget>,
        fallback_qualified: bool,
    ) -> Self {
        Self {
            agent_pubkey: agent_pubkey.to_ascii_lowercase(),
            channels: HashMap::new(),
            next_generation: 1,
            ledger: MutationLedger::open(ledger_path),
            status_seen: HashSet::new(),
            status_order: VecDeque::new(),
            routing_enabled,
            qualified_targets,
            fallback_qualified,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_ledger_path_and_routing(
        agent_pubkey: &str,
        ledger_path: PathBuf,
        routing_enabled: bool,
        fallback_qualified: bool,
    ) -> Self {
        let qualified_targets = if routing_enabled && fallback_qualified {
            HashSet::from([
                ModelTarget::Primary,
                ModelTarget::Fable5,
                ModelTarget::Opus5,
                ModelTarget::KimiK3,
            ])
        } else if routing_enabled {
            HashSet::from([ModelTarget::Primary])
        } else {
            HashSet::new()
        };
        Self::with_ledger_path_and_targets(
            agent_pubkey,
            ledger_path,
            routing_enabled,
            qualified_targets,
            routing_enabled && fallback_qualified,
        )
    }

    /// Whether this harness may use the private Pinchpoint ACP extension.
    ///
    /// Route leases continue to exist while disabled so queue/retry ownership
    /// remains uniform, but the harness must not forward them to prompt tasks
    /// or contact unrelated ACP children with `_pinchpoint/route/arm`.
    pub fn private_extension_enabled(&self) -> bool {
        self.routing_enabled
    }

    #[allow(clippy::too_many_arguments)]
    pub fn handle(
        &mut self,
        channel_id: Uuid,
        command: CommandParse,
        sender_pubkey: &str,
        owner_pubkey: &str,
        event_id: &str,
        created_at: u64,
        accepted_at: DateTime<Utc>,
    ) -> Option<ControlOutcome> {
        if command == CommandParse::NotModelCommand {
            return None;
        }
        if !sender_pubkey.eq_ignore_ascii_case(owner_pubkey) {
            return Some(ControlOutcome {
                result: self.rejected_result(
                    channel_id,
                    action_for_parse(&command),
                    "Model switch not applied: only the owner can change model routing.",
                ),
                consume: true,
                queue_inline_turn: false,
                invalidate_sessions: false,
            });
        }
        let now = accepted_at.timestamp();
        let event_ts = i64::try_from(created_at).unwrap_or(i64::MAX);
        if (event_ts - now).unsigned_abs() > CONTROL_FRESHNESS_SECS as u64 {
            return Some(ControlOutcome {
                result: self.rejected_result(
                    channel_id,
                    action_for_parse(&command),
                    "Model switch not applied: this control is stale or was already used.",
                ),
                consume: true,
                queue_inline_turn: false,
                invalidate_sessions: false,
            });
        }

        match command {
            CommandParse::NotModelCommand => None,
            CommandParse::Invalid => Some(ControlOutcome {
                result: self.rejected_result(
                    channel_id,
                    "invalid",
                    "Model switch not applied. Use /model use <primary|fable5|opus5|kimi-k3> --for <turn|session|1..1440m|1..24h>.",
                ),
                consume: true,
                queue_inline_turn: false,
                invalidate_sessions: false,
            }),
            CommandParse::Valid(ModelCommand::Status) => {
                if !self.remember_status(event_id) {
                    return Some(ControlOutcome {
                        result: self.rejected_result(
                            channel_id,
                            "status",
                            "Model switch not applied: this control is stale or was already used.",
                        ),
                        consume: true,
                        queue_inline_turn: false,
                        invalidate_sessions: false,
                    });
                }
                Some(ControlOutcome {
                    result: self.status_result(channel_id),
                    consume: true,
                    queue_inline_turn: false,
                    invalidate_sessions: false,
                })
            }
            CommandParse::Valid(ModelCommand::Reset) => {
                if let Err(result) = self.accept_mutation(
                    channel_id,
                    event_id,
                    created_at,
                    accepted_at,
                    None,
                    "reset",
                ) {
                    return Some(ControlOutcome {
                        result: *result,
                        consume: true,
                        queue_inline_turn: false,
                        invalidate_sessions: false,
                    });
                }
                Some(self.reset(channel_id))
            }
            CommandParse::Valid(ModelCommand::Use {
                target,
                scope,
                inline_prompt,
            }) => {
                if !self.qualified_targets.contains(&target) {
                    return Some(ControlOutcome {
                        result: self.rejected_result(
                            channel_id,
                            "use",
                            format!(
                                "Model switch not applied: {} is unavailable. Current routing is unchanged.",
                                target.display_name()
                            ),
                        ),
                        consume: true,
                        queue_inline_turn: false,
                        invalidate_sessions: false,
                    });
                }
                if inline_prompt.is_some()
                    && self
                        .channels
                        .get(&channel_id)
                        .and_then(|state| state.active_lease.as_ref())
                        .is_some()
                {
                    return Some(ControlOutcome {
                        result: self.rejected_result(
                            channel_id,
                            "use",
                            "Model switch not applied: finish or cancel the current turn before using an inline turn command.",
                        ),
                        consume: true,
                        queue_inline_turn: false,
                        invalidate_sessions: false,
                    });
                }
                let token = scope_token(scope);
                let activated_scope = scope.activate(accepted_at, &token);
                let expires_at = match &activated_scope {
                    OverrideScope::Duration { expires_at, .. } => Some(*expires_at),
                    _ => None,
                };
                if let Err(result) = self.accept_mutation(
                    channel_id,
                    event_id,
                    created_at,
                    accepted_at,
                    expires_at,
                    "use",
                ) {
                    return Some(ControlOutcome {
                        result: *result,
                        consume: true,
                        queue_inline_turn: false,
                        invalidate_sessions: false,
                    });
                }
                Some(self.use_override(
                    channel_id,
                    target,
                    activated_scope,
                    event_id.to_string(),
                    inline_prompt,
                ))
            }
        }
    }

    fn accept_mutation(
        &mut self,
        channel_id: Uuid,
        event_id: &str,
        created_at: u64,
        accepted_at: DateTime<Utc>,
        expires_at: Option<DateTime<Utc>>,
        action: &'static str,
    ) -> Result<(), Box<ControlResult>> {
        let hash = scope_hash(&self.agent_pubkey, channel_id);
        let safety_rejection = self.rejected_result(
            channel_id,
            action,
            "Model switch not applied: routing safety state is unavailable. Try again later.",
        );
        let ordering_rejection = self.rejected_result(
            channel_id,
            action,
            "Model switch not applied: this control is stale or was already used.",
        );
        let ledger = self
            .ledger
            .as_mut()
            .map_err(|_| Box::new(safety_rejection.clone()))?;
        if !ledger.is_strictly_newer(&hash, created_at, event_id) {
            return Err(Box::new(ordering_rejection));
        }
        ledger
            .persist(
                hash,
                created_at,
                event_id.to_string(),
                accepted_at,
                expires_at,
            )
            .map_err(|_| Box::new(safety_rejection))
    }

    fn remember_status(&mut self, event_id: &str) -> bool {
        if !self.status_seen.insert(event_id.to_string()) {
            return false;
        }
        self.status_order.push_back(event_id.to_string());
        if self.status_order.len() > STATUS_DEDUP_CAPACITY {
            if let Some(old) = self.status_order.pop_front() {
                self.status_seen.remove(&old);
            }
        }
        true
    }

    fn allocate_generation(&mut self) -> u64 {
        let generation = self.next_generation;
        self.next_generation = self.next_generation.saturating_add(1).max(1);
        generation
    }

    fn use_override(
        &mut self,
        channel_id: Uuid,
        target: ModelTarget,
        scope: OverrideScope,
        source_event_id: String,
        inline_prompt: Option<String>,
    ) -> ControlOutcome {
        let generation_id = self.allocate_generation();
        let replacement = RoutingOverride {
            target,
            scope: scope.clone(),
            generation_id,
            source_event_id,
            inline_prompt: inline_prompt.clone(),
        };
        let state = self.channels.entry(channel_id).or_default();
        let busy = state.active_lease.is_some();
        if busy {
            state.pending = Some(replacement);
            state.closing = true;
        } else {
            state.current = Some(replacement);
            state.pending = None;
            state.closing = false;
            if inline_prompt.is_some() {
                let current = state.current.as_ref().expect("just inserted");
                state.active_lease = Some(TurnRouteLease {
                    logical_turn_id: format!("inline-{}", current.source_event_id),
                    generation_id: current.generation_id,
                    target: Some(current.target),
                    explicit: true,
                    inline_prompt: current.inline_prompt.clone(),
                    source_event_id: Some(current.source_event_id.clone()),
                    attempt_started: Arc::new(AtomicBool::new(false)),
                });
                state.admission_reserved = true;
            }
        }

        let expires_at = match &scope {
            OverrideScope::Duration { expires_at, .. } => Some(rfc3339(*expires_at)),
            _ => None,
        };
        let message = if busy {
            format!(
                "Model switch queued: {} for {}. The current turn will finish first.",
                target.display_name(),
                scope.display_name()
            )
        } else {
            match &scope {
                OverrideScope::Turn if inline_prompt.is_some() => {
                    format!("Using {} for this turn.", target.display_name())
                }
                OverrideScope::Turn => format!(
                    "Model override set: {} for the next turn.",
                    target.display_name()
                ),
                OverrideScope::Session => format!(
                    "Model override set: {} for this session. Use /model reset to return to default.",
                    target.display_name()
                ),
                OverrideScope::Duration {
                    token,
                    expires_at,
                } => format!(
                    "Model override set: {} for {} (until {}). It will return to default automatically.",
                    target.display_name(),
                    token,
                    rfc3339(*expires_at)
                ),
            }
        };
        ControlOutcome {
            result: ControlResult {
                result_type: "model_route",
                action: "use",
                status: if busy { "pending" } else { "accepted" },
                target: Some(target.alias().to_string()),
                scope: Some(scope.wire_name().to_string()),
                expires_at,
                route_state: if busy {
                    "pending"
                } else if matches!(scope, OverrideScope::Turn) {
                    "armed"
                } else {
                    "active"
                },
                fallback_enabled: false,
                message,
            },
            consume: inline_prompt.is_none(),
            queue_inline_turn: inline_prompt.is_some(),
            invalidate_sessions: !busy,
        }
    }

    fn reset(&mut self, channel_id: Uuid) -> ControlOutcome {
        let Some(state) = self.channels.get_mut(&channel_id) else {
            return ControlOutcome {
                result: ControlResult {
                    result_type: "model_route",
                    action: "reset",
                    status: "unchanged",
                    target: None,
                    scope: None,
                    expires_at: None,
                    route_state: "default",
                    fallback_enabled: self.fallback_qualified,
                    message: "Model routing is already at the default.".to_string(),
                },
                consume: true,
                queue_inline_turn: false,
                invalidate_sessions: false,
            };
        };
        if state.current.is_none() && state.pending.is_none() && state.active_lease.is_none() {
            return ControlOutcome {
                result: ControlResult {
                    result_type: "model_route",
                    action: "reset",
                    status: "unchanged",
                    target: None,
                    scope: None,
                    expires_at: None,
                    route_state: "default",
                    fallback_enabled: self.fallback_qualified,
                    message: "Model routing is already at the default.".to_string(),
                },
                consume: true,
                queue_inline_turn: false,
                invalidate_sessions: false,
            };
        }
        state.current = None;
        state.pending = None;
        if state.active_lease.is_some() {
            let current_display = state
                .active_lease
                .as_ref()
                .map(TurnRouteLease::display_name)
                .unwrap_or("the current model");
            state.closing = true;
            ControlOutcome {
                result: ControlResult {
                    result_type: "model_route",
                    action: "reset",
                    status: "closing",
                    target: None,
                    scope: None,
                    expires_at: None,
                    route_state: "closing",
                    fallback_enabled: state.pending.is_none() && self.fallback_qualified,
                    message: format!(
                        "Model override cleared for future turns. The current turn will finish on {current_display}, then routing returns to default."
                    ),
                },
                consume: true,
                queue_inline_turn: false,
                invalidate_sessions: false,
            }
        } else {
            state.closing = false;
            ControlOutcome {
                result: ControlResult {
                    result_type: "model_route",
                    action: "reset",
                    status: "accepted",
                    target: None,
                    scope: None,
                    expires_at: None,
                    route_state: "default",
                    fallback_enabled: self.fallback_qualified,
                    message: default_cleared_message().to_string(),
                },
                consume: true,
                queue_inline_turn: false,
                invalidate_sessions: true,
            }
        }
    }

    pub fn acquire(&mut self, channel_id: Uuid, logical_turn_id: String) -> AcquireResult {
        self.expire_channel(channel_id, Utc::now());
        let state = self.channels.entry(channel_id).or_default();
        // A requeued logical turn keeps its immutable lease, including across
        // backoff and pool-slot replacement. Pending/reset/expiry state may
        // close the generation to *new* turns, but must not strand the already
        // admitted turn that owns this lease.
        if state.retry_reserved {
            let Some(lease) = state.active_lease.as_ref() else {
                state.retry_reserved = false;
                return AcquireResult {
                    kind: AcquireResultKind::Closing,
                    lease: None,
                };
            };
            state.retry_reserved = false;
            return AcquireResult {
                kind: AcquireResultKind::Acquired,
                lease: Some(lease.clone()),
            };
        }
        if state.admission_reserved {
            let Some(lease) = state.active_lease.as_ref() else {
                state.admission_reserved = false;
                return AcquireResult {
                    kind: AcquireResultKind::Closing,
                    lease: None,
                };
            };
            state.admission_reserved = false;
            return AcquireResult {
                kind: AcquireResultKind::Acquired,
                lease: Some(lease.clone()),
            };
        }
        if state.closing {
            return AcquireResult {
                kind: AcquireResultKind::Closing,
                lease: None,
            };
        }
        let lease = if let Some(current) = state.current.as_ref() {
            TurnRouteLease {
                logical_turn_id,
                generation_id: current.generation_id,
                target: Some(current.target),
                explicit: true,
                inline_prompt: current.inline_prompt.clone(),
                source_event_id: Some(current.source_event_id.clone()),
                attempt_started: Arc::new(AtomicBool::new(false)),
            }
        } else {
            TurnRouteLease::default_chain(logical_turn_id)
        };
        state.active_lease = Some(lease.clone());
        state.retry_reserved = false;
        AcquireResult {
            kind: AcquireResultKind::Acquired,
            lease: Some(lease),
        }
    }

    /// Reserve the current immutable lease for the queue retry that was just
    /// committed. A later unrelated dispatch cannot claim it; the reservation
    /// is consumed exactly once by the next channel retry attempt.
    pub fn preserve_for_retry(&mut self, channel_id: Uuid, logical_turn_id: &str) -> bool {
        let Some(state) = self.channels.get_mut(&channel_id) else {
            return false;
        };
        if state
            .active_lease
            .as_ref()
            .is_some_and(|lease| lease.logical_turn_id == logical_turn_id)
        {
            state.retry_reserved = true;
            true
        } else {
            false
        }
    }

    pub fn finish(&mut self, channel_id: Uuid, logical_turn_id: &str) -> LeaseCompletion {
        // A pending duration can expire while it waits behind an immutable
        // active lease. Re-read authoritative UTC at teardown so a delayed
        // timer wake can never activate an already-expired replacement.
        let mut lifecycle = self
            .expire_channel(channel_id, Utc::now())
            .map(|(_, result, _)| vec![result])
            .unwrap_or_default();
        let Some(state) = self.channels.get_mut(&channel_id) else {
            return LeaseCompletion {
                invalidate_sessions: false,
                lifecycle,
            };
        };
        let Some(lease) = state.active_lease.take() else {
            return LeaseCompletion {
                invalidate_sessions: false,
                lifecycle,
            };
        };
        state.admission_reserved = false;
        state.retry_reserved = false;
        if lease.logical_turn_id != logical_turn_id {
            state.active_lease = Some(lease);
            return LeaseCompletion {
                invalidate_sessions: false,
                lifecycle,
            };
        }

        let turn_scope_finished = state.current.as_ref().is_some_and(|current| {
            current.generation_id == lease.generation_id
                && matches!(current.scope, OverrideScope::Turn)
        });
        if turn_scope_finished {
            state.current = None;
            state.closing = true;
        }
        if !state.closing {
            return LeaseCompletion {
                invalidate_sessions: false,
                lifecycle,
            };
        }

        if let Some(next) = state.pending.take() {
            let target = next.target;
            let scope = next.scope.clone();
            state.current = Some(next);
            state.closing = false;
            lifecycle.push(ControlResult {
                result_type: "model_route",
                action: "activated",
                status: "accepted",
                target: Some(target.alias().to_string()),
                scope: Some(scope.wire_name().to_string()),
                expires_at: match scope {
                    OverrideScope::Duration { expires_at, .. } => Some(rfc3339(expires_at)),
                    _ => None,
                },
                route_state: if matches!(scope, OverrideScope::Turn) {
                    "armed"
                } else {
                    "active"
                },
                fallback_enabled: false,
                message: format!(
                    "Model switch active: {} for {}.",
                    target.display_name(),
                    scope.display_name()
                ),
            });
            LeaseCompletion {
                invalidate_sessions: true,
                lifecycle,
            }
        } else {
            state.current = None;
            state.closing = false;
            lifecycle.push(ControlResult {
                result_type: "model_route",
                action: "reverted",
                status: "accepted",
                target: None,
                scope: None,
                expires_at: None,
                route_state: "default",
                fallback_enabled: self.fallback_qualified,
                message: default_reverted_message().to_string(),
            });
            LeaseCompletion {
                invalidate_sessions: true,
                lifecycle,
            }
        }
    }

    pub fn expire_due(&mut self, now: DateTime<Utc>) -> Vec<(Uuid, ControlResult, bool)> {
        let channels: Vec<Uuid> = self.channels.keys().copied().collect();
        channels
            .into_iter()
            .filter_map(|channel_id| self.expire_channel(channel_id, now))
            .collect()
    }

    fn expire_channel(
        &mut self,
        channel_id: Uuid,
        now: DateTime<Utc>,
    ) -> Option<(Uuid, ControlResult, bool)> {
        let state = self.channels.get_mut(&channel_id)?;
        let current_expired = state.current.as_ref().is_some_and(|current| {
            matches!(
                current.scope,
                OverrideScope::Duration { expires_at, .. } if now >= expires_at
            )
        });
        let pending_expired = state.pending.as_ref().is_some_and(|pending| {
            matches!(
                pending.scope,
                OverrideScope::Duration { expires_at, .. } if now >= expires_at
            )
        });
        if !current_expired && !pending_expired {
            return None;
        }
        if current_expired {
            state.current = None;
        }
        if pending_expired {
            state.pending = None;
        }
        if state.active_lease.is_some() {
            let current_display = state
                .active_lease
                .as_ref()
                .map(TurnRouteLease::display_name)
                .unwrap_or("the current model");
            state.closing = true;
            Some((
                channel_id,
                ControlResult {
                    result_type: "model_route",
                    action: "expired",
                    status: "closing",
                    target: None,
                    scope: Some("duration".to_string()),
                    expires_at: None,
                    route_state: "closing",
                    fallback_enabled: state.pending.is_none() && self.fallback_qualified,
                    message: format!(
                        "Model override expired. The current turn will finish on {current_display}; new turns wait, then return to default."
                    ),
                },
                false,
            ))
        } else {
            // A pending override only exists behind an active lease. Fail
            // closed if state was externally disturbed: an expired
            // replacement must never revive the superseded route.
            if pending_expired {
                state.current = None;
            }
            state.closing = false;
            Some((
                channel_id,
                ControlResult {
                    result_type: "model_route",
                    action: "expired",
                    status: "accepted",
                    target: None,
                    scope: Some("duration".to_string()),
                    expires_at: None,
                    route_state: "default",
                    fallback_enabled: self.fallback_qualified,
                    message: default_expired_message().to_string(),
                },
                true,
            ))
        }
    }

    pub fn end_session_scope(&mut self, channel_id: Uuid) -> bool {
        let Some(state) = self.channels.get_mut(&channel_id) else {
            return false;
        };
        let current_session = state
            .current
            .as_ref()
            .is_some_and(|current| matches!(current.scope, OverrideScope::Session));
        let pending_session = state
            .pending
            .as_ref()
            .is_some_and(|pending| matches!(pending.scope, OverrideScope::Session));
        if current_session {
            state.current = None;
        }
        if pending_session {
            state.pending = None;
        }
        if current_session || pending_session {
            state.closing = state.active_lease.is_some();
            return true;
        }
        false
    }

    pub fn clear_channel(&mut self, channel_id: Uuid) {
        self.channels.remove(&channel_id);
    }

    pub fn next_expiry_delay(&self, now: DateTime<Utc>) -> Option<std::time::Duration> {
        self.channels
            .values()
            .flat_map(|state| {
                [state.current.as_ref(), state.pending.as_ref()]
                    .into_iter()
                    .flatten()
            })
            .filter_map(|route| match route.scope {
                OverrideScope::Duration { expires_at, .. } => Some(expires_at),
                OverrideScope::Turn | OverrideScope::Session => None,
            })
            .map(|expires_at| {
                let millis = (expires_at - now).num_milliseconds().max(0);
                std::time::Duration::from_millis(millis as u64)
            })
            .min()
    }

    fn fallback_for_next(&self, channel_id: Uuid) -> bool {
        let next_is_default = self.channels.get(&channel_id).is_none_or(|state| {
            if state.closing {
                state.pending.is_none()
            } else {
                state.current.is_none()
            }
        });
        next_is_default && self.fallback_qualified
    }

    fn rejected_result(
        &self,
        channel_id: Uuid,
        action: &'static str,
        message: impl Into<String>,
    ) -> ControlResult {
        let mut result = self.status_result(channel_id);
        result.action = action;
        result.status = "rejected";
        result.message = message.into();
        result
    }

    fn status_result(&self, channel_id: Uuid) -> ControlResult {
        let state = self.channels.get(&channel_id);
        let current = state
            .and_then(|state| state.active_lease.as_ref())
            .and_then(|lease| lease.target)
            .or_else(|| {
                state
                    .and_then(|state| state.current.as_ref())
                    .map(|o| o.target)
            });
        let next = state.and_then(|state| {
            state.pending.as_ref().or({
                if state.closing {
                    None
                } else {
                    state.current.as_ref()
                }
            })
        });
        let (scope, expires_at) = next
            .map(|value| {
                let scope = match &value.scope {
                    OverrideScope::Turn => "next turn".to_string(),
                    OverrideScope::Session => "session".to_string(),
                    OverrideScope::Duration { expires_at, .. } => {
                        format!("until {}", rfc3339(*expires_at))
                    }
                };
                let expires = match value.scope {
                    OverrideScope::Duration { expires_at, .. } => Some(rfc3339(expires_at)),
                    _ => None,
                };
                (scope, expires)
            })
            .unwrap_or_else(|| ("default".to_string(), None));
        let route_state = state.map_or("default", |state| {
            if state.closing {
                "closing"
            } else if state.pending.is_some() {
                "pending"
            } else if state
                .current
                .as_ref()
                .is_some_and(|o| matches!(o.scope, OverrideScope::Turn))
            {
                "armed"
            } else if state.current.is_some() {
                "active"
            } else {
                "default"
            }
        });
        let current_display = current.map(ModelTarget::display_name).unwrap_or("Default");
        let next_display = state
            .and_then(|state| state.pending.as_ref())
            .map(|value| value.target.display_name())
            .or_else(|| next.map(|value| value.target.display_name()))
            .unwrap_or("Default");
        let message = format!(
            "Model routing\nCurrent: {current_display}\nScope: {scope}\nState: {route_state}\nNext: {next_display}\nDefault: GPT-5.6 Sol\nFallback without an override: Opus 5, then Kimi K3"
        );
        ControlResult {
            result_type: "model_route",
            action: "status",
            status: "unchanged",
            target: next.map(|value| value.target.alias().to_string()),
            scope: next.map(|value| value.scope.wire_name().to_string()),
            expires_at,
            route_state,
            fallback_enabled: self.fallback_for_next(channel_id),
            message,
        }
    }
}

fn action_for_parse(command: &CommandParse) -> &'static str {
    match command {
        CommandParse::Valid(ModelCommand::Status) => "status",
        CommandParse::Valid(ModelCommand::Reset) => "reset",
        CommandParse::Valid(ModelCommand::Use { .. }) => "use",
        CommandParse::Invalid | CommandParse::NotModelCommand => "invalid",
    }
}

fn scope_token(scope: ParsedScope) -> String {
    match scope {
        ParsedScope::Turn => "turn".to_string(),
        ParsedScope::Session => "session".to_string(),
        ParsedScope::Minutes(value) => format!("{value}m"),
        ParsedScope::Hours(value) => format!("{value}h"),
    }
}

fn default_cleared_message() -> &'static str {
    "Model override cleared. New turns use GPT-5.6 Sol (fallback: Opus 5, then Kimi K3)."
}

fn default_expired_message() -> &'static str {
    "Model override expired. New turns use GPT-5.6 Sol (fallback: Opus 5, then Kimi K3)."
}

fn default_reverted_message() -> &'static str {
    "Routing returned to default: GPT-5.6 Sol (fallback: Opus 5, then Kimi K3)."
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(1);

    fn controller() -> ModelRouteController {
        let path = std::env::temp_dir().join(format!(
            "buzz-model-route-test-{}-{}.json",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_file(&path);
        ModelRouteController::with_ledger_path("aa", path, true)
    }

    fn parse_valid(value: &str) -> ModelCommand {
        match parse_command(value) {
            CommandParse::Valid(command) => command,
            other => panic!("expected valid command, got {other:?}"),
        }
    }

    #[test]
    fn exact_duration_grammar() {
        for valid in [
            "/model use opus5 --for turn",
            "/model use fable5 --for session",
            "/model use kimi-k3 --for 1m",
            "/model use primary --for 1440m",
            "/model use opus5 --for 1h",
            "/model use opus5 --for 24h",
            "/model use opus5 --for turn -- review this",
        ] {
            assert!(
                matches!(parse_command(valid), CommandParse::Valid(_)),
                "{valid}"
            );
        }
        for invalid in [
            "/model use opus5",
            "/model use opus5 --for 0m",
            "/model use opus5 --for 01m",
            "/model use opus5 --for 1441m",
            "/model use opus5 --for 00001m",
            "/model use opus5 --for 0h",
            "/model use opus5 --for 01h",
            "/model use opus5 --for 25h",
            "/model use opus5 --for 1.5h",
            "/model use opus5 --for -1h",
            "/model use opus5 --for 1H",
            "/model use opus5 --for session -- prompt",
            "/model use OPUS5 --for turn",
            "/model status extra",
        ] {
            assert_eq!(parse_command(invalid), CommandParse::Invalid, "{invalid}");
        }
        assert_eq!(
            parse_command("/goal inspect"),
            CommandParse::NotModelCommand
        );
    }

    #[test]
    fn status_does_not_advance_mutation_order() {
        let mut controller = controller();
        let channel = Uuid::new_v4();
        let now = Utc::now();
        let status = controller
            .handle(
                channel,
                CommandParse::Valid(ModelCommand::Status),
                "owner",
                "owner",
                "ff",
                now.timestamp() as u64,
                now,
            )
            .unwrap();
        assert_eq!(status.result.status, "unchanged");
        let use_result = controller
            .handle(
                channel,
                CommandParse::Valid(parse_valid("/model use opus5 --for session")),
                "owner",
                "owner",
                "00",
                now.timestamp() as u64,
                now,
            )
            .unwrap();
        assert_eq!(use_result.result.status, "accepted");
    }

    #[test]
    fn relay_order_is_created_at_then_event_id() {
        let mut controller = controller();
        let channel = Uuid::new_v4();
        let now = Utc::now();
        let accepted = controller
            .handle(
                channel,
                CommandParse::Valid(parse_valid("/model use opus5 --for session")),
                "owner",
                "owner",
                "bb",
                now.timestamp() as u64,
                now,
            )
            .unwrap();
        assert_eq!(accepted.result.status, "accepted");
        let rejected = controller
            .handle(
                channel,
                CommandParse::Valid(ModelCommand::Reset),
                "owner",
                "owner",
                "aa",
                now.timestamp() as u64,
                now,
            )
            .unwrap();
        assert_eq!(rejected.result.status, "rejected");
    }

    #[test]
    fn durable_ledger_rejects_replay_after_controller_restart_without_storing_route_text() {
        let path = std::env::temp_dir().join(format!(
            "buzz-model-route-restart-test-{}-{}.json",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_file(&path);
        let channel = Uuid::new_v4();
        let now = Utc::now();
        {
            let mut first =
                ModelRouteController::with_ledger_path("owner-pubkey", path.clone(), true);
            let accepted = first
                .handle(
                    channel,
                    CommandParse::Valid(parse_valid("/model use kimi-k3 --for session")),
                    "owner",
                    "owner",
                    "bb",
                    now.timestamp() as u64,
                    now,
                )
                .unwrap();
            assert_eq!(accepted.result.status, "accepted");
        }

        let serialized = fs::read_to_string(&path).unwrap();
        assert!(!serialized.contains("kimi"));
        assert!(!serialized.contains("owner-pubkey"));
        assert!(!serialized.contains("/model"));

        let mut restarted =
            ModelRouteController::with_ledger_path("owner-pubkey", path.clone(), true);
        let replay = restarted
            .handle(
                channel,
                CommandParse::Valid(ModelCommand::Reset),
                "owner",
                "owner",
                "aa",
                now.timestamp() as u64,
                now,
            )
            .unwrap();
        assert_eq!(replay.result.status, "rejected");
        let _ = fs::remove_file(path);
    }

    #[test]
    fn one_turn_lease_closes_and_reverts() {
        let mut controller = controller();
        let channel = Uuid::new_v4();
        let now = Utc::now();
        let result = controller
            .handle(
                channel,
                CommandParse::Valid(parse_valid("/model use fable5 --for turn")),
                "owner",
                "owner",
                "01",
                now.timestamp() as u64,
                now,
            )
            .unwrap();
        assert_eq!(result.result.status, "accepted");
        let acquired = controller.acquire(channel, "turn-1".to_string());
        let lease = acquired.lease.unwrap();
        assert_eq!(lease.target, Some(ModelTarget::Fable5));
        let completion = controller.finish(channel, "turn-1");
        assert!(completion.invalidate_sessions);
        let lifecycle = completion.lifecycle.last().unwrap();
        assert_eq!(lifecycle.action, "reverted");
        assert_eq!(lifecycle.status, "accepted");
    }

    #[test]
    fn replacement_waits_and_activates() {
        let mut controller = controller();
        let channel = Uuid::new_v4();
        let now = Utc::now();
        controller
            .handle(
                channel,
                CommandParse::Valid(parse_valid("/model use opus5 --for session")),
                "owner",
                "owner",
                "01",
                now.timestamp() as u64,
                now,
            )
            .unwrap();
        controller.acquire(channel, "turn-1".to_string());
        let pending = controller
            .handle(
                channel,
                CommandParse::Valid(parse_valid("/model use kimi-k3 --for session")),
                "owner",
                "owner",
                "02",
                now.timestamp() as u64 + 1,
                now,
            )
            .unwrap();
        assert_eq!(pending.result.status, "pending");
        let completion = controller.finish(channel, "turn-1");
        let lifecycle = completion.lifecycle.last().unwrap();
        assert_eq!(lifecycle.action, "activated");
        assert_eq!(lifecycle.target.as_deref(), Some("kimi-k3"));
    }

    #[test]
    fn requeue_reacquires_same_logical_turn_lease_before_pending_activation() {
        let mut controller = controller();
        let channel = Uuid::new_v4();
        let now = Utc::now();
        controller
            .handle(
                channel,
                CommandParse::Valid(parse_valid("/model use opus5 --for turn")),
                "owner",
                "owner",
                "01",
                now.timestamp() as u64,
                now,
            )
            .unwrap();
        let first = controller
            .acquire(channel, "logical-turn-1".to_string())
            .lease
            .unwrap();
        controller
            .handle(
                channel,
                CommandParse::Valid(parse_valid("/model use kimi-k3 --for session")),
                "owner",
                "owner",
                "02",
                now.timestamp() as u64 + 1,
                now,
            )
            .unwrap();
        assert!(controller.preserve_for_retry(channel, &first.logical_turn_id));

        let retry = controller
            .acquire(channel, "must-not-replace-id".to_string())
            .lease
            .unwrap();
        assert_eq!(retry.logical_turn_id, first.logical_turn_id);
        assert_eq!(retry.generation_id, first.generation_id);
        assert_eq!(retry.target, Some(ModelTarget::Opus5));

        let completion = controller.finish(channel, &retry.logical_turn_id);
        assert_eq!(completion.lifecycle.last().unwrap().action, "activated");
        assert_eq!(
            completion.lifecycle.last().unwrap().target.as_deref(),
            Some("kimi-k3")
        );
    }

    #[test]
    fn duration_uses_utc_and_closes_active_lease() {
        let mut controller = controller();
        let channel = Uuid::new_v4();
        let now = Utc::now();
        controller
            .handle(
                channel,
                CommandParse::Valid(parse_valid("/model use opus5 --for 1m")),
                "owner",
                "owner",
                "01",
                now.timestamp() as u64,
                now,
            )
            .unwrap();
        controller.acquire(channel, "turn-1".to_string());
        let events = controller.expire_due(now + chrono::Duration::minutes(2));
        assert_eq!(events[0].1.status, "closing");
        assert_eq!(
            controller.acquire(channel, "turn-2".to_string()).kind,
            AcquireResultKind::Closing
        );
        let completion = controller.finish(channel, "turn-1");
        assert_eq!(completion.lifecycle.last().unwrap().action, "reverted");
    }

    #[test]
    fn inline_turn_is_rejected_while_busy() {
        let mut controller = controller();
        let channel = Uuid::new_v4();
        let now = Utc::now();
        controller.acquire(channel, "turn-1".to_string());
        let outcome = controller
            .handle(
                channel,
                CommandParse::Valid(parse_valid("/model use opus5 --for turn -- inspect")),
                "owner",
                "owner",
                "01",
                now.timestamp() as u64,
                now,
            )
            .unwrap();
        assert_eq!(outcome.result.status, "rejected");
        assert!(!outcome.queue_inline_turn);
    }

    #[test]
    fn accepted_inline_turn_is_reserved_before_later_route_changes() {
        let mut controller = controller();
        let channel = Uuid::new_v4();
        let now = Utc::now();
        let inline = controller
            .handle(
                channel,
                CommandParse::Valid(parse_valid("/model use fable5 --for turn -- review this")),
                "owner",
                "owner",
                "inline-event",
                now.timestamp() as u64,
                now,
            )
            .unwrap();
        assert!(inline.queue_inline_turn);
        assert_eq!(inline.result.message, "Using Fable 5 for this turn.");

        let later = controller
            .handle(
                channel,
                CommandParse::Valid(parse_valid("/model use opus5 --for session")),
                "owner",
                "owner",
                "later-event",
                now.timestamp() as u64 + 1,
                now,
            )
            .unwrap();
        assert_eq!(later.result.status, "pending");

        let lease = controller
            .acquire(channel, "dispatch-id-must-not-win".to_string())
            .lease
            .unwrap();
        assert_eq!(lease.logical_turn_id, "inline-inline-event");
        assert_eq!(lease.target, Some(ModelTarget::Fable5));
        assert_eq!(lease.inline_prompt.as_deref(), Some("review this"));
        assert_eq!(lease.source_event_id.as_deref(), Some("inline-event"));

        let completion = controller.finish(channel, &lease.logical_turn_id);
        assert_eq!(completion.lifecycle.last().unwrap().action, "activated");
        assert_eq!(
            completion.lifecycle.last().unwrap().target.as_deref(),
            Some("opus5")
        );
    }

    #[test]
    fn pending_duration_expiry_never_activates_superseded_route() {
        let mut controller = controller();
        let channel = Uuid::new_v4();
        let now = Utc::now();
        controller
            .handle(
                channel,
                CommandParse::Valid(parse_valid("/model use opus5 --for session")),
                "owner",
                "owner",
                "01",
                now.timestamp() as u64,
                now,
            )
            .unwrap();
        controller.acquire(channel, "turn-1".to_string());
        controller
            .handle(
                channel,
                CommandParse::Valid(parse_valid("/model use kimi-k3 --for 1m")),
                "owner",
                "owner",
                "02",
                now.timestamp() as u64 + 1,
                now,
            )
            .unwrap();
        assert!(
            controller.next_expiry_delay(now).unwrap() <= std::time::Duration::from_secs(60),
            "a pending duration must schedule the UTC expiry wake"
        );

        let expired = controller.expire_due(now + chrono::Duration::minutes(2));
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].1.action, "expired");
        assert_eq!(expired[0].1.status, "closing");

        let status = controller
            .handle(
                channel,
                CommandParse::Valid(ModelCommand::Status),
                "owner",
                "owner",
                "status-1",
                now.timestamp() as u64 + 2,
                now,
            )
            .unwrap();
        assert_eq!(status.result.route_state, "closing");
        assert_eq!(status.result.target, None);
        assert!(status.result.fallback_enabled);
        assert!(status.result.message.contains("Next: Default"));

        let completion = controller.finish(channel, "turn-1");
        assert_eq!(completion.lifecycle.last().unwrap().action, "reverted");
        assert!(
            completion
                .lifecycle
                .iter()
                .all(|event| event.action != "activated"),
            "an expired pending duration must never activate"
        );
    }

    #[test]
    fn owner_rotate_removes_pending_session_without_reviving_old_scope() {
        let mut controller = controller();
        let channel = Uuid::new_v4();
        let now = Utc::now();
        controller
            .handle(
                channel,
                CommandParse::Valid(parse_valid("/model use opus5 --for 1h")),
                "owner",
                "owner",
                "01",
                now.timestamp() as u64,
                now,
            )
            .unwrap();
        let lease = controller
            .acquire(channel, "turn-1".to_string())
            .lease
            .unwrap();
        controller
            .handle(
                channel,
                CommandParse::Valid(parse_valid("/model use kimi-k3 --for session")),
                "owner",
                "owner",
                "02",
                now.timestamp() as u64 + 1,
                now,
            )
            .unwrap();

        assert!(controller.end_session_scope(channel));
        let completion = controller.finish(channel, &lease.logical_turn_id);
        assert_eq!(completion.lifecycle.last().unwrap().action, "reverted");
        assert!(completion
            .lifecycle
            .iter()
            .all(|event| event.action != "activated"));
        assert!(
            controller
                .acquire(channel, "turn-default".to_string())
                .lease
                .unwrap()
                .target
                .is_none(),
            "the older duration must not revive after its pending session replacement is rotated"
        );
    }

    #[test]
    fn authorization_freshness_and_status_dedup_are_fail_closed() {
        let mut controller = controller();
        let channel = Uuid::new_v4();
        let now = Utc::now();
        let unauthorized = controller
            .handle(
                channel,
                CommandParse::Valid(parse_valid("/model use opus5 --for session")),
                "sibling",
                "owner",
                "01",
                now.timestamp() as u64,
                now,
            )
            .unwrap();
        assert_eq!(unauthorized.result.status, "rejected");

        let stale = controller
            .handle(
                channel,
                CommandParse::Valid(parse_valid("/model use opus5 --for session")),
                "owner",
                "owner",
                "02",
                (now - chrono::Duration::seconds(CONTROL_FRESHNESS_SECS + 1)).timestamp() as u64,
                now,
            )
            .unwrap();
        assert_eq!(stale.result.status, "rejected");

        let first_status = controller
            .handle(
                channel,
                CommandParse::Valid(ModelCommand::Status),
                "owner",
                "owner",
                "status-duplicate",
                now.timestamp() as u64,
                now,
            )
            .unwrap();
        assert_eq!(first_status.result.status, "unchanged");
        let duplicate_status = controller
            .handle(
                channel,
                CommandParse::Valid(ModelCommand::Status),
                "owner",
                "owner",
                "status-duplicate",
                now.timestamp() as u64,
                now,
            )
            .unwrap();
        assert_eq!(duplicate_status.result.status, "rejected");

        let active = controller
            .handle(
                channel,
                CommandParse::Valid(parse_valid("/model use opus5 --for session")),
                "owner",
                "owner",
                "03",
                now.timestamp() as u64 + 1,
                now,
            )
            .unwrap();
        assert_eq!(active.result.status, "accepted");
        let rejected_reset = controller
            .handle(
                channel,
                CommandParse::Valid(ModelCommand::Reset),
                "sibling",
                "owner",
                "04",
                now.timestamp() as u64 + 2,
                now,
            )
            .unwrap();
        assert_eq!(rejected_reset.result.status, "rejected");
        assert_eq!(rejected_reset.result.target.as_deref(), Some("opus5"));
        assert_eq!(rejected_reset.result.scope.as_deref(), Some("session"));
        assert_eq!(rejected_reset.result.route_state, "active");
        assert!(!rejected_reset.result.fallback_enabled);
    }

    #[test]
    fn unqualified_target_and_unwritable_ledger_leave_route_unchanged() {
        let now = Utc::now();
        let channel = Uuid::new_v4();
        let base = std::env::temp_dir().join(format!(
            "buzz-model-route-ledger-parent-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&base, b"not a directory").unwrap();
        let ledger_path = base.join("ledger.json");
        let mut controller = ModelRouteController::with_ledger_path("owner", ledger_path, false);

        let unqualified = controller
            .handle(
                channel,
                CommandParse::Valid(parse_valid("/model use opus5 --for session")),
                "owner",
                "owner",
                "01",
                now.timestamp() as u64,
                now,
            )
            .unwrap();
        assert_eq!(unqualified.result.status, "rejected");

        let ledger_failure = controller
            .handle(
                channel,
                CommandParse::Valid(parse_valid("/model use primary --for session")),
                "owner",
                "owner",
                "02",
                now.timestamp() as u64 + 1,
                now,
            )
            .unwrap();
        assert_eq!(ledger_failure.result.status, "rejected");
        assert!(ledger_failure.result.message.contains("safety state"));
        let lease = controller
            .acquire(channel, "turn-default".to_string())
            .lease
            .unwrap();
        assert!(!lease.explicit);
        let _ = fs::remove_file(base);
    }
}
