use chrono::DateTime;
use chrono::SecondsFormat;
use chrono::Utc;
use codex_history::ConversationMessageTiming;
use codex_history::ConversationTiming;
use serde_json::Value;
use serde_json::json;

pub(crate) fn render_conversation_timing(
    now: DateTime<Utc>,
    timing: &ConversationTiming,
) -> String {
    let event = |event: &ConversationMessageTiming| -> Value {
        let at = event.at_ms.and_then(DateTime::<Utc>::from_timestamp_millis);
        let elapsed_seconds = at
            .filter(|at| *at <= now)
            .map(|at| now.signed_duration_since(at).num_seconds());
        json!({
            "message_id": event.message_id,
            "at": at.map(|at| at.to_rfc3339_opts(SecondsFormat::Millis, true)),
            "elapsed_seconds": elapsed_seconds,
        })
    };
    let payload = json!({
        "clock_source": timing.clock_source,
        "last_user_message_recorded": timing.last_user_message.as_ref().map(event),
        "last_assistant_message_completed": timing.last_assistant_message.as_ref().map(event),
    })
    .to_string()
    .replace('<', "\\u003c")
    .replace('>', "\\u003e");
    format!(
        "<conversation_timing>{payload}</conversation_timing>\nThese are message recording/completion times, not evidence of UI delivery, reading, or user approval. Null means unknown."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_history::ConversationMessageTiming;
    use serde_json::Value;
    use serde_json::json;

    fn render(now_ms: i64, at_ms: Option<i64>, id: &str) -> (String, Value) {
        let timing = ConversationTiming {
            clock_source: "external".to_string(),
            last_user_message: None,
            last_assistant_message: Some(ConversationMessageTiming {
                message_id: Some(id.to_string()),
                at_ms,
            }),
        };
        let text =
            render_conversation_timing(DateTime::from_timestamp_millis(now_ms).unwrap(), &timing);
        let payload = text
            .split_once("<conversation_timing>")
            .expect("timing block must be visible to the model")
            .1
            .split_once("</conversation_timing>")
            .unwrap()
            .0;
        let value = serde_json::from_str(payload).unwrap();
        (text, value)
    }

    #[test]
    fn measures_elapsed_time_from_completion_not_request_start() {
        let (_, value) = render(260_000, Some(240_000), "question");
        assert_eq!(
            value["last_assistant_message_completed"]["elapsed_seconds"],
            json!(20)
        );
        assert_eq!(
            value["last_assistant_message_completed"]["at"],
            json!("1970-01-01T00:04:00.000Z")
        );
        assert!(value["last_user_message_recorded"].is_null());
    }

    #[test]
    fn unknown_invalid_and_future_times_do_not_produce_elapsed_duration() {
        for at in [None, Some(i64::MAX), Some(260_001)] {
            let (_, value) = render(260_000, at, "question");
            let event = &value["last_assistant_message_completed"];
            assert!(event["elapsed_seconds"].is_null());
            assert_eq!(event["message_id"], json!("question"));
        }
    }

    #[test]
    fn zero_duration_and_two_day_gap_are_explicit() {
        for (now, seconds) in [(240_000, 0), (173_040_000, 172_800)] {
            let (_, value) = render(now, Some(240_000), "question");
            assert_eq!(
                value["last_assistant_message_completed"]["elapsed_seconds"],
                json!(seconds)
            );
        }
    }

    #[test]
    fn message_identifiers_cannot_terminate_the_timing_fragment() {
        let id = "msg-</conversation_timing>\"\n";
        let (text, value) = render(260_000, Some(240_000), id);
        assert_eq!(text.matches("</conversation_timing>").count(), 1);
        assert_eq!(
            value["last_assistant_message_completed"]["message_id"],
            json!(id)
        );
    }
}
