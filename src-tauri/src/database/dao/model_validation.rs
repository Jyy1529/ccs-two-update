//! Device-local, credential-free validation history. Exclude this table from
//! remote import/export and auto-sync dirty tracking, retaining local rows.

use crate::database::{lock_conn, Database};
use crate::error::AppError;
use crate::services::model_validation::ValidationRun;
use rusqlite::{params, Connection, OptionalExtension};

pub fn ensure_model_validation_tables(conn: &Connection) -> Result<(), AppError> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS model_validation_runs (
            id TEXT PRIMARY KEY NOT NULL,
            plan_id TEXT UNIQUE NOT NULL,
            app_id TEXT NOT NULL,
            provider_id TEXT NOT NULL,
            started_at TEXT NOT NULL,
            run_json TEXT NOT NULL
         );
         CREATE INDEX IF NOT EXISTS idx_model_validation_target
         ON model_validation_runs(app_id, provider_id, started_at DESC);",
    )
    .map_err(|_| AppError::Database("无法初始化本机模型验证历史表".into()))
}

impl Database {
    pub fn save_model_validation_run(&self, run: &ValidationRun) -> Result<(), AppError> {
        let json = serde_json::to_string(run).map_err(|_| history_error())?;
        if json.len() > 262_144 {
            return Err(history_error());
        }
        let conn = lock_conn!(self.conn);
        ensure_model_validation_tables(&conn)?;
        conn.execute(
            "INSERT INTO model_validation_runs (id, plan_id, app_id, provider_id, started_at, run_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(id) DO UPDATE SET run_json = excluded.run_json",
            params![run.id, run.plan.id, run.plan.target.app_id, run.plan.target.provider_id, run.started_at, json]
        ).map_err(|_| history_error())?;
        Ok(())
    }

    pub fn get_model_validation_run(&self, id: &str) -> Result<Option<ValidationRun>, AppError> {
        let conn = lock_conn!(self.conn);
        ensure_model_validation_tables(&conn)?;
        let json: Option<String> = conn
            .query_row(
                "SELECT run_json FROM model_validation_runs WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .optional()
            .map_err(|_| history_error())?;
        json.map(|s| serde_json::from_str(&s).map_err(|_| history_error()))
            .transpose()
    }

    pub fn get_model_validation_by_plan(
        &self,
        id: &str,
    ) -> Result<Option<ValidationRun>, AppError> {
        let conn = lock_conn!(self.conn);
        ensure_model_validation_tables(&conn)?;
        let json: Option<String> = conn
            .query_row(
                "SELECT run_json FROM model_validation_runs WHERE plan_id = ?1",
                [id],
                |r| r.get(0),
            )
            .optional()
            .map_err(|_| history_error())?;
        json.map(|s| serde_json::from_str(&s).map_err(|_| history_error()))
            .transpose()
    }

    pub fn list_model_validation_runs(
        &self,
        app_id: Option<&str>,
        provider_id: Option<&str>,
        limit: u32,
    ) -> Result<Vec<ValidationRun>, AppError> {
        let conn = lock_conn!(self.conn);
        ensure_model_validation_tables(&conn)?;
        let mut statement = conn
            .prepare(
                "SELECT run_json FROM model_validation_runs
             WHERE (?1 IS NULL OR app_id = ?1) AND (?2 IS NULL OR provider_id = ?2)
             ORDER BY started_at DESC, id DESC LIMIT ?3",
            )
            .map_err(|_| history_error())?;
        let rows = statement
            .query_map(params![app_id, provider_id, limit.clamp(1, 200)], |r| {
                r.get::<_, String>(0)
            })
            .map_err(|_| history_error())?;
        rows.map(|row| {
            serde_json::from_str(&row.map_err(|_| history_error())?).map_err(|_| history_error())
        })
        .collect()
    }
}

fn history_error() -> AppError {
    AppError::Database("无法保存或读取本机模型验证历史（诊断不记录凭据或原始响应）".into())
}
