# Conversation timing prototype

This prototype explores [issue #36713](https://github.com/openai/codex/issues/36713).
It supplies the model with the time of the latest recorded user message and the
latest completed assistant message, alongside the existing current-time reminder.

## Observable contract

- A request recorded at 12:00 followed by an assistant question completed at
  12:04 and another inference at 12:04:20 yields 20 seconds since assistant
  completion.
- Original message times survive saving and resuming a thread. Resuming two days
  later does not relabel old messages with the resume time.
- Compaction carries a timing checkpoint. Its generated summary does not count
  as a new assistant reply. Prefix compaction carries timing from the replaced
  prefix, before appending the unchanged suffix.
- Missing legacy timestamps, failed clock reads, future timestamps and a change
  of clock source must not produce a guessed elapsed duration.
- Tools, inter-agent messages and scheduler heartbeats do not count as user
  replies. Timing never grants permission to act.
- The option is disabled by default. Existing reminder text and clock access
  remain unchanged when it is disabled.

## Configuration

```toml
[features.current_time_reminder]
enabled = true
include_conversation_timing = true
```

## Scope

`last_user_message_recorded` means the time Codex records the input; queued input may
have been submitted earlier. `last_assistant_message_completed` means Codex has a
completed assistant message. Neither timestamp proves UI delivery or that a
person read a message. An asynchronous question tool needs a separate confirmed
delivery event; that is outside this prototype.

The latest assistant message may be a progress update rather than a question.
The message ID identifies which completion is being timed. This prototype does
not classify questions, maintain a queue of unanswered questions, or promise
that a model will always reason correctly from the supplied context.

Timing is included whenever the existing reminder is due, respecting its interval
and delivery mode. It uses the configured system or external `TimeProvider` for
both event timestamps and the current time. An external provider must retain the
same clock domain across resumes; changing providers behind the same `external`
label cannot be detected. A configured source change invalidates previous times.
Optional event-time reads have a one-second deadline; a failed or stalled read
records an unknown time without losing the accepted input or completed answer.

Timestamps are stored in host-only rollout metadata as cumulative checkpoints.
At inference time, the reminder renders UTC timestamps, message IDs and elapsed
whole seconds in a `<conversation_timing>` JSON block. A newer message with no
timestamp replaces the older known time with `null` rather than borrowing it.

## Verification

Validated locally on macOS arm64 with Rust 1.95.0. Tests use synthetic clocks and
a loopback Responses server; they do not call a hosted model or modify a running
Codex app. The integration tests inspect the actual model request payloads.

Run from `codex-rs`, with the repository's pinned dependencies:

```sh
export RUST_MIN_STACK=8388608
cargo test --locked -p codex-core --lib conversation_timing
cargo test --locked -p codex-core --test all conversation_timing
cargo test --locked -p codex-core --test all current_time_reminder
cargo test --locked -p codex-core --lib current_time_reminder
cargo test --locked -p codex-history conversation_timing
cargo test --locked -p codex-features conversation_timing
cargo test --locked -p codex-app-server --lib current_time
cargo test --locked -p codex-app-server --lib outgoing_message::tests
cargo clippy --locked -p codex-core --lib --no-deps
```

The test commands passed 14, 5, 29, 1, 1, 1, 3 and 20 tests respectively, with none
ignored (73 distinct tests; one backpressure case runs under two filters).
Core Clippy completed without warnings or errors. App-server builds may need the
pinned V8 artifacts configured by
[`setup-rusty-v8`](../.github/actions/setup-rusty-v8/action.yml).
The core integration and replay tests were observed failing on missing timing
before implementation; the pending-clock test failed on its completion deadline
before adding the bounded read. Config/history boundaries and the renderer also
had a red-to-green cycle using the actual production sources in a small external
Cargo harness before the full workspace build became available.

The bounded read also required app-server cancellation cleanup. Two regression
tests first demonstrated leaked pending callbacks when dropping a clock read
while awaiting a reply or while its send queue was full, then passed with a
request guard installed before callback registration.

The config schema was regenerated with `codex-write-config-schema` and compared
byte-for-byte with the checked-in fixture.
