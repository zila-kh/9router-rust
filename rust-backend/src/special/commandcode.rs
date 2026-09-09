use axum::{body::Body,http::{header,HeaderValue,Response,StatusCode}};
use serde_json::{json,Value};
use uuid::Uuid;

use crate::{error::AppError,state::AppState,streaming,translate::{self,Format}};

pub async fn execute(
    state:&AppState,
    caller:Format,
    wants_stream:bool,
    openai:Value,
    model:&str,
    connection:&Value,
    transport:&Value,
)->Result<Response<Body>,AppError>{
    let url=transport.get("baseUrl").and_then(Value::as_str).unwrap_or("https://api.commandcode.ai/alpha/generate");
    let body=to_commandcode(model,&openai);
    let token=connection.get("apiKey").or_else(||connection.get("accessToken")).and_then(Value::as_str).unwrap_or("");
    let mut rb=state.http.post(url)
        .header("content-type","application/json")
        .header("accept","text/event-stream")
        .header("x-session-id",Uuid::new_v4().to_string())
        .json(&body);
    if !token.is_empty(){rb=rb.bearer_auth(token)}
    if let Some(h)=transport.get("headers").and_then(Value::as_object){for(k,v)in h{if let Some(v)=v.as_str(){rb=rb.header(k,v)}}}
    let r=rb.send().await.map_err(|e|AppError::Upstream(format!("CommandCode request failed: {e}")))?;
    let status=r.status();let bytes=r.bytes().await.map_err(|e|AppError::Upstream(format!("CommandCode response read failed: {e}")))?;
    if !status.is_success(){let text=String::from_utf8_lossy(&bytes);return Err(AppError::Upstream(format!("CommandCode HTTP {}: {}",status.as_u16(),text.chars().take(2048).collect::<String>()))) }
    let canonical=ndjson_to_openai(&bytes,model)?;
    if wants_stream{let bytes=streaming::synthesize(&canonical,caller)?;return response(StatusCode::OK,"text/event-stream",bytes)}
    let value=translate::caller_response(canonical,caller)?;response(StatusCode::OK,"application/json",serde_json::to_vec(&value)?)
}

fn to_commandcode(model:&str,b:&Value)->Value{
    let mut messages=Vec::new();let mut system=Vec::new();
    for m in b.get("messages").and_then(Value::as_array).cloned().unwrap_or_default(){
        let role=m.get("role").and_then(Value::as_str).unwrap_or("user");
        if role=="system"||role=="developer"{let t=text_content(m.get("content"));if !t.is_empty(){system.push(t)}continue}
        if role=="tool"{messages.push(json!({"role":"tool","content":[{"type":"tool-result","toolCallId":m.get("tool_call_id").cloned().unwrap_or(json!("")),"toolName":m.get("name").cloned().unwrap_or(json!("")),"output":{"type":"text","value":text_content(m.get("content"))}}]}));continue}
        let mut content=Vec::new();let text=text_content(m.get("content"));if !text.is_empty(){content.push(json!({"type":"text","text":text}))}
        if role=="assistant"{for tc in m.get("tool_calls").and_then(Value::as_array).cloned().unwrap_or_default(){let input=tc.pointer("/function/arguments").and_then(Value::as_str).and_then(|s|serde_json::from_str::<Value>(s).ok()).unwrap_or(json!({}));content.push(json!({"type":"tool-call","toolCallId":tc.get("id").cloned().unwrap_or(json!("")),"toolName":tc.pointer("/function/name").cloned().unwrap_or(json!("")),"input":input}))}}
        if content.is_empty(){content.push(json!({"type":"text","text":""}))}
        messages.push(json!({"role":if role=="assistant"{"assistant"}else{"user"},"content":content}));
    }
    let tools:Vec<Value>=b.get("tools").and_then(Value::as_array).cloned().unwrap_or_default().into_iter().filter_map(|t|t.get("function").cloned()).map(|f|json!({"name":f.get("name").cloned().unwrap_or(json!("")),"description":f.get("description").cloned().unwrap_or(json!("")),"input_schema":f.get("parameters").cloned().unwrap_or(json!({"type":"object"}))})).collect();
    let mut params=json!({"model":model,"messages":messages,"stream":true,"max_tokens":b.get("max_tokens").or_else(||b.get("max_output_tokens")).cloned().unwrap_or(json!(32000)),"temperature":b.get("temperature").cloned().unwrap_or(json!(0.3))});
    if !system.is_empty(){params["system"]=json!(system.join("\n\n"))}if !tools.is_empty(){params["tools"]=Value::Array(tools)}if let Some(v)=b.get("top_p"){params["top_p"]=v.clone()}
    json!({"threadId":Uuid::new_v4().to_string(),"memory":"","config":{"workingDir":".","date":chrono::Utc::now().format("%Y-%m-%d").to_string(),"environment":std::env::consts::OS,"structure":[],"isGitRepo":false,"currentBranch":"","mainBranch":"","gitStatus":"","recentCommits":[]},"params":params})
}

fn ndjson_to_openai(bytes:&[u8],model:&str)->Result<Value,AppError>{
    let text=String::from_utf8_lossy(bytes);let mut content=String::new();let mut reasoning=String::new();let mut tools:Vec<Value>=Vec::new();let mut open:std::collections::HashMap<String,usize>=std::collections::HashMap::new();let mut finish="stop".to_string();let mut usage=json!({});
    for raw in text.lines(){let line=raw.trim().strip_prefix("data:").unwrap_or(raw.trim()).trim();if line.is_empty()||line=="[DONE]"{continue}let Ok(e)=serde_json::from_str::<Value>(line)else{continue};match e.get("type").and_then(Value::as_str){
        Some("text-delta")=>content.push_str(e.get("text").or_else(||e.get("delta")).and_then(Value::as_str).unwrap_or("")),
        Some("reasoning-delta")=>reasoning.push_str(e.get("text").and_then(Value::as_str).unwrap_or("")),
        Some("tool-input-start")=>{let id=e.get("id").or_else(||e.get("toolCallId")).and_then(Value::as_str).map(str::to_string).unwrap_or_else(||format!("call_{}",Uuid::new_v4().simple()));let idx=tools.len();open.insert(id.clone(),idx);tools.push(json!({"id":id,"type":"function","function":{"name":e.get("toolName").cloned().unwrap_or(json!("")),"arguments":""}}))},
        Some("tool-input-delta")=>{if let Some(id)=e.get("id").or_else(||e.get("toolCallId")).and_then(Value::as_str){if let Some(&idx)=open.get(id){let old=tools[idx].pointer("/function/arguments").and_then(Value::as_str).unwrap_or("").to_string();tools[idx]["function"]["arguments"]=json!(format!("{old}{}",e.get("delta").or_else(||e.get("inputTextDelta")).and_then(Value::as_str).unwrap_or("")))}}},
        Some("tool-call")=>{let id=e.get("toolCallId").and_then(Value::as_str).unwrap_or("");if !open.contains_key(id){tools.push(json!({"id":id,"type":"function","function":{"name":e.get("toolName").cloned().unwrap_or(json!("")),"arguments":if e.get("input").and_then(Value::as_str).is_some(){e.get("input").cloned().unwrap()}else{json!(serde_json::to_string(e.get("input").unwrap_or(&json!({}))).unwrap_or_else(|_|"{}".into()))}}}))}},
        Some("finish-step")=>{finish=map_finish(e.get("finishReason").and_then(Value::as_str));if let Some(u)=e.get("usage"){usage=u.clone()}},
        Some("finish")=>{if let Some(v)=e.get("finishReason").and_then(Value::as_str){finish=map_finish(Some(v))}if let Some(u)=e.get("totalUsage"){usage=u.clone()}},
        Some("error")=>return Err(AppError::Upstream(format!("CommandCode stream error: {}",e.get("error").or_else(||e.get("message")).cloned().unwrap_or(json!("unknown"))))),_=>{}
    }}
    let prompt=usage.get("inputTokens").or_else(||usage.get("promptTokens")).or_else(||usage.get("input_tokens")).and_then(Value::as_i64).unwrap_or(0);let completion=usage.get("outputTokens").or_else(||usage.get("completionTokens")).or_else(||usage.get("output_tokens")).and_then(Value::as_i64).unwrap_or(0);
    Ok(json!({"id":format!("chatcmpl-{}",Uuid::new_v4().simple()),"object":"chat.completion","created":chrono::Utc::now().timestamp(),"model":model,"choices":[{"index":0,"message":{"role":"assistant","content":content,"reasoning_content":if reasoning.is_empty(){Value::Null}else{json!(reasoning)},"tool_calls":tools},"finish_reason":if !tools.is_empty(){"tool_calls"}else{finish.as_str()}}],"usage":{"prompt_tokens":prompt,"completion_tokens":completion,"total_tokens":prompt+completion}}))
}
fn map_finish(v:Option<&str>)->String{match v.unwrap_or("").to_ascii_lowercase().as_str(){"length"|"max_tokens"|"max-tokens"=>"length".into(),"tool-calls"|"tool_calls"|"tool_use"=>"tool_calls".into(),_=>"stop".into()}}
fn text_content(v:Option<&Value>)->String{match v{Some(Value::String(s))=>s.clone(),Some(Value::Array(a))=>a.iter().filter_map(|p|p.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n"),Some(v)=>v.to_string(),None=>String::new()}}
fn response(status:StatusCode,ct:&str,bytes:Vec<u8>)->Result<Response<Body>,AppError>{let mut r=Response::new(Body::from(bytes));*r.status_mut()=status;r.headers_mut().insert(header::CONTENT_TYPE,HeaderValue::from_str(ct).map_err(|e|AppError::Internal(e.into()))?);Ok(r)}
