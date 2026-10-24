use std::sync::mpsc::{Receiver, Sender};

use anyhow::Result;

use crate::config::AiConfig;

// ── Commands UI → AI thread ────────────────────────────────────────────────────

#[derive(Debug)]
pub enum AiCommand {
    /// Generate SQL from a natural-language prompt.
    NlToSql {
        prompt: String,
        /// Compact schema summary built by Sidebar::schema_context_for_ai().
        schema_context: String,
    },
    /// Explain / optimize / fix an existing query.
    Assist {
        task: AiTask,
        sql: String,
        /// The error the query failed with (FixError only).
        error: Option<String>,
        /// Schema summary incl. indexes (`metadata::load_full_schema_for_ai`).
        schema_context: String,
    },
    /// Hot-update AI config without restarting the thread.
    SetConfig(AiConfig),
}

/// What `AiCommand::Assist` should do with the query.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AiTask {
    Explain,
    Optimize,
    FixError,
}

/// Parsed answer to an `AiCommand::Assist`.
#[derive(Debug, Clone)]
pub struct AiAssistResult {
    pub task: AiTask,
    /// Rewritten SQL (Optimize / FixError). `None` for Explain, or when the
    /// model concluded no change is needed.
    pub sql: Option<String>,
    /// Explanation text shown to the user.
    pub notes: String,
}

// ── Events AI thread → UI ──────────────────────────────────────────────────────

#[derive(Debug)]
pub enum AiEvent {
    /// Request dispatched; UI should show spinner.
    Thinking,
    /// AI returned a SQL string.
    SqlGenerated(String),
    /// Answer to an `AiCommand::Assist`.
    AssistDone(AiAssistResult),
    /// AI call failed.
    Error(String),
}

// ── Handle ────────────────────────────────────────────────────────────────────

pub struct AiHandle;

impl AiHandle {
    pub fn spawn(config: AiConfig, cmd_rx: Receiver<AiCommand>, evt_tx: Sender<AiEvent>) {
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("ai tokio rt");
            rt.block_on(ai_worker(config, cmd_rx, evt_tx));
        });
    }
}

// ── Worker ─────────────────────────────────────────────────────────────────────

async fn ai_worker(initial_cfg: AiConfig, cmd_rx: Receiver<AiCommand>, evt_tx: Sender<AiEvent>) {
    use std::sync::{Arc, Mutex};

    let cmd_rx = Arc::new(Mutex::new(cmd_rx));
    let mut cfg = initial_cfg;

    loop {
        let rx = Arc::clone(&cmd_rx);
        let cmd = match tokio::task::spawn_blocking(move || rx.lock().unwrap().recv()).await {
            Ok(Ok(c)) => c,
            _ => break,
        };

        match cmd {
            AiCommand::NlToSql { prompt, schema_context } => {
                let _ = evt_tx.send(AiEvent::Thinking);
                match call_ai(&cfg, &prompt, &schema_context).await {
                    Ok(sql) => { let _ = evt_tx.send(AiEvent::SqlGenerated(sql)); }
                    Err(e)  => { let _ = evt_tx.send(AiEvent::Error(e.to_string())); }
                }
            }
            AiCommand::Assist { task, sql, error, schema_context } => {
                let _ = evt_tx.send(AiEvent::Thinking);
                match call_assist(&cfg, task, &sql, error.as_deref(), &schema_context).await {
                    Ok(result) => { let _ = evt_tx.send(AiEvent::AssistDone(result)); }
                    Err(e)     => { let _ = evt_tx.send(AiEvent::Error(e.to_string())); }
                }
            }
            AiCommand::SetConfig(new_cfg) => {
                cfg = new_cfg;
            }
        }
    }
}

// ── Dispatch to correct provider ───────────────────────────────────────────────

async fn call_ai(cfg: &AiConfig, prompt: &str, schema_context: &str) -> Result<String> {
    if !cfg.is_configured() {
        anyhow::bail!(
            "AI not configured. Set provider + API key via Settings → AI."
        );
    }

    let system = "\
You are a senior PostgreSQL expert. Generate exactly ONE valid PostgreSQL query based on the user's request.

Rules:
- Output ONLY the SQL query. No explanations, no comments, no markdown, no code fences.
- The query MUST be syntactically correct and executable in PostgreSQL.
- Use ONLY the tables and columns provided in the schema. Never invent names.
- If the request references non-existent tables or columns, do not guess — return: SELECT 1;
- If the request cannot be fulfilled with the given schema, return: SELECT 1;
- Prefer explicit column names over SELECT *.
- Always qualify columns with table aliases when multiple tables are involved.
- Use appropriate JOIN types (INNER, LEFT) based on context. Prefer JOINs over subqueries.
- Apply LIMIT when the result set could be large, unless the user explicitly asks for all data.
- Use ORDER BY with deterministic columns (id, created_at, etc.) when LIMIT is applied.
- Ensure correct aggregations: always include GROUP BY when using aggregate functions.
- Use PostgreSQL-specific features when appropriate (ILIKE, RETURNING, window functions, CTEs).
- Handle NULLs safely (use COALESCE or IS NOT NULL where relevant).
- Avoid scanning entire large tables without WHERE filters unless explicitly requested.
- Do NOT perform destructive operations (DELETE, DROP, TRUNCATE, UPDATE) unless explicitly requested.
- Prefer indexed columns for filtering when inferable from column names (id, *_id, created_at, status).";

    let user_msg = if schema_context.is_empty() {
        prompt.to_owned()
    } else {
        format!("Schema:\n{schema_context}\n\nQuery: {prompt}")
    };

    call_provider(cfg, system, &user_msg, 1024).await
}

async fn call_provider(cfg: &AiConfig, system: &str, user_msg: &str, max_tokens: u32) -> Result<String> {
    match cfg.provider.as_str() {
        "claude" => call_claude(cfg, system, user_msg, max_tokens).await,
        _        => call_openai_compat(cfg, system, user_msg, max_tokens).await,
    }
}

// ── Explain / Optimize / Fix ───────────────────────────────────────────────────

const ASSIST_FORMAT: &str = "\
Answer in exactly this format, nothing before or after:
<sql>
the complete rewritten query, or empty if no change is needed
</sql>
<explanation>
your explanation
</explanation>";

async fn call_assist(
    cfg: &AiConfig,
    task: AiTask,
    sql: &str,
    error: Option<&str>,
    schema_context: &str,
) -> Result<AiAssistResult> {
    if !cfg.is_configured() {
        anyhow::bail!("AI not configured. Set provider + API key via Settings → AI.");
    }
    let system = match task {
        AiTask::Explain => "\
You are a senior PostgreSQL expert. Explain what the given query does, in plain language, for a developer:
what it returns, how the tables are joined and filtered, and anything surprising (implicit casts, NULL
handling, duplicate rows from joins, missing LIMIT on large tables, performance pitfalls given the indexes).
Be concise: short paragraphs or bullet points, at most ~200 words. Do not rewrite the query.
Answer with plain text only — no markdown headings, no code fences.".to_owned(),
        AiTask::Optimize => format!("\
You are a senior PostgreSQL performance expert. Rewrite the given query to run faster on PostgreSQL while
returning exactly the same result (same rows, columns, column names and order semantics).
Use the schema and the listed indexes. Typical wins: sargable predicates that can use an index, EXISTS
instead of IN/DISTINCT joins, avoiding functions on indexed columns, removing redundant joins/sorts,
CTE inlining. Never change the meaning of the query and never add data-modifying statements.
If an index would help more than a rewrite, say so in the explanation and include the CREATE INDEX
statement there (not in <sql>). If the query is already fine, leave <sql> empty.
Use only tables and columns from the schema.
{ASSIST_FORMAT}"),
        AiTask::FixError => format!("\
You are a senior PostgreSQL expert. The given query failed with the given error. Return a corrected query
that does what the original was evidently meant to do. Change as little as possible. Use only tables and
columns from the schema (the error is often a misspelled or non-existent name — pick the right one from the
schema). Never add data-modifying or destructive statements that the original did not have.
In the explanation, state the cause of the error in one or two sentences.
{ASSIST_FORMAT}"),
    };

    let mut user_msg = String::new();
    if !schema_context.is_empty() {
        user_msg.push_str(&format!("Schema:\n{schema_context}\n\n"));
    }
    user_msg.push_str(&format!("Query:\n{sql}\n"));
    if let Some(err) = error {
        user_msg.push_str(&format!("\nError:\n{err}\n"));
    }

    let raw = call_provider(cfg, &system, &user_msg, 2048).await?;
    Ok(match task {
        AiTask::Explain => AiAssistResult { task, sql: None, notes: raw },
        _ => {
            let (sql, notes) = parse_assist_response(&raw);
            AiAssistResult { task, sql, notes }
        }
    })
}

/// Split a `<sql>…</sql><explanation>…</explanation>` answer. Tolerates a
/// missing explanation tag, stray code fences inside `<sql>`, and a model that
/// ignored the format entirely (then the whole text is treated as notes).
pub fn parse_assist_response(raw: &str) -> (Option<String>, String) {
    fn between<'a>(s: &'a str, open: &str, close: &str) -> Option<&'a str> {
        let start = s.find(open)? + open.len();
        let end = s[start..].find(close).map_or(s.len(), |e| start + e);
        Some(&s[start..end])
    }
    let sql = between(raw, "<sql>", "</sql>").map(|s| {
        let s = s.trim();
        let s = s.strip_prefix("```sql").or_else(|| s.strip_prefix("```")).unwrap_or(s);
        s.strip_suffix("```").unwrap_or(s).trim().to_owned()
    });
    let notes = between(raw, "<explanation>", "</explanation>")
        .map(|n| n.trim().to_owned())
        .unwrap_or_else(|| match sql {
            // Text after </sql> (if any) is the explanation.
            Some(_) => raw.split_once("</sql>").map_or("", |(_, rest)| rest).trim().to_owned(),
            None => raw.trim().to_owned(),
        });
    (sql.filter(|s| !s.is_empty()), notes)
}

/// Statement-leading keywords that modify data or schema.
const WRITE_KEYWORDS: &[&str] = &[
    "insert", "update", "delete", "merge", "truncate", "drop", "alter", "create", "grant", "revoke",
    "copy", "vacuum", "reindex", "cluster", "comment", "call", "do",
];

/// First keyword of every statement in `sql` that writes (lowercased). Looks
/// past a leading `WITH …` CTE only shallowly — a data-modifying CTE is caught
/// because the word appears as `insert`/`update`/`delete` after `(`.
fn write_keywords(sql: &str) -> std::collections::BTreeSet<String> {
    let mut out = std::collections::BTreeSet::new();
    for stmt in sql.split(';') {
        let lower = stmt.to_lowercase();
        if let Some(first) = lower.split_whitespace().next() {
            if WRITE_KEYWORDS.contains(&first) {
                out.insert(first.to_owned());
            }
        }
        if lower.trim_start().starts_with("with") {
            for kw in ["insert", "update", "delete"] {
                if lower.contains(&format!("({kw} ")) || lower.contains(&format!("( {kw} ")) {
                    out.insert(kw.to_owned());
                }
            }
        }
    }
    out
}

/// Safety check on AI-generated SQL before the user adopts it: the write
/// operations (INSERT/UPDATE/DELETE/DROP/…) it contains that the original
/// query did not. Empty = no new side effects.
pub fn new_write_operations(original: &str, suggested: &str) -> Vec<String> {
    let before = write_keywords(original);
    write_keywords(suggested)
        .into_iter()
        .filter(|k| !before.contains(k))
        .map(|k| k.to_uppercase())
        .collect()
}

// ── Anthropic Claude ───────────────────────────────────────────────────────────

const CLAUDE_API_URL: &str = "https://api.anthropic.com/v1/messages";

async fn call_claude(cfg: &AiConfig, system: &str, user_msg: &str, max_tokens: u32) -> Result<String> {
    let body = serde_json::json!({
        "model": cfg.effective_model(),
        "max_tokens": max_tokens,
        "system": system,
        "messages": [{ "role": "user", "content": user_msg }]
    });

    let client = reqwest::Client::new();
    let resp = client
        .post(CLAUDE_API_URL)
        .header("x-api-key", &cfg.api_key)
        .header("anthropic-version", "2023-06-01")
        .json(&body)
        .send()
        .await?;

    let status = resp.status();
    let json: serde_json::Value = resp.json().await?;

    if !status.is_success() {
        let msg = json["error"]["message"]
            .as_str()
            .unwrap_or("unknown error")
            .to_owned();
        anyhow::bail!("Claude API {status}: {msg}");
    }

    extract_text(&json, "content[0].text")
}

// ── OpenAI-compatible (Groq / Ollama / OpenAI / OpenRouter / custom) ───────────

async fn call_openai_compat(cfg: &AiConfig, system: &str, user_msg: &str, max_tokens: u32) -> Result<String> {
    let base = cfg.effective_base_url();
    let url = format!("{base}/chat/completions");

    let body = serde_json::json!({
        "model": cfg.effective_model(),
        "max_tokens": max_tokens,
        "messages": [
            { "role": "system", "content": system },
            { "role": "user",   "content": user_msg }
        ]
    });

    let client = reqwest::Client::new();
    let mut req = client.post(&url).json(&body);

    if !cfg.api_key.is_empty() {
        req = req.header("Authorization", format!("Bearer {}", cfg.api_key));
    }

    let resp = req.send().await?;
    let status = resp.status();
    let json: serde_json::Value = resp.json().await?;

    if !status.is_success() {
        let msg = json["error"]["message"]
            .as_str()
            .unwrap_or("unknown error")
            .to_owned();
        anyhow::bail!("{} API {status}: {msg}", cfg.provider);
    }

    extract_text(&json, "choices[0].message.content")
}

// ── Helper ─────────────────────────────────────────────────────────────────────

/// Navigate a dotted path like `"choices[0].message.content"` into a JSON value.
fn extract_text(json: &serde_json::Value, path: &str) -> Result<String> {
    let mut cur = json;
    for part in path.split('.') {
        if let Some(idx_start) = part.find('[') {
            let key = &part[..idx_start];
            let idx: usize = part[idx_start + 1..part.len() - 1].parse()?;
            if !key.is_empty() {
                cur = &cur[key];
            }
            cur = &cur[idx];
        } else {
            cur = &cur[part];
        }
    }
    cur.as_str()
        .map(|s| s.trim().to_owned())
        .ok_or_else(|| anyhow::anyhow!("Unexpected AI response format"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tagged_response() {
        let raw = "<sql>\n```sql\nSELECT id FROM users WHERE email = 'x';\n```\n</sql>\n<explanation>\nUses the email index.\n</explanation>";
        let (sql, notes) = parse_assist_response(raw);
        assert_eq!(sql.as_deref(), Some("SELECT id FROM users WHERE email = 'x';"));
        assert_eq!(notes, "Uses the email index.");
    }

    #[test]
    fn empty_sql_means_no_change_and_untagged_text_is_notes() {
        let (sql, notes) = parse_assist_response("<sql>\n</sql>\n<explanation>Already optimal.</explanation>");
        assert_eq!(sql, None);
        assert_eq!(notes, "Already optimal.");

        let (sql, notes) = parse_assist_response("The query is fine as is.");
        assert_eq!(sql, None);
        assert_eq!(notes, "The query is fine as is.");

        let (sql, notes) = parse_assist_response("<sql>SELECT 1</sql> because reasons");
        assert_eq!(sql.as_deref(), Some("SELECT 1"));
        assert_eq!(notes, "because reasons");
    }

    #[test]
    fn flags_new_write_operations_only() {
        assert!(new_write_operations("SELECT * FROM t", "SELECT a FROM t WHERE x = 1").is_empty());
        assert_eq!(new_write_operations("SELECT * FROM t", "DELETE FROM t; SELECT 1"), vec!["DELETE"]);
        assert!(new_write_operations("UPDATE t SET a = 1", "UPDATE t SET a = 1 WHERE id = 2").is_empty());
        assert_eq!(
            new_write_operations("SELECT 1", "WITH d AS (DELETE FROM t RETURNING *) SELECT * FROM d"),
            vec!["DELETE"]
        );
        assert_eq!(new_write_operations("select 1", "  drop table t"), vec!["DROP"]);
    }
}
