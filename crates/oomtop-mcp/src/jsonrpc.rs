//! JSON-RPC 2.0 framing helpers (MCP 2025-06-18 uses JSON-RPC without batching).

use serde_json::{json, Value};

pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;

/// One decoded JSON-RPC message.
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    /// A request expecting a response.
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    /// A notification (no id, no response).
    Notification { method: String, params: Value },
    /// A response to a request we sent.
    Response {
        id: Value,
        result: Result<Value, RpcError>,
    },
    /// Not a valid JSON-RPC 2.0 message; `id` is echoed when recoverable (else `null`).
    Invalid { id: Value, message: String },
}

/// An error object from a JSON-RPC response.
#[derive(Debug, Clone, PartialEq)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

fn valid_id(id: &Value) -> bool {
    id.is_string() || id.is_i64() || id.is_u64()
}

/// Classifies a parsed JSON value.
pub fn classify(v: &Value) -> Incoming {
    if v.is_array() {
        return Incoming::Invalid {
            id: Value::Null,
            message: "JSON-RPC batches are not supported (MCP 2025-06-18)".into(),
        };
    }
    let Some(obj) = v.as_object() else {
        return Incoming::Invalid {
            id: Value::Null,
            message: "a JSON-RPC message must be an object".into(),
        };
    };
    let raw_id = obj.get("id").cloned();
    let echo_id = raw_id.clone().filter(valid_id).unwrap_or(Value::Null);
    if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Incoming::Invalid {
            id: echo_id,
            message: "\"jsonrpc\" must be \"2.0\"".into(),
        };
    }
    let params = obj.get("params").cloned().unwrap_or(Value::Null);
    if let Some(m) = obj.get("method") {
        let Some(method) = m.as_str() else {
            return Incoming::Invalid {
                id: echo_id,
                message: "\"method\" must be a string".into(),
            };
        };
        if !(params.is_null() || params.is_object() || params.is_array()) {
            return Incoming::Invalid {
                id: echo_id,
                message: "\"params\" must be an object".into(),
            };
        }
        return match raw_id {
            None => Incoming::Notification {
                method: method.to_string(),
                params,
            },
            Some(id) if valid_id(&id) => Incoming::Request {
                id,
                method: method.to_string(),
                params,
            },
            Some(_) => Incoming::Invalid {
                id: Value::Null,
                message: "\"id\" must be a string or an integer".into(),
            },
        };
    }
    match raw_id {
        Some(id) if valid_id(&id) => {
            if let Some(r) = obj.get("result") {
                Incoming::Response {
                    id,
                    result: Ok(r.clone()),
                }
            } else if let Some(e) = obj.get("error") {
                Incoming::Response {
                    id,
                    result: Err(RpcError {
                        code: e.get("code").and_then(Value::as_i64).unwrap_or(INTERNAL_ERROR),
                        message: e.get("message").and_then(Value::as_str).unwrap_or("").to_string(),
                    }),
                }
            } else {
                Incoming::Invalid {
                    id,
                    message: "message has neither \"method\" nor \"result\"/\"error\"".into(),
                }
            }
        }
        _ => Incoming::Invalid {
            id: Value::Null,
            message: "message has no \"method\" and no valid \"id\"".into(),
        },
    }
}

pub fn ok(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

pub fn err(id: &Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

pub fn request(id: &Value, method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_messages() {
        assert!(matches!(
            classify(&json!({"jsonrpc":"2.0","id":1,"method":"ping"})),
            Incoming::Request { .. }
        ));
        assert!(matches!(
            classify(&json!({"jsonrpc":"2.0","method":"notifications/initialized"})),
            Incoming::Notification { .. }
        ));
        assert!(matches!(
            classify(&json!({"jsonrpc":"2.0","id":"a","result":{}})),
            Incoming::Response { result: Ok(_), .. }
        ));
        match classify(&json!({"jsonrpc":"2.0","id":"a","error":{"code":-1,"message":"no"}})) {
            Incoming::Response { result: Err(e), .. } => assert_eq!(e.code, -1),
            other => panic!("{other:?}"),
        }
        for bad in [
            json!([]),
            json!(3),
            json!({"id":1,"method":"x"}),
            json!({"jsonrpc":"2.0","id":null,"method":"x"}),
            json!({"jsonrpc":"2.0","id":1.5,"method":"x"}),
            json!({"jsonrpc":"2.0","id":1,"method":7}),
            json!({"jsonrpc":"2.0","id":1,"method":"x","params":3}),
            json!({"jsonrpc":"2.0"}),
        ] {
            assert!(matches!(classify(&bad), Incoming::Invalid { .. }), "{bad}");
        }
        // The id is echoed when it is valid.
        match classify(&json!({"jsonrpc":"1.0","id":9,"method":"x"})) {
            Incoming::Invalid { id, .. } => assert_eq!(id, json!(9)),
            other => panic!("{other:?}"),
        }
    }
}
