//! Interpreter state: variables, scopes, name resolution and assignment (PG `pl_comp.c` namespaces,
//! `exec_assign_value`, `exec_move_row`).
use std::cell::RefCell;
use std::rc::Rc;

use pgarbor_common::{Oid, PgError, Result, SqlState};
use pgarbor_sql::plpgsql::{PlOptions, PlTarget, VarConflict};
use pgarbor_types::{oids, Datum, TypeId};

use crate::context::ExecContext;
use crate::plfunc;

/// One PL/pgSQL variable (scalar, row, record or cursor).
#[derive(Clone, Debug)]
pub struct Var {
    pub name: String,
    /// Declared type (`record` for record variables, a composite OID for row variables).
    pub ty: TypeId,
    pub value: Datum,
    /// Record variables: the field names/types of the current value (`None` = not assigned yet).
    pub rec_fields: Option<Vec<(String, TypeId)>>,
    pub constant: bool,
    pub not_null: bool,
    /// Bound cursor declaration (`c CURSOR (args) FOR query`).
    pub cursor: Option<Rc<CursorDecl>>,
}

#[derive(Clone, Debug)]
pub struct CursorDecl {
    pub scroll: Option<bool>,
    pub args: Vec<(String, TypeId)>,
    pub query: pgarbor_sql::plpgsql::PlSql,
    /// Variables with an index at or above this were declared after the cursor: not visible to its query.
    pub visible_vars: usize,
}

impl Var {
    pub fn is_record(&self) -> bool {
        self.ty.oid == oids::RECORD
    }
}

/// A namespace level (a block, or the function's parameter level labeled with the function name).
#[derive(Clone, Debug, Default)]
pub struct Scope {
    pub label: Option<String>,
    pub names: Vec<(String, usize)>,
}

/// A reference to a variable or one field of a row / record variable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VarRef {
    Var(usize),
    Field(usize, String),
}

/// Where an error/notice is attributed in this frame (`estate->err_stmt` / `err_text`).
#[derive(Clone, Debug)]
pub enum ErrSite {
    None,
    Stmt { line: u32, what: &'static str },
    Text(String),
}

/// Loop / block exit signal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Flow {
    Normal,
    Exit(Option<String>),
    Continue(Option<String>),
    Return,
}

/// The state of one PL/pgSQL call frame.
pub struct Interp {
    /// `f(integer)` / `inline_code_block` for CONTEXT lines.
    pub sig: String,
    pub fname: String,
    pub fn_oid: Oid,
    pub options: PlOptions,
    pub vars: Rc<RefCell<Vec<Var>>>,
    pub scopes: Rc<RefCell<Vec<Scope>>>,
    /// Variables of the input parameters in order (`$1..$n`).
    pub param_slots: Vec<usize>,
    pub out_vars: Vec<usize>,
    pub found_var: usize,
    pub ret_type: TypeId,
    pub retset: bool,
    pub is_procedure: bool,
    pub is_inline: bool,
    /// Transaction control is not allowed (any call that is not a top-level CALL / DO).
    pub atomic: bool,
    pub retval: Option<(Datum, TypeId)>,
    pub returned: bool,
    pub ret_rows: Vec<Datum>,
    pub row_count: u64,
    pub err_site: ErrSite,
    /// Notices before this index already carry this frame's CONTEXT line.
    pub notice_mark: usize,
    /// Errors being handled (innermost last): `GET STACKED DIAGNOSTICS`, bare `RAISE`.
    pub handling: Vec<PgError>,
    /// Cursors opened by this frame without a holder outside (closed at exit when unnamed).
    pub local_portals: Vec<String>,
    /// Counter for `<unnamed portal N>` names.
    pub portal_seq: u32,
}

impl Interp {
    pub fn new(sig: String, fname: String, fn_oid: Oid, options: PlOptions) -> Interp {
        Interp {
            sig,
            fname,
            fn_oid,
            options,
            vars: Rc::new(RefCell::new(Vec::new())),
            scopes: Rc::new(RefCell::new(Vec::new())),
            param_slots: Vec::new(),
            out_vars: Vec::new(),
            found_var: 0,
            ret_type: TypeId::of(oids::VOID),
            retset: false,
            is_procedure: false,
            is_inline: false,
            atomic: true,
            retval: None,
            returned: false,
            ret_rows: Vec::new(),
            row_count: 0,
            err_site: ErrSite::None,
            notice_mark: 0,
            handling: Vec::new(),
            local_portals: Vec::new(),
            portal_seq: 0,
        }
    }

    pub fn conflict(&self) -> VarConflict {
        self.options.variable_conflict.unwrap_or(VarConflict::Error)
    }

    pub fn push_scope(&mut self, label: Option<String>) {
        self.scopes.borrow_mut().push(Scope { label, names: Vec::new() });
    }

    pub fn pop_scope(&mut self) {
        self.scopes.borrow_mut().pop();
    }

    /// Add a new variable to the innermost scope; returns its index.
    pub fn declare_var(&mut self, cx: &ExecContext, name: &str, ty: TypeId, value: Datum) -> Result<usize> {
        let rec_fields = if ty.oid == oids::RECORD { None } else { plfunc::composite_fields(cx, ty.oid) };
        let is_rec = ty.oid == oids::RECORD;
        let var = Var {
            name: name.to_string(),
            ty,
            rec_fields: if is_rec { None } else { rec_fields },
            value,
            constant: false,
            not_null: false,
            cursor: None,
        };
        let idx = {
            let mut vars = self.vars.borrow_mut();
            vars.push(var);
            vars.len() - 1
        };
        self.add_alias(name, idx);
        Ok(idx)
    }

    /// Make `name` refer to variable `idx` in the innermost scope.
    pub fn add_alias(&mut self, name: &str, idx: usize) {
        if let Some(s) = self.scopes.borrow_mut().last_mut() {
            s.names.push((name.to_string(), idx));
        }
    }

    pub fn declare_found(&mut self, cx: &ExecContext) -> Result<()> {
        self.found_var = self.declare_var(cx, "found", TypeId::BOOL, Datum::Bool(false))?;
        Ok(())
    }

    pub fn set_found(&mut self, v: bool) {
        if let Some(var) = self.vars.borrow_mut().get_mut(self.found_var) {
            var.value = Datum::Bool(v);
        }
    }

    pub fn var_value(&self, idx: usize) -> Datum {
        self.vars.borrow().get(idx).map(|v| v.value.clone()).unwrap_or(Datum::Null)
    }

    pub fn var(&self, idx: usize) -> Option<Var> {
        self.vars.borrow().get(idx).cloned()
    }

    /// Innermost variable named `name`.
    pub fn lookup(&self, name: &str) -> Option<usize> {
        lookup_in(&self.scopes.borrow(), name)
    }

    /// Resolve a (possibly qualified) name (PG `plpgsql_ns_lookup` + record fields).
    pub fn resolve(&self, parts: &[String]) -> Option<VarRef> {
        resolve_parts(&self.scopes.borrow(), &self.vars.borrow(), parts)
    }

    /// Close cursors this frame opened that nobody else can reach (unnamed portals stay open while
    /// the refcursor value may be returned; PG closes portals at transaction end only).
    pub fn close_local_cursors(&mut self) {}

    /// Assign `value` (of type `from`) to a target (PG `exec_assign_value`).
    pub fn assign_target(&mut self, cx: &mut ExecContext, target: &PlTarget, value: Datum, from: TypeId) -> Result<()> {
        let r = self.resolve(&target.name).ok_or_else(|| not_a_variable(&target.text))?;
        if !target.subscripts.is_empty() {
            return self.assign_subscripted(cx, &r, target, value, from);
        }
        self.assign_ref(cx, &r, value, from)
    }

    pub fn assign_ref(&mut self, cx: &mut ExecContext, r: &VarRef, value: Datum, from: TypeId) -> Result<()> {
        match r {
            VarRef::Var(idx) => self.assign_var(cx, *idx, value, from),
            VarRef::Field(idx, field) => self.assign_field(cx, *idx, field, value, from),
        }
    }

    /// Assign a whole variable.
    pub fn assign_var(&mut self, cx: &mut ExecContext, idx: usize, value: Datum, from: TypeId) -> Result<()> {
        let var = self.var(idx).ok_or_else(|| PgError::internal("PL/pgSQL variable index out of range"))?;
        if var.is_record() {
            // A record takes the value and its row type.
            let fields = match &value {
                Datum::Null if from.oid == oids::RECORD || from.oid == oids::UNKNOWN => None,
                _ => plfunc::composite_fields(cx, from.oid).or_else(|| record_fields_of(&value, from)),
            };
            if !matches!(value, Datum::Record(_) | Datum::Null) {
                return Err(PgError::new(SqlState::DATATYPE_MISMATCH, "cannot assign non-composite value to a record variable"));
            }
            let mut vars = self.vars.borrow_mut();
            let v = &mut vars[idx];
            v.value = value;
            if fields.is_some() || v.rec_fields.is_none() {
                v.rec_fields = fields;
            }
            return Ok(());
        }
        if let Some(fields) = var.rec_fields.clone() {
            // Row variable of a composite type: field-wise coercion.
            let new = match value {
                Datum::Null => Datum::Null,
                Datum::Record(vals) => {
                    let src_fields = plfunc::composite_fields(cx, from.oid).or_else(|| record_fields_of(&Datum::Record(vals.clone()), from));
                    let mut out = Vec::with_capacity(fields.len());
                    for (i, (_, ft)) in fields.iter().enumerate() {
                        let v = vals.get(i).cloned().unwrap_or(Datum::Null);
                        let st = src_fields.as_ref().and_then(|s| s.get(i)).map(|s| s.1).unwrap_or(*ft);
                        out.push(plfunc::coerce_value(cx, v, st, *ft)?);
                    }
                    Datum::Record(out)
                }
                _ => return Err(PgError::new(SqlState::DATATYPE_MISMATCH, "cannot assign non-composite value to a row variable")),
            };
            self.vars.borrow_mut()[idx].value = new;
            return Ok(());
        }
        let v = plfunc::coerce_value(cx, value, from, var.ty)?;
        if v.is_null() && var.not_null {
            return Err(PgError::new(
                SqlState::NULL_VALUE_NOT_ALLOWED,
                format!("null value cannot be assigned to variable \"{}\" declared NOT NULL", var.name),
            ));
        }
        self.vars.borrow_mut()[idx].value = v;
        Ok(())
    }

    fn assign_field(&mut self, cx: &mut ExecContext, idx: usize, field: &str, value: Datum, from: TypeId) -> Result<()> {
        let var = self.var(idx).ok_or_else(|| PgError::internal("PL/pgSQL variable index out of range"))?;
        let fields = match &var.rec_fields {
            Some(f) => f.clone(),
            None => return Err(not_assigned(&var.name)),
        };
        let pos = fields.iter().position(|(n, _)| n == field).ok_or_else(|| no_field(&var.name, field))?;
        let ft = fields[pos].1;
        let v = plfunc::coerce_value(cx, value, from, ft)?;
        let mut vars = self.vars.borrow_mut();
        let target = &mut vars[idx];
        let mut vals = match std::mem::replace(&mut target.value, Datum::Null) {
            Datum::Record(v) => v,
            _ => vec![Datum::Null; fields.len()],
        };
        if vals.len() < fields.len() {
            vals.resize(fields.len(), Datum::Null);
        }
        vals[pos] = v;
        target.value = Datum::Record(vals);
        Ok(())
    }

    /// `a[i] := v` / `a[i][j] := v` / `a[lo:hi] := arr`.
    fn assign_subscripted(&mut self, cx: &mut ExecContext, r: &VarRef, target: &PlTarget, value: Datum, from: TypeId) -> Result<()> {
        let (cur, arr_ty) = match r {
            VarRef::Var(idx) => {
                let v = self.var(*idx).ok_or_else(|| PgError::internal("PL/pgSQL variable index out of range"))?;
                (v.value, v.ty)
            }
            VarRef::Field(idx, f) => {
                let (val, ty) = self.field_value(*idx, f)?;
                (val, ty)
            }
        };
        let base_arr = pgarbor_types::registry::base_type(&cx.se.env, arr_ty.oid);
        let elem = pgarbor_types::registry::element_type_of(&cx.se.env, base_arr).unwrap_or(0);
        if elem == 0 {
            return Err(PgError::new(
                SqlState::DATATYPE_MISMATCH,
                format!("cannot subscript type {} because it does not support subscripting", plfunc::type_name(cx, arr_ty)),
            ));
        }
        let mut subs = Vec::with_capacity(target.subscripts.len());
        let mut is_slice = false;
        for s in &target.subscripts {
            if let Some((lo, hi)) = split_slice(&s.text) {
                is_slice = true;
                let lo = if lo.trim().is_empty() { None } else { Some(self.eval_int(cx, lo)?) };
                let hi = if hi.trim().is_empty() { None } else { Some(self.eval_int(cx, hi)?) };
                subs.push((lo, hi.map(|h| h), true));
            } else {
                let i = self.eval_int(cx, &s.text)?;
                subs.push((Some(i), None, false));
            }
        }
        let mut items: Vec<Option<pgarbor_types::array::subscript::Subscript>> = Vec::new();
        for (lo, hi, slice) in subs {
            if slice || is_slice {
                let lo = lo.flatten();
                let hi = hi.flatten();
                items.push(Some(pgarbor_types::array::subscript::Subscript::Slice(lo, hi)));
            } else {
                match lo.flatten() {
                    Some(i) => items.push(Some(pgarbor_types::array::subscript::Subscript::Index(i))),
                    None => {
                        return Err(PgError::new(SqlState::NULL_VALUE_NOT_ALLOWED, "array subscript in assignment must not be null"));
                    }
                }
            }
        }
        let elem_ty = TypeId::of(elem);
        let v = if is_slice { plfunc::coerce_value(cx, value, from, TypeId::of(base_arr))? } else { plfunc::coerce_value(cx, value, from, elem_ty)? };
        let cur_arr = match &cur {
            Datum::Array(a) => Some(&**a),
            _ => None,
        };
        let new = pgarbor_types::array::subscript::assign(cur_arr, elem, &items, v)?;
        let newv = Datum::Array(Box::new(new));
        match r {
            VarRef::Var(idx) => {
                self.vars.borrow_mut()[*idx].value = newv;
                Ok(())
            }
            VarRef::Field(idx, f) => {
                let ft = arr_ty;
                self.assign_field(cx, *idx, f, newv, ft)
            }
        }
    }

    /// Evaluate an integer expression (subscripts).
    fn eval_int(&mut self, cx: &mut ExecContext, text: &str) -> Result<Option<i32>> {
        self.eval_int_inner(cx, text)
    }

    fn eval_int_inner(&mut self, cx: &mut ExecContext, text: &str) -> Result<Option<i32>> {
        let sql = pgarbor_sql::plpgsql::PlSql { text: text.to_string(), offset: 0 };
        let (v, t) = self.eval_expr(cx, &sql, None)?;
        let v = plfunc::coerce_value(cx, v, t, TypeId::INT4)?;
        Ok(match v {
            Datum::Int4(i) => Some(i),
            _ => None,
        })
    }

    /// Value and type of field `f` of row/record variable `idx`.
    pub fn field_value(&self, idx: usize, f: &str) -> Result<(Datum, TypeId)> {
        let vars = self.vars.borrow();
        let v = vars.get(idx).ok_or_else(|| PgError::internal("PL/pgSQL variable index out of range"))?;
        field_of(v, f)
    }
}

/// Field `f` of a row/record variable.
pub fn field_of(v: &Var, f: &str) -> Result<(Datum, TypeId)> {
    let fields = match &v.rec_fields {
        Some(fl) => fl,
        None => return Err(not_assigned(&v.name)),
    };
    let pos = fields.iter().position(|(n, _)| n == f).ok_or_else(|| no_field(&v.name, f))?;
    let ty = fields[pos].1;
    let val = match &v.value {
        Datum::Record(vals) => vals.get(pos).cloned().unwrap_or(Datum::Null),
        _ => Datum::Null,
    };
    Ok((val, ty))
}

/// `lo:hi` at bracket depth 0 of a subscript text.
fn split_slice(text: &str) -> Option<(&str, &str)> {
    let mut depth = 0i32;
    let b = text.as_bytes();
    for (i, &c) in b.iter().enumerate() {
        match c {
            b'(' | b'[' => depth += 1,
            b')' | b']' => depth -= 1,
            b':' if depth == 0 => {
                if b.get(i + 1) == Some(&b':') || (i > 0 && b[i - 1] == b':') {
                    continue;
                }
                return Some((&text[..i], &text[i + 1..]));
            }
            _ => {}
        }
    }
    None
}

/// Field names/types of an anonymous record value (`f1`, `f2`, ... with types read from the values).
pub fn record_fields_of(value: &Datum, ty: TypeId) -> Option<Vec<(String, TypeId)>> {
    let _ = ty;
    match value {
        Datum::Record(vals) => Some(
            vals.iter()
                .enumerate()
                .map(|(i, v)| (format!("f{}", i + 1), TypeId::of(datum_type(v))))
                .collect(),
        ),
        _ => None,
    }
}

/// A plausible type for a bare datum (anonymous record fields).
pub fn datum_type(v: &Datum) -> Oid {
    match v {
        Datum::Bool(_) => oids::BOOL,
        Datum::Int2(_) => oids::INT2,
        Datum::Int4(_) => oids::INT4,
        Datum::Int8(_) => oids::INT8,
        Datum::Float4(_) => oids::FLOAT4,
        Datum::Float8(_) => oids::FLOAT8,
        Datum::Numeric(_) => oids::NUMERIC,
        Datum::Date(_) => oids::DATE,
        Datum::Timestamp(_) => oids::TIMESTAMP,
        Datum::TimestampTz(_) => oids::TIMESTAMPTZ,
        Datum::Interval(_) => oids::INTERVAL,
        Datum::Jsonb(_) => oids::JSONB,
        Datum::Json(_) => oids::JSON,
        Datum::Bytea(_) => oids::BYTEA,
        Datum::Uuid(_) => oids::UUID,
        Datum::Record(_) => oids::RECORD,
        _ => oids::TEXT,
    }
}

pub fn lookup_in(scopes: &[Scope], name: &str) -> Option<usize> {
    for s in scopes.iter().rev() {
        for (n, idx) in s.names.iter().rev() {
            if n == name {
                return Some(*idx);
            }
        }
    }
    None
}

fn lookup_label<'s>(scopes: &'s [Scope], label: &str) -> Option<&'s Scope> {
    scopes.iter().rev().find(|s| s.label.as_deref() == Some(label))
}

fn in_scope(s: &Scope, name: &str) -> Option<usize> {
    s.names.iter().rev().find(|(n, _)| n == name).map(|(_, i)| *i)
}

fn is_composite_var(v: &Var) -> bool {
    v.is_record() || v.rec_fields.is_some()
}

/// Resolve `a`, `a.b`, `a.b.c` (module docs of [`Interp::resolve`]).
pub fn resolve_parts(scopes: &[Scope], vars: &[Var], parts: &[String]) -> Option<VarRef> {
    match parts {
        [a] => lookup_in(scopes, a).map(VarRef::Var),
        [a, b] => {
            if let Some(s) = lookup_label(scopes, a) {
                if let Some(i) = in_scope(s, b) {
                    return Some(VarRef::Var(i));
                }
            }
            let i = lookup_in(scopes, a)?;
            if vars.get(i).is_some_and(is_composite_var) {
                return Some(VarRef::Field(i, b.clone()));
            }
            None
        }
        [a, b, c] => {
            let s = lookup_label(scopes, a)?;
            let i = in_scope(s, b)?;
            if vars.get(i).is_some_and(is_composite_var) {
                return Some(VarRef::Field(i, c.clone()));
            }
            None
        }
        _ => None,
    }
}

pub fn not_assigned(name: &str) -> PgError {
    PgError::new(SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE, format!("record \"{name}\" is not assigned yet"))
        .with_detail("The tuple structure of a not-yet-assigned record is indeterminate.")
}

pub fn no_field(rec: &str, field: &str) -> PgError {
    PgError::new(SqlState::UNDEFINED_COLUMN, format!("record \"{rec}\" has no field \"{field}\""))
}

pub fn not_a_variable(text: &str) -> PgError {
    PgError::new(SqlState::SYNTAX_ERROR, format!("\"{text}\" is not a known variable"))
}
