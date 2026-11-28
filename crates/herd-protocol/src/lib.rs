use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Protocol version. Bumped 1.0 -> 1.1 when the G1 (park/link) and G4
/// (services) methods were added, 1.1 -> 1.2 for the G2 (isolate) and
/// G3 (secure) methods, 1.2 -> 1.3 for the PR-1 proxy methods, and
/// 1.3 -> 1.4 for the PR-2 DNS methods; see BSD: method or field
/// changes must raise the protocol version, not just the app version.
pub const PROTOCOL_VERSION: &str = "1.4";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub jsonrpc: String,
    pub id: Value,
    pub method: String,
    #[serde(default)]
    pub params: Value,
    pub token: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub jsonrpc: String,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
}

impl Response {
    pub fn success(id: Value, result: Value) -> Self {
        Self { jsonrpc: "2.0".into(), id, result: Some(result), error: None }
    }

    pub fn error(id: Value, code: i32, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: None,
            error: Some(RpcError { code, message: message.into() }),
        }
    }
}
