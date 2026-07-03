//! In-process live share for `aivo code --share` / `/share`: reuses the
//! `aivo logs share` server + tunnel but returns the URL instead of printing
//! it. The refresher follows the on-disk session, so the viewer tracks the
//! conversation as `persist_history` writes each turn.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};

use crate::services::share_local_server::{build_state, start_local_server};
use crate::services::share_redact::{RedactCtx, redact};
use crate::services::share_resolver::{ResolverContext, resolve_session};
use crate::services::share_tunnel::{self, TunnelUi};
use crate::services::shutdown_signal::ShutdownSignal;

/// A bit over the tunnel's 10s connect timeout, to cover the full handshake.
const URL_TIMEOUT: Duration = Duration::from_secs(15);

/// A running in-process live share. Its server + tunnel tasks are detached and
/// run until `stop()` (or the shared signal) fires; dropping the handle does NOT
/// stop the share.
pub struct LiveShareHandle {
    url: String,
    shutdown: ShutdownSignal,
}

impl LiveShareHandle {
    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn stop(&self) {
        self.shutdown.fire();
    }
}

#[cfg(test)]
impl LiveShareHandle {
    pub(crate) fn for_test(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            shutdown: ShutdownSignal::new(),
        }
    }
}

/// Start a live share for an already-persisted `session_id`, returning once the
/// public URL arrives. On handshake failure the tunnel's specific error (e.g. an
/// unlinked device) is surfaced and everything started here is torn down.
pub async fn start_live_share(
    session_id: String,
    resolver_ctx: Arc<ResolverContext>,
    redact_secrets: bool,
) -> Result<LiveShareHandle> {
    // Initial snapshot; the refresher re-resolves from disk on every later turn.
    let resolved = resolve_session(&session_id, &resolver_ctx).await?;
    let mut payload = resolved.payload;
    let redact_ctx = RedactCtx::from_system();
    if redact_secrets {
        let (red, _hits) = redact(payload, &redact_ctx);
        payload = red;
    } else {
        payload.meta.redacted = false;
    }

    let state = build_state(session_id, payload, redact_ctx, Some(resolver_ctx));

    let shutdown = ShutdownSignal::new();
    // Detached: the server loop runs until `shutdown` fires.
    let (port, _server) = start_local_server("127.0.0.1:0", state, shutdown.clone()).await?;
    let local_base = format!("http://127.0.0.1:{port}");

    let (url_tx, url_rx) = tokio::sync::oneshot::channel::<String>();
    let tunnel_shutdown = shutdown.clone();
    let tunnel = tokio::spawn(async move {
        share_tunnel::run_tunnel(local_base, TunnelUi::Headless { url_tx }, tunnel_shutdown).await
    });

    match tokio::time::timeout(URL_TIMEOUT, url_rx).await {
        Ok(Ok(url)) => Ok(LiveShareHandle { url, shutdown }),
        // Sender dropped → tunnel died mid-handshake; surface its error.
        Ok(Err(_)) => {
            shutdown.fire();
            let msg = match tunnel.await {
                Ok(Err(e)) => format!("{e}"),
                _ => "couldn't reach the aivo share service".to_string(),
            };
            Err(anyhow!(msg))
        }
        Err(_) => {
            shutdown.fire();
            let _ = tunnel.await;
            Err(anyhow!(
                "timed out reaching the aivo share service — check your connection and try again"
            ))
        }
    }
}
