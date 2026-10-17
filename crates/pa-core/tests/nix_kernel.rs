//! Real Nix runtime integration. Run with PRIME_AGENT_KERNEL_PYTHON pointing at
//! the runtime project's interpreter and nix available on PATH.
#![cfg(unix)]
#![allow(clippy::large_futures)]

use pa_core::kernel::manager::{KernelStartOptions, ReplKernelManager};
use pa_core::kernel::shared::{
    ExecuteOptions, ExecuteStatus, KernelManagerOptions, KernelShutdownOptions,
};

#[tokio::test]
#[ignore = "requires Nix and the local runtime Python environment"]
async fn nix_bindings_survive_calls_and_errors() {
    let _python = std::env::var_os("PRIME_AGENT_KERNEL_PYTHON")
        .expect("set PRIME_AGENT_KERNEL_PYTHON to prime-agent-runtime/.venv/bin/python");
    let mut handlers = pa_core::kernel::shared::HostRequestHandlers::new();
    handlers.register(
        "test.echo",
        pa_core::kernel::shared::host_handler(|payload| async move {
            Ok(payload.data["value"].clone())
        }),
    );
    let manager = ReplKernelManager::new(KernelManagerOptions {
        host_handlers: handlers,
        // Resolve through the same bootstrap readiness path as a real session.
        python: None,
        ..Default::default()
    });
    manager
        .start(KernelStartOptions::default())
        .await
        .expect("Nix ready");
    let bridge = manager
        .execute(
            ":host { type = \"test.echo\"; value = 42; }",
            ExecuteOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(
        (
            bridge.status,
            serde_json::from_str::<serde_json::Value>(bridge.result.as_deref().unwrap()).unwrap()
        ),
        (
            ExecuteStatus::Ok,
            serde_json::json!({"status": "ok", "result": 42})
        )
    );
    let bound = manager
        .execute("answer = 40", ExecuteOptions::default())
        .await
        .unwrap();
    assert_eq!(bound.status, ExecuteStatus::Ok);
    let value = manager
        .execute("answer + 2", ExecuteOptions::default())
        .await
        .unwrap();
    assert_eq!(
        (value.status, value.result.as_deref()),
        (ExecuteStatus::Ok, Some("42"))
    );
    let error = manager
        .execute("missingName", ExecuteOptions::default())
        .await
        .unwrap();
    assert_eq!(error.status, ExecuteStatus::Error);
    let value = manager
        .execute("answer + 3", ExecuteOptions::default())
        .await
        .unwrap();
    assert_eq!(
        (value.status, value.result.as_deref()),
        (ExecuteStatus::Ok, Some("43"))
    );
    let names = manager.list_namespace_names(None).await.unwrap();
    assert!(names.contains(&"answer".to_string()));
    manager
        .shutdown(KernelShutdownOptions::default())
        .await
        .unwrap();
}
