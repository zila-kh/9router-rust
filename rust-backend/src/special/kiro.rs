use axum::{body::Body,http::{header,HeaderValue,Response,StatusCode}};
use bytes::BytesMut;
use serde_json::{json,Value};
use uuid::Uuid;

use crate::{error::AppError,protocol::eventstream,state::AppState,streaming,translate::{self,Format}};

pub async fn execute(
    state:&AppState,
    caller:Format,
    wants_stream:bool,
    openai:Value,
    model:&str,
    connection:&Value,
    transport:&Value,
)->Result<Response<Body>,AppError>{
    let body=to_kiro(model,&openai,connection)?;
    let urls=ordered_urls(transport,connection);
    let mut last=None;
    for url in urls{
        match execute_url(state,&url,&body,connection,transport).await{
            Ok(bytes)=>{
                let canonical=eventstream_to_openai(&bytes,model)?;
                if wants_stream{return response(StatusCode::OK,"text/event-stream",streaming::synthesize(&canonical,caller)?)}
                return response(StatusCode::OK,"application/json",serde_json::to_vec(&translate::caller_response(canonical,caller)?)?)
            },
            Err(e)=>{tracing::warn!(url=%url,error=%e,"Kiro endpoint failed");last=Some(e)}
        }
    }
    Err(last.unwrap_or_else(||AppError::Upstream("Kiro has no usable endpoint".into())))
}

async fn execute_url(state:&AppState,url:&str,body:&Value,connection:&Value,transport:&Value)->Result<Vec<u8>,AppError>{
    let mut rb=state.http.post(url).json(body).header("accept","application/vnd.amazon.eventstream").header("content-type","application/json").header("amz-sdk-request","attempt=1; max=3").header("amz-sdk-invocation-id",Uuid::new_v4().to_string());
    if let Some(h)=transport.get("headers").and_then(Value::as_object){for(k,v)in h{if let Some(v)=v.as_str(){rb=rb.header(k,v)}}}
    let method=connection.pointer("/providerSpecificData/authMethod").and_then(Value::as_str).unwrap_or("");
    let token=connection.get("apiKey").or_else(||connection.get("accessToken")).and_then(Value::as_str).unwrap_or("");
    if !token.is_empty(){rb=rb.bearer_auth(token)}
    if method=="api_key"{rb=rb.header("TokenType","API_KEY")}else if method=="external_idp"{rb=rb.header("TokenType","EXTERNAL_IDP")}
    if url.contains("://codewhisperer."){rb=rb.header("X-Amz-Target","CodeWhispererService.GenerateAssistantResponse")}
    let r=rb.send().await.map_err(|e|AppError::Upstream(format!("Kiro request failed: {e}")))?;let status=r.status();let bytes=r.bytes().await.map_err(|e|AppError::Upstream(format!("Kiro read failed: {e}")))?;
    if !status.is_success(){return Err(AppError::Upstream(format!("Kiro HTTP {}: {}",status.as_u16(),String::from_utf8_lossy(&bytes).chars().take(2048).collect::<String>()))) }Ok(bytes.to_vec())
}

fn ordered_urls(t:&Value,c:&Value)->Vec<String>{
    let mut urls=t.get("baseUrls").and_then(Value::as_array).cloned().unwrap_or_default().into_iter().filter_map(|v|v.as_str().map(str::to_string)).collect::<Vec<_>>();
    if urls.is_empty(){if let Some(u)=t.get("baseUrl").and_then(Value::as_str){urls.push(u.to_string())}}
    let method=c.pointer("/providerSpecificData/authMethod").and_then(Value::as_str).unwrap_or("");
    if matches!(method,"api_key"|"external_idp"|"idc"){
        let region=c.pointer("/providerSpecificData/region").and_then(Value::as_str).unwrap_or("us-east-1");
        for u in &mut urls{if u.contains("amazonaws.com")&&region!="us-east-1"{*u=u.replace("us-east-1.amazonaws.com",&format!("{region}.amazonaws.com"))}}
        urls.sort_by_key(|u|if method=="api_key"&&u.contains("://q."){0}else if u.contains("amazonaws.com"){1}else{2});
    }
    urls
}

fn to_kiro(model:&str,b:&Value,c:&Value)->Result<Value,AppError>{
    let (upstream,agentic,thinking)=kiro_model_intent(model);
    let mut history=Vec::new();let mut current:Option<Value>=None;let mut system=Vec::new();
    for m in b.get("messages").and_then(Value::as_array).cloned().unwrap_or_default(){
        let role=m.get("role").and_then(Value::as_str).unwrap_or("user");
        if role=="system"||role=="developer"{let s=text(m.get("content"));if !s.is_empty(){system.push(s)}continue}
        if role=="tool"{
            let user=json!({"userInputMessage":{"content":"continue","modelId":upstream,"userInputMessageContext":{"toolResults":[{"toolUseId":m.get("tool_call_id").cloned().unwrap_or(json!("")),"status":"success","content":[{"text":text(m.get("content"))}]}]}}});
            history.push(user.clone());current=Some(user);continue
        }
        if role=="assistant"{
            let mut a=json!({"assistantResponseMessage":{"content":text(m.get("content"))}});let uses:Vec<Value>=m.get("tool_calls").and_then(Value::as_array).cloned().unwrap_or_default().into_iter().map(|tc|json!({"toolUseId":tc.get("id").cloned().unwrap_or(json!("")),"name":tc.pointer("/function/name").cloned().unwrap_or(json!("")),"input":tc.pointer("/function/arguments").and_then(Value::as_str).and_then(|s|serde_json::from_str::<Value>(s).ok()).unwrap_or(json!({}))})).collect();if !uses.is_empty(){a["assistantResponseMessage"]["toolUses"]=Value::Array(uses)}history.push(a);continue
        }
        let u=json!({"userInputMessage":{"content":text(m.get("content")),"modelId":upstream}});history.push(u.clone());current=Some(u);
    }
    if current.is_some(){if let Some(pos)=history.iter().rposition(|v|v.get("userInputMessage").is_some()){current=Some(history.remove(pos));}}
    let mut current=current.unwrap_or_else(||json!({"userInputMessage":{"content":"","modelId":upstream}}));
    let mut prefixes=Vec::new();if thinking{prefixes.push("<thinking_mode>enabled</thinking_mode>".to_string())}if agentic{prefixes.push("You are operating in an agentic coding mode. Complete the requested work using available tools when appropriate.".to_string())}prefixes.extend(system);prefixes.push(format!("[Context: Current time is {}]",chrono::Utc::now().to_rfc3339()));let prefix=prefixes.join("\n\n");let existing=current.pointer("/userInputMessage/content").and_then(Value::as_str).unwrap_or("").to_string();current["userInputMessage"]["content"]=json!(if existing.is_empty(){prefix}else{format!("{prefix}\n\n{existing}")});
    if let Some(tools)=b.get("tools").and_then(Value::as_array){let specs:Vec<Value>=tools.iter().filter_map(|t|t.get("function")).map(|f|json!({"toolSpecification":{"name":sanitize_tool_name(f.get("name").and_then(Value::as_str).unwrap_or("tool")),"description":f.get("description").cloned().unwrap_or(json!("")),"inputSchema":{"json":f.get("parameters").cloned().unwrap_or(json!({"type":"object"}))}}})).collect();if !specs.is_empty(){current["userInputMessage"]["userInputMessageContext"]["tools"]=Value::Array(specs)}}
    let mut payload=json!({"conversationState":{"chatTriggerType":"MANUAL","conversationId":Uuid::new_v4().to_string(),"agentContinuationId":Uuid::new_v4().to_string(),"agentTaskType":"vibe","currentMessage":current,"history":history},"agentMode":"vibe","inferenceConfig":{"maxTokens":b.get("max_tokens").cloned().unwrap_or(json!(32000))}});
    if let Some(v)=b.get("temperature"){payload["inferenceConfig"]["temperature"]=v.clone()}if let Some(v)=b.get("top_p"){payload["inferenceConfig"]["topP"]=v.clone()}
    let auth=c.pointer("/providerSpecificData/authMethod").and_then(Value::as_str).unwrap_or("");let profile=c.pointer("/providerSpecificData/profileArn").and_then(Value::as_str).unwrap_or("");if !profile.is_empty()&&!matches!(auth,"api_key"|"idc"|"external_idp"){payload["profileArn"]=json!(profile)}else if !profile.is_empty(){payload["profileArn"]=json!(profile)}
    Ok(payload)
}

fn eventstream_to_openai(bytes:&[u8],model:&str)->Result<Value,AppError>{
    let mut buf=BytesMut::from(bytes);let mut content=String::new();let mut reasoning=String::new();let mut tools:std::collections::BTreeMap<String,(String,String)>=std::collections::BTreeMap::new();let mut stop="stop".to_string();let mut prompt=0i64;let mut completion=0i64;
    while let Some(msg)=eventstream::decode_one(&mut buf)?{
        let message_type=msg.header_str(":message-type").unwrap_or("event");let ty=msg.header_str(":event-type").unwrap_or("");let data:Value=serde_json::from_slice(&msg.payload).unwrap_or_else(|_|json!({"content":String::from_utf8_lossy(&msg.payload)}));
        if message_type=="error"||message_type=="exception"{return Err(AppError::Upstream(data.get("message").and_then(Value::as_str).unwrap_or("Kiro EventStream error").to_string()))}
        match ty{
            "assistantResponseEvent"|"codeEvent"=>{if let Some(s)=data.get("content").and_then(Value::as_str){content.push_str(s)}},
            "reasoningContentEvent"=>{let v=data.get("reasoningContentEvent").unwrap_or(&data);if let Some(s)=v.as_str().or_else(||v.get("text").and_then(Value::as_str)).or_else(||v.get("content").and_then(Value::as_str)){reasoning.push_str(s)}},
            "toolUseEvent"=>{let vals=if let Some(a)=data.as_array(){a.clone()}else{vec![data.clone()]};for v in vals{let id=v.get("toolUseId").and_then(Value::as_str).map(str::to_string).unwrap_or_else(||format!("call_{}",Uuid::new_v4().simple()));let name=v.get("name").and_then(Value::as_str).unwrap_or("tool").to_string();let fragment=match v.get("input"){Some(Value::String(s))=>s.clone(),Some(x)=>serde_json::to_string(x).unwrap_or_else(|_|"{}".into()),None=>String::new()};let e=tools.entry(id).or_insert((name,String::new()));e.1.push_str(&fragment)}},
            "messageStopEvent"=>{let r=data.get("stopReason").or_else(||data.get("stop_reason")).and_then(Value::as_str).unwrap_or(if tools.is_empty(){"end_turn"}else{"tool_use"});stop=match r{"max_tokens"|"model_context_window_exceeded"=>"length", "tool_use"=>"tool_calls", _=>"stop"}.into()},
            "metricsEvent"=>{let v=data.get("metricsEvent").unwrap_or(&data);prompt=v.get("inputTokens").and_then(Value::as_i64).unwrap_or(prompt);completion=v.get("outputTokens").and_then(Value::as_i64).unwrap_or(completion)},_=>{}
        }
    }
    let calls:Vec<Value>=tools.into_iter().map(|(id,(name,args))|{let normalized=if serde_json::from_str::<Value>(&args).is_ok(){args}else{serde_json::to_string(&json!({"raw":args})).unwrap()};json!({"id":id,"type":"function","function":{"name":name,"arguments":normalized}})}).collect();if !calls.is_empty(){stop="tool_calls".into()}
    Ok(json!({"id":format!("chatcmpl-{}",Uuid::new_v4().simple()),"object":"chat.completion","created":chrono::Utc::now().timestamp(),"model":model,"choices":[{"index":0,"message":{"role":"assistant","content":content,"reasoning_content":if reasoning.is_empty(){Value::Null}else{json!(reasoning)},"tool_calls":calls},"finish_reason":stop}],"usage":{"prompt_tokens":prompt,"completion_tokens":completion,"total_tokens":prompt+completion}}))
}

fn kiro_model_intent(model:&str)->(String,bool,bool){let mut m=model.to_string();let agentic=m.contains("-agentic");let thinking=m.contains("-thinking");m=m.replace("-thinking-agentic","").replace("-agentic","").replace("-thinking","");(m,agentic,thinking)}
fn text(v:Option<&Value>)->String{match v{Some(Value::String(s))=>s.clone(),Some(Value::Array(a))=>a.iter().filter_map(|p|p.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n"),Some(v)=>v.to_string(),None=>String::new()}}
fn sanitize_tool_name(v:&str)->String{let mut s=v.chars().map(|c|if c.is_ascii_alphanumeric()||matches!(c,'_'|'-'){c}else{'_'}).collect::<String>();if s.is_empty(){s="tool".into()}s.truncate(64);s}
fn response(status:StatusCode,ct:&str,bytes:Vec<u8>)->Result<Response<Body>,AppError>{let mut r=Response::new(Body::from(bytes));*r.status_mut()=status;r.headers_mut().insert(header::CONTENT_TYPE,HeaderValue::from_str(ct).map_err(|e|AppError::Internal(e.into()))?);Ok(r)}
