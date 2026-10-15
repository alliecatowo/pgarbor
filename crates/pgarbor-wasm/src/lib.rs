//! WebAssembly build of PGArbor: an in-memory database (MemVfs) behind one `exec(sql) -> JSON` call.
//!
//! Success: `{"ok":true,"results":[{"columns":[{"name","type_oid"}],"rows":[[..]],"tag":".."}]}`.
//! Failure: `{"ok":false,"error":{"sqlstate":"23505","message":"..","detail":..,"hint":..}}`.
//! The database lives for the lifetime of the module instance (wasm is single threaded, so the
//! engine's thread-local handle tables are process-wide here).
use pgarbor::ffi::{self, FLAG_IN_MEMORY, FfiError, Handle};
use std::cell::Cell;
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = console, js_name = error)]
    fn console_error(msg: &str);
}

/// Route Rust panics to `console.error` (runs once when the module is instantiated).
#[wasm_bindgen(start)]
pub fn start() {
    std::panic::set_hook(Box::new(|info| console_error(&info.to_string())));
}

thread_local! {
    static SESSION: Cell<Handle> = const { Cell::new(0) };
}

fn json_str(out: &mut String, s: &str) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

fn opt(out: &mut String, s: &Option<String>) {
    match s {
        Some(s) => json_str(out, s),
        None => out.push_str("null"),
    }
}

fn error_json(e: &FfiError) -> String {
    let mut o = String::from("{\"ok\":false,\"error\":{\"sqlstate\":");
    json_str(&mut o, &e.sqlstate);
    o.push_str(",\"message\":");
    json_str(&mut o, &e.message);
    o.push_str(",\"detail\":");
    opt(&mut o, &e.detail);
    o.push_str(",\"hint\":");
    opt(&mut o, &e.hint);
    o.push_str("}}");
    o
}

fn session() -> Result<Handle, FfiError> {
    let h = SESSION.with(Cell::get);
    if h != 0 {
        return Ok(h);
    }
    let db = ffi::db_open("memory.pga", FLAG_IN_MEMORY)?;
    let s = ffi::session_open(db, "main", "postgres", "postgres")?;
    SESSION.with(|c| c.set(s));
    Ok(s)
}

/// Run one or more SQL statements and return the JSON document described in the module docs.
pub fn exec_to_json(sql: &str) -> String {
    match session().and_then(|s| ffi::exec_json(s, sql)) {
        Ok(results) => format!("{{\"ok\":true,\"results\":{results}}}"),
        Err(e) => error_json(&e),
    }
}

/// Run SQL against the module's in-memory database; always returns a JSON string.
#[wasm_bindgen]
pub fn exec(sql: &str) -> String {
    exec_to_json(sql)
}

/// Engine version reported by PGArbor (PostgreSQL compatibility target).
#[wasm_bindgen]
pub fn version() -> String {
    ffi::version().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_roundtrip() {
        assert!(exec_to_json("SELECT 1").contains("\"ok\":true"));
        exec_to_json("CREATE TABLE t(id int primary key)");
        assert!(exec_to_json("INSERT INTO t VALUES (1)").contains("INSERT 0 1"));
        assert!(exec_to_json("INSERT INTO t VALUES (1)").contains("23505"));
    }
}
