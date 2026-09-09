use crate::error::AppError;
use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Map, Value};
use std::{
    fs,
    path::Path,
    sync::{Arc, Mutex},
};
use uuid::Uuid;

#[derive(Clone)]
pub struct Db {
    inner: Arc<Mutex<Connection>>,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self, AppError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| AppError::Internal(e.into()))?;
        }
        let conn = Connection::open(path)?;
        let db = Self {
            inner: Arc::new(Mutex::new(conn)),
        };
        db.init()?;
        Ok(db)
    }

    fn with_conn<T>(
        &self,
        f: impl FnOnce(&mut Connection) -> Result<T, AppError>,
    ) -> Result<T, AppError> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| AppError::Internal(anyhow::anyhow!("database mutex poisoned")))?;
        f(&mut guard)
    }

    fn init(&self) -> Result<(), AppError> {
        self.with_conn(|db| {
            db.execute_batch(r#"
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA temp_store = MEMORY;
PRAGMA mmap_size = 30000000;
PRAGMA cache_size = -64000;
PRAGMA foreign_keys = ON;
PRAGMA busy_timeout = 5000;
CREATE TABLE IF NOT EXISTS _meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS settings (id INTEGER PRIMARY KEY CHECK (id = 1), data TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS providerConnections (
  id TEXT PRIMARY KEY, provider TEXT NOT NULL, authType TEXT NOT NULL, name TEXT, email TEXT,
  priority INTEGER, isActive INTEGER DEFAULT 1, data TEXT NOT NULL, createdAt TEXT NOT NULL, updatedAt TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_pc_provider ON providerConnections(provider);
CREATE INDEX IF NOT EXISTS idx_pc_provider_active ON providerConnections(provider, isActive);
CREATE INDEX IF NOT EXISTS idx_pc_priority ON providerConnections(provider, priority);
CREATE TABLE IF NOT EXISTS providerNodes (
  id TEXT PRIMARY KEY, type TEXT, name TEXT, data TEXT NOT NULL, createdAt TEXT NOT NULL, updatedAt TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_pn_type ON providerNodes(type);
CREATE TABLE IF NOT EXISTS proxyPools (
  id TEXT PRIMARY KEY, isActive INTEGER DEFAULT 1, testStatus TEXT, data TEXT NOT NULL, createdAt TEXT NOT NULL, updatedAt TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_pp_active ON proxyPools(isActive);
CREATE INDEX IF NOT EXISTS idx_pp_status ON proxyPools(testStatus);
CREATE TABLE IF NOT EXISTS apiKeys (
  id TEXT PRIMARY KEY, key TEXT UNIQUE NOT NULL, name TEXT, machineId TEXT, isActive INTEGER DEFAULT 1, createdAt TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_ak_key ON apiKeys(key);
CREATE TABLE IF NOT EXISTS combos (
  id TEXT PRIMARY KEY, name TEXT UNIQUE NOT NULL, kind TEXT, models TEXT NOT NULL, createdAt TEXT NOT NULL, updatedAt TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_combo_name ON combos(name);
CREATE TABLE IF NOT EXISTS kv (scope TEXT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL, PRIMARY KEY (scope, key));
CREATE INDEX IF NOT EXISTS idx_kv_scope ON kv(scope);
CREATE TABLE IF NOT EXISTS usageHistory (
  id INTEGER PRIMARY KEY AUTOINCREMENT, timestamp TEXT NOT NULL, provider TEXT, model TEXT, connectionId TEXT,
  apiKey TEXT, endpoint TEXT, promptTokens INTEGER DEFAULT 0, completionTokens INTEGER DEFAULT 0,
  cost REAL DEFAULT 0, status TEXT, tokens TEXT, meta TEXT
);
CREATE INDEX IF NOT EXISTS idx_uh_ts ON usageHistory(timestamp DESC);
CREATE INDEX IF NOT EXISTS idx_uh_provider ON usageHistory(provider);
CREATE INDEX IF NOT EXISTS idx_uh_model ON usageHistory(model);
CREATE INDEX IF NOT EXISTS idx_uh_conn ON usageHistory(connectionId);
CREATE TABLE IF NOT EXISTS usageDaily (dateKey TEXT PRIMARY KEY, data TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS requestDetails (
  id TEXT PRIMARY KEY, timestamp TEXT NOT NULL, provider TEXT, model TEXT, connectionId TEXT, status TEXT, data TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_rd_ts ON requestDetails(timestamp DESC);
CREATE INDEX IF NOT EXISTS idx_rd_provider ON requestDetails(provider);
CREATE INDEX IF NOT EXISTS idx_rd_model ON requestDetails(model);
CREATE INDEX IF NOT EXISTS idx_rd_conn ON requestDetails(connectionId);
INSERT INTO _meta(key,value) VALUES('schema_version','1') ON CONFLICT(key) DO NOTHING;
"#)?;
            Ok(())
        })
    }

    pub fn settings(&self) -> Result<Value, AppError> {
        let raw = self.with_conn(|db| {
            let s: Option<String> = db
                .query_row("SELECT data FROM settings WHERE id=1", [], |r| r.get(0))
                .optional()?;
            Ok(s.and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_else(|| json!({})))
        })?;
        Ok(merge_settings_defaults(raw))
    }

    pub fn update_settings(&self, update: Value) -> Result<Value, AppError> {
        let patch = update
            .as_object()
            .ok_or_else(|| AppError::BadRequest("settings body must be an object".into()))?;
        self.with_conn(|db| {
            let tx = db.transaction()?;
            let raw: Option<String> = tx.query_row("SELECT data FROM settings WHERE id=1", [], |r| r.get(0)).optional()?;
            let mut current: Map<String, Value> = raw.and_then(|s| serde_json::from_str::<Value>(&s).ok())
                .and_then(|v| v.as_object().cloned()).unwrap_or_default();
            for (k,v) in patch { current.insert(k.clone(), v.clone()); }
            let data = serde_json::to_string(&Value::Object(current.clone()))?;
            tx.execute("INSERT INTO settings(id,data) VALUES(1,?1) ON CONFLICT(id) DO UPDATE SET data=excluded.data", params![data])?;
            tx.commit()?;
            Ok(merge_settings_defaults(Value::Object(current)))
        })
    }

    pub fn provider_connections(
        &self,
        provider: Option<&str>,
        active: Option<bool>,
    ) -> Result<Vec<Value>, AppError> {
        self.with_conn(|db| {
            let mut sql = String::from("SELECT id,provider,authType,name,email,priority,isActive,data,createdAt,updatedAt FROM providerConnections");
            let mut clauses = vec![];
            if provider.is_some() { clauses.push("provider = ?1"); }
            if active.is_some() { clauses.push(if provider.is_some() { "isActive = ?2" } else { "isActive = ?1" }); }
            if !clauses.is_empty() { sql.push_str(" WHERE "); sql.push_str(&clauses.join(" AND ")); }
            sql.push_str(" ORDER BY COALESCE(priority,999), updatedAt DESC");
            let mut stmt = db.prepare(&sql)?;
            let mut rows = match (provider,active) {
                (Some(p),Some(a)) => stmt.query(params![p, if a {1}else{0}])?,
                (Some(p),None) => stmt.query(params![p])?,
                (None,Some(a)) => stmt.query(params![if a {1}else{0}])?,
                (None,None) => stmt.query([])?,
            };
            let mut out = Vec::new();
            while let Some(r) = rows.next()? { out.push(connection_row(r)?); }
            Ok(out)
        })
    }

    pub fn provider_connection(&self, id: &str) -> Result<Option<Value>, AppError> {
        self.with_conn(|db| {
            let mut stmt = db.prepare("SELECT id,provider,authType,name,email,priority,isActive,data,createdAt,updatedAt FROM providerConnections WHERE id=?1")?;
            let v = stmt.query_row(params![id], |r| connection_row_sql(r)).optional()?;
            Ok(v)
        })
    }

    pub fn create_connection(&self, mut v: Value) -> Result<Value, AppError> {
        let o = v
            .as_object_mut()
            .ok_or_else(|| AppError::BadRequest("provider body must be object".into()))?;
        let provider = o
            .get("provider")
            .and_then(Value::as_str)
            .ok_or_else(|| AppError::BadRequest("provider is required".into()))?
            .to_string();
        let auth_type = o
            .get("authType")
            .and_then(Value::as_str)
            .unwrap_or("apikey")
            .to_string();
        let name = o
            .get("name")
            .or_else(|| o.get("displayName"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let email = o.get("email").and_then(Value::as_str).map(str::to_string);
        let priority = o.get("priority").and_then(Value::as_i64).unwrap_or(0);
        let is_active = o.get("isActive").and_then(Value::as_bool).unwrap_or(true);
        let now = Utc::now().to_rfc3339();
        let id = o
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        o.insert("id".into(), Value::String(id.clone()));
        o.insert("provider".into(), Value::String(provider.clone()));
        o.insert("authType".into(), Value::String(auth_type.clone()));
        o.insert("priority".into(), json!(priority));
        o.insert("isActive".into(), json!(is_active));
        o.insert("createdAt".into(), Value::String(now.clone()));
        o.insert("updatedAt".into(), Value::String(now.clone()));
        let extra = strip_fields(
            &v,
            &[
                "id",
                "provider",
                "authType",
                "name",
                "email",
                "priority",
                "isActive",
                "createdAt",
                "updatedAt",
            ],
        );
        let extra_json = serde_json::to_string(&extra)?;
        self.with_conn(|db| {
            db.execute("INSERT INTO providerConnections(id,provider,authType,name,email,priority,isActive,data,createdAt,updatedAt) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                params![id, provider, auth_type, name, email, priority, if is_active{1}else{0}, extra_json, now, now])?;
            Ok(())
        })?;
        Ok(v)
    }

    pub fn update_connection(&self, id: &str, patch: Value) -> Result<Value, AppError> {
        let current = self
            .provider_connection(id)?
            .ok_or_else(|| AppError::NotFound("provider connection".into()))?;
        let mut merged = current.as_object().cloned().unwrap_or_default();
        let p = patch
            .as_object()
            .ok_or_else(|| AppError::BadRequest("body must be object".into()))?;
        for (k, v) in p {
            merged.insert(k.clone(), v.clone());
        }
        merged.insert("updatedAt".into(), Value::String(Utc::now().to_rfc3339()));
        let v = Value::Object(merged);
        let o = v.as_object().unwrap();
        let provider = o
            .get("provider")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let auth_type = o
            .get("authType")
            .and_then(Value::as_str)
            .unwrap_or("apikey");
        let name = o.get("name").and_then(Value::as_str);
        let email = o.get("email").and_then(Value::as_str);
        let priority = o.get("priority").and_then(Value::as_i64).unwrap_or(999);
        let active = o.get("isActive").and_then(Value::as_bool).unwrap_or(true);
        let created = o
            .get("createdAt")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let updated = o
            .get("updatedAt")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let extra = serde_json::to_string(&strip_fields(
            &v,
            &[
                "id",
                "provider",
                "authType",
                "name",
                "email",
                "priority",
                "isActive",
                "createdAt",
                "updatedAt",
            ],
        ))?;
        self.with_conn(|db| {
            db.execute("UPDATE providerConnections SET provider=?2,authType=?3,name=?4,email=?5,priority=?6,isActive=?7,data=?8,createdAt=?9,updatedAt=?10 WHERE id=?1",
                params![id,provider,auth_type,name,email,priority,if active{1}else{0},extra,created,updated])?;
            Ok(())
        })?;
        Ok(v)
    }

    pub fn delete_connection(&self, id: &str) -> Result<bool, AppError> {
        self.with_conn(|db| {
            Ok(db.execute("DELETE FROM providerConnections WHERE id=?1", params![id])? > 0)
        })
    }

    pub fn api_keys(&self) -> Result<Vec<Value>, AppError> {
        self.with_conn(|db| {
            let mut stmt = db.prepare("SELECT id,key,name,machineId,isActive,createdAt FROM apiKeys ORDER BY createdAt DESC")?;
            let rows = stmt.query_map([], |r| Ok(json!({"id":r.get::<_,String>(0)?,"key":r.get::<_,String>(1)?,"name":r.get::<_,Option<String>>(2)?,"machineId":r.get::<_,Option<String>>(3)?,"isActive":r.get::<_,i64>(4)?!=0,"createdAt":r.get::<_,String>(5)?})))?;
            Ok(rows.collect::<Result<Vec<_>,_>>()?)
        })
    }

    pub fn validate_api_key(&self, key: &str) -> Result<bool, AppError> {
        self.with_conn(|db| {
            let n: i64 = db.query_row(
                "SELECT COUNT(*) FROM apiKeys WHERE key=?1 AND isActive=1",
                params![key],
                |r| r.get(0),
            )?;
            Ok(n > 0)
        })
    }

    pub fn create_api_key(
        &self,
        name: Option<&str>,
        machine_id: Option<&str>,
    ) -> Result<Value, AppError> {
        let id = Uuid::new_v4().to_string();
        let key = format!("9r-{}", Uuid::new_v4().simple());
        let now = Utc::now().to_rfc3339();
        self.with_conn(|db| { db.execute("INSERT INTO apiKeys(id,key,name,machineId,isActive,createdAt) VALUES(?1,?2,?3,?4,1,?5)",params![id,key,name,machine_id,now])?; Ok(()) })?;
        Ok(
            json!({"id":id,"key":key,"name":name,"machineId":machine_id,"isActive":true,"createdAt":now}),
        )
    }

    pub fn delete_api_key(&self, id: &str) -> Result<bool, AppError> {
        self.with_conn(|db| Ok(db.execute("DELETE FROM apiKeys WHERE id=?1", params![id])? > 0))
    }

    pub fn combos(&self) -> Result<Vec<Value>, AppError> {
        self.with_conn(|db| {
            let mut stmt = db.prepare("SELECT id,name,kind,models,createdAt,updatedAt FROM combos ORDER BY createdAt")?;
            let rows=stmt.query_map([],|r| {
                let models:String=r.get(3)?; let models:Value=serde_json::from_str(&models).unwrap_or_else(|_|json!([]));
                Ok(json!({"id":r.get::<_,String>(0)?,"name":r.get::<_,String>(1)?,"kind":r.get::<_,Option<String>>(2)?,"models":models,"createdAt":r.get::<_,String>(4)?,"updatedAt":r.get::<_,String>(5)?}))
            })?;
            Ok(rows.collect::<Result<Vec<_>,_>>()?)
        })
    }

    pub fn combo_by_name(&self, name: &str) -> Result<Option<Value>, AppError> {
        Ok(self
            .combos()?
            .into_iter()
            .find(|v| v.get("name").and_then(Value::as_str) == Some(name)))
    }

    pub fn upsert_combo(&self, body: Value) -> Result<Value, AppError> {
        let o = body
            .as_object()
            .ok_or_else(|| AppError::BadRequest("combo must be object".into()))?;
        let name = o
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| AppError::BadRequest("name required".into()))?;
        let models = o.get("models").cloned().unwrap_or_else(|| json!([]));
        let kind = o.get("kind").and_then(Value::as_str).unwrap_or("fallback");
        let id = o
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        let now = Utc::now().to_rfc3339();
        let models_s = serde_json::to_string(&models)?;
        self.with_conn(|db|{ db.execute("INSERT INTO combos(id,name,kind,models,createdAt,updatedAt) VALUES(?1,?2,?3,?4,?5,?5) ON CONFLICT(name) DO UPDATE SET kind=excluded.kind,models=excluded.models,updatedAt=excluded.updatedAt",params![id,name,kind,models_s,now])?; Ok(())})?;
        Ok(json!({"id":id,"name":name,"kind":kind,"models":models,"createdAt":now,"updatedAt":now}))
    }

    pub fn provider_node(&self, id: &str) -> Result<Option<Value>, AppError> {
        Ok(self
            .list_json_table("providerNodes")?
            .into_iter()
            .find(|v| v.get("id").and_then(Value::as_str) == Some(id)))
    }
    pub fn create_provider_node(&self, mut body: Value) -> Result<Value, AppError> {
        let o = body
            .as_object_mut()
            .ok_or_else(|| AppError::BadRequest("provider node body must be object".into()))?;
        let now = Utc::now().to_rfc3339();
        let id = o
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        let ty = o.get("type").and_then(Value::as_str).map(str::to_string);
        let name = o.get("name").and_then(Value::as_str).map(str::to_string);
        o.insert("id".into(), json!(id.clone()));
        o.insert("createdAt".into(), json!(now.clone()));
        o.insert("updatedAt".into(), json!(now.clone()));
        let data = serde_json::to_string(&strip_fields(
            &body,
            &["id", "type", "name", "createdAt", "updatedAt"],
        ))?;
        self.with_conn(|db|{db.execute("INSERT INTO providerNodes(id,type,name,data,createdAt,updatedAt) VALUES(?1,?2,?3,?4,?5,?5)",params![id,ty,name,data,now])?;Ok(())})?;
        Ok(body)
    }
    pub fn update_provider_node(&self, id: &str, patch: Value) -> Result<Value, AppError> {
        let current = self
            .provider_node(id)?
            .ok_or_else(|| AppError::NotFound("Provider node not found".into()))?;
        let mut o = current.as_object().cloned().unwrap_or_default();
        for (k, v) in patch
            .as_object()
            .ok_or_else(|| AppError::BadRequest("provider node body must be object".into()))?
        {
            o.insert(k.clone(), v.clone());
        }
        o.insert("updatedAt".into(), json!(Utc::now().to_rfc3339()));
        let v = Value::Object(o);
        let ty = v.get("type").and_then(Value::as_str);
        let name = v.get("name").and_then(Value::as_str);
        let updated = v.get("updatedAt").and_then(Value::as_str).unwrap_or("");
        let data = serde_json::to_string(&strip_fields(
            &v,
            &["id", "type", "name", "createdAt", "updatedAt"],
        ))?;
        self.with_conn(|db| {
            db.execute(
                "UPDATE providerNodes SET type=?2,name=?3,data=?4,updatedAt=?5 WHERE id=?1",
                params![id, ty, name, data, updated],
            )?;
            Ok(())
        })?;
        Ok(v)
    }
    pub fn delete_provider_node(&self, id: &str) -> Result<bool, AppError> {
        self.with_conn(|db| {
            Ok(db.execute("DELETE FROM providerNodes WHERE id=?1", params![id])? > 0)
        })
    }
    pub fn delete_connections_by_provider(&self, provider: &str) -> Result<usize, AppError> {
        self.with_conn(|db| {
            Ok(db.execute(
                "DELETE FROM providerConnections WHERE provider=?1",
                params![provider],
            )?)
        })
    }

    pub fn proxy_pool(&self, id: &str) -> Result<Option<Value>, AppError> {
        Ok(self
            .list_json_table("proxyPools")?
            .into_iter()
            .find(|v| v.get("id").and_then(Value::as_str) == Some(id)))
    }
    pub fn create_proxy_pool(&self, mut body: Value) -> Result<Value, AppError> {
        let o = body
            .as_object_mut()
            .ok_or_else(|| AppError::BadRequest("proxy pool body must be object".into()))?;
        let now = Utc::now().to_rfc3339();
        let id = o
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        let active = o.get("isActive").and_then(Value::as_bool).unwrap_or(true);
        let status = o
            .get("testStatus")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        o.insert("id".into(), json!(id.clone()));
        o.insert("isActive".into(), json!(active));
        o.insert("testStatus".into(), json!(status.clone()));
        o.insert("createdAt".into(), json!(now.clone()));
        o.insert("updatedAt".into(), json!(now.clone()));
        let data = serde_json::to_string(&strip_fields(
            &body,
            &["id", "isActive", "testStatus", "createdAt", "updatedAt"],
        ))?;
        self.with_conn(|db|{db.execute("INSERT INTO proxyPools(id,isActive,testStatus,data,createdAt,updatedAt) VALUES(?1,?2,?3,?4,?5,?5)",params![id,if active{1}else{0},status,data,now])?;Ok(())})?;
        Ok(body)
    }
    pub fn update_proxy_pool(&self, id: &str, patch: Value) -> Result<Value, AppError> {
        let current = self
            .proxy_pool(id)?
            .ok_or_else(|| AppError::NotFound("Proxy pool not found".into()))?;
        let mut o = current.as_object().cloned().unwrap_or_default();
        for (k, v) in patch
            .as_object()
            .ok_or_else(|| AppError::BadRequest("proxy pool body must be object".into()))?
        {
            o.insert(k.clone(), v.clone());
        }
        o.insert("updatedAt".into(), json!(Utc::now().to_rfc3339()));
        let v = Value::Object(o);
        let active = v.get("isActive").and_then(Value::as_bool).unwrap_or(true);
        let status = v
            .get("testStatus")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let updated = v.get("updatedAt").and_then(Value::as_str).unwrap_or("");
        let data = serde_json::to_string(&strip_fields(
            &v,
            &["id", "isActive", "testStatus", "createdAt", "updatedAt"],
        ))?;
        self.with_conn(|db| {
            db.execute(
                "UPDATE proxyPools SET isActive=?2,testStatus=?3,data=?4,updatedAt=?5 WHERE id=?1",
                params![id, if active { 1 } else { 0 }, status, data, updated],
            )?;
            Ok(())
        })?;
        Ok(v)
    }
    pub fn delete_proxy_pool(&self, id: &str) -> Result<bool, AppError> {
        self.with_conn(|db| Ok(db.execute("DELETE FROM proxyPools WHERE id=?1", params![id])? > 0))
    }

    pub fn list_json_table(&self, table: &str) -> Result<Vec<Value>, AppError> {
        match table {
            "providerNodes" => self.with_conn(|db| {
                list_data_rows(db, "providerNodes", "id,type,name,data,createdAt,updatedAt")
            }),
            "proxyPools" => self.with_conn(|db| list_proxy_pool_rows(db)),
            _ => Err(AppError::BadRequest("unsupported table".into())),
        }
    }

    pub fn usage_record(
        &self,
        provider: Option<&str>,
        model: Option<&str>,
        connection_id: Option<&str>,
        endpoint: &str,
        prompt: i64,
        completion: i64,
        status: &str,
        meta: &Value,
    ) -> Result<(), AppError> {
        let ts = Utc::now().to_rfc3339();
        let meta = serde_json::to_string(meta)?;
        let tokens = serde_json::to_string(
            &json!({"prompt_tokens":prompt,"completion_tokens":completion,"total_tokens":prompt+completion}),
        )?;
        self.with_conn(|db|{db.execute("INSERT INTO usageHistory(timestamp,provider,model,connectionId,endpoint,promptTokens,completionTokens,cost,status,tokens,meta) VALUES(?1,?2,?3,?4,?5,?6,?7,0,?8,?9,?10)",params![ts,provider,model,connection_id,endpoint,prompt,completion,status,tokens,meta])?;Ok(())})
    }

    pub fn usage_stats(&self, period: &str) -> Result<Value, AppError> {
        let cutoff = usage_cutoff(period)?;
        self.with_conn(|db|{
            let sql=if cutoff.is_some(){"SELECT timestamp,provider,model,connectionId,apiKey,endpoint,promptTokens,completionTokens,cost,status,tokens FROM usageHistory WHERE timestamp>=?1 ORDER BY id ASC"}else{"SELECT timestamp,provider,model,connectionId,apiKey,endpoint,promptTokens,completionTokens,cost,status,tokens FROM usageHistory ORDER BY id ASC"};
            let mut stmt=db.prepare(sql)?;
            let mut rows=if let Some(c)=cutoff.as_deref(){stmt.query(params![c])?}else{stmt.query([])?};
            let mut total_requests=0i64;let mut total_prompt=0i64;let mut total_completion=0i64;let mut total_cached=0i64;let mut total_cost=0f64;
            let mut by_provider=Map::new();let mut by_model=Map::new();let mut by_account=Map::new();let mut by_api_key=Map::new();let mut by_endpoint=Map::new();
            let mut recent=Vec::new();let now=Utc::now();let ten_ago=now-chrono::Duration::minutes(10);let mut buckets=vec![json!({"requests":0,"promptTokens":0,"completionTokens":0,"cost":0.0});10];
            while let Some(r)=rows.next()?{
                let ts:String=r.get(0)?;let provider:Option<String>=r.get(1)?;let model:Option<String>=r.get(2)?;let conn:Option<String>=r.get(3)?;let api_key:Option<String>=r.get(4)?;let endpoint:Option<String>=r.get(5)?;let prompt:i64=r.get::<_,Option<i64>>(6)?.unwrap_or(0);let completion:i64=r.get::<_,Option<i64>>(7)?.unwrap_or(0);let cost:f64=r.get::<_,Option<f64>>(8)?.unwrap_or(0.0);let status:Option<String>=r.get(9)?;let tokens_s:Option<String>=r.get(10)?;
                let tokens:Value=tokens_s.as_deref().and_then(|x|serde_json::from_str(x).ok()).unwrap_or_else(||json!({"prompt_tokens":prompt,"completion_tokens":completion,"total_tokens":prompt+completion}));
                let cached=tokens.get("cached_tokens").or_else(||tokens.get("cache_read_input_tokens")).and_then(Value::as_i64).unwrap_or(0);
                total_requests+=1;total_prompt+=prompt;total_completion+=completion;total_cached+=cached;total_cost+=cost;
                if let Some(p)=provider.as_deref(){counter_add(&mut by_provider,p,prompt,completion,cached,cost,None);}
                let model_name=model.as_deref().unwrap_or("");let prov=provider.as_deref().unwrap_or("");let mk=if prov.is_empty(){model_name.to_string()}else{format!("{model_name}|{prov}")};counter_add(&mut by_model,&mk,prompt,completion,cached,cost,Some(json!({"rawModel":model_name,"provider":prov})));
                if let Some(c)=conn.as_deref(){counter_add(&mut by_account,c,prompt,completion,cached,cost,Some(json!({"rawModel":model_name,"provider":prov})));}
                let ak=api_key.as_deref().unwrap_or("local-no-key");let akk=format!("{ak}|{model_name}|{}",if prov.is_empty(){"unknown"}else{prov});counter_add(&mut by_api_key,&akk,prompt,completion,cached,cost,Some(json!({"rawModel":model_name,"provider":prov,"apiKey":api_key.as_deref()})));
                let ep=endpoint.as_deref().unwrap_or("Unknown");let epk=format!("{ep}|{model_name}|{}",if prov.is_empty(){"unknown"}else{prov});counter_add(&mut by_endpoint,&epk,prompt,completion,cached,cost,Some(json!({"endpoint":ep,"rawModel":model_name,"provider":prov})));
                recent.push(json!({"timestamp":ts,"model":model_name,"provider":prov,"promptTokens":prompt,"completionTokens":completion,"status":status.unwrap_or_else(||"ok".into()),"cost":cost,"apiKeyMasked":mask_key(api_key.as_deref())}));if recent.len()>50{recent.remove(0);}
                if let Ok(dt)=chrono::DateTime::parse_from_rfc3339(&ts){let dt=dt.with_timezone(&Utc);if dt>=ten_ago&&dt<=now{let mins=(now-dt).num_minutes().clamp(0,9);let idx=(9-mins) as usize;if let Some(o)=buckets[idx].as_object_mut(){let requests=o.get("requests").and_then(Value::as_i64).unwrap_or(0)+1;let pt=o.get("promptTokens").and_then(Value::as_i64).unwrap_or(0)+prompt;let ct=o.get("completionTokens").and_then(Value::as_i64).unwrap_or(0)+completion;let co=o.get("cost").and_then(Value::as_f64).unwrap_or(0.0)+cost;o.insert("requests".into(),json!(requests));o.insert("promptTokens".into(),json!(pt));o.insert("completionTokens".into(),json!(ct));o.insert("cost".into(),json!(co));}}}
            }
            recent.reverse();recent.truncate(20);
            Ok(json!({"totalRequests":total_requests,"totalPromptTokens":total_prompt,"totalCompletionTokens":total_completion,"totalCachedTokens":total_cached,"totalCost":total_cost,"byProvider":by_provider,"byModel":by_model,"byAccount":by_account,"byApiKey":by_api_key,"byEndpoint":by_endpoint,"last10Minutes":buckets,"pending":{"byModel":{},"byAccount":{}},"activeRequests":[],"recentRequests":recent,"errorProvider":""}))
        })
    }

    pub fn kv_get(&self, scope: &str, key: &str) -> Result<Option<Value>, AppError> {
        self.with_conn(|db| {
            let s: Option<String> = db
                .query_row(
                    "SELECT value FROM kv WHERE scope=?1 AND key=?2",
                    params![scope, key],
                    |r| r.get(0),
                )
                .optional()?;
            Ok(s.and_then(|s| serde_json::from_str(&s).ok()))
        })
    }
    pub fn kv_set(&self, scope: &str, key: &str, value: &Value) -> Result<(), AppError> {
        let s = serde_json::to_string(value)?;
        self.with_conn(|db|{db.execute("INSERT INTO kv(scope,key,value) VALUES(?1,?2,?3) ON CONFLICT(scope,key) DO UPDATE SET value=excluded.value",params![scope,key,s])?;Ok(())})
    }
    pub fn kv_all(&self, scope: &str) -> Result<serde_json::Map<String, Value>, AppError> {
        self.with_conn(|db| {
            let mut stmt = db.prepare("SELECT key,value FROM kv WHERE scope=?1")?;
            let rows = stmt.query_map(params![scope], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?;
            let mut out = serde_json::Map::new();
            for row in rows {
                let (k, v) = row?;
                out.insert(k, serde_json::from_str(&v).unwrap_or(Value::String(v)));
            }
            Ok(out)
        })
    }
    pub fn kv_delete(&self, scope: &str, key: &str) -> Result<bool, AppError> {
        self.with_conn(|db| {
            Ok(db.execute(
                "DELETE FROM kv WHERE scope=?1 AND key=?2",
                params![scope, key],
            )? > 0)
        })
    }
    pub fn delete_combo(&self, id_or_name: &str) -> Result<bool, AppError> {
        self.with_conn(|db| {
            Ok(db.execute(
                "DELETE FROM combos WHERE id=?1 OR name=?1",
                params![id_or_name],
            )? > 0)
        })
    }

    pub fn export_db(&self) -> Result<Value, AppError> {
        let settings = self.with_conn(|db| {
            let raw: Option<String> = db
                .query_row("SELECT data FROM settings WHERE id=1", [], |r| r.get(0))
                .optional()?;
            Ok(raw
                .and_then(|value| serde_json::from_str::<Value>(&value).ok())
                .unwrap_or_else(|| json!({})))
        })?;
        let provider_connections = self.provider_connections(None, None)?;
        let provider_nodes = self.list_json_table("providerNodes")?;
        let proxy_pools = self.list_json_table("proxyPools")?;
        let api_keys = self.api_keys()?;
        let combos = self.combos()?;
        let model_aliases = Value::Object(self.kv_all("modelAliases")?);
        let custom_models = Value::Array(self.kv_all("customModels")?.into_values().collect());
        let mitm_alias = Value::Object(self.kv_all("mitmAlias")?);
        let pricing = Value::Object(self.kv_all("pricing")?);
        Ok(json!({
            "settings": settings,
            "providerConnections": provider_connections,
            "providerNodes": provider_nodes,
            "proxyPools": proxy_pools,
            "apiKeys": api_keys,
            "combos": combos,
            "modelAliases": model_aliases,
            "customModels": custom_models,
            "mitmAlias": mitm_alias,
            "pricing": pricing
        }))
    }

    pub fn import_db(&self, payload: &Value) -> Result<Value, AppError> {
        let root = payload
            .as_object()
            .ok_or_else(|| AppError::BadRequest("Invalid database payload".into()))?;
        self.with_conn(|db| {
            let tx = db.transaction()?;
            tx.execute("DELETE FROM settings", [])?;
            tx.execute("DELETE FROM providerConnections", [])?;
            tx.execute("DELETE FROM providerNodes", [])?;
            tx.execute("DELETE FROM proxyPools", [])?;
            tx.execute("DELETE FROM apiKeys", [])?;
            tx.execute("DELETE FROM combos", [])?;
            tx.execute("DELETE FROM kv WHERE scope IN ('modelAliases','customModels','mitmAlias','pricing')", [])?;

            if let Some(settings) = root.get("settings") {
                let data = serde_json::to_string(settings)?;
                tx.execute(
                    "INSERT INTO settings(id,data) VALUES(1,?1) ON CONFLICT(id) DO UPDATE SET data=excluded.data",
                    params![data],
                )?;
            }

            for item in root
                .get("providerConnections")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let o = item.as_object().ok_or_else(|| AppError::BadRequest("Invalid provider connection in database payload".into()))?;
                let id = required_string(o, "id")?;
                let provider = required_string(o, "provider")?;
                let auth_type = o.get("authType").and_then(Value::as_str).unwrap_or("oauth").to_string();
                let name = o.get("name").and_then(Value::as_str).map(str::to_string);
                let email = o.get("email").and_then(Value::as_str).map(str::to_string);
                let priority = o.get("priority").and_then(Value::as_i64);
                let active = o.get("isActive").and_then(Value::as_bool).unwrap_or(true);
                let now = Utc::now().to_rfc3339();
                let created = o.get("createdAt").and_then(Value::as_str).unwrap_or(&now).to_string();
                let updated = o.get("updatedAt").and_then(Value::as_str).unwrap_or(&now).to_string();
                let data = serde_json::to_string(&strip_fields(item, &["id","provider","authType","name","email","priority","isActive","createdAt","updatedAt"]))?;
                tx.execute(
                    "INSERT OR REPLACE INTO providerConnections(id,provider,authType,name,email,priority,isActive,data,createdAt,updatedAt) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                    params![id,provider,auth_type,name,email,priority,if active {1} else {0},data,created,updated],
                )?;
            }

            for item in root.get("providerNodes").and_then(Value::as_array).into_iter().flatten() {
                let o = item.as_object().ok_or_else(|| AppError::BadRequest("Invalid provider node in database payload".into()))?;
                let id = required_string(o, "id")?;
                let ty = o.get("type").and_then(Value::as_str).map(str::to_string);
                let name = o.get("name").and_then(Value::as_str).map(str::to_string);
                let now = Utc::now().to_rfc3339();
                let created = o.get("createdAt").and_then(Value::as_str).unwrap_or(&now).to_string();
                let updated = o.get("updatedAt").and_then(Value::as_str).unwrap_or(&now).to_string();
                let data = serde_json::to_string(&strip_fields(item, &["id","type","name","createdAt","updatedAt"]))?;
                tx.execute(
                    "INSERT OR REPLACE INTO providerNodes(id,type,name,data,createdAt,updatedAt) VALUES(?1,?2,?3,?4,?5,?6)",
                    params![id,ty,name,data,created,updated],
                )?;
            }

            for item in root.get("proxyPools").and_then(Value::as_array).into_iter().flatten() {
                let o = item.as_object().ok_or_else(|| AppError::BadRequest("Invalid proxy pool in database payload".into()))?;
                let id = required_string(o, "id")?;
                let active = o.get("isActive").and_then(Value::as_bool).unwrap_or(true);
                let test_status = o.get("testStatus").and_then(Value::as_str).unwrap_or("unknown").to_string();
                let now = Utc::now().to_rfc3339();
                let created = o.get("createdAt").and_then(Value::as_str).unwrap_or(&now).to_string();
                let updated = o.get("updatedAt").and_then(Value::as_str).unwrap_or(&now).to_string();
                let data = serde_json::to_string(&strip_fields(item, &["id","isActive","testStatus","createdAt","updatedAt"]))?;
                tx.execute(
                    "INSERT OR REPLACE INTO proxyPools(id,isActive,testStatus,data,createdAt,updatedAt) VALUES(?1,?2,?3,?4,?5,?6)",
                    params![id,if active {1} else {0},test_status,data,created,updated],
                )?;
            }

            for item in root.get("apiKeys").and_then(Value::as_array).into_iter().flatten() {
                let o = item.as_object().ok_or_else(|| AppError::BadRequest("Invalid API key in database payload".into()))?;
                let id = required_string(o, "id")?;
                let key = required_string(o, "key")?;
                let name = o.get("name").and_then(Value::as_str).map(str::to_string);
                let machine_id = o.get("machineId").and_then(Value::as_str).map(str::to_string);
                let active = o.get("isActive").and_then(Value::as_bool).unwrap_or(true);
                let created = o.get("createdAt").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| Utc::now().to_rfc3339());
                tx.execute(
                    "INSERT OR REPLACE INTO apiKeys(id,key,name,machineId,isActive,createdAt) VALUES(?1,?2,?3,?4,?5,?6)",
                    params![id,key,name,machine_id,if active {1} else {0},created],
                )?;
            }

            for item in root.get("combos").and_then(Value::as_array).into_iter().flatten() {
                let o = item.as_object().ok_or_else(|| AppError::BadRequest("Invalid combo in database payload".into()))?;
                let id = required_string(o, "id")?;
                let name = required_string(o, "name")?;
                let kind = o.get("kind").and_then(Value::as_str).map(str::to_string);
                let models_value = o.get("models").cloned().unwrap_or_else(|| Value::Array(vec![]));
                let models = serde_json::to_string(&models_value)?;
                let now = Utc::now().to_rfc3339();
                let created = o.get("createdAt").and_then(Value::as_str).unwrap_or(&now).to_string();
                let updated = o.get("updatedAt").and_then(Value::as_str).unwrap_or(&now).to_string();
                tx.execute(
                    "INSERT OR REPLACE INTO combos(id,name,kind,models,createdAt,updatedAt) VALUES(?1,?2,?3,?4,?5,?6)",
                    params![id,name,kind,models,created,updated],
                )?;
            }

            if let Some(aliases) = root.get("modelAliases").and_then(Value::as_object) {
                for (alias, model) in aliases {
                    tx.execute(
                        "INSERT OR REPLACE INTO kv(scope,key,value) VALUES('modelAliases',?1,?2)",
                        params![alias,serde_json::to_string(model)?],
                    )?;
                }
            }
            for model in root.get("customModels").and_then(Value::as_array).into_iter().flatten() {
                let o = model.as_object().ok_or_else(|| AppError::BadRequest("Invalid custom model in database payload".into()))?;
                let provider = required_string(o, "providerAlias")?;
                let id = required_string(o, "id")?;
                let ty = o.get("type").and_then(Value::as_str).unwrap_or("llm");
                let key = format!("{provider}|{id}|{ty}");
                tx.execute(
                    "INSERT OR REPLACE INTO kv(scope,key,value) VALUES('customModels',?1,?2)",
                    params![key,serde_json::to_string(model)?],
                )?;
            }
            for (scope, key) in [("mitmAlias", "mitmAlias"), ("pricing", "pricing")] {
                if let Some(values) = root.get(key).and_then(Value::as_object) {
                    for (name, value) in values {
                        tx.execute(
                            "INSERT OR REPLACE INTO kv(scope,key,value) VALUES(?1,?2,?3)",
                            params![scope,name,serde_json::to_string(value)?],
                        )?;
                    }
                }
            }
            tx.commit()?;
            Ok(())
        })?;
        self.export_db()
    }
}

fn required_string(o: &Map<String, Value>, key: &str) -> Result<String, AppError> {
    o.get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| AppError::BadRequest(format!("database payload missing {key}")))
}

fn connection_row_sql(r: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    let extra_s: String = r.get(7)?;
    let mut o = serde_json::from_str::<Value>(&extra_s)
        .ok()
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    o.insert("id".into(), json!(r.get::<_, String>(0)?));
    o.insert("provider".into(), json!(r.get::<_, String>(1)?));
    o.insert("authType".into(), json!(r.get::<_, String>(2)?));
    o.insert("name".into(), json!(r.get::<_, Option<String>>(3)?));
    o.insert("email".into(), json!(r.get::<_, Option<String>>(4)?));
    o.insert("priority".into(), json!(r.get::<_, Option<i64>>(5)?));
    o.insert("isActive".into(), json!(r.get::<_, i64>(6)? != 0));
    o.insert("createdAt".into(), json!(r.get::<_, String>(8)?));
    o.insert("updatedAt".into(), json!(r.get::<_, String>(9)?));
    Ok(Value::Object(o))
}
fn connection_row(r: &rusqlite::Row<'_>) -> Result<Value, AppError> {
    Ok(connection_row_sql(r)?)
}
fn strip_fields(v: &Value, fields: &[&str]) -> Value {
    let mut o = v.as_object().cloned().unwrap_or_default();
    for k in fields {
        o.remove(*k);
    }
    Value::Object(o)
}
fn list_data_rows(db: &mut Connection, table: &str, cols: &str) -> Result<Vec<Value>, AppError> {
    let sql = format!("SELECT {cols} FROM {table} ORDER BY createdAt");
    let mut stmt = db.prepare(&sql)?;
    let rows = stmt.query_map([], |r| {
        let data_s: String = r.get(3)?;
        let mut o = serde_json::from_str::<Value>(&data_s)
            .ok()
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default();
        o.insert("id".into(), json!(r.get::<_, String>(0)?));
        o.insert("type".into(), json!(r.get::<_, Option<String>>(1)?));
        o.insert("name".into(), json!(r.get::<_, Option<String>>(2)?));
        o.insert("createdAt".into(), json!(r.get::<_, String>(4)?));
        o.insert("updatedAt".into(), json!(r.get::<_, String>(5)?));
        Ok(Value::Object(o))
    })?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

fn list_proxy_pool_rows(db: &mut Connection) -> Result<Vec<Value>, AppError> {
    let mut stmt = db.prepare(
        "SELECT id,isActive,testStatus,data,createdAt,updatedAt FROM proxyPools ORDER BY createdAt",
    )?;
    let rows = stmt.query_map([], |r| {
        let data_s: String = r.get(3)?;
        let mut o = serde_json::from_str::<Value>(&data_s)
            .ok()
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default();
        o.insert("id".into(), json!(r.get::<_, String>(0)?));
        o.insert("isActive".into(), json!(r.get::<_, i64>(1)? != 0));
        o.insert("testStatus".into(), json!(r.get::<_, Option<String>>(2)?));
        o.insert("createdAt".into(), json!(r.get::<_, String>(4)?));
        o.insert("updatedAt".into(), json!(r.get::<_, String>(5)?));
        Ok(Value::Object(o))
    })?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

fn counter_add(
    map: &mut Map<String, Value>,
    key: &str,
    prompt: i64,
    completion: i64,
    cached: i64,
    cost: f64,
    meta: Option<Value>,
) {
    let entry = map.entry(key.to_string()).or_insert_with(
        || json!({"requests":0,"promptTokens":0,"completionTokens":0,"cachedTokens":0,"cost":0.0}),
    );
    if let Some(o) = entry.as_object_mut() {
        let requests = o.get("requests").and_then(Value::as_i64).unwrap_or(0) + 1;
        let pt = o.get("promptTokens").and_then(Value::as_i64).unwrap_or(0) + prompt;
        let ct = o
            .get("completionTokens")
            .and_then(Value::as_i64)
            .unwrap_or(0)
            + completion;
        let cached_total = o.get("cachedTokens").and_then(Value::as_i64).unwrap_or(0) + cached;
        let total_cost = o.get("cost").and_then(Value::as_f64).unwrap_or(0.0) + cost;
        o.insert("requests".into(), json!(requests));
        o.insert("promptTokens".into(), json!(pt));
        o.insert("completionTokens".into(), json!(ct));
        o.insert("cachedTokens".into(), json!(cached_total));
        o.insert("cost".into(), json!(total_cost));
        if let Some(Value::Object(m)) = meta {
            for (k, v) in m {
                o.insert(k, v);
            }
        }
    }
}
fn mask_key(key: Option<&str>) -> Option<String> {
    key.map(|k| {
        if k.len() <= 8 {
            format!("{}***", k.chars().next().unwrap_or('*'))
        } else {
            format!("{}***", &k[..8])
        }
    })
}
fn usage_cutoff(period: &str) -> Result<Option<String>, AppError> {
    let now = Utc::now();
    let dt = match period {
        "all" => return Ok(None),
        "24h" => now - chrono::Duration::hours(24),
        "7d" => now - chrono::Duration::days(7),
        "30d" => now - chrono::Duration::days(30),
        "60d" => now - chrono::Duration::days(60),
        "today" => {
            let local = chrono::Local::now();
            let start = local.date_naive().and_hms_opt(0, 0, 0).unwrap();
            match start.and_local_timezone(chrono::Local) {
                chrono::LocalResult::Single(v) => v.with_timezone(&Utc),
                chrono::LocalResult::Ambiguous(v, _) => v.with_timezone(&Utc),
                chrono::LocalResult::None => now - chrono::Duration::hours(24),
            }
        }
        _ => return Err(AppError::BadRequest("Invalid period".into())),
    };
    Ok(Some(dt.to_rfc3339()))
}

pub fn merge_settings_defaults(mut raw: Value) -> Value {
    let defaults = json!({
      "cloudEnabled":false,"tunnelEnabled":false,"tunnelUrl":"","tunnelProvider":"cloudflare","tailscaleEnabled":false,"tailscaleUrl":"",
      "stickyRoundRobinLimit":3,"providerStrategies":{},"quotaVisibility":{},"comboStrategy":"fallback","comboStickyRoundRobinLimit":1,"comboStrategies":{},
      "capacityAdapter":{"vision":{"enabled":true,"roundRobin":false,"models":[]},"pdf":{"enabled":false,"roundRobin":false,"models":[]},"audioInput":{"enabled":true,"roundRobin":false,"models":[]},"videoInput":{"enabled":false,"roundRobin":false,"models":[]}},
      "requireLogin":true,"requireApiKey":true,"tunnelDashboardAccess":true,"authMode":"password","ssoType":"oidc","oidcIssuerUrl":"","oidcClientId":"","oidcClientSecret":"","oidcScopes":"openid profile email","oidcLoginLabel":"Sign in with OIDC",
      "samlEntryPoint":"","samlIssuer":"urn:9router:sp","samlCert":"","samlLoginLabel":"Sign in with SAML SSO","samlAttributeEmail":"email","samlAttributeName":"name",
      "enableObservability":false,"observabilityMaxRecords":1000,"observabilityBatchSize":20,"observabilityFlushIntervalMs":5000,"observabilityMaxJsonSize":5,
      "outboundProxyEnabled":false,"outboundProxyUrl":"","outboundNoProxy":"","mitmRouterBaseUrl":"http://localhost:20128","dnsToolEnabled":{},"rtkEnabled":true,
      "headroomEnabled":false,"headroomUrl":"http://localhost:8787","headroomCompressUserMessages":false,"headroomTimeoutMs":3000,
      "cavemanEnabled":false,"cavemanLevel":"full","ponytailEnabled":false,"ponytailLevel":"full","pxpipeEnabled":false,"pxpipeAutoInstall":true,"pxpipeMinChars":25000,"pxpipeTimeoutMs":15000
    });
    let mut d = defaults.as_object().cloned().unwrap_or_default();
    if let Some(o) = raw.as_object_mut() {
        for (k, v) in std::mem::take(o) {
            d.insert(k, v);
        }
    }
    Value::Object(d)
}
