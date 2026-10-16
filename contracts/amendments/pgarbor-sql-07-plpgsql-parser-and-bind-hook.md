# pgarbor-sql amendment: PL/pgSQL parser and procedural bind hook

- `plpgsql` module: tokenizer, parser (`parse_body`) and AST (`PlFunction`, `PlBlock`, `PlStmtKind`). It checks
  EXIT / CONTINUE label rules and RAISE placeholder counts at parse time.
- `binder::plhook`: `bind_with_hook` and `PlVarHook` resolve routine variables in column references (`PlConflict`
  = `#variable_conflict` modes).
- `CREATE FUNCTION ... LANGUAGE plpgsql` runs `parse_body` after the lexical heuristics; `table.column%TYPE`
  anchors naming a missing column are 42703 at creation.
- CALL resolution matches OUT-parameter arguments with implicit coercion and reports 42725 when an
  OUT-parameter overload has the same full signature as the chosen procedure.
