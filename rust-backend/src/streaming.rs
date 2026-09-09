use crate::{error::AppError, translate::Format};
use serde_json::{json, Value};

pub fn looks_streaming(content_type: Option<&str>, bytes: &[u8]) -> bool {
    content_type
        .map(|s| {
            s.contains("text/event-stream")
                || s.contains("amazon.eventstream")
                || s.contains("grpc-web")
        })
        .unwrap_or(false)
        || bytes.starts_with(b"data:")
        || bytes.windows(6).any(|w| w == b"data: ")
}

fn sse_jsons(bytes: &[u8]) -> Vec<Value> {
    let text = String::from_utf8_lossy(bytes);
    let mut out = Vec::new();
    for block in text.replace("\r\n", "\n").split("\n\n") {
        for line in block.lines() {
            if let Some(data) = line.strip_prefix("data:") {
                let data = data.trim();
                if data.is_empty() || data == "[DONE]" {
                    continue;
                }
                if let Ok(v) = serde_json::from_str::<Value>(data) {
                    out.push(v)
                }
            }
        }
    }
    out
}

pub fn reduce_stream(bytes: &[u8], format: Format, model: &str) -> Result<Value, AppError> {
    match format {
        Format::OpenAi => reduce_openai(bytes, model),
        Format::Claude => reduce_claude(bytes, model),
        Format::Gemini => reduce_gemini(bytes),
        Format::Responses => reduce_responses(bytes, model),
    }
}

fn reduce_openai(bytes: &[u8], model: &str) -> Result<Value, AppError> {
    let events = sse_jsons(bytes);
    let mut content = String::new();
    let mut reasoning = String::new();
    let mut finish = "stop".to_string();
    let mut prompt = 0;
    let mut completion = 0;
    let mut calls: Vec<Value> = Vec::new();
    let mut id = None;
    for e in events {
        if id.is_none() {
            id = e.get("id").cloned()
        }
        if let Some(u) = e.get("usage") {
            prompt = u
                .get("prompt_tokens")
                .and_then(Value::as_i64)
                .unwrap_or(prompt);
            completion = u
                .get("completion_tokens")
                .and_then(Value::as_i64)
                .unwrap_or(completion)
        }
        if let Some(c) = e
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
        {
            if let Some(f) = c.get("finish_reason").and_then(Value::as_str) {
                finish = f.into()
            }
            let d = c
                .get("delta")
                .or_else(|| c.get("message"))
                .cloned()
                .unwrap_or(json!({}));
            if let Some(t) = d.get("content").and_then(Value::as_str) {
                content.push_str(t)
            }
            if let Some(t) = d.get("reasoning_content").and_then(Value::as_str) {
                reasoning.push_str(t)
            }
            if let Some(tc) = d.get("tool_calls").and_then(Value::as_array) {
                for item in tc {
                    let idx = item
                        .get("index")
                        .and_then(Value::as_u64)
                        .unwrap_or(calls.len() as u64) as usize;
                    while calls.len() <= idx {
                        calls.push(json!({"id":"","type":"function","function":{"name":"","arguments":""}}))
                    }
                    if let Some(s) = item.get("id").and_then(Value::as_str) {
                        calls[idx]["id"] = json!(s)
                    }
                    if let Some(s) = item.pointer("/function/name").and_then(Value::as_str) {
                        calls[idx]["function"]["name"] = json!(s)
                    }
                    if let Some(s) = item.pointer("/function/arguments").and_then(Value::as_str) {
                        let old = calls[idx]["function"]["arguments"].as_str().unwrap_or("");
                        calls[idx]["function"]["arguments"] = json!(format!("{old}{s}"))
                    }
                }
            }
        }
    }
    let mut msg = json!({"role":"assistant","content":content});
    if !reasoning.is_empty() {
        msg["reasoning_content"] = json!(reasoning)
    }
    if !calls.is_empty() {
        msg["tool_calls"] = Value::Array(calls)
    }
    Ok(
        json!({"id":id.unwrap_or(json!(format!("chatcmpl-{}",uuid::Uuid::new_v4().simple()))),"object":"chat.completion","model":model,"choices":[{"index":0,"message":msg,"finish_reason":finish}],"usage":{"prompt_tokens":prompt,"completion_tokens":completion,"total_tokens":prompt+completion}}),
    )
}

fn reduce_claude(bytes: &[u8], model: &str) -> Result<Value, AppError> {
    let events = sse_jsons(bytes);
    let mut id = None;
    let mut content: Vec<Value> = Vec::new();
    let mut stop = "end_turn".to_string();
    let mut input = 0;
    let mut output = 0;
    let mut active: Option<usize> = None;
    for e in events {
        match e.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                let m = e.get("message").cloned().unwrap_or(json!({}));
                id = m.get("id").cloned();
                input = m
                    .pointer("/usage/input_tokens")
                    .and_then(Value::as_i64)
                    .unwrap_or(input)
            }
            Some("content_block_start") => {
                let idx = e
                    .get("index")
                    .and_then(Value::as_u64)
                    .unwrap_or(content.len() as u64) as usize;
                while content.len() <= idx {
                    content.push(json!({}))
                }
                content[idx] = e
                    .get("content_block")
                    .cloned()
                    .unwrap_or(json!({"type":"text","text":""}));
                active = Some(idx)
            }
            Some("content_block_delta") => {
                let idx = e
                    .get("index")
                    .and_then(Value::as_u64)
                    .or(active.map(|i| i as u64))
                    .unwrap_or(0) as usize;
                while content.len() <= idx {
                    content.push(json!({"type":"text","text":""}))
                }
                let d = e.get("delta").cloned().unwrap_or(json!({}));
                match d.get("type").and_then(Value::as_str) {
                    Some("text_delta") => {
                        let s = d.get("text").and_then(Value::as_str).unwrap_or("");
                        let old = content[idx]
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        content[idx]["type"] = json!("text");
                        content[idx]["text"] = json!(format!("{old}{s}"))
                    }
                    Some("input_json_delta") => {
                        let s = d.get("partial_json").and_then(Value::as_str).unwrap_or("");
                        let old = content[idx]
                            .get("_partial_json")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        content[idx]["_partial_json"] = json!(format!("{old}{s}"))
                    }
                    Some("thinking_delta") => {
                        let s = d.get("thinking").and_then(Value::as_str).unwrap_or("");
                        let old = content[idx]
                            .get("thinking")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        content[idx]["type"] = json!("thinking");
                        content[idx]["thinking"] = json!(format!("{old}{s}"))
                    }
                    _ => {}
                }
            }
            Some("content_block_stop") => {
                if let Some(i) = e.get("index").and_then(Value::as_u64).map(|x| x as usize) {
                    if let Some(s) = content
                        .get(i)
                        .and_then(|v| v.get("_partial_json"))
                        .and_then(Value::as_str)
                    {
                        content[i]["input"] = serde_json::from_str(s).unwrap_or(json!({}));
                        if let Some(o) = content[i].as_object_mut() {
                            o.remove("_partial_json");
                        }
                    }
                }
            }
            Some("message_delta") => {
                stop = e
                    .pointer("/delta/stop_reason")
                    .and_then(Value::as_str)
                    .unwrap_or(&stop)
                    .to_string();
                output = e
                    .pointer("/usage/output_tokens")
                    .and_then(Value::as_i64)
                    .unwrap_or(output)
            }
            _ => {}
        }
    }
    Ok(
        json!({"id":id.unwrap_or(json!(format!("msg_{}",uuid::Uuid::new_v4().simple()))),"type":"message","role":"assistant","model":model,"content":content,"stop_reason":stop,"stop_sequence":null,"usage":{"input_tokens":input,"output_tokens":output}}),
    )
}

fn reduce_gemini(bytes: &[u8]) -> Result<Value, AppError> {
    let mut events = sse_jsons(bytes);
    if events.is_empty() {
        let text = String::from_utf8_lossy(bytes);
        if let Ok(Value::Array(a)) = serde_json::from_str::<Value>(&text) {
            events = a
        }
    }
    let mut text = String::new();
    let mut calls = Vec::new();
    let mut finish = "STOP".to_string();
    let mut usage = json!({});
    for e in events {
        if let Some(c) = e
            .get("candidates")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
        {
            if let Some(f) = c.get("finishReason").and_then(Value::as_str) {
                finish = f.into()
            }
            for p in c
                .pointer("/content/parts")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
            {
                if let Some(t) = p.get("text").and_then(Value::as_str) {
                    text.push_str(t)
                }
                if p.get("functionCall").is_some() {
                    calls.push(p)
                }
            }
        }
        if e.get("usageMetadata").is_some() {
            usage = e.get("usageMetadata").cloned().unwrap()
        }
    }
    let mut parts = Vec::new();
    if !text.is_empty() {
        parts.push(json!({"text":text}))
    }
    parts.extend(calls);
    Ok(
        json!({"candidates":[{"content":{"role":"model","parts":parts},"finishReason":finish,"index":0}],"usageMetadata":usage}),
    )
}

fn reduce_responses(bytes: &[u8], model: &str) -> Result<Value, AppError> {
    let events = sse_jsons(bytes);
    let mut id = None;
    let mut text = String::new();
    let mut output = Vec::new();
    let mut usage = json!({});
    let mut status = "completed".to_string();
    for e in events {
        let ty = e.get("type").and_then(Value::as_str).unwrap_or("");
        if id.is_none() {
            id = e
                .pointer("/response/id")
                .cloned()
                .or_else(|| e.get("response_id").cloned())
        }
        match ty {
            "response.output_text.delta" => {
                text.push_str(e.get("delta").and_then(Value::as_str).unwrap_or(""))
            }
            "response.output_item.done" => {
                if let Some(item) = e.get("item") {
                    if item.get("type").and_then(Value::as_str) == Some("function_call") {
                        output.push(item.clone())
                    }
                }
            }
            "response.completed" => {
                if let Some(r) = e.get("response") {
                    status = r
                        .get("status")
                        .and_then(Value::as_str)
                        .unwrap_or("completed")
                        .into();
                    usage = r.get("usage").cloned().unwrap_or(json!({}));
                    if let Some(a) = r.get("output").and_then(Value::as_array) {
                        for item in a {
                            if item.get("type").and_then(Value::as_str) == Some("function_call")
                                && !output.iter().any(|x| x == item)
                            {
                                output.push(item.clone());
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    if !text.is_empty() {
        output.insert(0,json!({"type":"message","id":format!("msg_{}",uuid::Uuid::new_v4().simple()),"status":"completed","role":"assistant","content":[{"type":"output_text","text":text,"annotations":[]}]}))
    }
    Ok(
        json!({"id":id.unwrap_or(json!(format!("resp_{}",uuid::Uuid::new_v4().simple()))),"object":"response","status":status,"model":model,"output":output,"usage":usage}),
    )
}

pub fn synthesize(openai: &Value, caller: Format) -> Result<Vec<u8>, AppError> {
    match caller {
        Format::OpenAi => openai_sse(openai),
        Format::Claude => claude_sse(openai),
        Format::Gemini => gemini_sse(openai),
        Format::Responses => responses_sse(openai),
    }
}
fn line(v: &Value) -> Vec<u8> {
    format!("data: {}\n\n", serde_json::to_string(v).unwrap()).into_bytes()
}
fn openai_sse(b: &Value) -> Result<Vec<u8>, AppError> {
    let id = b
        .get("id")
        .cloned()
        .unwrap_or(json!(format!("chatcmpl-{}", uuid::Uuid::new_v4().simple())));
    let model = b.get("model").cloned().unwrap_or(json!(""));
    let choice = b
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .cloned()
        .unwrap_or(json!({}));
    let message = choice.get("message").cloned().unwrap_or(json!({}));

    let mut out = Vec::new();
    out.extend(line(&json!({
        "id": id,
        "object": "chat.completion.chunk",
        "model": model,
        "choices": [{
            "index": 0,
            "delta": {"role": "assistant"},
            "finish_reason": null
        }]
    })));

    if let Some(text) = message.get("content").and_then(Value::as_str) {
        if !text.is_empty() {
            out.extend(line(&json!({
                "id": id,
                "object": "chat.completion.chunk",
                "model": model,
                "choices": [{
                    "index": 0,
                    "delta": {"content": text},
                    "finish_reason": null
                }]
            })));
        }
    }

    if let Some(tool_calls) = message.get("tool_calls") {
        out.extend(line(&json!({
            "id": id,
            "object": "chat.completion.chunk",
            "model": model,
            "choices": [{
                "index": 0,
                "delta": {"tool_calls": tool_calls},
                "finish_reason": null
            }]
        })));
    }

    out.extend(line(&json!({
        "id": id,
        "object": "chat.completion.chunk",
        "model": model,
        "choices": [{
            "index": 0,
            "delta": {},
            "finish_reason": choice
                .get("finish_reason")
                .cloned()
                .unwrap_or(json!("stop"))
        }],
        "usage": b.get("usage").cloned().unwrap_or(json!({}))
    })));
    out.extend_from_slice(b"data: [DONE]\n\n");
    Ok(out)
}
fn claude_sse(b: &Value) -> Result<Vec<u8>, AppError> {
    let c = crate::translate::caller_response(b.clone(), Format::Claude)?;
    let mut out = Vec::new();
    out.extend_from_slice(format!("event: message_start\ndata: {}\n\n",serde_json::to_string(&json!({"type":"message_start","message":{"id":c.get("id"),"type":"message","role":"assistant","model":c.get("model"),"content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":c.pointer("/usage/input_tokens").cloned().unwrap_or(json!(0)),"output_tokens":0}}})).unwrap()).as_bytes());
    for (i, block) in c
        .get("content")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .enumerate()
    {
        out.extend_from_slice(format!("event: content_block_start\ndata: {}\n\n",serde_json::to_string(&json!({"type":"content_block_start","index":i,"content_block":match block.get("type").and_then(Value::as_str){Some("text")=>json!({"type":"text","text":""}),Some("tool_use")=>json!({"type":"tool_use","id":block.get("id"),"name":block.get("name"),"input":{}}),_=>block.clone()}})).unwrap()).as_bytes());
        match block.get("type").and_then(Value::as_str){Some("text")=>out.extend_from_slice(format!("event: content_block_delta\ndata: {}\n\n",serde_json::to_string(&json!({"type":"content_block_delta","index":i,"delta":{"type":"text_delta","text":block.get("text")}})).unwrap()).as_bytes()),Some("tool_use")=>out.extend_from_slice(format!("event: content_block_delta\ndata: {}\n\n",serde_json::to_string(&json!({"type":"content_block_delta","index":i,"delta":{"type":"input_json_delta","partial_json":serde_json::to_string(block.get("input").unwrap_or(&json!({}))).unwrap()}})).unwrap()).as_bytes()),_=>{}}
        out.extend_from_slice(
            format!(
                "event: content_block_stop\ndata: {}\n\n",
                serde_json::to_string(&json!({"type":"content_block_stop","index":i})).unwrap()
            )
            .as_bytes(),
        )
    }
    out.extend_from_slice(format!("event: message_delta\ndata: {}\n\n",serde_json::to_string(&json!({"type":"message_delta","delta":{"stop_reason":c.get("stop_reason"),"stop_sequence":null},"usage":{"output_tokens":c.pointer("/usage/output_tokens").cloned().unwrap_or(json!(0))}})).unwrap()).as_bytes());
    out.extend_from_slice(b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");
    Ok(out)
}
fn gemini_sse(b: &Value) -> Result<Vec<u8>, AppError> {
    let v = crate::translate::caller_response(b.clone(), Format::Gemini)?;
    Ok(line(&v))
}
fn responses_sse(b: &Value) -> Result<Vec<u8>, AppError> {
    let r = crate::translate::caller_response(b.clone(), Format::Responses)?;
    let id = r.get("id").cloned().unwrap_or(json!(""));
    let mut out = Vec::new();
    out.extend(line(&json!({"type":"response.created","response":{"id":id,"object":"response","status":"in_progress","model":r.get("model"),"output":[]}})));
    for (oi, item) in r
        .get("output")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .enumerate()
    {
        out.extend(line(
            &json!({"type":"response.output_item.added","output_index":oi,"item":item}),
        ));
        if item.get("type").and_then(Value::as_str) == Some("message") {
            for (ci, c) in item
                .get("content")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .enumerate()
            {
                if let Some(t) = c.get("text").and_then(Value::as_str) {
                    out.extend(line(&json!({"type":"response.output_text.delta","output_index":oi,"content_index":ci,"delta":t})));
                    out.extend(line(&json!({"type":"response.output_text.done","output_index":oi,"content_index":ci,"text":t})))
                }
            }
        }
        out.extend(line(
            &json!({"type":"response.output_item.done","output_index":oi,"item":item}),
        ))
    }
    out.extend(line(&json!({"type":"response.completed","response":r})));
    Ok(out)
}
