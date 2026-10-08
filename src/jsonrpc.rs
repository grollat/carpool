use serde_json::{json, Value};

pub fn method(v: &Value) -> Option<&str> {
    v.get("method").and_then(Value::as_str)
}

pub fn is_request(v: &Value) -> bool {
    v.get("method").is_some() && v.get("id").is_some_and(|id| !id.is_null())
}

pub fn is_notification(v: &Value) -> bool {
    v.get("method").is_some() && v.get("id").is_none()
}

pub fn is_response(v: &Value) -> bool {
    v.get("method").is_none() && v.get("id").is_some() && (v.get("result").is_some() || v.get("error").is_some())
}

/// Stable map key for a JSON-RPC id (string and number ids never collide).
pub fn id_key(id: &Value) -> String {
    id.to_string()
}

pub fn result(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

pub fn error(id: &Value, code: i64, message: impl Into<String>) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message.into()}})
}

pub fn notification(method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "method": method, "params": params})
}

pub const INTERNAL_ERROR: i64 = -32603;
pub const METHOD_NOT_FOUND: i64 = -32601;
