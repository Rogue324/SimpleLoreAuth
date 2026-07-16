use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use rand_core::OsRng;
use rusqlite::{Connection, OptionalExtension, params};
use uuid::Uuid;

#[derive(Clone, Debug)]
pub struct Database {
    path: PathBuf,
}

#[derive(Clone, Debug)]
pub struct User {
    pub id: String,
    pub username: String,
    pub display_name: String,
    pub password_hash: String,
    pub disabled: bool,
    pub is_admin: bool,
}

#[derive(Clone, Debug)]
pub struct Grant {
    pub resource_id: String,
    pub permissions: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct UserGrant {
    pub username: String,
    pub resource_id: String,
    pub permissions: Vec<String>,
}

impl Database {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let db = Self { path };
        db.initialize()?;
        Ok(db)
    }

    fn connect(&self) -> Result<Connection> {
        let conn = Connection::open(&self.path)
            .with_context(|| format!("opening {}", self.path.display()))?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Ok(conn)
    }

    fn initialize(&self) -> Result<()> {
        self.connect()?.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS users (
                id TEXT PRIMARY KEY,
                username TEXT NOT NULL UNIQUE COLLATE NOCASE,
                display_name TEXT NOT NULL,
                password_hash TEXT NOT NULL,
                disabled INTEGER NOT NULL DEFAULT 0,
                is_admin INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS repositories (
                resource_id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                created_by TEXT NOT NULL REFERENCES users(id),
                created_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS grants (
                user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
                resource_id TEXT NOT NULL,
                permissions TEXT NOT NULL,
                PRIMARY KEY(user_id, resource_id)
            );
            "#,
        )?;
        Ok(())
    }

    pub fn create_user(
        &self,
        username: &str,
        display_name: &str,
        password: &str,
        is_admin: bool,
    ) -> Result<User> {
        validate_username(username)?;
        if display_name.trim().is_empty() {
            bail!("display name must not be empty");
        }
        if password.chars().count() < 10 {
            bail!("password must contain at least 10 characters");
        }
        let salt = SaltString::generate(&mut OsRng);
        let password_hash = Argon2::default()
            .hash_password(password.as_bytes(), &salt)
            .map_err(|e| anyhow::anyhow!("hashing password: {e}"))?
            .to_string();
        let user = User {
            id: Uuid::new_v4().to_string(),
            username: username.trim().to_string(),
            display_name: display_name.trim().to_string(),
            password_hash,
            disabled: false,
            is_admin,
        };
        self.connect()?.execute(
            "INSERT INTO users(id, username, display_name, password_hash, disabled, is_admin, created_at) VALUES(?1, ?2, ?3, ?4, 0, ?5, ?6)",
            params![user.id, user.username, user.display_name, user.password_hash, i64::from(is_admin), chrono::Utc::now().timestamp()],
        ).context("creating user (the username may already exist)")?;
        Ok(user)
    }

    pub fn ensure_bootstrap_admin(
        &self,
        username: Option<&str>,
        password: Option<&str>,
    ) -> Result<()> {
        let username = username
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .context("LORE_AUTH_BOOTSTRAP_USERNAME must be set")?;
        if let Some(user) = self.find_user_by_username(username)? {
            self.connect()?.execute(
                "UPDATE users SET disabled = 0, is_admin = 1 WHERE id = ?1",
                [&user.id],
            )?;
            self.set_grant(username, "urc-*", &["*".to_string()])?;
            return Ok(());
        }

        let password = password
            .context("bootstrap administrator does not exist; set LORE_AUTH_BOOTSTRAP_PASSWORD")?;
        let user = self.create_user(username, username, password, true)?;
        self.set_grant(&user.username, "urc-*", &["*".to_string()])?;
        tracing::warn!(
            username,
            "created bootstrap administrator; rotate its password after first login"
        );
        Ok(())
    }

    pub fn authenticate(&self, username: &str, password: &str) -> Result<Option<User>> {
        let Some(user) = self.find_user_by_username(username)? else {
            // Keep the failure path expensive enough to discourage username probing.
            let salt = SaltString::generate(&mut OsRng);
            let _ = Argon2::default().hash_password(password.as_bytes(), &salt);
            return Ok(None);
        };
        if user.disabled {
            return Ok(None);
        }
        let parsed = PasswordHash::new(&user.password_hash)
            .map_err(|e| anyhow::anyhow!("stored password hash is invalid: {e}"))?;
        if Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok()
        {
            Ok(Some(user))
        } else {
            Ok(None)
        }
    }

    pub fn find_user_by_username(&self, username: &str) -> Result<Option<User>> {
        self.connect()?
            .query_row(
                "SELECT id, username, display_name, password_hash, disabled, is_admin FROM users WHERE username = ?1 COLLATE NOCASE",
                [username],
                user_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn find_user_by_id(&self, id: &str) -> Result<Option<User>> {
        self.connect()?
            .query_row(
                "SELECT id, username, display_name, password_hash, disabled, is_admin FROM users WHERE id = ?1",
                [id],
                user_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn list_users(&self) -> Result<Vec<User>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT id, username, display_name, password_hash, disabled, is_admin FROM users ORDER BY username",
        )?;
        Ok(stmt
            .query_map([], user_from_row)?
            .collect::<Result<Vec<_>, _>>()?)
    }

    pub fn set_disabled(&self, username: &str, disabled: bool) -> Result<()> {
        let changed = self.connect()?.execute(
            "UPDATE users SET disabled = ?1 WHERE username = ?2 COLLATE NOCASE",
            params![i64::from(disabled), username],
        )?;
        if changed == 0 {
            bail!("user not found: {username}");
        }
        Ok(())
    }

    pub fn set_password(&self, username: &str, password: &str) -> Result<()> {
        if password.chars().count() < 10 {
            bail!("password must contain at least 10 characters");
        }
        let salt = SaltString::generate(&mut OsRng);
        let hash = Argon2::default()
            .hash_password(password.as_bytes(), &salt)
            .map_err(|e| anyhow::anyhow!("hashing password: {e}"))?
            .to_string();
        let changed = self.connect()?.execute(
            "UPDATE users SET password_hash = ?1 WHERE username = ?2 COLLATE NOCASE",
            params![hash, username],
        )?;
        if changed == 0 {
            bail!("user not found: {username}");
        }
        Ok(())
    }

    pub fn delete_user(&self, username: &str, repository_successor: &str) -> Result<()> {
        let user = self
            .find_user_by_username(username)?
            .with_context(|| format!("user not found: {username}"))?;
        let successor = self
            .find_user_by_username(repository_successor)?
            .with_context(|| format!("repository successor not found: {repository_successor}"))?;
        if user.id == successor.id {
            bail!("a user cannot be their own repository successor");
        }

        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        tx.execute(
            "UPDATE repositories SET created_by = ?1 WHERE created_by = ?2",
            params![successor.id, user.id],
        )?;
        let changed = tx.execute("DELETE FROM users WHERE id = ?1", [&user.id])?;
        if changed == 0 {
            bail!("user not found: {username}");
        }
        tx.commit()?;
        Ok(())
    }

    pub fn set_grant(
        &self,
        username: &str,
        resource_id: &str,
        permissions: &[String],
    ) -> Result<()> {
        validate_resource(resource_id)?;
        if permissions.is_empty() {
            bail!("at least one permission is required");
        }
        let user = self
            .find_user_by_username(username)?
            .with_context(|| format!("user not found: {username}"))?;
        let permissions = normalize_permissions(permissions);
        self.connect()?.execute(
            "INSERT INTO grants(user_id, resource_id, permissions) VALUES(?1, ?2, ?3) ON CONFLICT(user_id, resource_id) DO UPDATE SET permissions = excluded.permissions",
            params![user.id, resource_id, serde_json::to_string(&permissions)?],
        )?;
        Ok(())
    }

    pub fn revoke_grant(&self, username: &str, resource_id: &str) -> Result<()> {
        let user = self
            .find_user_by_username(username)?
            .with_context(|| format!("user not found: {username}"))?;
        self.connect()?.execute(
            "DELETE FROM grants WHERE user_id = ?1 AND resource_id = ?2",
            params![user.id, resource_id],
        )?;
        Ok(())
    }

    pub fn permissions_for(&self, user_id: &str, resource_id: &str) -> Result<Vec<String>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT permissions FROM grants WHERE user_id = ?1 AND resource_id IN (?2, 'urc-*')",
        )?;
        let mut merged = BTreeSet::new();
        for encoded in
            stmt.query_map(params![user_id, resource_id], |row| row.get::<_, String>(0))?
        {
            for permission in serde_json::from_str::<Vec<String>>(&encoded?)? {
                merged.insert(permission);
            }
        }
        Ok(merged.into_iter().collect())
    }

    pub fn list_grants(&self, user_id: &str, resource_filter: &str) -> Result<Vec<Grant>> {
        let conn = self.connect()?;
        let pattern = format!("{resource_filter}%");
        let mut stmt = conn.prepare(
            "SELECT resource_id, permissions FROM grants WHERE user_id = ?1 AND resource_id LIKE ?2 AND resource_id != 'urc-*' ORDER BY resource_id",
        )?;
        Ok(stmt
            .query_map(params![user_id, pattern], |row| {
                let encoded: String = row.get(1)?;
                let permissions = serde_json::from_str(&encoded).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        encoded.len(),
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?;
                Ok(Grant {
                    resource_id: row.get(0)?,
                    permissions,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?)
    }

    pub fn list_all_user_grants(&self) -> Result<Vec<UserGrant>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT users.username, grants.resource_id, grants.permissions FROM grants JOIN users ON users.id = grants.user_id ORDER BY users.username, grants.resource_id",
        )?;
        Ok(stmt
            .query_map([], |row| {
                let encoded: String = row.get(2)?;
                let permissions = serde_json::from_str(&encoded).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        encoded.len(),
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })?;
                Ok(UserGrant {
                    username: row.get(0)?,
                    resource_id: row.get(1)?,
                    permissions,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?)
    }

    pub fn list_repositories(&self, resource_filter: &str) -> Result<Vec<String>> {
        let conn = self.connect()?;
        let pattern = format!("{resource_filter}%");
        let mut stmt = conn.prepare(
            "SELECT resource_id FROM repositories WHERE resource_id LIKE ?1 ORDER BY resource_id",
        )?;
        Ok(stmt
            .query_map([pattern], |row| row.get(0))?
            .collect::<Result<Vec<_>, _>>()?)
    }

    pub fn create_repository(&self, actor: &User, resource_id: &str, name: &str) -> Result<()> {
        validate_resource(resource_id)?;
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO repositories(resource_id, name, created_by, created_at) VALUES(?1, ?2, ?3, ?4)",
            params![resource_id, name, actor.id, chrono::Utc::now().timestamp()],
        ).context("repository resource already exists")?;
        let permissions = serde_json::to_string(&vec![
            "owner",
            "admin",
            "read",
            "write",
            "obliterate",
            "migrate",
        ])?;
        tx.execute(
            "INSERT INTO grants(user_id, resource_id, permissions) VALUES(?1, ?2, ?3) ON CONFLICT(user_id, resource_id) DO UPDATE SET permissions = excluded.permissions",
            params![actor.id, resource_id, permissions],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn delete_repository(&self, resource_id: &str) -> Result<()> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM grants WHERE resource_id = ?1", [resource_id])?;
        let changed = tx.execute(
            "DELETE FROM repositories WHERE resource_id = ?1",
            [resource_id],
        )?;
        if changed == 0 {
            bail!("repository resource not found");
        }
        tx.commit()?;
        Ok(())
    }
}

fn user_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<User> {
    Ok(User {
        id: row.get(0)?,
        username: row.get(1)?,
        display_name: row.get(2)?,
        password_hash: row.get(3)?,
        disabled: row.get::<_, i64>(4)? != 0,
        is_admin: row.get::<_, i64>(5)? != 0,
    })
}

fn validate_username(username: &str) -> Result<()> {
    let username = username.trim();
    if username.len() < 3 || username.len() > 64 {
        bail!("username must contain 3 to 64 characters");
    }
    if !username
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        bail!("username may contain only ASCII letters, digits, '.', '-' and '_'");
    }
    Ok(())
}

fn validate_resource(resource_id: &str) -> Result<()> {
    if resource_id == "urc-*" {
        return Ok(());
    }
    let Some(id) = resource_id.strip_prefix("urc-") else {
        bail!("resource must be 'urc-*' or 'urc-<32 hex characters>'");
    };
    if id.len() != 32 || !id.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("resource must be 'urc-*' or 'urc-<32 hex characters>'");
    }
    Ok(())
}

fn normalize_permissions(permissions: &[String]) -> Vec<String> {
    permissions
        .iter()
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn users_and_grants_round_trip() {
        let temp = tempfile::tempdir().unwrap();
        let db = Database::open(temp.path().join("auth.db")).unwrap();
        let user = db
            .create_user("alice", "Alice", "correct horse battery staple", false)
            .unwrap();
        assert!(
            db.authenticate("alice", "wrong password")
                .unwrap()
                .is_none()
        );
        assert_eq!(
            db.authenticate("alice", "correct horse battery staple")
                .unwrap()
                .unwrap()
                .id,
            user.id
        );

        let resource = "urc-0123456789abcdef0123456789abcdef";
        db.set_grant("alice", resource, &["read".into(), "write".into()])
            .unwrap();
        assert_eq!(
            db.permissions_for(&user.id, resource).unwrap(),
            vec!["read", "write"]
        );
    }

    #[test]
    fn bootstrap_admin_is_restored_without_resetting_password() {
        let temp = tempfile::tempdir().unwrap();
        let db = Database::open(temp.path().join("auth.db")).unwrap();
        db.create_user("root", "Root", "original long password", false)
            .unwrap();
        db.set_disabled("root", true).unwrap();

        db.ensure_bootstrap_admin(Some("root"), None).unwrap();

        let root = db.find_user_by_username("root").unwrap().unwrap();
        assert!(root.is_admin);
        assert!(!root.disabled);
        assert!(
            db.authenticate("root", "original long password")
                .unwrap()
                .is_some()
        );
        assert_eq!(db.permissions_for(&root.id, "urc-*").unwrap(), vec!["*"]);
        assert_eq!(db.list_all_user_grants().unwrap().len(), 1);
    }

    #[test]
    fn deleting_user_removes_its_grants() {
        let temp = tempfile::tempdir().unwrap();
        let db = Database::open(temp.path().join("auth.db")).unwrap();
        let root = db
            .create_user("root", "Root", "root administrator password", true)
            .unwrap();
        let user = db
            .create_user("alice", "Alice", "correct horse battery staple", false)
            .unwrap();
        let resource = "urc-0123456789abcdef0123456789abcdef";
        db.set_grant("alice", resource, &["read".into()]).unwrap();
        db.create_repository(&user, resource, "Alice's repository")
            .unwrap();

        db.delete_user("alice", "root").unwrap();

        assert!(db.find_user_by_username("alice").unwrap().is_none());
        assert!(db.list_grants(&user.id, "").unwrap().is_empty());
        db.delete_repository(resource).unwrap();
        assert!(db.find_user_by_id(&root.id).unwrap().is_some());
    }
}
