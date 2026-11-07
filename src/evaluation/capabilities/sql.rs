//! The agent's SQL access to its database. It shares the file with the
//! trajectory: the agent can read its history and keep its own tables, but an
//! authorizer stops it from changing the trajectory or reaching other files.

use std::{
    path::Path,
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, Instant},
};

use rusqlite::{
    Connection, OpenFlags,
    hooks::{AuthAction, AuthContext, Authorization},
    limits::Limit,
    types::{Value as SqlValue, ValueRef},
};
use serde::Deserialize;
use serde_json::{Map, Value, json};

/// Longest a single statement may run.
const STATEMENT_TIME: Duration = Duration::from_secs(5);
/// Most rows one query returns.
const MAX_ROWS: usize = 10_000;
/// Largest string or blob, in bytes.
const MAX_LENGTH: i32 = 10 << 20;
/// Pragmas that only describe the schema.
const SCHEMA_PRAGMAS: [&str; 6] = [
    "table_info",
    "table_xinfo",
    "table_list",
    "index_list",
    "index_info",
    "foreign_key_list",
];

#[derive(Clone)]
pub struct Sql {
    connection: Arc<Mutex<Connection>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SqlRequest {
    sql: String,
    #[serde(default)]
    params: Vec<Value>,
    mode: Mode,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Mode {
    /// Rows as objects.
    Query,
    /// Changes and the last inserted row ID.
    Run,
}

impl Sql {
    /// Open the database for the agent, protecting `protected_table`.
    pub fn open(path: &Path, protected_table: &str) -> rusqlite::Result<Self> {
        let mut connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        connection.busy_timeout(Duration::from_secs(1))?;
        connection.set_limit(Limit::SQLITE_LIMIT_ATTACHED, 0)?;
        connection.set_limit(Limit::SQLITE_LIMIT_LENGTH, MAX_LENGTH)?;
        let protected = protected_table.to_owned();
        connection.authorizer(Some(move |context: AuthContext<'_>| {
            authorize(&context.action, &protected)
        }))?;
        Ok(Sql {
            connection: Arc::new(Mutex::new(connection)),
        })
    }

    pub async fn execute(&self, request: SqlRequest) -> Result<Value, String> {
        let connection = self.connection.clone();
        tokio::task::spawn_blocking(move || {
            let mut connection = connection.lock().unwrap_or_else(PoisonError::into_inner);
            run(&mut connection, &request)
        })
        .await
        .map_err(|e| format!("query failed: {e}"))?
    }
}

fn run(connection: &mut Connection, request: &SqlRequest) -> Result<Value, String> {
    let started = Instant::now();
    connection
        .progress_handler(10_000, Some(move || started.elapsed() > STATEMENT_TIME))
        .map_err(|e| e.to_string())?;
    let result = statement(connection, request);
    connection
        .progress_handler(0, None::<fn() -> bool>)
        .map_err(|e| e.to_string())?;
    result
}

fn statement(connection: &Connection, request: &SqlRequest) -> Result<Value, String> {
    let failed = |e: rusqlite::Error| e.to_string();
    let params: Vec<SqlValue> = request.params.iter().map(to_sql).collect();
    let mut statement = connection.prepare(&request.sql).map_err(failed)?;
    match request.mode {
        Mode::Run => {
            let changes = statement.execute(rusqlite::params_from_iter(params)).map_err(failed)?;
            Ok(json!({ "changes": changes, "lastInsertRowid": connection.last_insert_rowid() }))
        }
        Mode::Query => {
            let columns: Vec<String> = statement.column_names().map(str::to_owned).collect();
            let mut rows = statement.query(rusqlite::params_from_iter(params)).map_err(failed)?;
            let mut objects = Vec::new();
            while let Some(row) = rows.next().map_err(failed)? {
                if objects.len() == MAX_ROWS {
                    return Err(format!("more than {MAX_ROWS} rows; add a LIMIT"));
                }
                let mut object = Map::new();
                for (i, column) in columns.iter().enumerate() {
                    object.insert(column.clone(), to_json(row.get_ref(i).map_err(failed)?));
                }
                objects.push(Value::Object(object));
            }
            Ok(Value::Array(objects))
        }
    }
}

/// Allow ordinary work on the agent's own tables. Deny anything that changes
/// the protected table, reaches other databases or files, or loads code.
fn authorize(action: &AuthAction<'_>, protected: &str) -> Authorization {
    let other = |table: &str| !table.eq_ignore_ascii_case(protected);
    let allowed = match action {
        AuthAction::Select
        | AuthAction::Read { .. }
        | AuthAction::Recursive
        | AuthAction::Transaction { .. }
        | AuthAction::Savepoint { .. }
        | AuthAction::Reindex { .. }
        | AuthAction::CreateTable { .. }
        | AuthAction::CreateTempTable { .. }
        | AuthAction::CreateView { .. }
        | AuthAction::CreateTempView { .. }
        | AuthAction::DropView { .. }
        | AuthAction::DropTempView { .. }
        | AuthAction::DropTempTable { .. } => true,
        AuthAction::Insert { table_name }
        | AuthAction::Delete { table_name }
        | AuthAction::Update { table_name, .. }
        | AuthAction::DropTable { table_name }
        | AuthAction::AlterTable { table_name, .. }
        | AuthAction::Analyze { table_name }
        | AuthAction::CreateIndex { table_name, .. }
        | AuthAction::CreateTempIndex { table_name, .. }
        | AuthAction::DropIndex { table_name, .. }
        | AuthAction::DropTempIndex { table_name, .. }
        // A trigger on the trajectory would run inside the runtime's own writes.
        | AuthAction::CreateTrigger { table_name, .. }
        | AuthAction::CreateTempTrigger { table_name, .. }
        | AuthAction::DropTrigger { table_name, .. }
        | AuthAction::DropTempTrigger { table_name, .. } => other(table_name),
        AuthAction::Function { function_name } => !function_name.eq_ignore_ascii_case("load_extension"),
        // These only describe the schema; their argument is a table or index name.
        AuthAction::Pragma { pragma_name, .. } => {
            SCHEMA_PRAGMAS.iter().any(|allowed| pragma_name.eq_ignore_ascii_case(allowed))
        }
        // Attaching reaches other files; virtual tables run module code.
        _ => false,
    };
    if allowed {
        Authorization::Allow
    } else {
        Authorization::Deny
    }
}

fn to_sql(value: &Value) -> SqlValue {
    match value {
        Value::Null => SqlValue::Null,
        Value::Bool(b) => SqlValue::Integer(i64::from(*b)),
        Value::Number(n) => match n.as_i64() {
            Some(i) => SqlValue::Integer(i),
            None => n.as_f64().map_or(SqlValue::Null, SqlValue::Real),
        },
        Value::String(s) => SqlValue::Text(s.clone()),
        Value::Array(_) | Value::Object(_) => SqlValue::Text(value.to_string()),
    }
}

fn to_json(value: ValueRef<'_>) -> Value {
    match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => json!(i),
        ValueRef::Real(r) => serde_json::Number::from_f64(r).map_or(Value::Null, Value::Number),
        ValueRef::Text(t) => Value::String(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => json!(b),
    }
}
