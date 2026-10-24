use std::fmt;

/// A single cell value in a result set.
#[derive(Debug, Clone)]
pub enum CellValue {
    Null,
    Text(Box<str>),
    Integer(i64),
    Float(f64),
    Boolean(bool),
    Bytes(Vec<u8>),
}

impl fmt::Display for CellValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CellValue::Null => write!(f, ""),
            CellValue::Text(s) => write!(f, "{s}"),
            CellValue::Integer(i) => write!(f, "{i}"),
            CellValue::Float(v) => write!(f, "{v}"),
            CellValue::Boolean(b) => write!(f, "{b}"),
            CellValue::Bytes(b) => write!(f, "\\x{}", hex::encode(b)),
        }
    }
}

impl CellValue {
    pub fn is_null(&self) -> bool {
        matches!(self, CellValue::Null)
    }
}

/// Parse a PostgreSQL text-protocol cell string into a typed CellValue.
/// Called for every cell returned by simple_query (text format).
pub fn parse_text_cell(s: &str) -> CellValue {
    // PostgreSQL sends booleans as "t" / "f" in text protocol.
    if s == "t" {
        return CellValue::Boolean(true);
    }
    if s == "f" {
        return CellValue::Boolean(false);
    }
    // Try integer, then float.
    if let Ok(i) = s.parse::<i64>() {
        return CellValue::Integer(i);
    }
    if let Ok(f) = s.parse::<f64>() {
        return CellValue::Float(f);
    }
    CellValue::Text(s.into())
}

/// Always-quoted SQL identifier (`"name"`, embedded `"` doubled).
pub fn quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// SQL string literal (`'text'`, embedded `'` doubled). Assumes
/// `standard_conforming_strings = on` (the default since PG 9.1), so backslashes
/// are taken literally.
pub fn quote_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Render a cell as an SQL literal for INSERT export. Every non-NULL value is
/// emitted as a quoted literal — Postgres coerces an untyped literal to the
/// target column's type, so `'42'` works for int columns and `'t'` for bools.
pub fn cell_literal(cell: &CellValue) -> String {
    match cell {
        CellValue::Null => "NULL".to_owned(),
        // Text protocol sends booleans as t/f — keep that form, it round-trips
        // for both bool and text columns better than "true"/"false".
        CellValue::Boolean(b) => if *b { "'t'".to_owned() } else { "'f'".to_owned() },
        other => quote_literal(&other.to_string()),
    }
}

/// Rows per `INSERT ... VALUES (...), (...)` statement in generated scripts.
pub const INSERT_BATCH_ROWS: usize = 100;

/// Build `INSERT INTO <table> (cols) VALUES ...;` statements for `rows`, batched
/// `INSERT_BATCH_ROWS` rows per statement. `table` is used verbatim, so the
/// caller passes an already-quoted (and optionally schema-qualified) name.
pub fn insert_statements(table: &str, columns: &[String], rows: &[Vec<CellValue>]) -> String {
    let col_list = columns.iter().map(|c| quote_ident(c)).collect::<Vec<_>>().join(", ");
    let mut out = String::new();
    for chunk in rows.chunks(INSERT_BATCH_ROWS) {
        out.push_str(&format!("INSERT INTO {table} ({col_list}) VALUES\n"));
        for (i, row) in chunk.iter().enumerate() {
            let vals = row.iter().map(cell_literal).collect::<Vec<_>>().join(", ");
            out.push_str("    (");
            out.push_str(&vals);
            out.push_str(if i + 1 == chunk.len() { ");\n" } else { "),\n" });
        }
    }
    out
}

/// Best-effort guess of the single table a SELECT reads from — used to name the
/// target table of an INSERT export in free-form query mode. Returns `None` for
/// anything that isn't obviously one table (joins, comma joins, subqueries, or
/// more than one FROM), so the caller can fall back to a placeholder.
pub fn guess_single_table(sql: &str) -> Option<String> {
    let tokens: Vec<&str> = sql.split_whitespace().collect();
    let lower: Vec<String> = tokens.iter().map(|t| t.to_lowercase()).collect();
    if lower.iter().filter(|t| *t == "from").count() != 1 || lower.iter().any(|t| t == "join") {
        return None;
    }
    let pos = lower.iter().position(|t| t == "from")?;
    let raw = *tokens.get(pos + 1)?;
    if raw.starts_with('(') {
        return None;
    }
    // Any comma in the FROM clause (before the next clause keyword) is a comma join.
    const CLAUSE_END: [&str; 10] =
        ["where", "group", "order", "limit", "offset", "having", "window", "union", "fetch", "for"];
    if tokens[pos + 1..]
        .iter()
        .zip(&lower[pos + 1..])
        .take_while(|(_, l)| !CLAUSE_END.contains(&l.as_str()))
        .any(|(t, _)| t.contains(','))
    {
        return None;
    }
    let table = raw.trim_end_matches(';').trim_end_matches(')');
    if table.is_empty() { None } else { Some(table.to_owned()) }
}

/// Result of a query execution.
#[derive(Debug, Default, Clone)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<CellValue>>,
    /// Number of rows affected (for INSERT/UPDATE/DELETE).
    pub rows_affected: Option<u64>,
    /// Query execution time in milliseconds.
    pub elapsed_ms: f64,
}

impl QueryResult {
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_escapes_embedded_quotes() {
        assert_eq!(quote_ident(r#"we"ird"#), r#""we""ird""#);
        assert_eq!(quote_literal("it's"), "'it''s'");
        assert_eq!(quote_literal(r"C:\tmp"), r"'C:\tmp'");
    }

    #[test]
    fn insert_statements_batches_and_renders_literals() {
        let cols = vec!["id".to_owned(), "name".to_owned(), "ok".to_owned()];
        let rows = vec![
            vec![CellValue::Integer(1), CellValue::Text("O'Neil".into()), CellValue::Boolean(true)],
            vec![CellValue::Integer(2), CellValue::Null, CellValue::Boolean(false)],
        ];
        let sql = insert_statements("\"public\".\"t\"", &cols, &rows);
        assert_eq!(
            sql,
            "INSERT INTO \"public\".\"t\" (\"id\", \"name\", \"ok\") VALUES\n    ('1', 'O''Neil', 't'),\n    ('2', NULL, 'f');\n"
        );

        let many: Vec<Vec<CellValue>> = (0..INSERT_BATCH_ROWS + 1).map(|i| vec![CellValue::Integer(i as i64)]).collect();
        let sql = insert_statements("t", &["id".to_owned()], &many);
        assert_eq!(sql.matches("INSERT INTO").count(), 2);
    }

    #[test]
    fn guess_single_table_cases() {
        assert_eq!(guess_single_table("SELECT * FROM public.users WHERE id = 1").as_deref(), Some("public.users"));
        assert_eq!(guess_single_table("select a from \"Users\";").as_deref(), Some("\"Users\""));
        assert_eq!(guess_single_table("SELECT * FROM t LIMIT 10").as_deref(), Some("t"));
        assert_eq!(guess_single_table("SELECT * FROM a JOIN b ON a.id = b.id"), None);
        assert_eq!(guess_single_table("SELECT * FROM a x, b y WHERE x.id = y.id"), None);
        assert_eq!(guess_single_table("SELECT * FROM (SELECT 1) s"), None);
        assert_eq!(guess_single_table("SELECT (SELECT max(id) FROM b) FROM a"), None);
        assert_eq!(guess_single_table("SELECT 1"), None);
    }
}
