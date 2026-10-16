//! Cursors (portals opened by SQL `DECLARE` or PL/pgSQL `OPEN`), shared by both so a refcursor returned
//! from a function can be fetched with SQL `FETCH`. Results are materialized when the cursor opens;
//! positioning follows PG `PortalRunFetch` / `PortalRunSelect` exactly (`atStart` / `atEnd` / current
//! row), including the NO SCROLL rule (`cursor can only scan forward`, 55000).
//!
//! The registry is per backend (`EngineServices::backend_pid`). A cursor belongs to the transaction
//! that opened it: a non-holdable cursor is invisible (and dropped) once another transaction is
//! current; the session additionally calls [`end_transaction`] so an aborted transaction drops the
//! holdable cursors it created.
use std::cell::RefCell;
use std::collections::HashMap;

use pgarbor_common::{PgError, Result, SqlState};
use pgarbor_plan::plan::ColumnDesc;
use pgarbor_types::Datum;

/// `FETCH_ALL`.
pub const ALL: i64 = i64::MAX;

#[derive(Clone, Debug)]
pub struct Portal {
    pub name: String,
    pub columns: Vec<ColumnDesc>,
    pub rows: Vec<Vec<Datum>>,
    /// Current row, 1-based; 0 = before the first row, `rows.len() + 1` = after the last.
    pub cur: usize,
    pub no_scroll: bool,
    pub scroll: bool,
    pub holdable: bool,
    pub binary: bool,
    /// Transaction that created the cursor.
    pub txn_id: u64,
    /// Source text (`pg_cursors.statement`).
    pub statement: String,
    pub creation_ms: i64,
}

impl Portal {
    pub fn new(name: &str, columns: Vec<ColumnDesc>, rows: Vec<Vec<Datum>>, txn_id: u64) -> Portal {
        Portal {
            name: name.to_string(),
            columns,
            rows,
            cur: 0,
            no_scroll: false,
            scroll: false,
            holdable: false,
            binary: false,
            txn_id,
            statement: String::new(),
            creation_ms: 0,
        }
    }

    fn len(&self) -> usize {
        self.rows.len()
    }
    fn at_start(&self) -> bool {
        self.cur == 0
    }
    fn at_end(&self) -> bool {
        self.cur > self.len()
    }
    /// PG `portalPos`: rows fetched up to the current position.
    fn portal_pos(&self) -> usize {
        self.cur.min(self.len())
    }

    /// PG `PortalRunSelect`.
    fn run_select(&mut self, forward: bool, count: i64, out: &mut Option<Vec<Vec<Datum>>>) -> Result<u64> {
        if forward {
            if self.at_end() || count <= 0 {
                return Ok(0);
            }
            let avail = self.len() - self.cur;
            let n = if count == ALL { avail } else { (count as usize).min(avail) };
            if let Some(o) = out.as_mut() {
                o.extend(self.rows[self.cur..self.cur + n].iter().cloned());
            }
            self.cur += n;
            if count == ALL || (n as i64) < count {
                self.cur = self.len() + 1;
            }
            Ok(n as u64)
        } else {
            if self.no_scroll {
                return Err(PgError::new(SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE, "cursor can only scan forward")
                    .with_hint("Declare it with SCROLL option to enable backward scan."));
            }
            if self.at_start() || count <= 0 {
                return Ok(0);
            }
            let avail = self.cur - 1;
            let n = if count == ALL { avail } else { (count as usize).min(avail) };
            for i in 0..n {
                let row_idx = self.cur - 1 - i; // 1-based row
                if let Some(o) = out.as_mut() {
                    o.push(self.rows[row_idx - 1].clone());
                }
            }
            self.cur -= n;
            if count == ALL || (n as i64) < count {
                self.cur = 0;
            }
            Ok(n as u64)
        }
    }

    fn rewind(&mut self) -> Result<()> {
        if self.at_start() && !self.at_end() {
            return Ok(());
        }
        if self.no_scroll {
            return Err(PgError::new(SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE, "cursor can only scan forward")
                .with_hint("Declare it with SCROLL option to enable backward scan."));
        }
        self.cur = 0;
        Ok(())
    }

    /// PG `DoPortalRunFetch`. `fetch_rows` = the rows are wanted (FETCH) or not (MOVE). Returns the rows
    /// (empty for MOVE) and the processed count.
    pub fn fetch(&mut self, dir: Direction, count: i64, fetch_rows: bool) -> Result<(Vec<Vec<Datum>>, u64)> {
        let mut out: Option<Vec<Vec<Datum>>> = if fetch_rows { Some(Vec::new()) } else { None };
        let mut none: Option<Vec<Vec<Datum>>> = None;
        let mut dir = dir;
        let mut count = count;
        match dir {
            Direction::Forward => {
                if count < 0 {
                    dir = Direction::Backward;
                    count = -count;
                }
            }
            Direction::Backward => {
                if count < 0 {
                    dir = Direction::Forward;
                    count = -count;
                }
            }
            Direction::Absolute => {
                if count > 0 {
                    let pos = self.portal_pos() as i64;
                    if count - 1 <= pos / 2 {
                        self.rewind()?;
                        if count > 1 {
                            self.run_select(true, count - 1, &mut none)?;
                        }
                    } else {
                        let mut p = pos;
                        if self.at_end() {
                            p += 1;
                        }
                        if count <= p {
                            self.run_select(false, p - count + 1, &mut none)?;
                        } else if count > p + 1 {
                            self.run_select(true, count - p - 1, &mut none)?;
                        }
                    }
                    let n = self.run_select(true, 1, &mut out)?;
                    return Ok((out.unwrap_or_default(), n));
                } else if count < 0 {
                    self.run_select(true, ALL, &mut none)?;
                    if count < -1 {
                        self.run_select(false, -count - 1, &mut none)?;
                    }
                    let n = self.run_select(false, 1, &mut out)?;
                    return Ok((out.unwrap_or_default(), n));
                } else {
                    self.rewind()?;
                    return Ok((Vec::new(), 0));
                }
            }
            Direction::Relative => {
                if count > 0 {
                    if count > 1 {
                        self.run_select(true, count - 1, &mut none)?;
                    }
                    let n = self.run_select(true, 1, &mut out)?;
                    return Ok((out.unwrap_or_default(), n));
                } else if count < 0 {
                    if count < -1 {
                        self.run_select(false, -count - 1, &mut none)?;
                    }
                    let n = self.run_select(false, 1, &mut out)?;
                    return Ok((out.unwrap_or_default(), n));
                }
                dir = Direction::Forward;
            }
        }
        let mut forward = dir == Direction::Forward;
        if count == 0 {
            let on_row = !self.at_start() && !self.at_end();
            if !fetch_rows {
                return Ok((Vec::new(), on_row as u64));
            }
            if on_row {
                self.run_select(false, 1, &mut none)?;
                count = 1;
                forward = true;
            }
        }
        if !forward && count == ALL && !fetch_rows {
            let mut result = self.portal_pos() as u64;
            if result > 0 && !self.at_end() {
                result -= 1;
            }
            self.rewind()?;
            return Ok((Vec::new(), result));
        }
        let n = self.run_select(forward, count, &mut out)?;
        Ok((out.unwrap_or_default(), n))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Forward,
    Backward,
    Absolute,
    Relative,
}

thread_local! {
    static REGISTRY: RefCell<HashMap<(i32, String), Portal>> = RefCell::new(HashMap::new());
    static UNNAMED: RefCell<HashMap<i32, u32>> = RefCell::new(HashMap::new());
}

/// Drop the cursors of `pid` that do not survive into transaction `txn_id`.
fn prune(pid: i32, txn_id: u64) {
    REGISTRY.with(|r| {
        r.borrow_mut().retain(|(p, _), portal| *p != pid || portal.holdable || portal.txn_id == txn_id);
    });
}

/// Is cursor `name` open?
pub fn exists(pid: i32, txn_id: u64, name: &str) -> bool {
    prune(pid, txn_id);
    REGISTRY.with(|r| r.borrow().contains_key(&(pid, name.to_string())))
}

/// Register a new cursor; `in_use_msg` = PL/pgSQL wording (`already in use`) else SQL (`already exists`).
pub fn open(pid: i32, txn_id: u64, portal: Portal, in_use_msg: bool) -> Result<()> {
    prune(pid, txn_id);
    REGISTRY.with(|r| {
        let mut m = r.borrow_mut();
        let key = (pid, portal.name.clone());
        if m.contains_key(&key) {
            let msg = if in_use_msg {
                format!("cursor \"{}\" already in use", portal.name)
            } else {
                format!("cursor \"{}\" already exists", portal.name)
            };
            return Err(PgError::new(SqlState::from_code("42P03").unwrap_or(SqlState::DUPLICATE_OBJECT), msg));
        }
        m.insert(key, portal);
        Ok(())
    })
}

pub fn missing(name: &str) -> PgError {
    PgError::new(SqlState::INVALID_CURSOR_NAME, format!("cursor \"{name}\" does not exist"))
}

/// Run `f` on cursor `name` (34000 when it does not exist).
pub fn with_portal<T>(pid: i32, txn_id: u64, name: &str, f: impl FnOnce(&mut Portal) -> Result<T>) -> Result<T> {
    prune(pid, txn_id);
    REGISTRY.with(|r| {
        let mut m = r.borrow_mut();
        match m.get_mut(&(pid, name.to_string())) {
            Some(p) => f(p),
            None => Err(missing(name)),
        }
    })
}

/// Close cursor `name`; false when it does not exist.
pub fn close(pid: i32, txn_id: u64, name: &str) -> bool {
    prune(pid, txn_id);
    REGISTRY.with(|r| r.borrow_mut().remove(&(pid, name.to_string())).is_some())
}

/// `CLOSE ALL`.
pub fn close_all(pid: i32) {
    REGISTRY.with(|r| r.borrow_mut().retain(|(p, _), _| *p != pid));
}

/// Transaction end: non-holdable cursors go; an abort also drops holdable cursors created in it.
pub fn end_transaction(pid: i32, txn_id: u64, committed: bool) {
    REGISTRY.with(|r| {
        r.borrow_mut().retain(|(p, _), portal| {
            if *p != pid {
                return true;
            }
            if !portal.holdable {
                return false;
            }
            committed || portal.txn_id != txn_id
        })
    });
}

/// Make holdable cursors independent of their transaction after it committed.
pub fn list(pid: i32, txn_id: u64) -> Vec<Portal> {
    prune(pid, txn_id);
    REGISTRY.with(|r| r.borrow().iter().filter(|((p, _), _)| *p == pid).map(|(_, v)| v.clone()).collect())
}

/// Next `<unnamed portal N>` name of this backend.
pub fn unnamed_name(pid: i32, txn_id: u64) -> String {
    loop {
        let n = UNNAMED.with(|u| {
            let mut m = u.borrow_mut();
            let e = m.entry(pid).or_insert(0);
            *e += 1;
            *e
        });
        let name = format!("<unnamed portal {n}>");
        if !exists(pid, txn_id, &name) {
            return name;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn portal(n: usize) -> Portal {
        Portal::new("c", Vec::new(), (1..=n as i32).map(|i| vec![Datum::Int4(i)]).collect(), 1)
    }

    fn vals(rows: &[Vec<Datum>]) -> Vec<i32> {
        rows.iter().map(|r| r[0].as_i32().unwrap_or(-1)).collect()
    }

    #[test]
    fn forward_backward_and_absolute() {
        let mut p = portal(5);
        let (r, _) = p.fetch(Direction::Forward, 2, true).expect("fetch 2");
        assert_eq!(vals(&r), vec![1, 2]);
        let (r, _) = p.fetch(Direction::Backward, 1, true).expect("prior");
        assert_eq!(vals(&r), vec![1]);
        let (r, _) = p.fetch(Direction::Absolute, -1, true).expect("last");
        assert_eq!(vals(&r), vec![5]);
        let (r, _) = p.fetch(Direction::Forward, 1, true).expect("next at end");
        assert!(r.is_empty());
        let (r, _) = p.fetch(Direction::Backward, 1, true).expect("prior from end");
        assert_eq!(vals(&r), vec![5]);
        let (r, _) = p.fetch(Direction::Relative, 0, true).expect("relative 0");
        assert_eq!(vals(&r), vec![5]);
        let (r, _) = p.fetch(Direction::Absolute, 2, true).expect("absolute 2");
        assert_eq!(vals(&r), vec![2]);
    }

    #[test]
    fn no_scroll_rejects_backward() {
        let mut p = portal(3);
        p.no_scroll = true;
        p.fetch(Direction::Forward, 1, true).expect("next");
        assert!(p.fetch(Direction::Backward, 1, true).is_err());
    }

    #[test]
    fn move_counts() {
        let mut p = portal(3);
        let (_, n) = p.fetch(Direction::Forward, ALL, false).expect("move all");
        assert_eq!(n, 3);
        let (_, n) = p.fetch(Direction::Backward, ALL, false).expect("move backward all");
        assert_eq!(n, 3);
    }
}
