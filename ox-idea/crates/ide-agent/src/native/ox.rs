//! The OX agent (`ox-hareness`): AgentScope harness assembled in-process.
//!
//! Unlike every other provider there is no child process and no socket: the
//! agent endpoint (`agentscope-acp`'s serve loop) runs on one half of an
//! in-memory duplex, the task's ACP client peer on the other. The ACP frames
//! never leave the process; `spec.env` carries the model configuration
//! (`AGENTSCOPE_*`) that would otherwise come from the process environment.

use crate::provider::ProviderSpec;
use ide_jsonrpc::{Framing, Handler, Peer};
use std::path::Path;
use std::sync::Arc;

pub(crate) fn connect(
    spec: &ProviderSpec,
    _cwd: &Path,
    client: Arc<dyn Handler>,
    _on_stderr: Box<dyn Fn(String) + Send + Sync>,
) -> std::io::Result<Peer> {
    let (ours, theirs) = tokio::io::duplex(1 << 20);
    let (or, ow) = tokio::io::split(ours);
    let (tr, tw) = tokio::io::split(theirs);
    let env = spec.env.clone();
    tokio::spawn(async move {
        if let Err(error) = agentscope_acp::serve(tr, tw, env).await {
            tracing::warn!(%error, "ox-hareness: agent endpoint exited");
        }
    });
    Ok(Peer::connect_framed(or, ow, client, Framing::Lines))
}
