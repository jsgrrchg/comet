use super::*;
use crate::adapter_install::{NpmPin, ensure_installed_shim, launch_for_entry};
use crate::{NativeForkControls, NativeForkError};
use zeron_proto::{NativeForkAvailability, NativeForkBoundary, NativeForkPoint, NativeForkResult};

const SDK: NpmPin = NpmPin {
    name: "@anthropic-ai/claude-agent-sdk",
    version: "0.3.284",
};
const HELPER: &str = include_str!("fork.mjs");

pub(super) async fn support() -> NativeForkAvailability {
    let Some(node) = crate::executable::find_on_paths("node", Vec::new()) else {
        return NativeForkAvailability::unavailable(
            "Node 18 or newer is required for native Claude forks",
        );
    };
    let mut cmd = Command::new(&node);
    cmd.arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    match tokio::time::timeout(Duration::from_secs(5), cmd.output()).await {
        Ok(Ok(output))
            if output.status.success()
                && String::from_utf8_lossy(&output.stdout)
                    .trim()
                    .trim_start_matches('v')
                    .split('.')
                    .next()
                    .and_then(|s| s.parse::<u32>().ok())
                    .is_some_and(|v| v >= 18) =>
        {
            if crate::adapter_install::installed_shim(&SDK, "fork.mjs", HELPER).is_some()
                || crate::adapter_install::find_npm().is_some()
            {
                NativeForkAvailability::available()
            } else {
                NativeForkAvailability::unavailable(
                    "npm is required to prepare the Claude fork helper",
                )
            }
        }
        _ => NativeForkAvailability::unavailable(
            "Node 18 or newer is required for native Claude forks",
        ),
    }
}

pub(super) async fn helper(
    request: serde_json::Value,
    controls: NativeForkControls,
) -> Result<NativeForkResult, NativeForkError> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let available = support().await;
    if !available.available {
        return Err(NativeForkError::Rejected(
            available.reason.unwrap_or_default(),
        ));
    }
    // Managed installation is shared with the SDK adapters. The execution lease
    // spans preparation, the helper, and child reaping.
    let shim = ensure_installed_shim(SDK, "Claude session fork", "fork.mjs", HELPER)
        .await
        .map_err(|e| NativeForkError::Rejected(e.to_string()))?;
    let (node, args) =
        launch_for_entry(&shim).map_err(|e| NativeForkError::Rejected(e.to_string()))?;
    let cwd = request["dir"].as_str().unwrap_or("").to_string();
    let mut cmd = Command::new(&node);
    crate::compose_child_path(&mut cmd, &node);
    cmd.args(args)
        .current_dir(&cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = cmd
        .spawn()
        .map_err(|e| NativeForkError::Rejected(e.to_string()))?;
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let operation = async {
        stdin
            .write_all(request.to_string().as_bytes())
            .await
            .map_err(|e| NativeForkError::Indeterminate(e.to_string()))?;
        stdin
            .shutdown()
            .await
            .map_err(|e| NativeForkError::Indeterminate(e.to_string()))?;
        drop(stdin);
        let mut output = Vec::new();
        (&mut stdout)
            .take(65536)
            .read_to_end(&mut output)
            .await
            .map_err(|e| NativeForkError::Indeterminate(e.to_string()))?;
        let value: serde_json::Value = serde_json::from_slice(&output)
            .map_err(|e| NativeForkError::Indeterminate(e.to_string()))?;
        if let Some(error) = value["error"].as_str() {
            return Err(if value["indeterminate"] == true {
                NativeForkError::Indeterminate(error.into())
            } else {
                NativeForkError::Rejected(error.into())
            });
        }
        let id = value["ok"]["sessionId"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                NativeForkError::Indeterminate("Claude helper returned no session ID".into())
            })?;
        Ok(NativeForkResult {
            session_id: id.into(),
            cwd,
        })
    };
    let result = tokio::select! {
        result = tokio::time::timeout(controls.timeout, operation) => result.unwrap_or_else(|_| Err(NativeForkError::Indeterminate("Claude fork timed out".into()))),
        _ = controls.interrupt.cancelled() => Err(NativeForkError::Indeterminate("Claude fork cancelled".into())),
    };
    let _ = child.kill().await;
    let _ = child.wait().await;
    drop(controls.execution_lease);
    result
}

pub(super) async fn fork(
    point: &NativeForkPoint,
    controls: NativeForkControls,
) -> Result<NativeForkResult, NativeForkError> {
    point.validate().map_err(NativeForkError::Rejected)?;
    let NativeForkBoundary::ClaudeMessage { uuid } = &point.boundary else {
        return Err(NativeForkError::Rejected(
            "Expected a Claude transcript UUID".into(),
        ));
    };
    helper(serde_json::json!({"sourceSessionId":point.source_session_id,"dir":point.cwd,"upToMessageId":uuid}), controls).await
}
