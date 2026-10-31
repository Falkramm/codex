use codex_history::ConversationMessageTiming;
use codex_history::ConversationTiming;
use codex_history::ResponseItemEnvelope;
use codex_history::UserInputOrigin;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::InterAgentCommunication;

/// Recover the latest observations from the surviving history, including explicit
/// unknown boundaries written while timing was disabled or by an older client.
pub(super) fn recover(items: &[ResponseItemEnvelope]) -> ConversationTiming {
    let mut timing = ConversationTiming::default();
    for envelope in items {
        if let Some(snapshot) = envelope
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.conversation_timing.as_ref())
        {
            // Compactions can carry the original observations on a synthetic item.
            timing = snapshot.clone();
            continue;
        }
        if envelope
            .metadata
            .as_ref()
            .is_some_and(|metadata| metadata.compaction_output)
        {
            continue;
        }
        let unknown = || ConversationMessageTiming {
            message_id: envelope.item.id().map(ToString::to_string),
            at_ms: None,
        };
        if is_human_message(&envelope.item) {
            timing.last_user_message = Some(unknown());
        } else if let ResponseItem::Message { role, content, .. } = &envelope.item
            && role == "assistant"
            && !InterAgentCommunication::is_message_content(content)
        {
            timing.last_assistant_message = Some(unknown());
        }
    }
    timing
}

/// Keep event identity across a clock-source change without comparing timestamps
/// obtained from different providers.
pub(super) fn with_clock_source(
    mut timing: ConversationTiming,
    source: &str,
) -> ConversationTiming {
    if timing.clock_source != source {
        timing.clock_source = source.to_string();
        for event in timing
            .last_user_message
            .iter_mut()
            .chain(timing.last_assistant_message.iter_mut())
        {
            event.at_ms = None;
        }
    }
    timing
}

pub(super) fn is_human_message(item: &ResponseItem) -> bool {
    if !crate::context::is_user_authorization_message(item)
        || UserInputOrigin::from_message(item) == UserInputOrigin::Heartbeat
    {
        return false;
    }
    let ResponseItem::Message {
        content,
        internal_chat_message_metadata_passthrough,
        ..
    } = item
    else {
        return false;
    };
    let explicitly_user_authored = internal_chat_message_metadata_passthrough
        .as_ref()
        .and_then(|metadata| metadata.content_item_kinds.as_ref())
        .filter(|kinds| kinds.len() == content.len())
        .is_some_and(|kinds| {
            kinds.iter().any(|kind| {
                kind.0.starts_with("user.") && kind.0 != codex_history::HEARTBEAT_CONTENT_KIND
            })
        });
    if explicitly_user_authored {
        // A real user may quote context markers or send text alongside host context.
        return true;
    }
    // Older history lacks host annotations. Exclude wholly synthetic messages but
    // preserve mixed content containing a real user message.
    let entirely_contextual = !content.is_empty()
        && content.iter().all(|part| {
            crate::context::is_contextual_user_fragment(part)
                || matches!(part, ContentItem::InputText { text }
                    if crate::compact::is_summary_message(text))
        });
    !entirely_contextual
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_history::CodexHarnessMetadata;
    use codex_history::ConversationMessageTiming;
    use codex_protocol::AgentPath;
    use codex_protocol::ResponseItemId;
    use codex_protocol::models::ContentItemKind;
    use codex_protocol::models::InternalChatMessageMetadataPassthrough;
    use codex_protocol::protocol::InterAgentCommunication;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    fn message(role: &str, id: &str, text: &str) -> ResponseItemEnvelope {
        let content_kind = if role == "assistant" {
            "output_text"
        } else {
            "input_text"
        };
        ResponseItemEnvelope::new(
            serde_json::from_value(json!({
                "type": "message",
                "role": role,
                "id": id,
                "content": [{"type": content_kind, "text": text}],
            }))
            .unwrap(),
        )
    }

    fn snapshot() -> ConversationTiming {
        ConversationTiming {
            clock_source: "external".to_string(),
            last_user_message: Some(ConversationMessageTiming {
                message_id: Some("user-old".to_string()),
                at_ms: Some(1_000),
            }),
            last_assistant_message: Some(ConversationMessageTiming {
                message_id: Some("answer-old".to_string()),
                at_ms: Some(2_000),
            }),
        }
    }

    fn checkpoint() -> ResponseItemEnvelope {
        let mut envelope = message("assistant", "answer-old", "Which option?");
        envelope.metadata = Some(CodexHarnessMetadata {
            conversation_timing: Some(snapshot()),
            ..Default::default()
        });
        envelope
    }

    #[test]
    fn newer_unstamped_messages_replace_known_times_with_unknown() {
        let mut items = vec![checkpoint(), message("user", "user-new", "Continue")];
        let mut expected = snapshot();
        expected.last_user_message = Some(ConversationMessageTiming {
            message_id: Some("user-new".to_string()),
            at_ms: None,
        });
        assert_eq!(recover(&items), expected);

        items.push(message("assistant", "answer-new", "Done"));
        expected.last_assistant_message = Some(ConversationMessageTiming {
            message_id: Some("answer-new".to_string()),
            at_ms: None,
        });
        assert_eq!(recover(&items), expected);
    }

    #[test]
    fn changing_clock_source_invalidates_times_without_losing_message_identity() {
        let mut expected = snapshot();
        expected.clock_source = "system".to_string();
        expected.last_user_message.as_mut().unwrap().at_ms = None;
        expected.last_assistant_message.as_mut().unwrap().at_ms = None;
        assert_eq!(with_clock_source(snapshot(), "system"), expected);
        assert_eq!(with_clock_source(snapshot(), "external"), snapshot());
    }

    #[test]
    fn background_context_tools_and_agent_messages_do_not_replace_human_times() {
        let mut heartbeat = message("user", "heartbeat", "Check status");
        if let ResponseItem::Message {
            internal_chat_message_metadata_passthrough,
            ..
        } = &mut heartbeat.item
        {
            *internal_chat_message_metadata_passthrough =
                Some(InternalChatMessageMetadataPassthrough {
                    content_item_kinds: Some(vec![ContentItemKind(
                        codex_history::HEARTBEAT_CONTENT_KIND.to_string(),
                    )]),
                    ..Default::default()
                });
        }
        let communication = InterAgentCommunication::new(
            AgentPath::root().join("worker").unwrap(),
            AgentPath::root(),
            Vec::new(),
            "Analysis finished".to_string(),
            true,
        );
        let ignored = vec![
            heartbeat,
            message(
                "user",
                "environment",
                "<environment_context>\n<cwd>/tmp</cwd>\n</environment_context>",
            ),
            message("developer", "instruction", "A runtime update"),
            ResponseItemEnvelope::new(
                serde_json::from_value(json!({
                    "type": "function_call_output", "call_id": "call-1", "output": "Result"
                }))
                .unwrap(),
            ),
            ResponseItemEnvelope::new(communication.to_model_input_item()),
            message(
                "assistant",
                "legacy-agent",
                &serde_json::to_string(&communication).unwrap(),
            ),
        ];
        for item in &ignored {
            assert!(!is_human_message(&item.item));
        }
        assert!(is_human_message(&message("user", "human", "Continue").item));
        let mut items = vec![checkpoint()];
        items.extend(ignored);
        assert_eq!(recover(&items), snapshot());
    }

    #[test]
    fn human_text_is_preserved_when_quoting_or_accompanying_context() {
        let context = "<environment_context>\n<cwd>/tmp</cwd>\n</environment_context>";
        let mut quoted = message("user", "quoted", context);
        if let ResponseItem::Message {
            internal_chat_message_metadata_passthrough,
            ..
        } = &mut quoted.item
        {
            *internal_chat_message_metadata_passthrough =
                Some(InternalChatMessageMetadataPassthrough {
                    content_item_kinds: Some(vec![ContentItemKind("user.text".to_string())]),
                    ..Default::default()
                });
        }
        assert!(is_human_message(&quoted.item));

        let mut mixed = message("user", "mixed", "Continue with option A");
        if let ResponseItem::Message { content, .. } = &mut mixed.item {
            content.push(ContentItem::InputText {
                text: context.to_string(),
            });
        }
        // Legacy mixed messages and explicitly classified mixed messages both
        // contain a user contribution; the host context must not hide it.
        assert!(is_human_message(&mixed.item));
        if let ResponseItem::Message {
            internal_chat_message_metadata_passthrough,
            ..
        } = &mut mixed.item
        {
            *internal_chat_message_metadata_passthrough =
                Some(InternalChatMessageMetadataPassthrough {
                    content_item_kinds: Some(vec![
                        ContentItemKind("user.text".to_string()),
                        ContentItemKind("environment.context".to_string()),
                    ]),
                    ..Default::default()
                });
        }
        assert!(is_human_message(&mixed.item));
        let restored = recover(&[checkpoint(), quoted, mixed]);
        assert_eq!(
            restored.last_user_message,
            Some(ConversationMessageTiming {
                message_id: Some("mixed".to_string()),
                at_ms: None,
            })
        );
        assert_eq!(
            restored.last_assistant_message,
            snapshot().last_assistant_message
        );
    }

    #[test]
    fn compaction_checkpoint_retains_original_times_and_ignores_summary_generation() {
        let mut compacted = ResponseItemEnvelope::new(ResponseItem::Compaction {
            id: Some(ResponseItemId::from_server("compaction".to_string())),
            encrypted_content: "summary".to_string(),
            internal_chat_message_metadata_passthrough: None,
        });
        compacted.metadata = Some(CodexHarnessMetadata {
            conversation_timing: Some(snapshot()),
            compaction_output: true,
            inherited_user_message: true,
            ..Default::default()
        });
        let mut summary = message("assistant", "summary", "Summary text");
        summary.metadata = Some(CodexHarnessMetadata {
            compaction_output: true,
            ..Default::default()
        });
        let legacy_summary = message(
            "user",
            "legacy-summary",
            &format!("{}\nSummary text", crate::compact::SUMMARY_PREFIX),
        );
        assert!(!is_human_message(&legacy_summary.item));
        assert_eq!(recover(&[compacted, summary, legacy_summary]), snapshot());
    }

    #[test]
    fn unknown_checkpoint_time_does_not_fall_back_to_older_known_completion() {
        let mut unknown = checkpoint();
        let mut expected = snapshot();
        expected.last_assistant_message = Some(ConversationMessageTiming {
            message_id: Some("newer-answer".to_string()),
            at_ms: None,
        });
        unknown.metadata.as_mut().unwrap().conversation_timing = Some(expected.clone());
        assert_eq!(recover(&[checkpoint(), unknown]), expected);
    }
}
