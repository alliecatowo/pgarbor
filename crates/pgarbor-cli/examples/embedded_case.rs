//! Test transport for the differential harness. Runs one case per process, without sockets.
//! This tests embedded SQL semantics; it does not test PostgreSQL wire framing or client behavior.
use std::collections::HashMap;
use std::io::{self, Read};
use pgarbor::{Database, FormatCode, Session, SessionOptions, StatementResult};
use pgarbor_common::{PgError, Result};
use serde_json::{json, Value};

fn error(e: &PgError) -> Value {
    json!({"severity":e.severity.as_str(),"sqlstate":e.sqlstate.code(),"message":e.message,
        "detail":e.detail,"hint":e.hint,"position":e.position.map(|p|p.to_string()),
        "internal_position":null,"internal_query":null,"context":null,"schema_name":e.schema,
        "table_name":e.table,"column_name":e.column,"datatype_name":e.datatype,"constraint_name":e.constraint})
}
fn result(s: &Session, r: &StatementResult) -> Result<Value> {
    let mut v = json!({"kind":"command","columns":[],"rows":[],"command_tag":r.tag(),"error":null,"param_type_oids":null});
    match r {
        StatementResult::Empty => {v["kind"]=json!("empty");v["command_tag"]=Value::Null;},
        StatementResult::Command { .. } => {},
        StatementResult::Rows { columns, rows, .. } => {
            v["kind"]=json!("rows");
            v["columns"]=json!(columns.iter().map(|c|json!({"name":c.name,"type_oid":c.type_oid,"typmod":c.type_mod,"format":0,"table_oid":c.table_oid,"column_attnum":c.column_attnum})).collect::<Vec<_>>());
            let mut encoded = Vec::new();
            for row in rows {
                let mut values = Vec::new();
                if row.len()!=columns.len() {return Err(PgError::internal("row/descriptor width mismatch"));}
                for (c,value) in columns.iter().zip(row) {
                    let bytes=s.encode_value(pgarbor_types_id(c.type_oid,c.type_mod),value,FormatCode::Text)?;
                    values.push(match bytes {None=>Value::Null,Some(b)=>match String::from_utf8(b) {Ok(text)=>json!(text),Err(e)=>json!({"hex":e.into_bytes().iter().map(|b|format!("{b:02x}")).collect::<String>()})}});
                }
                encoded.push(values);
            }
            v["rows"]=json!(encoded);
        }
    }
    Ok(v)
}
// TypeId is re-exported by pgarbor for embedded callers.
fn pgarbor_types_id(oid: u32, typmod: i32) -> pgarbor::TypeId { pgarbor::TypeId { oid, typmod } }
fn observation(s: &mut Session, index: usize, name: &str, kind: &str, sql: &str) -> Result<Value> {
    let out=s.simple_query_raw(sql);
    let mut results=Vec::new();let mut notices=Vec::new();
    for statement in &out.statements {results.push(result(s,&statement.result)?);for n in &statement.notices {notices.push(error(n));}}
    for n in &out.error_notices {notices.push(error(n));}
    if let Some(e)=&out.error {results.push(json!({"kind":"error","columns":[],"rows":[],"command_tag":null,"error":error(e),"param_type_oids":null}));}
    Ok(json!({"index":index,"session":name,"kind":kind,"results":results,"notices":notices,"txn_status":(out.status.byte() as char).to_string(),"blocked_observed":null,"timed_out":false,"connection_lost":false,"harness_error":null,"elapsed_ms":0}))
}
fn run(req: Value) -> Result<Value> {
    let db=Database::in_memory()?;
    let mut options=SessionOptions::default();
    if let Some(gucs)=req["gucs"].as_object() {for (k,v) in gucs {if let Some(v)=v.as_str(){options.startup_gucs.push((k.clone(),v.into()));}}}
    let mut setup=Vec::new();
    if let Some(sql)=req["setup"].as_str().filter(|s|!s.is_empty()) {let mut s=db.session("main",options.clone())?;setup.push(observation(&mut s,0,"setup","sql",sql)?);s.close();}
    let mut sessions=HashMap::<String,Session>::new();let mut steps=Vec::new();
    let input=req["steps"].as_array().ok_or_else(||PgError::internal("missing steps"))?;
    for (index,step) in input.iter().enumerate() {
        let name=step["session"].as_str().ok_or_else(||PgError::internal("missing session"))?;
        if !sessions.contains_key(name) {sessions.insert(name.into(),db.session("main",options.clone())?);}
        let s=sessions.get_mut(name).ok_or_else(||PgError::internal("missing live session"))?;
        let kind=step["kind"].as_str().ok_or_else(||PgError::internal("missing step kind"))?;
        if kind=="reconnect" {let replacement=db.session("main",options.clone())?; let old=std::mem::replace(s,replacement);old.close();steps.push(json!({"index":index,"session":name,"kind":kind,"results":[],"notices":[],"txn_status":"I"}));}
        else if kind=="sql" {steps.push(observation(s,index,name,kind,step["sql"].as_str().ok_or_else(||PgError::internal("missing SQL"))?)?);}
        else {return Err(PgError::unimplemented("CLI-10","embedded bridge step kind"));}
    }
    for (_,s) in sessions {s.close();}
    db.close()?;
    Ok(json!({"case_id":req["id"],"endpoint":"target","setup":setup,"steps":steps}))
}
fn main() {
    let mut input=String::new();
    let response=match io::stdin().take(16*1024*1024).read_to_string(&mut input) {
        Ok(_)=> match serde_json::from_str(&input) {Ok(req)=>match run(req) {Ok(t)=>json!({"transcript":t}),Err(e)=>json!({"bridge_error":error(&e)})},Err(e)=>json!({"bridge_error":e.to_string()})},
        Err(e)=>json!({"bridge_error":e.to_string()}),
    };
    println!("{response}");
}
