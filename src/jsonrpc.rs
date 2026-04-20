use serde_json::Value;

pub struct Inspected<'a> {
    pub id: Option<Value>,
    pub method: Option<&'a str>,
    pub params: Option<&'a Value>,
    pub result: Option<&'a Value>,
    pub error: Option<&'a Value>,
}

pub fn inspect(msg: &Value) -> Inspected<'_> {
    Inspected {
        id: msg.get("id").cloned(),
        method: msg.get("method").and_then(Value::as_str),
        params: msg.get("params"),
        result: msg.get("result"),
        error: msg.get("error"),
    }
}

/// True if the message is a `tools/call` request — the governed action.
pub fn is_tools_call(inspected: &Inspected) -> bool {
    inspected.method == Some("tools/call") && inspected.params.is_some()
}

/// True if the message is a response (has id + either result or error, no method).
pub fn is_response(inspected: &Inspected) -> bool {
    inspected.id.is_some() && inspected.method.is_none()
}

pub fn tool_name<'a>(params: &'a Value) -> Option<&'a str> {
    params.get("name").and_then(Value::as_str)
}

pub fn tool_arguments<'a>(params: &'a Value) -> Option<&'a Value> {
    params.get("arguments")
}
