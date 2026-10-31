use std::time::Duration;

use chrono::DateTime;
use chrono::Utc;
use codex_features::CurrentTimeReminderDeliveryMode;
use codex_features::CurrentTimeSource;
use codex_features::Feature;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::models::ResponseItem;

use super::session::Session;
use super::turn_context::TurnContext;
use crate::config::Config;
use crate::config::CurrentTimeReminderConfig;
use crate::context::ContextualUserFragment;
use crate::context::CurrentTimeUnavailable;
use crate::context_manager::is_user_turn_boundary;

const CONVERSATION_TIME_CAPTURE_TIMEOUT: Duration = Duration::from_secs(1);

pub(super) struct ConversationTimeCapture {
    pub(super) clock_source: &'static str,
    pub(super) at_ms: Option<i64>,
}

fn clock_source_name(source: CurrentTimeSource) -> &'static str {
    match source {
        CurrentTimeSource::System => "system",
        CurrentTimeSource::External => "external",
    }
}

pub(super) fn apply_persistent_defaults(config: &mut Config) {
    if config.current_time_reminder.is_some()
        || config
            .config_layer_stack
            .effective_config()
            .get("features")
            .and_then(|features| features.get("current_time_reminder"))
            .is_some()
    {
        return;
    }

    // Apply defaults only to this turn; explicit settings and managed policy win.
    if config.features.enable(Feature::CurrentTimeReminder).is_ok()
        && config.features.enabled(Feature::CurrentTimeReminder)
    {
        config.current_time_reminder = Some(CurrentTimeReminderConfig {
            sleep_tool: true,
            ..CurrentTimeReminderConfig::default()
        });
    }
}

#[derive(Default)]
pub(crate) struct CurrentTimeReminderState {
    last_delivery_time: Option<DateTime<Utc>>,
    last_window_id: Option<String>,
    last_clock_failure: Option<(String, uuid::Uuid)>,
    pending_user_or_tool_output_boundary: bool,
}

impl CurrentTimeReminderState {
    pub(super) fn note_recorded_items(&mut self, items: &[ResponseItem]) {
        if items.iter().any(|item| {
            is_user_turn_boundary(item)
                || matches!(
                    item,
                    ResponseItem::FunctionCallOutput { .. }
                        | ResponseItem::CustomToolCallOutput { .. }
                        | ResponseItem::ToolSearchOutput { .. }
                )
        }) {
            self.pending_user_or_tool_output_boundary = true;
        }
    }

    fn take_reminder_due(
        &mut self,
        window_id: &str,
        current_time: DateTime<Utc>,
        interval_seconds: u64,
        delivery_mode: CurrentTimeReminderDeliveryMode,
    ) -> bool {
        let is_new_window = self.last_window_id.as_deref() != Some(window_id);
        // Consume the boundary for this inference even if the interval suppresses delivery.
        let follows_user_or_tool_output =
            std::mem::take(&mut self.pending_user_or_tool_output_boundary);
        if delivery_mode == CurrentTimeReminderDeliveryMode::AfterUserOrToolOutput
            && !is_new_window
            && !follows_user_or_tool_output
        {
            return false;
        }

        let reminder_is_due = is_new_window
            || interval_seconds == 0
            || self.last_delivery_time.is_none_or(|last_delivery_time| {
                current_time
                    .signed_duration_since(last_delivery_time)
                    .num_seconds()
                    >= i64::try_from(interval_seconds).unwrap_or(i64::MAX)
            });

        if reminder_is_due {
            self.last_delivery_time = Some(current_time);
            self.last_window_id = Some(window_id.to_string());
        }

        reminder_is_due
    }
}

impl Session {
    pub(super) async fn capture_conversation_time(
        &self,
        turn_context: &TurnContext,
    ) -> Option<ConversationTimeCapture> {
        let config = turn_context.config.current_time_reminder?;
        if !config.include_conversation_timing
            || !turn_context
                .config
                .features
                .enabled(Feature::CurrentTimeReminder)
        {
            return None;
        }
        // Bound optional clock reads so accepted input and completed answers reach history.
        let at_ms = match tokio::time::timeout(
            CONVERSATION_TIME_CAPTURE_TIMEOUT,
            self.services.time_provider.current_time(self.thread_id),
        )
        .await
        {
            Ok(Ok(time)) => Some(time.timestamp_millis()),
            Ok(Err(_)) | Err(_) => {
                tracing::warn!(thread_id = %self.thread_id, "conversation event time unavailable or timed out");
                None
            }
        };
        Some(ConversationTimeCapture {
            clock_source: clock_source_name(config.clock_source),
            at_ms,
        })
    }

    pub(super) async fn read_clock_for_context(
        &self,
        turn_context: &TurnContext,
        clock_read: &'static str,
    ) -> CodexResult<Option<DateTime<Utc>>> {
        let error = match self
            .services
            .time_provider
            .current_time(self.thread_id)
            .await
        {
            Ok(time) => {
                let mut state = self.state.lock().await;
                state.current_time_reminder.last_clock_failure = None;
                return Ok(Some(time));
            }
            Err(error) => error,
        };
        if !turn_context
            .config
            .features
            .enabled(Feature::NonfatalClockReadErrors)
        {
            return Err(CodexErr::Fatal(format!(
                "failed to read current time: {error:#}"
            )));
        }
        tracing::error!(
            clock_read,
            thread_id = %self.thread_id,
            turn_id = %turn_context.sub_id,
            "failed to read current time; the clock provider may be stalled"
        );
        {
            let mut state = self.state.lock().await;
            let failure = (
                turn_context.sub_id.clone(),
                state.auto_compact_window_ids().window_id,
            );
            // A compacted window may no longer contain the earlier notice.
            if state.current_time_reminder.last_clock_failure.as_ref() == Some(&failure) {
                return Ok(None);
            }
            state.current_time_reminder.last_clock_failure = Some(failure);
        }
        let response_item = ContextualUserFragment::into(CurrentTimeUnavailable);
        self.record_conversation_items(
            turn_context,
            turn_context.model_info(),
            std::slice::from_ref(&response_item),
        )
        .await;
        Ok(None)
    }
}

pub(super) async fn maybe_record_current_time_reminder(
    sess: &Session,
    turn_context: &TurnContext,
    window_id: &str,
) -> CodexResult<()> {
    if !turn_context
        .config
        .features
        .enabled(Feature::CurrentTimeReminder)
    {
        return Ok(());
    }
    let Some(config) = turn_context.config.current_time_reminder else {
        return Ok(());
    };

    let Some(current_time) = sess
        .read_clock_for_context(turn_context, "reminder")
        .await?
    else {
        return Ok(());
    };

    let reminder_is_due = {
        let mut state = sess.state.lock().await;
        state.current_time_reminder.take_reminder_due(
            window_id,
            current_time,
            config.reminder_interval_seconds,
            config.delivery_mode,
        )
    };
    if !reminder_is_due {
        return Ok(());
    }

    let mut reminder = crate::context::CurrentTimeReminder::new(current_time);
    if config.include_conversation_timing {
        let state = sess.state.lock().await;
        let timing = super::conversation_timing::with_clock_source(
            super::conversation_timing::recover(state.history.annotated_items()),
            clock_source_name(config.clock_source),
        );
        reminder = reminder.with_conversation_timing(timing);
    }
    let response_item = ContextualUserFragment::into(reminder);
    sess.record_conversation_items(
        turn_context,
        turn_context.model_info(),
        std::slice::from_ref(&response_item),
    )
    .await;

    Ok(())
}
