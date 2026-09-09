use once_cell::sync::Lazy;
use serde_json::{json, Value};
use crate::{error::AppError, state::AppState};

static CATALOG: Lazy<Value> = Lazy::new(|| serde_json::from_str(include_str!("../assets/provider-catalog.json")).unwrap_or_else(|_| json!({"registry":[],"providers":{},"models":{},"oauth":{},"media":{}})));

#[derive(Debug,Clone)]
pub struct ResolvedModel { pub requested:String, pub provider:String, pub model:String, pub connection:Value }

pub fn catalog()->&'static Value { &CATALOG }
pub fn provider_entry(id:&str)->Option<&'static Value>{
    CATALOG.get("registry")?.as_array()?.iter().find(|e| e.get("id").and_then(Value::as_str)==Some(id) || e.get("alias").and_then(Value::as_str)==Some(id) || e.get("aliases").and_then(Value::as_array).map(|a|a.iter().any(|v|v.as_str()==Some(id))).unwrap_or(false))
}
pub fn transport(id:&str)->Value{
    if id.starts_with("anthropic-compatible-") { return json!({"format":"claude"}); }
    if id.starts_with("openai-compatible-") { return json!({"format":"openai"}); }
    let canonical=provider_entry(id).and_then(|e|e.get("id")).and_then(Value::as_str).unwrap_or(id);
    CATALOG.get("providers").and_then(|p|p.get(canonical)).cloned().unwrap_or_else(||provider_entry(id).and_then(|e|e.get("transport")).cloned().unwrap_or_else(||json!({})))
}
pub fn media_config(id:&str, kind:&str)->Value {
    let canonical=canonical_provider(id);
    let key=match kind {
        "embedding"=>"embeddingConfig", "tts"=>"ttsConfig", "stt"=>"sttConfig",
        "image"=>"imageConfig", "search"=>"searchConfig", "fetch"=>"fetchConfig", _=>""
    };
    if key.is_empty(){return json!({});}
    CATALOG.get("media").and_then(|m|m.get(&canonical)).and_then(|v|v.get(key)).cloned()
        .or_else(||provider_entry(&canonical).and_then(|e|e.get(key)).cloned())
        .unwrap_or_else(||json!({}))
}
pub fn models_for(id:&str)->Vec<Value>{
    let alias=provider_entry(id).and_then(|e|e.get("alias")).and_then(Value::as_str).unwrap_or(id);
    CATALOG.get("models").and_then(|m|m.get(alias)).and_then(Value::as_array).cloned().unwrap_or_default()
}
pub fn all_models_openai()->Value{
    let mut data=Vec::new(); if let Some(entries)=CATALOG.get("registry").and_then(Value::as_array){for e in entries{let id=e.get("id").and_then(Value::as_str).unwrap_or_default();let alias=e.get("alias").and_then(Value::as_str).unwrap_or(id);for m in models_for(id){if let Some(mid)=m.get("id").and_then(Value::as_str){data.push(json!({"id":format!("{alias}/{mid}"),"object":"model","owned_by":id,"root":mid,"capabilities":m.get("capabilities").cloned().unwrap_or(Value::Null),"kind":m.get("kind").cloned().unwrap_or(json!("llm"))}))}}}}
    json!({"object":"list","data":data})
}

pub fn split_model(requested:&str)->(Option<&str>,&str){if let Some((p,m))=requested.split_once('/') { (Some(p),m) } else {(None,requested)}}
pub fn canonical_provider(id:&str)->String{provider_entry(id).and_then(|e|e.get("id")).and_then(Value::as_str).unwrap_or(id).to_string()}

pub fn resolve_model(state:&AppState,requested:&str)->Result<ResolvedModel,AppError>{
    let (explicit,model)=split_model(requested);
    if let Some(p)=explicit {return resolve_for_provider(state,p,requested,model)}
    if let Some(target)=state.db.kv_get("modelAliases",requested)?.and_then(|v|v.as_str().map(str::to_string)){return resolve_model(state,&target)}
    let aliases=state.db.kv_all("modelAliases")?;
    if let Some((target,_))=aliases.iter().find(|(_,v)|v.as_str()==Some(requested)){return resolve_model(state,target)}
    let inferred = if requested.starts_with("claude-") { Some("anthropic") } else if requested.starts_with("gemini-") { Some("gemini") } else if requested.starts_with("gpt-") || requested.starts_with("o1") || requested.starts_with("o3") || requested.starts_with("o4") { Some("openai") } else if requested.starts_with("deepseek-") { Some("openrouter") } else { None };
    if let Some(p)=inferred { if let Ok(r)=resolve_for_provider(state,p,requested,model){return Ok(r)} }
    let active=state.db.provider_connections(None,Some(true))?;
    for c in &active{if c.get("defaultModel").and_then(Value::as_str)==Some(requested){let p=c.get("provider").and_then(Value::as_str).unwrap_or_default();return Ok(ResolvedModel{requested:requested.into(),provider:p.into(),model:model.into(),connection:c.clone()})}}
    let mut candidates=Vec::new(); if let Some(entries)=CATALOG.get("registry").and_then(Value::as_array){for e in entries{let p=e.get("id").and_then(Value::as_str).unwrap_or_default();if models_for(p).iter().any(|m|m.get("id").and_then(Value::as_str)==Some(model)){candidates.push(p)}}}
    for p in candidates {if let Ok(r)=resolve_for_provider(state,p,requested,model){return Ok(r)}}
    Err(AppError::NotFound(format!("no active provider for model {requested}")))
}

fn resolve_for_provider(state:&AppState,p:&str,requested:&str,model:&str)->Result<ResolvedModel,AppError>{
    let provider=canonical_provider(p); let conns=state.db.provider_connections(Some(&provider),Some(true))?;
    let connection=conns.into_iter().next().ok_or_else(||AppError::NotFound(format!("no active connection for provider {provider}")))?;
    let upstream=model_upstream_id(&provider,model).unwrap_or_else(||model.to_string());
    Ok(ResolvedModel{requested:requested.into(),provider,model:upstream,connection})
}

pub fn model_upstream_id(provider:&str,model:&str)->Option<String>{models_for(provider).into_iter().find(|m|m.get("id").and_then(Value::as_str)==Some(model)).and_then(|m|m.get("upstreamModelId").and_then(Value::as_str).map(str::to_string))}

pub fn auth_header(connection:&Value, t:&Value)->Option<(String,String)>{
    let auth=t.get("auth");
    let api=connection.get("apiKey").and_then(Value::as_str);
    let access=connection.get("accessToken").and_then(Value::as_str);
    let format=t.get("format").and_then(Value::as_str).unwrap_or("openai");
    let spec = if let Some(a)=auth {
        if a.get("combined").and_then(Value::as_bool).unwrap_or(false) || a.get("header").is_some() { Some(a) }
        else if api.is_some() { a.get("apiKey") } else { a.get("oauth") }
    } else { None };
    let (header,scheme,token)=if let Some(spec)=spec {
        (spec.get("header").and_then(Value::as_str).unwrap_or("Authorization"),spec.get("scheme").and_then(Value::as_str).unwrap_or("bearer"),api.or(access)?)
    } else if format=="claude" { ("x-api-key","raw",api.or(access)?) }
    else { ("Authorization","bearer",api.or(access)?) };
    let value=if scheme=="bearer"{format!("Bearer {token}")}else{token.to_string()};Some((header.into(),value))
}

pub fn endpoint(provider:&str,connection:&Value,kind:&str,model:&str)->Result<(String,String),AppError>{
    let t=transport(provider); let format=t.get("format").and_then(Value::as_str).unwrap_or("openai").to_string();
    let custom=connection.pointer("/providerSpecificData/baseUrl").and_then(Value::as_str);
    let mut url=custom.or_else(||t.get("baseUrl").and_then(Value::as_str)).unwrap_or("").to_string();
    if url.is_empty(){return Err(AppError::BadRequest(format!("provider {provider} has no transport baseUrl")))}
    if custom.is_some(){
        url=url.trim_end_matches('/').to_string();
        if kind=="chat" && provider.starts_with("anthropic-compatible-") && !url.ends_with("/messages") { url.push_str("/messages"); }
        else if kind=="chat" && !provider.starts_with("anthropic-compatible-") && !url.ends_with("/chat/completions") && !url.ends_with("/responses") { url.push_str("/chat/completions"); }
    }
    if format=="gemini" { url=url.replace("{model}",model); }
    if kind!="chat" {if let Some(media)=CATALOG.get("media").and_then(|m|m.get(provider)){let key=match kind{"embedding"=>"embeddingConfig","tts"=>"ttsConfig","stt"=>"sttConfig","image"=>"imageConfig","search"=>"searchConfig","fetch"=>"fetchConfig",_=>""};if let Some(base)=media.get(key).and_then(|c|c.get("baseUrl")).and_then(Value::as_str){url=base.into()}}}
    Ok((url,format))
}
