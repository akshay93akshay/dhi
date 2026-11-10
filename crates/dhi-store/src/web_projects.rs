//! Uploaded browser-folder web project metadata.
//!
//! The bytes and the derived tree live on disk under the configured
//! `DHI_WEB_PROJECTS_ROOT`; this table only owns the identity, display name,
//! and the absolute root that was in force when the project was created.

use chrono::{DateTime, Utc};
use sqlx::FromRow;
use uuid::Uuid;

use super::{Store, StoreError};

/// One `web_projects` row.
#[derive(Debug, Clone, FromRow)]
pub struct WebProjectRecord {
    pub id: Uuid,
    pub name: String,
    /// Absolute root as configured at creation time, kept for audit.
    pub root_path: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Store {
    /// Inserts one project row and returns the persisted record.
    pub async fn create_web_project(
        &self,
        id: Uuid,
        name: &str,
        root_path: &str,
    ) -> Result<WebProjectRecord, StoreError> {
        let record = sqlx::query_as::<_, WebProjectRecord>(
            "INSERT INTO web_projects (id, name, root_path)
             VALUES ($1, $2, $3)
             RETURNING id, name, root_path, created_at, updated_at",
        )
        .bind(id)
        .bind(name)
        .bind(root_path)
        .fetch_one(&self.pool)
        .await?;
        Ok(record)
    }

    /// Every project, newest first.
    pub async fn list_web_projects(&self) -> Result<Vec<WebProjectRecord>, StoreError> {
        let rows = sqlx::query_as::<_, WebProjectRecord>(
            "SELECT id, name, root_path, created_at, updated_at
             FROM web_projects
             ORDER BY created_at DESC, id",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// One project by id.
    pub async fn get_web_project(&self, id: Uuid) -> Result<Option<WebProjectRecord>, StoreError> {
        let row = sqlx::query_as::<_, WebProjectRecord>(
            "SELECT id, name, root_path, created_at, updated_at
             FROM web_projects
             WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }
}
