//! Host-recorded conversation timing retained independently of model message payloads.

use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;

/// Cumulative observations through the history item carrying this snapshot.
///
/// This is temporal context, not evidence of user authorization or UI delivery.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ConversationTiming {
    /// Clock used for these observations, such as `system` or `external`.
    /// Observations from different clock sources must not be combined.
    pub clock_source: String,
    /// Latest user message recorded by the harness, including an unknown time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_user_message: Option<ConversationMessageTiming>,
    /// Latest assistant message completed by the harness, including an unknown time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_assistant_message: Option<ConversationMessageTiming>,
}

/// One observed message and the time at which the harness recorded its boundary.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ConversationMessageTiming {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_id: Option<String>,
    /// Unix time in milliseconds. `None` means this message's time is unknown;
    /// consumers must not substitute an earlier message's known timestamp.
    #[serde(default)]
    pub at_ms: Option<i64>,
}
