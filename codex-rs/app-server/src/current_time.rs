use std::sync::Arc;
use std::sync::Weak;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use chrono::DateTime;
use chrono::Utc;
use codex_app_server_protocol::CurrentTimeReadParams;
use codex_app_server_protocol::CurrentTimeReadResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ServerRequestPayload;
use codex_core::SleepFuture;
use codex_core::TimeFuture;
use codex_core::TimeProvider;
use codex_protocol::ThreadId;
use tokio::time::Duration;
use tokio::time::Instant;
use tokio::time::timeout_at;

use crate::outgoing_message::ConnectionId;
use crate::outgoing_message::OutgoingMessageSender;
use crate::thread_state::ThreadStateManager;

const CURRENT_TIME_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const CURRENT_TIME_POLL_INTERVAL: Duration = Duration::from_secs(1);

pub(crate) fn app_server_time_provider(
    outgoing: Arc<OutgoingMessageSender>,
    thread_state_manager: ThreadStateManager,
) -> Arc<dyn TimeProvider> {
    Arc::new(AppServerTimeProvider {
        outgoing: Arc::downgrade(&outgoing),
        thread_state_manager,
    })
}

struct AppServerTimeProvider {
    outgoing: Weak<OutgoingMessageSender>,
    thread_state_manager: ThreadStateManager,
}

/// Optional callers can stop waiting before the provider's own timeout. Keep
/// callback cleanup alive even when they drop the request during a queue wait.
struct CurrentTimeRequestGuard {
    outgoing: Arc<OutgoingMessageSender>,
    request_id: Option<RequestId>,
    runtime: tokio::runtime::Handle,
}

impl CurrentTimeRequestGuard {
    fn new(outgoing: Arc<OutgoingMessageSender>, request_id: RequestId) -> Self {
        Self {
            outgoing,
            request_id: Some(request_id),
            runtime: tokio::runtime::Handle::current(),
        }
    }

    fn disarm(&mut self) {
        self.request_id = None;
    }
}

impl Drop for CurrentTimeRequestGuard {
    fn drop(&mut self) {
        let Some(request_id) = self.request_id.take() else {
            return;
        };
        let outgoing = Arc::clone(&self.outgoing);
        self.runtime.spawn(async move {
            outgoing.cancel_request(&request_id).await;
        });
    }
}

impl TimeProvider for AppServerTimeProvider {
    fn current_time(&self, thread_id: ThreadId) -> TimeFuture<'_> {
        let outgoing = self.outgoing.clone();
        let thread_state_manager = self.thread_state_manager.clone();
        Box::pin(async move {
            let outgoing = outgoing
                .upgrade()
                .context("app-server current-time provider is unavailable")?;
            request_current_time(outgoing, thread_state_manager, thread_id).await
        })
    }

    fn sleep(&self, thread_id: ThreadId, duration: Duration) -> SleepFuture<'_> {
        let outgoing = self.outgoing.clone();
        let thread_state_manager = self.thread_state_manager.clone();
        Box::pin(async move {
            let outgoing = outgoing
                .upgrade()
                .context("app-server current-time provider is unavailable")?;
            let started_at =
                request_current_time(outgoing.clone(), thread_state_manager.clone(), thread_id)
                    .await?;
            let wake_at = started_at
                .checked_add_signed(
                    chrono::Duration::from_std(duration)
                        .context("external sleep duration is outside the supported range")?,
                )
                .context("external sleep deadline is outside the supported range")?;

            loop {
                tokio::time::sleep(CURRENT_TIME_POLL_INTERVAL).await;
                if request_current_time(outgoing.clone(), thread_state_manager.clone(), thread_id)
                    .await?
                    >= wake_at
                {
                    return Ok(());
                }
            }
        })
    }
}

async fn request_current_time(
    outgoing: Arc<OutgoingMessageSender>,
    thread_state_manager: ThreadStateManager,
    thread_id: ThreadId,
) -> Result<DateTime<Utc>> {
    let deadline = Instant::now() + CURRENT_TIME_REQUEST_TIMEOUT;
    timeout_at(
        deadline,
        thread_state_manager.wait_for_thread_subscriber(thread_id),
    )
    .await
    .map_err(|_| {
        anyhow!(
            "timed out waiting for a client to subscribe to the thread after {}s",
            CURRENT_TIME_REQUEST_TIMEOUT.as_secs()
        )
    })?;
    let connection_ids = thread_state_manager
        .subscribed_connection_ids(thread_id)
        .await;
    let connection_id = require_single_current_time_connection(&connection_ids)?;
    let connection_ids = [connection_id];
    let request_id = outgoing.next_request_id();
    let mut request_guard = CurrentTimeRequestGuard::new(Arc::clone(&outgoing), request_id.clone());
    let (_, rx) = outgoing
        .send_request_to_connections_with_id(
            request_id.clone(),
            Some(&connection_ids),
            ServerRequestPayload::CurrentTimeRead(CurrentTimeReadParams {
                thread_id: thread_id.to_string(),
            }),
            /*thread_id*/ None,
        )
        .await;

    let response = timeout_at(deadline, rx).await;
    if response.is_err() {
        outgoing.cancel_request(&request_id).await;
    }
    request_guard.disarm();
    let result = match response {
        Ok(Ok(Ok(result))) => result,
        Ok(Ok(Err(err))) => {
            bail!(
                "current-time request failed: code={} message={}",
                err.code,
                err.message
            );
        }
        Ok(Err(err)) => bail!("current-time request was canceled: {err}"),
        Err(_) => {
            bail!(
                "current-time request timed out after {}s",
                CURRENT_TIME_REQUEST_TIMEOUT.as_secs()
            );
        }
    };
    let response: CurrentTimeReadResponse =
        serde_json::from_value(result).context("invalid current-time response")?;

    DateTime::from_timestamp(response.current_time_at, 0)
        .ok_or_else(|| anyhow!("current-time response is outside the supported range"))
}

fn require_single_current_time_connection(connection_ids: &[ConnectionId]) -> Result<ConnectionId> {
    // External clocks are not interchangeable, so do not choose one silently.
    match connection_ids {
        [connection_id] => Ok(*connection_id),
        _ => bail!(
            "expected exactly one client subscribed to the thread, found {}",
            connection_ids.len()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::request_current_time;
    use super::require_single_current_time_connection;
    use crate::outgoing_message::ConnectionId;
    use crate::outgoing_message::OutgoingEnvelope;
    use crate::outgoing_message::OutgoingMessage;
    use crate::outgoing_message::OutgoingMessageSender;
    use crate::thread_state::ConnectionCapabilities;
    use crate::thread_state::ThreadStateManager;
    use codex_protocol::ThreadId;
    use std::sync::Arc;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn dropping_current_time_request_removes_pending_callback() {
        let (tx, mut rx) = mpsc::channel(1);
        let outgoing = Arc::new(OutgoingMessageSender::new(
            tx,
            codex_analytics::AnalyticsEventsClient::disabled(),
        ));
        let thread_state_manager = ThreadStateManager::new();
        let thread_id = ThreadId::new();
        let connection_id = ConnectionId(7);
        thread_state_manager
            .connection_initialized(connection_id, ConnectionCapabilities::default())
            .await;
        assert!(
            thread_state_manager
                .try_add_connection_to_thread(thread_id, connection_id)
                .await
        );
        let mut request = Box::pin(request_current_time(
            Arc::clone(&outgoing),
            thread_state_manager,
            thread_id,
        ));
        assert!(futures::poll!(&mut request).is_pending());
        let OutgoingEnvelope::ToConnection {
            connection_id: recipient,
            message: OutgoingMessage::Request(sent),
            ..
        } = rx
            .try_recv()
            .expect("the clock request must have been sent")
        else {
            panic!("expected a clock request to the subscribed connection");
        };
        assert_eq!(recipient, connection_id);
        let request_id = sent.id().clone();

        // Optional clock capture can time out before the provider's own deadline.
        // Dropping its future must not leave a callback waiting for a late client.
        drop(request);
        tokio::task::yield_now().await;
        assert!(
            !outgoing.cancel_request(&request_id).await,
            "dropping the clock future must remove its pending callback"
        );
    }

    #[test]
    fn current_time_connection_must_be_unambiguous() {
        assert_eq!(
            require_single_current_time_connection(&[ConnectionId(7)]).unwrap(),
            ConnectionId(7)
        );
        assert_eq!(
            require_single_current_time_connection(&[])
                .unwrap_err()
                .to_string(),
            "expected exactly one client subscribed to the thread, found 0"
        );
        assert_eq!(
            require_single_current_time_connection(&[ConnectionId(7), ConnectionId(8)])
                .unwrap_err()
                .to_string(),
            "expected exactly one client subscribed to the thread, found 2"
        );
    }
}
