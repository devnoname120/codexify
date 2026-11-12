pub mod agents;
mod companion;
pub mod conversation_recovery;
pub mod goal_blocking;
mod install;
mod model_fallback;
mod process;
mod recovery;
mod store;
mod supervisor;
pub(crate) mod turn_analytics;
pub mod turn_renewal;

pub use recovery::{
    BrowserCompanionControl, BrowserRecoveryWorker, watch as watch_browser_companion,
};
pub(crate) use store::conversation_url_for_retry;

pub use companion::{
    ActionPrompt, BrowserCompanionClient, COMPANION_KIND, CancelActionReceipt, CapabilityCatalog,
    CompanionActionSnapshot, CompanionActionState, CompanionAttention, CompanionEvent,
    CompanionEventKind, CompanionHealth, CompanionSelection, CompanionSessionStatus,
    CompanionUnreadMessage, EffortCapability, ExecuteActionRequest, ModelCapability,
    PROTOCOL_BUILD_SUFFIX, PROTOCOL_SCHEMA_HASH, PROTOCOL_VERSION, RevealWindowReceipt,
    RevealWindowRequest,
};
pub use install::{
    BrowserArchiveKind, BrowserCompanionManager, BrowserCompanionStatus, BrowserInstallReceipt,
    BrowserLifecycleState, BrowserReleaseAsset, BrowserRuntimeLayout, BrowserRuntimeManifest,
};
pub use store::{
    AcceptedChatBatch, AgentAutomationStore, AutomationActionKind, AutomationActionState,
    AutomationActionStatus, AutomationTaskState, ChatMirrorState, GoalFinish, GoalRecord,
    GoalStatus, GoalWakeState, MAX_GOAL_BYTES, OwnerGoalCommand, OwnerGoalCommandKind,
    OwnerGoalCommandReceipt, PAIRING_REFERENCE_TTL_MS, PAIRING_VERIFICATION_GRACE_MS, PairingIssue,
    PairingState, SelectionState,
};
pub use supervisor::{
    AgentAutomationSupervisor, AutomationExecution, AutomationExecutionError, AutomationExecutor,
    AutomationLiveState, AutomationSupervisorWorker, AutomationTaskHandle,
    BrowserAutomationExecutor, PairingCommit,
};
