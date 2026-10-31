use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Result;
use chrono::DateTime;
use chrono::Utc;
use codex_core::SleepFuture;
use codex_core::TimeFuture;
use codex_core::TimeProvider;
use codex_core::config::CurrentTimeReminderConfig;
use codex_features::CurrentTimeSource;
use codex_features::Feature;
use codex_protocol::ThreadId;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use core_test_support::responses::ResponsesRequest;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use serde_json::Value;
use serde_json::json;

const START: i64 = 1_781_717_655;

struct ConversationClock {
    now: AtomicI64,
    initial_reads: Mutex<VecDeque<Option<i64>>>,
}

impl ConversationClock {
    fn after_four_minute_response() -> Self {
        Self {
            now: AtomicI64::new(START + 240),
            // User recording, first inference, assistant completion.
            initial_reads: Mutex::new(VecDeque::from([
                Some(START),
                Some(START),
                Some(START + 240),
            ])),
        }
    }
}

impl TimeProvider for ConversationClock {
    fn current_time(&self, _thread_id: ThreadId) -> TimeFuture<'_> {
        let time = self
            .initial_reads
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Some(self.now.load(Ordering::Relaxed)));
        Box::pin(async move {
            let time = time.ok_or_else(|| anyhow::anyhow!("synthetic event clock failure"))?;
            Ok(DateTime::<Utc>::from_timestamp(time, 0).unwrap())
        })
    }

    fn sleep(&self, _thread_id: ThreadId, _duration: Duration) -> SleepFuture<'_> {
        Box::pin(async { Ok(()) })
    }
}

fn configure(config: &mut codex_core::config::Config) {
    config.include_environment_context = false;
    config
        .features
        .enable(Feature::CurrentTimeReminder)
        .unwrap();
    config.current_time_reminder = Some(CurrentTimeReminderConfig {
        reminder_interval_seconds: 0,
        clock_source: CurrentTimeSource::External,
        include_conversation_timing: true,
        ..Default::default()
    });
}

fn timing(request: &ResponsesRequest) -> Value {
    let text = request
        .message_input_texts("developer")
        .into_iter()
        .rev()
        .find(|text| text.contains("<conversation_timing>"))
        .expect("the model request must include conversation timing");
    let body = text.split_once("<conversation_timing>").unwrap().1;
    serde_json::from_str(body.split_once("</conversation_timing>").unwrap().0).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conversation_timing_measures_from_response_completion_and_survives_resume() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let requests = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_assistant_message("question", "Which option do you prefer?"),
                ev_completed("first"),
            ]),
            sse(vec![ev_completed("second")]),
            sse(vec![ev_completed("resumed")]),
        ],
    )
    .await;
    let clock = Arc::new(ConversationClock::after_four_minute_response());
    let mut builder = test_codex()
        .with_config(configure)
        .with_external_time_provider(clock.clone());
    let test = builder.build(&server).await?;
    test.submit_turn("Please compare the options").await?;
    assert!(clock.initial_reads.lock().unwrap().is_empty());
    assert!(timing(&requests.requests()[0])["last_assistant_message_completed"].is_null());
    clock.now.store(START + 260, Ordering::Relaxed);
    test.submit_turn("How long ago did you ask?").await?;

    let second = timing(&requests.requests()[1]);
    assert_eq!(
        second["last_assistant_message_completed"]["elapsed_seconds"],
        json!(20)
    );
    assert_eq!(
        second["last_assistant_message_completed"]["message_id"],
        json!("question")
    );
    assert_eq!(
        second["last_user_message_recorded"]["elapsed_seconds"],
        json!(0)
    );

    let path = test.session_configured.rollout_path.clone().unwrap();
    test.codex.submit(Op::Shutdown).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::ShutdownComplete)
    })
    .await;
    clock.now.store(START + 240 + 172_800, Ordering::Relaxed);
    // Builder configuration mutators are consumed by each build.
    builder = builder.with_config(configure);
    let resumed = builder.resume(&server, test.home.clone(), path).await?;
    resumed.submit_turn("Continue after two days").await?;
    let after_resume = timing(&requests.requests()[2]);
    assert_eq!(
        after_resume["last_assistant_message_completed"]["elapsed_seconds"],
        json!(172_800)
    );
    assert_eq!(
        after_resume["last_assistant_message_completed"]["at"],
        second["last_assistant_message_completed"]["at"]
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conversation_timing_reports_unknown_elapsed_when_the_clock_moves_backward() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let requests = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_assistant_message("question", "Which option?"),
                ev_completed("first"),
            ]),
            sse(vec![ev_completed("after-clock-change")]),
        ],
    )
    .await;
    let clock = Arc::new(ConversationClock::after_four_minute_response());
    let test = test_codex()
        .with_config(configure)
        .with_external_time_provider(clock.clone())
        .build(&server)
        .await?;
    test.submit_turn("Compare the options").await?;
    clock.now.store(START + 120, Ordering::Relaxed);
    test.submit_turn("Continue").await?;
    let after_clock_change = timing(&requests.requests()[1]);
    let completion = &after_clock_change["last_assistant_message_completed"];
    assert_eq!(completion["message_id"], json!("question"));
    assert!(completion["at"].is_string());
    assert!(completion["elapsed_seconds"].is_null());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conversation_timing_keeps_completed_output_when_its_clock_read_fails() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let requests = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_assistant_message("question", "Which option?"),
                ev_completed("first"),
            ]),
            sse(vec![ev_completed("after-clock-failure")]),
        ],
    )
    .await;
    let clock = Arc::new(ConversationClock::after_four_minute_response());
    clock.initial_reads.lock().unwrap()[2] = None;
    let test = test_codex()
        .with_config(configure)
        .with_external_time_provider(clock.clone())
        .build(&server)
        .await?;
    test.submit_turn("Compare the options").await?;
    clock.now.store(START + 260, Ordering::Relaxed);
    test.submit_turn("Continue").await?;
    let request = &requests.requests()[1];
    assert!(request.inputs_of_type("message").iter().any(|item| {
        item["role"] == "assistant"
            && item["content"].as_array().is_some_and(|content| {
                content
                    .iter()
                    .any(|span| span["type"] == "output_text" && span["text"] == "Which option?")
            })
    }));
    let after_failure = timing(request);
    let completion = &after_failure["last_assistant_message_completed"];
    assert_eq!(completion["message_id"], json!("question"));
    assert!(completion["at"].is_null());
    assert!(completion["elapsed_seconds"].is_null());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conversation_timing_keeps_completed_output_when_its_clock_read_never_returns() -> Result<()>
{
    skip_if_no_network!(Ok(()));

    struct HangingCompletionClock {
        reads: AtomicUsize,
    }

    impl TimeProvider for HangingCompletionClock {
        fn current_time(&self, _thread_id: ThreadId) -> TimeFuture<'_> {
            let read = self.reads.fetch_add(1, Ordering::Relaxed);
            Box::pin(async move {
                // User recording and inference succeed, but completion never supplies a time.
                if read == 2 {
                    return std::future::pending().await;
                }
                let seconds = START + if read < 2 { 0 } else { 260 };
                Ok(DateTime::<Utc>::from_timestamp(seconds, 0).unwrap())
            })
        }

        fn sleep(&self, _thread_id: ThreadId, _duration: Duration) -> SleepFuture<'_> {
            Box::pin(async { Ok(()) })
        }
    }

    let server = start_mock_server().await;
    let requests = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_assistant_message("question", "Which option?"),
                ev_completed("first"),
            ]),
            sse(vec![ev_completed("after-clock-timeout")]),
        ],
    )
    .await;
    let test = test_codex()
        .with_config(configure)
        .with_external_time_provider(Arc::new(HangingCompletionClock {
            reads: AtomicUsize::new(0),
        }))
        .build(&server)
        .await?;
    tokio::time::timeout(
        Duration::from_secs(2),
        test.submit_turn("Compare the options"),
    )
    .await
    .expect("optional completion timing must not prevent the turn from completing")?;
    tokio::time::timeout(Duration::from_secs(2), test.submit_turn("Continue"))
        .await
        .expect("the next turn must proceed after an unavailable completion time")?;
    let request = &requests.requests()[1];
    assert!(request.inputs_of_type("message").iter().any(|item| {
        item["role"] == "assistant"
            && item["content"].as_array().is_some_and(|content| {
                content
                    .iter()
                    .any(|span| span["type"] == "output_text" && span["text"] == "Which option?")
            })
    }));
    let after_timeout = timing(request);
    let completion = &after_timeout["last_assistant_message_completed"];
    assert_eq!(completion["message_id"], json!("question"));
    assert!(completion["at"].is_null());
    assert!(completion["elapsed_seconds"].is_null());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conversation_timing_does_not_treat_compaction_as_an_assistant_reply() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let requests = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_assistant_message("question", "Which option?"),
                ev_completed("first"),
            ]),
            sse(vec![
                ev_assistant_message("summary", "Conversation summary"),
                ev_completed("compact"),
            ]),
            sse(vec![ev_completed("after-compact")]),
        ],
    )
    .await;
    let clock = Arc::new(ConversationClock::after_four_minute_response());
    let test = test_codex()
        .with_config(|config| {
            configure(config);
            config.model_provider.name = "Non-OpenAI Model provider".to_string();
        })
        .with_external_time_provider(clock.clone())
        .build(&server)
        .await?;
    test.submit_turn("Compare the options").await?;
    clock.now.store(START + 250, Ordering::Relaxed);
    test.codex.submit(Op::Compact).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    clock.now.store(START + 260, Ordering::Relaxed);
    test.submit_turn("Continue").await?;
    let after_compact = timing(&requests.requests()[2]);
    assert_eq!(
        after_compact["last_assistant_message_completed"]["message_id"],
        json!("question")
    );
    assert_eq!(
        after_compact["last_assistant_message_completed"]["elapsed_seconds"],
        json!(20)
    );
    Ok(())
}
