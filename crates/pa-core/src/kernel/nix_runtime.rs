//! Native JSONL adapter for a persistent Nix evaluator.
#[cfg(unix)]
mod evaluator;
#[cfg(unix)]
mod runtime;
#[cfg(unix)]
mod state;
#[cfg(all(test, unix))]
mod tests;

/// Serve protocol-v3 requests on stdin until shutdown or EOF.
///
/// # Errors
/// Returns transport, startup, or evaluator process errors.
pub async fn serve_nix_runtime() -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        runtime::serve().await
    }
    #[cfg(not(unix))]
    {
        anyhow::bail!("The native Nix runtime requires Unix")
    }
}
