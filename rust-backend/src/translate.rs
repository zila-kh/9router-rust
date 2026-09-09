use crate::error::AppError;
use serde_json::{json, Map, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    OpenAi,
    Claude,
    Gemini,
    Responses,
}
impl Format {
    pub fn from_provider(s: &str) -> Self {
        match s {
            "claude" => Self::Claude,
            "gemini" => Self::Gemini,
            "openai-responses" | "responses" => Self::Responses,
            _ => Self::OpenAi,
        }
    }
}

pub fn caller_for_path(path: &str) -> Format {
    if path.contains("/messages") {
        Format::Claude
    } else if path.starts_with("/v1beta") || path.starts_with("/api/v1beta") {
        Format::Gemini
    } else if path.ends_with("/responses") || path == "/responses" || path.starts_with("/codex") {
        Format::Responses
    } else {
        Format::OpenAi
    }
}

pub fn normalize_request(body: Value, caller: Format) -> Result<Value, AppError> {
    match caller {
        Format::OpenAi => Ok(body),
        Format::Claude => claude_to_openai_request(body),
        Format::Gemini => gemini_to_openai_request(body),
        Format::Responses => responses_to_openai_request(body),
    }
}
pub fn provider_request(openai: Value, provider: Format) -> Result<Value, AppError> {
    match provider {
        Format::OpenAi => Ok(openai),
        Format::Claude => openai_to_claude_request(openai),
        Format::Gemini => openai_to_gemini_request(openai),
        Format::Responses => openai_to_responses_request(openai),
    }
}
pub fn normalize_response(body: Value, provider: Format) -> Result<Value, AppError> {
    match provider {
        Format::OpenAi => Ok(body),
        Format::Claude => claude_to_openai_response(body),
        Format::Gemini => gemini_to_openai_response(body),
        Format::Responses => responses_to_openai_response(body),
    }
}
pub fn caller_response(openai: Value, caller: Format) -> Result<Value, AppError> {
    match caller {
        Format::OpenAi => Ok(openai),
        Format::Claude => openai_to_claude_response(openai),
        Format::Gemini => openai_to_gemini_response(openai),
        Format::Responses => openai_to_responses_response(openai),
    }
}

fn text_from_content(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(a) => a
            .iter()
            .filter_map(|b| {
                let ty = b.get("type").and_then(Value::as_str);
                match ty {
                    Some("text") | Some("input_text") | Some("output_text") => {
                        b.get("text").and_then(Value::as_str).map(str::to_string)
                    }
                    _ => None,
                }
            })
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

fn claude_to_openai_request(mut b: Value) -> Result<Value, AppError> {
    let o = b
        .as_object_mut()
        .ok_or_else(|| AppError::BadRequest("Claude request must be object".into()))?;
    let mut out = Map::new();
    if let Some(v) = o.remove("model") {
        out.insert("model".into(), v);
    }
    if let Some(v) = o.remove("max_tokens") {
        out.insert("max_tokens".into(), v);
    }
    if let Some(v) = o.remove("temperature") {
        out.insert("temperature".into(), v);
    }
    if let Some(v) = o.remove("top_p") {
        out.insert("top_p".into(), v);
    }
    if let Some(v) = o.remove("stop_sequences") {
        out.insert("stop".into(), v);
    }
    if let Some(v) = o.remove("stream") {
        out.insert("stream".into(), v);
    }
    let mut messages = Vec::new();
    if let Some(system) = o.remove("system") {
        messages.push(json!({"role":"system","content":text_from_content(&system)}))
    }
    for m in o
        .remove("messages")
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
    {
        let role = m.get("role").and_then(Value::as_str).unwrap_or("user");
        let content = m
            .get("content")
            .cloned()
            .unwrap_or(Value::String(String::new()));
        let mut text_parts = Vec::new();
        let mut tool_calls = Vec::new();
        let mut tool_results = Vec::new();
        match content {
            Value::String(s) => text_parts.push(Value::String(s)),
            Value::Array(blocks) => {
                for block in blocks {
                    match block.get("type").and_then(Value::as_str){Some("text")=>text_parts.push(block.get("text").cloned().unwrap_or(json!(""))),Some("image")=>{if let Some(src)=block.get("source"){let st=src.get("type").and_then(Value::as_str).unwrap_or("");let url=if st=="base64"{format!("data:{};base64,{}",src.get("media_type").and_then(Value::as_str).unwrap_or("application/octet-stream"),src.get("data").and_then(Value::as_str).unwrap_or(""))}else{src.get("url").and_then(Value::as_str).unwrap_or("").to_string()};text_parts.push(json!({"type":"image_url","image_url":{"url":url}}));}},Some("tool_use")=>tool_calls.push(json!({"id":block.get("id").cloned().unwrap_or(json!("")),"type":"function","function":{"name":block.get("name").cloned().unwrap_or(json!("")),"arguments":serde_json::to_string(block.get("input").unwrap_or(&json!({}))).unwrap_or_else(|_|"{}".into())}})),Some("tool_result")=>tool_results.push(json!({"role":"tool","tool_call_id":block.get("tool_use_id").cloned().unwrap_or(json!("")),"content":text_from_content(block.get("content").unwrap_or(&json!("")))})),Some("thinking")=>text_parts.push(block.get("thinking").cloned().unwrap_or(json!(""))),_=>{}}
                }
            }
            _ => {}
        }
        let content = if text_parts.len() == 1 && text_parts[0].is_string() {
            text_parts.remove(0)
        } else {
            Value::Array(
                text_parts
                    .into_iter()
                    .map(|v| {
                        if v.is_string() {
                            json!({"type":"text","text":v})
                        } else {
                            v
                        }
                    })
                    .collect(),
            )
        };
        let mut msg = json!({"role":role,"content":content});
        if !tool_calls.is_empty() {
            msg["tool_calls"] = Value::Array(tool_calls)
        }
        messages.push(msg);
        messages.extend(tool_results);
    }
    out.insert("messages".into(), Value::Array(messages));
    if let Some(tools) = o.remove("tools") {
        let arr=tools.as_array().cloned().unwrap_or_default().into_iter().map(|t|json!({"type":"function","function":{"name":t.get("name").cloned().unwrap_or(json!("")),"description":t.get("description").cloned().unwrap_or(json!("")),"parameters":t.get("input_schema").cloned().unwrap_or(json!({"type":"object"}))}})).collect();
        out.insert("tools".into(), Value::Array(arr));
    }
    if let Some(tc) = o.remove("tool_choice") {
        let v = match tc.get("type").and_then(Value::as_str) {
            Some("auto") => json!("auto"),
            Some("any") => json!("required"),
            Some("tool") => {
                json!({"type":"function","function":{"name":tc.get("name").cloned().unwrap_or(json!(""))}})
            }
            _ => tc,
        };
        out.insert("tool_choice".into(), v);
    }
    for (k, v) in o.iter() {
        if !out.contains_key(k) {
            out.insert(k.clone(), v.clone());
        }
    }
    Ok(Value::Object(out))
}

fn openai_to_claude_request(mut b: Value) -> Result<Value, AppError> {
    let object = b
        .as_object_mut()
        .ok_or_else(|| AppError::BadRequest("OpenAI request must be object".into()))?;
    let mut out = Map::new();

    for key in ["model", "temperature", "top_p", "stream"] {
        if let Some(value) = object.remove(key) {
            out.insert(key.to_string(), value);
        }
    }
    if let Some(value) = object
        .remove("max_tokens")
        .or_else(|| object.remove("max_completion_tokens"))
    {
        out.insert("max_tokens".into(), value);
    } else {
        out.insert("max_tokens".into(), json!(4096));
    }

    let mut system = Vec::new();
    let mut messages = Vec::new();
    for message in object
        .remove("messages")
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
    {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user");
        let message_content = message.get("content").cloned().unwrap_or(json!(""));

        if role == "system" || role == "developer" {
            let text = text_from_content(&message_content);
            if !text.is_empty() {
                system.push(json!({"type": "text", "text": text}));
            }
            continue;
        }

        if role == "tool" {
            messages.push(json!({
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": message
                        .get("tool_call_id")
                        .cloned()
                        .unwrap_or(json!("")),
                    "content": text_from_content(&message_content)
                }]
            }));
            continue;
        }

        let mut blocks = Vec::new();
        match &message_content {
            Value::String(text) => {
                if !text.is_empty() {
                    blocks.push(json!({"type": "text", "text": text}));
                }
            }
            Value::Array(parts) => {
                for part in parts {
                    match part.get("type").and_then(Value::as_str) {
                        Some("text") | Some("input_text") => {
                            blocks.push(json!({
                                "type": "text",
                                "text": part.get("text").cloned().unwrap_or(json!(""))
                            }));
                        }
                        Some("image_url") => {
                            if let Some(url) =
                                part.pointer("/image_url/url").and_then(Value::as_str)
                            {
                                if let Some(rest) = url.strip_prefix("data:") {
                                    if let Some((meta, data)) = rest.split_once(',') {
                                        blocks.push(json!({
                                            "type": "image",
                                            "source": {
                                                "type": "base64",
                                                "media_type": meta.trim_end_matches(";base64"),
                                                "data": data
                                            }
                                        }));
                                    }
                                } else {
                                    blocks.push(json!({
                                        "type": "image",
                                        "source": {"type": "url", "url": url}
                                    }));
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }

        if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                let input = call
                    .pointer("/function/arguments")
                    .and_then(Value::as_str)
                    .and_then(|s| serde_json::from_str::<Value>(s).ok())
                    .unwrap_or(json!({}));
                blocks.push(json!({
                    "type": "tool_use",
                    "id": call.get("id").cloned().unwrap_or(json!("")),
                    "name": call
                        .pointer("/function/name")
                        .cloned()
                        .unwrap_or(json!("")),
                    "input": input
                }));
            }
        }

        messages.push(json!({
            "role": if role == "assistant" { "assistant" } else { "user" },
            "content": blocks
        }));
    }

    if !system.is_empty() {
        out.insert("system".into(), Value::Array(system));
    }
    out.insert("messages".into(), Value::Array(messages));

    if let Some(tools) = object.remove("tools") {
        let mapped = tools
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|tool| tool.get("function").cloned())
            .map(|function| {
                json!({
                    "name": function.get("name").cloned().unwrap_or(json!("")),
                    "description": function
                        .get("description")
                        .cloned()
                        .unwrap_or(json!("")),
                    "input_schema": function
                        .get("parameters")
                        .cloned()
                        .unwrap_or(json!({"type": "object"}))
                })
            })
            .collect::<Vec<_>>();
        out.insert("tools".into(), Value::Array(mapped));
    }

    if let Some(choice) = object.remove("tool_choice") {
        let mapped = match choice.as_str() {
            Some("auto") => json!({"type": "auto"}),
            Some("required") => json!({"type": "any"}),
            _ if choice.get("type").and_then(Value::as_str) == Some("function") => json!({
                "type": "tool",
                "name": choice
                    .pointer("/function/name")
                    .cloned()
                    .unwrap_or(json!(""))
            }),
            _ => choice,
        };
        out.insert("tool_choice".into(), mapped);
    }

    for (key, value) in object.iter() {
        if !out.contains_key(key) {
            out.insert(key.clone(), value.clone());
        }
    }

    Ok(Value::Object(out))
}
fn gemini_to_openai_request(b: Value) -> Result<Value, AppError> {
    let model = b.get("model").cloned().unwrap_or(json!(""));
    let stream = b.get("stream").cloned().unwrap_or(json!(true));
    let mut messages = Vec::new();
    if let Some(sys) = b.get("systemInstruction") {
        messages
            .push(json!({"role":"system","content":parts_text(sys.get("parts").unwrap_or(sys))}))
    }
    for c in b
        .get("contents")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
    {
        let role = if c.get("role").and_then(Value::as_str) == Some("model") {
            "assistant"
        } else {
            "user"
        };
        let mut content = Vec::new();
        for p in c
            .get("parts")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
        {
            if let Some(t) = p.get("text") {
                content.push(json!({"type":"text","text":t}))
            } else if let Some(d) = p.get("inlineData") {
                content.push(json!({"type":"image_url","image_url":{"url":format!("data:{};base64,{}",d.get("mimeType").and_then(Value::as_str).unwrap_or("application/octet-stream"),d.get("data").and_then(Value::as_str).unwrap_or(""))}}))
            } else if let Some(fc) = p.get("functionCall") {
                content.push(json!({"type":"text","text":""}));
                messages.push(json!({"role":"assistant","content":null,"tool_calls":[{"id":format!("call_{}",uuid::Uuid::new_v4().simple()),"type":"function","function":{"name":fc.get("name").cloned().unwrap_or(json!("")),"arguments":serde_json::to_string(fc.get("args").unwrap_or(&json!({}))).unwrap()}}]}))
            } else if let Some(fr) = p.get("functionResponse") {
                messages.push(json!({"role":"tool","tool_call_id":fr.get("id").cloned().unwrap_or(json!("")),"content":serde_json::to_string(fr.get("response").unwrap_or(&json!({}))).unwrap()}))
            }
        }
        if !content.is_empty() {
            messages.push(json!({"role":role,"content":content}))
        }
    }
    let mut out = json!({"model":model,"messages":messages,"stream":stream});
    if let Some(gc) = b.get("generationConfig") {
        if let Some(v) = gc.get("temperature") {
            out["temperature"] = v.clone()
        }
        if let Some(v) = gc.get("topP") {
            out["top_p"] = v.clone()
        }
        if let Some(v) = gc.get("maxOutputTokens") {
            out["max_tokens"] = v.clone()
        }
        if let Some(v) = gc.get("stopSequences") {
            out["stop"] = v.clone()
        }
    }
    if let Some(tools) = b.get("tools").and_then(Value::as_array) {
        let mut funcs = Vec::new();
        for t in tools {
            for f in t
                .get("functionDeclarations")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
            {
                funcs.push(json!({"type":"function","function":{"name":f.get("name").cloned().unwrap_or(json!("")),"description":f.get("description").cloned().unwrap_or(json!("")),"parameters":f.get("parameters").cloned().unwrap_or(json!({"type":"object"}))}}))
            }
        }
        out["tools"] = Value::Array(funcs)
    }
    Ok(out)
}
fn parts_text(v: &Value) -> String {
    if let Some(a) = v.as_array() {
        a.iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("")
    } else {
        v.get("text").and_then(Value::as_str).unwrap_or("").into()
    }
}

fn openai_to_gemini_request(b: Value) -> Result<Value, AppError> {
    let mut contents = Vec::new();
    let mut system_parts = Vec::new();

    for message in b
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
    {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user");
        let content = message.get("content").cloned().unwrap_or(json!(""));

        if role == "system" || role == "developer" {
            let text = text_from_content(&content);
            if !text.is_empty() {
                system_parts.push(json!({"text": text}));
            }
            continue;
        }

        let mut parts = Vec::new();
        match &content {
            Value::String(text) => {
                if !text.is_empty() {
                    parts.push(json!({"text": text}));
                }
            }
            Value::Array(items) => {
                for item in items {
                    if let Some(text) = item.get("text").and_then(Value::as_str) {
                        parts.push(json!({"text": text}));
                        continue;
                    }
                    if let Some(url) = item.pointer("/image_url/url").and_then(Value::as_str) {
                        if let Some(rest) = url.strip_prefix("data:") {
                            if let Some((meta, data)) = rest.split_once(',') {
                                parts.push(json!({
                                    "inlineData": {
                                        "mimeType": meta.trim_end_matches(";base64"),
                                        "data": data
                                    }
                                }));
                            }
                        }
                    }
                }
            }
            _ => {}
        }

        if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                let args = call
                    .pointer("/function/arguments")
                    .and_then(Value::as_str)
                    .and_then(|s| serde_json::from_str::<Value>(s).ok())
                    .unwrap_or(json!({}));
                parts.push(json!({
                    "functionCall": {
                        "name": call
                            .pointer("/function/name")
                            .cloned()
                            .unwrap_or(json!("")),
                        "args": args
                    }
                }));
            }
        }

        if role == "tool" {
            let function_name = message
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("tool");
            parts.push(json!({
                "functionResponse": {
                    "name": function_name,
                    "response": {"content": text_from_content(&content)}
                }
            }));
        }

        if parts.is_empty() {
            parts.push(json!({"text": ""}));
        }
        contents.push(json!({
            "role": if role == "assistant" { "model" } else { "user" },
            "parts": parts
        }));
    }

    let mut out = json!({"contents": contents});
    if !system_parts.is_empty() {
        out["systemInstruction"] = json!({"parts": system_parts});
    }

    let mut generation_config = Map::new();
    for (source, target) in [
        ("temperature", "temperature"),
        ("top_p", "topP"),
        ("max_tokens", "maxOutputTokens"),
        ("stop", "stopSequences"),
    ] {
        if let Some(value) = b.get(source) {
            generation_config.insert(target.to_string(), value.clone());
        }
    }
    if !generation_config.is_empty() {
        out["generationConfig"] = Value::Object(generation_config);
    }

    if let Some(tools) = b.get("tools").and_then(Value::as_array) {
        let declarations = tools
            .iter()
            .filter_map(|tool| tool.get("function"))
            .map(|function| {
                json!({
                    "name": function.get("name").cloned().unwrap_or(json!("")),
                    "description": function
                        .get("description")
                        .cloned()
                        .unwrap_or(json!("")),
                    "parameters": function
                        .get("parameters")
                        .cloned()
                        .unwrap_or(json!({"type": "object"}))
                })
            })
            .collect::<Vec<_>>();
        if !declarations.is_empty() {
            out["tools"] = json!([{"functionDeclarations": declarations}]);
        }
    }

    Ok(out)
}
fn responses_to_openai_request(b: Value) -> Result<Value, AppError> {
    let mut messages = Vec::new();
    if let Some(inst) = b.get("instructions").and_then(Value::as_str) {
        messages.push(json!({"role":"system","content":inst}))
    }
    match b.get("input") {
        Some(Value::String(s)) => messages.push(json!({"role":"user","content":s})),
        Some(Value::Array(items)) => {
            for i in items {
                let ty = i.get("type").and_then(Value::as_str).unwrap_or("message");
                if ty == "message" {
                    messages.push(json!({"role":i.get("role").cloned().unwrap_or(json!("user")),"content":i.get("content").cloned().unwrap_or(json!(""))}))
                } else if ty == "function_call_output" {
                    messages.push(json!({"role":"tool","tool_call_id":i.get("call_id").cloned().unwrap_or(json!("")),"content":i.get("output").cloned().unwrap_or(json!(""))}))
                }
            }
        }
        _ => {}
    }
    let mut out = json!({"model":b.get("model").cloned().unwrap_or(json!("")),"messages":messages,"stream":b.get("stream").cloned().unwrap_or(json!(false))});
    if let Some(v) = b.get("temperature") {
        out["temperature"] = v.clone()
    }
    if let Some(v) = b.get("max_output_tokens") {
        out["max_tokens"] = v.clone()
    }
    if let Some(v) = b.get("tools") {
        out["tools"] = v.clone()
    }
    if let Some(v) = b.get("tool_choice") {
        out["tool_choice"] = v.clone()
    }
    Ok(out)
}

fn openai_to_responses_request(b: Value) -> Result<Value, AppError> {
    let mut input = Vec::new();
    let mut instructions = Vec::new();
    for m in b
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
    {
        let role = m.get("role").and_then(Value::as_str).unwrap_or("user");
        if role == "system" || role == "developer" {
            instructions.push(text_from_content(m.get("content").unwrap_or(&json!(""))));
            continue;
        }
        if role == "tool" {
            input.push(json!({"type":"function_call_output","call_id":m.get("tool_call_id").cloned().unwrap_or(json!("")),"output":text_from_content(m.get("content").unwrap_or(&json!("")))}));
            continue;
        }
        input.push(json!({"type":"message","role":role,"content":m.get("content").cloned().unwrap_or(json!(""))}));
        if let Some(calls) = m.get("tool_calls").and_then(Value::as_array) {
            for c in calls {
                input.push(json!({"type":"function_call","call_id":c.get("id").cloned().unwrap_or(json!("")),"name":c.pointer("/function/name").cloned().unwrap_or(json!("")),"arguments":c.pointer("/function/arguments").cloned().unwrap_or(json!("{}"))}))
            }
        }
    }
    let mut out = json!({"model":b.get("model").cloned().unwrap_or(json!("")),"input":input,"stream":b.get("stream").cloned().unwrap_or(json!(false))});
    if !instructions.is_empty() {
        out["instructions"] = json!(instructions.join("\n\n"))
    }
    if let Some(v) = b.get("tools") {
        out["tools"] = v.clone()
    }
    if let Some(v) = b.get("tool_choice") {
        out["tool_choice"] = v.clone()
    }
    if let Some(v) = b.get("temperature") {
        out["temperature"] = v.clone()
    }
    if let Some(v) = b.get("max_tokens") {
        out["max_output_tokens"] = v.clone()
    }
    Ok(out)
}

fn claude_to_openai_response(b: Value) -> Result<Value, AppError> {
    let mut text = String::new();
    let mut calls = Vec::new();
    for c in b
        .get("content")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
    {
        match c.get("type").and_then(Value::as_str){Some("text")=>text.push_str(c.get("text").and_then(Value::as_str).unwrap_or("")),Some("tool_use")=>calls.push(json!({"id":c.get("id").cloned().unwrap_or(json!("")),"type":"function","function":{"name":c.get("name").cloned().unwrap_or(json!("")),"arguments":serde_json::to_string(c.get("input").unwrap_or(&json!({}))).unwrap_or_else(|_|"{}".into())}})),_=>{}}
    }
    let stop = match b.get("stop_reason").and_then(Value::as_str) {
        Some("tool_use") => "tool_calls",
        Some("max_tokens") => "length",
        _ => "stop",
    };
    let usage = b.get("usage").cloned().unwrap_or(json!({}));
    Ok(
        json!({"id":b.get("id").cloned().unwrap_or(json!(format!("chatcmpl-{}",uuid::Uuid::new_v4().simple()))),"object":"chat.completion","model":b.get("model").cloned().unwrap_or(json!("")),"choices":[{"index":0,"message":{"role":"assistant","content":text,"tool_calls":calls},"finish_reason":stop}],"usage":{"prompt_tokens":usage.get("input_tokens").cloned().unwrap_or(json!(0)),"completion_tokens":usage.get("output_tokens").cloned().unwrap_or(json!(0)),"total_tokens":usage.get("input_tokens").and_then(Value::as_i64).unwrap_or(0)+usage.get("output_tokens").and_then(Value::as_i64).unwrap_or(0)}}),
    )
}
fn gemini_to_openai_response(b: Value) -> Result<Value, AppError> {
    let c = b
        .get("candidates")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .cloned()
        .unwrap_or(json!({}));
    let mut text = String::new();
    let mut calls = Vec::new();
    for p in c
        .pointer("/content/parts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
    {
        if let Some(t) = p.get("text").and_then(Value::as_str) {
            text.push_str(t)
        }
        if let Some(fc) = p.get("functionCall") {
            calls.push(json!({"id":format!("call_{}",uuid::Uuid::new_v4().simple()),"type":"function","function":{"name":fc.get("name").cloned().unwrap_or(json!("")),"arguments":serde_json::to_string(fc.get("args").unwrap_or(&json!({}))).unwrap()}}))
        }
    }
    let fr = match c.get("finishReason").and_then(Value::as_str) {
        Some("MAX_TOKENS") => "length",
        _ if !calls.is_empty() => "tool_calls",
        _ => "stop",
    };
    let u = b.get("usageMetadata").cloned().unwrap_or(json!({}));
    Ok(
        json!({"id":format!("chatcmpl-{}",uuid::Uuid::new_v4().simple()),"object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":text,"tool_calls":calls},"finish_reason":fr}],"usage":{"prompt_tokens":u.get("promptTokenCount").cloned().unwrap_or(json!(0)),"completion_tokens":u.get("candidatesTokenCount").cloned().unwrap_or(json!(0)),"total_tokens":u.get("totalTokenCount").cloned().unwrap_or(json!(0))}}),
    )
}
fn responses_to_openai_response(b: Value) -> Result<Value, AppError> {
    let mut text = String::new();
    let mut calls = Vec::new();
    for i in b
        .get("output")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
    {
        match i.get("type").and_then(Value::as_str){Some("message")=>for c in i.get("content").and_then(Value::as_array).cloned().unwrap_or_default(){if let Some(t)=c.get("text").and_then(Value::as_str){text.push_str(t)}},Some("function_call")=>calls.push(json!({"id":i.get("call_id").or_else(||i.get("id")).cloned().unwrap_or(json!("")),"type":"function","function":{"name":i.get("name").cloned().unwrap_or(json!("")),"arguments":i.get("arguments").cloned().unwrap_or(json!("{}"))}})),_=>{}}
    }
    let u = b.get("usage").cloned().unwrap_or(json!({}));
    Ok(
        json!({"id":b.get("id").cloned().unwrap_or(json!(format!("chatcmpl-{}",uuid::Uuid::new_v4().simple()))),"object":"chat.completion","model":b.get("model").cloned().unwrap_or(json!("")),"choices":[{"index":0,"message":{"role":"assistant","content":text,"tool_calls":calls},"finish_reason":if calls.is_empty(){"stop"}else{"tool_calls"}}],"usage":{"prompt_tokens":u.get("input_tokens").cloned().unwrap_or(json!(0)),"completion_tokens":u.get("output_tokens").cloned().unwrap_or(json!(0)),"total_tokens":u.get("total_tokens").cloned().unwrap_or(json!(0))}}),
    )
}

fn openai_to_claude_response(b: Value) -> Result<Value, AppError> {
    let choice = b
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .cloned()
        .unwrap_or(json!({}));
    let msg = choice.get("message").cloned().unwrap_or(json!({}));
    let mut content = Vec::new();
    let t = text_from_content(msg.get("content").unwrap_or(&json!("")));
    if !t.is_empty() {
        content.push(json!({"type":"text","text":t}))
    }
    for c in msg
        .get("tool_calls")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
    {
        let input = c
            .pointer("/function/arguments")
            .and_then(Value::as_str)
            .and_then(|s| serde_json::from_str::<Value>(s).ok())
            .unwrap_or(json!({}));
        content.push(json!({"type":"tool_use","id":c.get("id").cloned().unwrap_or(json!("")),"name":c.pointer("/function/name").cloned().unwrap_or(json!("")),"input":input}))
    }
    let u = b.get("usage").cloned().unwrap_or(json!({}));
    Ok(
        json!({"id":b.get("id").cloned().unwrap_or(json!(format!("msg_{}",uuid::Uuid::new_v4().simple()))),"type":"message","role":"assistant","model":b.get("model").cloned().unwrap_or(json!("")),"content":content,"stop_reason":match choice.get("finish_reason").and_then(Value::as_str){Some("tool_calls")=>"tool_use",Some("length")=>"max_tokens",_=>"end_turn"},"stop_sequence":null,"usage":{"input_tokens":u.get("prompt_tokens").cloned().unwrap_or(json!(0)),"output_tokens":u.get("completion_tokens").cloned().unwrap_or(json!(0))}}),
    )
}
fn openai_to_gemini_response(b: Value) -> Result<Value, AppError> {
    let choice = b
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .cloned()
        .unwrap_or(json!({}));
    let msg = choice.get("message").cloned().unwrap_or(json!({}));
    let mut parts = Vec::new();
    let t = text_from_content(msg.get("content").unwrap_or(&json!("")));
    if !t.is_empty() {
        parts.push(json!({"text":t}))
    }
    for c in msg
        .get("tool_calls")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
    {
        let args = c
            .pointer("/function/arguments")
            .and_then(Value::as_str)
            .and_then(|s| serde_json::from_str::<Value>(s).ok())
            .unwrap_or(json!({}));
        parts.push(json!({"functionCall":{"name":c.pointer("/function/name").cloned().unwrap_or(json!("")),"args":args}}))
    }
    let u = b.get("usage").cloned().unwrap_or(json!({}));
    Ok(
        json!({"candidates":[{"content":{"role":"model","parts":parts},"finishReason":match choice.get("finish_reason").and_then(Value::as_str){Some("length")=>"MAX_TOKENS",_=>"STOP"},"index":0}],"usageMetadata":{"promptTokenCount":u.get("prompt_tokens").cloned().unwrap_or(json!(0)),"candidatesTokenCount":u.get("completion_tokens").cloned().unwrap_or(json!(0)),"totalTokenCount":u.get("total_tokens").cloned().unwrap_or(json!(0))}}),
    )
}
fn openai_to_responses_response(b: Value) -> Result<Value, AppError> {
    let choice = b
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .cloned()
        .unwrap_or(json!({}));
    let msg = choice.get("message").cloned().unwrap_or(json!({}));
    let mut output = Vec::new();
    let t = text_from_content(msg.get("content").unwrap_or(&json!("")));
    if !t.is_empty() {
        output.push(json!({"type":"message","id":format!("msg_{}",uuid::Uuid::new_v4().simple()),"status":"completed","role":"assistant","content":[{"type":"output_text","text":t,"annotations":[]}]}))
    }
    for c in msg
        .get("tool_calls")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
    {
        output.push(json!({"type":"function_call","id":format!("fc_{}",uuid::Uuid::new_v4().simple()),"call_id":c.get("id").cloned().unwrap_or(json!("")),"name":c.pointer("/function/name").cloned().unwrap_or(json!("")),"arguments":c.pointer("/function/arguments").cloned().unwrap_or(json!("{}")),"status":"completed"}))
    }
    let u = b.get("usage").cloned().unwrap_or(json!({}));
    Ok(
        json!({"id":b.get("id").cloned().unwrap_or(json!(format!("resp_{}",uuid::Uuid::new_v4().simple()))),"object":"response","status":"completed","model":b.get("model").cloned().unwrap_or(json!("")),"output":output,"usage":{"input_tokens":u.get("prompt_tokens").cloned().unwrap_or(json!(0)),"output_tokens":u.get("completion_tokens").cloned().unwrap_or(json!(0)),"total_tokens":u.get("total_tokens").cloned().unwrap_or(json!(0))}}),
    )
}
