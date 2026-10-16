//! PL/pgSQL syntax tree (PG `plpgsql.h` statement nodes, without the compile-time datum resolution:
//! names stay names and are resolved by the executor against its variable scopes).
//!
//! SQL fragments (expressions, queries, type names) are kept as source text ([`PlSql`]) with their byte
//! offset in the routine body, so the executor can parse/bind them and report positions.

/// A SQL fragment of the body: text plus byte offset of its first character in the body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlSql {
    pub text: String,
    pub offset: usize,
}

/// A possibly qualified name (`v`, `blk.v`, `rec.field`, `fn.param`), parts folded.
pub type PlName = Vec<String>;

/// Assignment / INTO / FOR target: a name with optional subscripts and field selections
/// (`a[1]`, `r.f`, `blk.v[2:3]`). `subscripts` hold the source text of each `[...]` content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlTarget {
    pub name: PlName,
    /// `[expr]` / `[lo:hi]` subscripts after the name, in order.
    pub subscripts: Vec<PlSql>,
    /// Source text of the whole target (for error messages).
    pub text: String,
    pub offset: usize,
}

/// Declared type of a variable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlType {
    /// A SQL type name (`integer`, `numeric(10,2)`, `text[]`, `my_schema.my_type`, `record`).
    Named(PlSql),
    /// `name%TYPE` (`name` = variable or `table.column`), optional trailing `[]`.
    VarType { name: PlName, array: bool, offset: usize },
    /// `table%ROWTYPE` / `type%ROWTYPE`.
    RowType { name: PlName, offset: usize },
}

/// One declaration of a block.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlDecl {
    Var {
        name: String,
        constant: bool,
        ty: PlType,
        collate: Option<String>,
        not_null: bool,
        default: Option<PlSql>,
        line: u32,
        offset: usize,
    },
    Alias { name: String, target: PlName, line: u32 },
    Cursor {
        name: String,
        /// `None` = unspecified, `Some(true)` SCROLL, `Some(false)` NO SCROLL.
        scroll: Option<bool>,
        args: Vec<(String, PlType)>,
        query: PlSql,
        line: u32,
    },
}

/// `WHEN ... THEN` condition of an exception handler.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlCondition {
    Others,
    /// Five-character SQLSTATE; a class match (`22000`) matches every code in the class.
    SqlState(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlHandler {
    pub conditions: Vec<PlCondition>,
    pub body: Vec<PlStmt>,
    pub line: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlBlock {
    pub label: Option<String>,
    pub decls: Vec<PlDecl>,
    pub body: Vec<PlStmt>,
    /// `None` = no EXCEPTION clause (no subtransaction is used).
    pub handlers: Option<Vec<PlHandler>>,
    pub line: u32,
}

/// The query source of FOR-over-query, RETURN QUERY and OPEN.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlQuery {
    Static(PlSql),
    Execute { query: PlSql, using: Vec<PlSql> },
}

/// RAISE level.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RaiseLevel {
    Debug,
    Log,
    Info,
    Notice,
    Warning,
    Exception,
}

/// What a RAISE names before USING.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RaiseWhat {
    /// `RAISE;` re-throw / `RAISE level USING ...` only.
    Nothing,
    /// `'format', args...`
    Format { format: String, args: Vec<PlSql> },
    /// Condition name (already validated) with its SQLSTATE.
    Condition { name: String, sqlstate: String },
    /// `SQLSTATE 'xxxxx'`.
    SqlState(String),
}

/// `USING option = expr` of RAISE.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RaiseOption {
    Errcode,
    Message,
    Detail,
    Hint,
    Column,
    Constraint,
    Datatype,
    Table,
    Schema,
}

/// FETCH / MOVE direction (`FETCH_FORWARD` etc. of PG).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FetchDirection {
    Next,
    Prior,
    First,
    Last,
    Absolute(PlSql),
    Relative(PlSql),
    Forward(Option<PlSql>),
    ForwardAll,
    Backward(Option<PlSql>),
    BackwardAll,
}

/// GET DIAGNOSTICS item.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiagItem {
    RowCount,
    PgContext,
    PgRoutineOid,
    ReturnedSqlstate,
    MessageText,
    PgExceptionDetail,
    PgExceptionHint,
    PgExceptionContext,
    ColumnName,
    ConstraintName,
    PgDatatypeName,
    TableName,
    SchemaName,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlStmtKind {
    Block(PlBlock),
    /// `full` = the source text from the target to the end of the expression (`v := a FROM t`).
    Assign { target: PlTarget, expr: PlSql, full: PlSql },
    If { arms: Vec<(PlSql, Vec<PlStmt>)>, else_body: Option<Vec<PlStmt>> },
    Case { expr: Option<PlSql>, arms: Vec<(PlSql, Vec<PlStmt>)>, else_body: Option<Vec<PlStmt>> },
    Loop { label: Option<String>, body: Vec<PlStmt> },
    While { label: Option<String>, cond: PlSql, body: Vec<PlStmt> },
    ForInt {
        label: Option<String>,
        var: String,
        reverse: bool,
        lower: PlSql,
        upper: PlSql,
        step: Option<PlSql>,
        body: Vec<PlStmt>,
    },
    ForQuery { label: Option<String>, targets: Vec<PlTarget>, query: PlQuery, body: Vec<PlStmt> },
    ForCursor { label: Option<String>, target: String, cursor: String, args: Option<PlSql>, body: Vec<PlStmt> },
    Foreach { label: Option<String>, targets: Vec<PlTarget>, slice: u32, array: PlSql, body: Vec<PlStmt> },
    Exit { is_continue: bool, label: Option<String>, cond: Option<PlSql> },
    Return { expr: Option<PlSql> },
    ReturnNext { expr: Option<PlSql> },
    ReturnQuery { query: PlQuery },
    Raise { level: RaiseLevel, what: RaiseWhat, options: Vec<(RaiseOption, PlSql)> },
    Assert { cond: PlSql, message: Option<PlSql> },
    Perform { query: PlSql },
    /// A SQL statement; `into` = `(strict, targets)`.
    ExecSql { sql: PlSql, into: Option<(bool, Vec<PlTarget>)>, first_word: String },
    DynExecute { query: PlSql, into: Option<(bool, Vec<PlTarget>)>, using: Vec<PlSql> },
    GetDiag { stacked: bool, items: Vec<(PlTarget, DiagItem)> },
    Open { cursor: String, scroll: Option<bool>, query: Option<PlQuery>, args: Option<PlSql> },
    Fetch { cursor: String, direction: FetchDirection, targets: Vec<PlTarget>, is_move: bool },
    Close { cursor: String },
    Null,
    Commit { chain: bool },
    Rollback { chain: bool },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlStmt {
    pub kind: PlStmtKind,
    /// 1-based line of the statement's first token in the body.
    pub line: u32,
    /// Byte offset of the statement's first token in the body.
    pub offset: usize,
}

impl PlStmt {
    /// PG `plpgsql_stmt_typename` (used in CONTEXT lines: `line 3 at RAISE`).
    pub fn type_name(&self) -> &'static str {
        match &self.kind {
            PlStmtKind::Block(_) => "statement block",
            PlStmtKind::Assign { .. } => "assignment",
            PlStmtKind::If { .. } => "IF",
            PlStmtKind::Case { .. } => "CASE",
            PlStmtKind::Loop { .. } => "LOOP",
            PlStmtKind::While { .. } => "WHILE",
            PlStmtKind::ForInt { .. } => "FOR with integer loop variable",
            PlStmtKind::ForQuery { query: PlQuery::Static(_), .. } => "FOR over SELECT rows",
            PlStmtKind::ForQuery { query: PlQuery::Execute { .. }, .. } => "FOR over EXECUTE statement",
            PlStmtKind::ForCursor { .. } => "FOR over cursor",
            PlStmtKind::Foreach { .. } => "FOREACH over array",
            PlStmtKind::Exit { is_continue: false, .. } => "EXIT",
            PlStmtKind::Exit { is_continue: true, .. } => "CONTINUE",
            PlStmtKind::Return { .. } => "RETURN",
            PlStmtKind::ReturnNext { .. } => "RETURN NEXT",
            PlStmtKind::ReturnQuery { .. } => "RETURN QUERY",
            PlStmtKind::Raise { .. } => "RAISE",
            PlStmtKind::Assert { .. } => "ASSERT",
            PlStmtKind::Perform { .. } => "PERFORM",
            PlStmtKind::ExecSql { first_word, .. } => match first_word.as_str() {
                "call" => "CALL",
                "do" => "DO",
                _ => "SQL statement",
            },
            PlStmtKind::DynExecute { .. } => "EXECUTE",
            PlStmtKind::GetDiag { stacked: false, .. } => "GET DIAGNOSTICS",
            PlStmtKind::GetDiag { stacked: true, .. } => "GET STACKED DIAGNOSTICS",
            PlStmtKind::Open { .. } => "OPEN",
            PlStmtKind::Fetch { is_move: false, .. } => "FETCH",
            PlStmtKind::Fetch { is_move: true, .. } => "MOVE",
            PlStmtKind::Close { .. } => "CLOSE",
            PlStmtKind::Null => "NULL",
            PlStmtKind::Commit { .. } => "COMMIT",
            PlStmtKind::Rollback { .. } => "ROLLBACK",
        }
    }
}

/// `#variable_conflict` / `#print_strict_params` directives.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PlOptions {
    /// `None` = default (`error`), `Some("use_variable")`, `Some("use_column")`.
    pub variable_conflict: Option<VarConflict>,
    pub print_strict_params: Option<bool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VarConflict {
    Error,
    UseVariable,
    UseColumn,
}

/// A parsed routine body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlFunction {
    pub options: PlOptions,
    pub block: PlBlock,
}
