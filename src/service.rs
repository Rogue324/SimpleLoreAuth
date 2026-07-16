use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::RwLock;
use tonic::{Request, Response, Status};
use uuid::Uuid;

use crate::db::{Database, User};
use crate::keys::{Claims, ResourceClaim, TokenIssuer};
use crate::proto::epic_urc::target_user;
use crate::proto::epic_urc::urc_auth_api_server::UrcAuthApi;
use crate::proto::epic_urc::*;
use crate::proto::rebac::rebac_api_server::RebacApi;
use crate::proto::rebac::*;

#[derive(Clone, Debug)]
pub struct PendingSession {
    pub client_state: String,
    pub user_token: Option<UserToken>,
    pub expires_at: i64,
    pub failed_attempts: u8,
}

#[derive(Clone, Debug)]
pub struct AdminSession {
    pub user_id: String,
    pub csrf_token: String,
    pub expires_at: i64,
}

const MAX_PENDING_SESSIONS: usize = 10_000;
const MAX_LOGIN_ATTEMPTS: u8 = 5;

#[derive(Clone)]
pub struct AuthService {
    pub db: Database,
    pub tokens: TokenIssuer,
    pub public_base_url: String,
    pub session_ttl_seconds: u64,
    pub sessions: Arc<RwLock<HashMap<String, PendingSession>>>,
    pub bootstrap_username: String,
    pub lore_grpc_url: Option<String>,
    pub admin_sessions: Arc<RwLock<HashMap<String, AdminSession>>>,
}

impl AuthService {
    pub fn new(
        db: Database,
        tokens: TokenIssuer,
        public_base_url: String,
        session_ttl_seconds: u64,
        bootstrap_username: String,
        lore_grpc_url: Option<String>,
    ) -> Self {
        Self {
            db,
            tokens,
            public_base_url: public_base_url.trim_end_matches('/').to_string(),
            session_ttl_seconds,
            sessions: Arc::new(RwLock::new(HashMap::new())),
            bootstrap_username,
            lore_grpc_url: lore_grpc_url.filter(|value| !value.trim().is_empty()),
            admin_sessions: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn create_admin_session(&self, user: &User) -> Option<(String, AdminSession)> {
        if !user.is_admin
            || user.disabled
            || !user.username.eq_ignore_ascii_case(&self.bootstrap_username)
        {
            return None;
        }
        let token = Uuid::new_v4().to_string();
        let session = AdminSession {
            user_id: user.id.clone(),
            csrf_token: Uuid::new_v4().to_string(),
            expires_at: chrono::Utc::now().timestamp() + 8 * 60 * 60,
        };
        let now = chrono::Utc::now().timestamp();
        let mut sessions = self.admin_sessions.write().await;
        sessions.retain(|_, value| value.expires_at >= now);
        sessions.insert(token.clone(), session.clone());
        Some((token, session))
    }

    pub async fn admin_session(&self, token: &str) -> Option<(AdminSession, User)> {
        let session = self.admin_sessions.read().await.get(token).cloned()?;
        if session.expires_at < chrono::Utc::now().timestamp() {
            self.admin_sessions.write().await.remove(token);
            return None;
        }
        let db = self.db.clone();
        let user_id = session.user_id.clone();
        let user = tokio::task::spawn_blocking(move || db.find_user_by_id(&user_id))
            .await
            .ok()?
            .ok()??;
        if user.disabled
            || !user.is_admin
            || !user.username.eq_ignore_ascii_case(&self.bootstrap_username)
        {
            return None;
        }
        Some((session, user))
    }

    pub async fn remove_admin_session(&self, token: &str) {
        self.admin_sessions.write().await.remove(token);
    }

    pub async fn complete_login(
        &self,
        session_code: &str,
        client_state: &str,
        username: &str,
        password: &str,
    ) -> Result<User, LoginFailure> {
        let pending = self
            .sessions
            .read()
            .await
            .get(session_code)
            .cloned()
            .ok_or(LoginFailure::Expired)?;
        if pending.client_state != client_state
            || pending.expires_at < chrono::Utc::now().timestamp()
        {
            return Err(LoginFailure::Expired);
        }
        let db = self.db.clone();
        let username = username.to_string();
        let password = password.to_string();
        let user = tokio::task::spawn_blocking(move || db.authenticate(&username, &password))
            .await
            .map_err(|_| LoginFailure::Internal)?
            .map_err(|_| LoginFailure::Internal)?;
        let Some(user) = user else {
            let mut sessions = self.sessions.write().await;
            let remove = if let Some(session) = sessions.get_mut(session_code) {
                session.failed_attempts = session.failed_attempts.saturating_add(1);
                session.failed_attempts >= MAX_LOGIN_ATTEMPTS
            } else {
                false
            };
            if remove {
                sessions.remove(session_code);
            }
            return Err(LoginFailure::InvalidCredentials);
        };
        let (token, expires) = self
            .tokens
            .issue_authentication(&user)
            .map_err(|_| LoginFailure::Internal)?;
        let user_token = UserToken {
            user_token: token,
            expires_at: expires as i64,
            user_id: user.id.clone(),
            user_name: user.display_name.clone(),
        };
        if let Some(session) = self.sessions.write().await.get_mut(session_code) {
            session.user_token = Some(user_token);
        }
        Ok(user)
    }

    async fn actor_from_token(&self, token: &str) -> Result<(Claims, User), Status> {
        let claims = self
            .tokens
            .verify(token)
            .map_err(|_| Status::unauthenticated("invalid or expired token"))?;
        let db = self.db.clone();
        let id = claims.user_id.clone();
        let user = tokio::task::spawn_blocking(move || db.find_user_by_id(&id))
            .await
            .map_err(|_| Status::internal("database task failed"))?
            .map_err(internal)?
            .filter(|user| !user.disabled)
            .ok_or_else(|| Status::unauthenticated("user is disabled or no longer exists"))?;
        Ok((claims, user))
    }

    async fn actor_from_request<T>(&self, request: &Request<T>) -> Result<(Claims, User), Status> {
        let token = bearer(request)?;
        self.actor_from_token(token).await
    }

    async fn actor_from_target_or_request<T>(
        &self,
        request: &Request<T>,
        target: Option<&TargetUser>,
    ) -> Result<(Claims, User), Status> {
        if let Some(TargetUser {
            user: Some(target_user::User::UserToken(token)),
        }) = target
        {
            self.actor_from_token(token).await
        } else {
            self.actor_from_request(request).await
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum LoginFailure {
    InvalidCredentials,
    Expired,
    Internal,
}

#[tonic::async_trait]
impl UrcAuthApi for AuthService {
    async fn health_check(
        &self,
        _request: Request<HealthCheckRequest>,
    ) -> Result<Response<HealthCheckResponse>, Status> {
        Ok(Response::new(HealthCheckResponse {
            status: "ok".into(),
        }))
    }

    async fn start_auth_session(
        &self,
        request: Request<StartAuthSessionRequest>,
    ) -> Result<Response<StartAuthSessionResponse>, Status> {
        let client_state = request.into_inner().client_state;
        if client_state.trim().is_empty() || client_state.len() > 256 {
            return Err(Status::invalid_argument("invalid client_state"));
        }
        let session_code = Uuid::new_v4().to_string();
        let expires_at = chrono::Utc::now().timestamp() + self.session_ttl_seconds as i64;
        let mut sessions = self.sessions.write().await;
        sessions.retain(|_, value| value.expires_at >= chrono::Utc::now().timestamp());
        if sessions.len() >= MAX_PENDING_SESSIONS {
            return Err(Status::resource_exhausted(
                "too many pending login sessions",
            ));
        }
        sessions.insert(
            session_code.clone(),
            PendingSession {
                client_state: client_state.clone(),
                user_token: None,
                expires_at,
                failed_attempts: 0,
            },
        );
        let login_url = format!(
            "{}/login?session_code={}&client_state={}",
            self.public_base_url,
            urlencoding::encode(&session_code),
            urlencoding::encode(&client_state)
        );
        Ok(Response::new(StartAuthSessionResponse {
            session_code,
            login_url,
        }))
    }

    async fn get_auth_session(
        &self,
        request: Request<GetAuthSessionRequest>,
    ) -> Result<Response<GetAuthSessionResponse>, Status> {
        let request = request.into_inner();
        let mut sessions = self.sessions.write().await;
        let session = sessions
            .get(&request.session_code)
            .ok_or_else(|| Status::not_found("auth session not found"))?;
        if session.client_state != request.client_state {
            return Err(Status::permission_denied("client_state mismatch"));
        }
        if session.expires_at < chrono::Utc::now().timestamp() {
            sessions.remove(&request.session_code);
            return Err(Status::not_found("auth session expired"));
        }
        let token = session.user_token.clone();
        if token.is_some() {
            sessions.remove(&request.session_code);
        }
        Ok(Response::new(GetAuthSessionResponse { user_token: token }))
    }

    async fn refresh_auth_session(
        &self,
        request: Request<RefreshAuthSessionRequest>,
    ) -> Result<Response<RefreshAuthSessionResponse>, Status> {
        let (_, user) = self.actor_from_request(&request).await?;
        let (token, expires) = self.tokens.issue_authentication(&user).map_err(internal)?;
        Ok(Response::new(RefreshAuthSessionResponse {
            user_token: Some(as_user_token(&user, token, expires)),
        }))
    }

    async fn verify_user(
        &self,
        request: Request<VerifyUserRequest>,
    ) -> Result<Response<VerifyUserResponse>, Status> {
        let target = request.get_ref().target_user.as_ref();
        let (_, user) = self.actor_from_target_or_request(&request, target).await?;
        Ok(Response::new(VerifyUserResponse {
            user_info: Some(as_user_info(&user)),
        }))
    }

    async fn exchange_external_token_for_user_token(
        &self,
        _request: Request<ExchangeExternalTokenForUserTokenRequest>,
    ) -> Result<Response<ExchangeExternalTokenForUserTokenResponse>, Status> {
        Err(Status::unimplemented(
            "external identity providers are not configured",
        ))
    }

    async fn exchange_api_key_for_user_token(
        &self,
        _request: Request<ExchangeApiKeyForUserTokenRequest>,
    ) -> Result<Response<ExchangeApiKeyForUserTokenResponse>, Status> {
        Err(Status::unimplemented("API keys are not enabled"))
    }

    async fn exchange_user_token_for_multiresource_token(
        &self,
        request: Request<ExchangeUserTokenForMultiresourceTokenRequest>,
    ) -> Result<Response<ExchangeUserTokenForMultiresourceTokenResponse>, Status> {
        let (_, user) = self.actor_from_request(&request).await?;
        let resource_ids = request.get_ref().resource_id.clone();
        if resource_ids.is_empty() || resource_ids.len() > 100 {
            return Err(Status::invalid_argument(
                "one to 100 resources are required",
            ));
        }
        let db = self.db.clone();
        let user_id = user.id.clone();
        let resources =
            tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<ResourceClaim>> {
                let mut allowed = Vec::new();
                for resource_id in resource_ids {
                    let permission = db.permissions_for(&user_id, &resource_id)?;
                    if !permission.is_empty() {
                        allowed.push(ResourceClaim {
                            resource_id,
                            permission,
                        });
                    }
                }
                Ok(allowed)
            })
            .await
            .map_err(|_| Status::internal("database task failed"))?
            .map_err(internal)?;
        if resources.is_empty() {
            return Err(Status::permission_denied(
                "user is not authorized for the requested resources",
            ));
        }
        let (token, expires) = self
            .tokens
            .issue_authorization(&user, resources)
            .map_err(internal)?;
        Ok(Response::new(
            ExchangeUserTokenForMultiresourceTokenResponse {
                token: Some(as_user_token(&user, token, expires)),
            },
        ))
    }

    async fn check_user_permission(
        &self,
        request: Request<CheckUserPermissionRequest>,
    ) -> Result<Response<CheckUserPermissionResponse>, Status> {
        let target = request.get_ref().target_user.as_ref();
        let (_, user) = self.actor_from_target_or_request(&request, target).await?;
        let resource_ids = request.get_ref().resource_id.clone();
        let db = self.db.clone();
        let user_id = user.id.clone();
        let (allowed, denied) = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let mut allowed = Vec::new();
            let mut denied = Vec::new();
            for resource_id in resource_ids {
                let permission = db.permissions_for(&user_id, &resource_id)?;
                let item = ResourcePermission {
                    resource_id,
                    permission,
                };
                if item.permission.is_empty() {
                    denied.push(item)
                } else {
                    allowed.push(item)
                }
            }
            Ok((allowed, denied))
        })
        .await
        .map_err(|_| Status::internal("database task failed"))?
        .map_err(internal)?;
        Ok(Response::new(CheckUserPermissionResponse {
            allowed_resource_permission: allowed,
            denied_resource_permission: denied,
        }))
    }

    async fn lookup_user_permissions(
        &self,
        request: Request<LookupUserPermissionsRequest>,
    ) -> Result<Response<LookupUserPermissionsResponse>, Status> {
        let (_, user) = self.actor_from_request(&request).await?;
        let filter = request.get_ref().resource_filter.clone();
        let db = self.db.clone();
        let user_id = user.id.clone();
        let permissions =
            tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<ResourcePermission>> {
                let wildcard = db.permissions_for(&user_id, "urc-*")?;
                if !wildcard.is_empty() {
                    return Ok(db
                        .list_repositories(&filter)?
                        .into_iter()
                        .map(|resource_id| ResourcePermission {
                            resource_id,
                            permission: wildcard.clone(),
                        })
                        .collect());
                }
                Ok(db
                    .list_grants(&user_id, &filter)?
                    .into_iter()
                    .map(|grant| ResourcePermission {
                        resource_id: grant.resource_id,
                        permission: grant.permissions,
                    })
                    .collect())
            })
            .await
            .map_err(|_| Status::internal("database task failed"))?
            .map_err(internal)?;
        Ok(Response::new(LookupUserPermissionsResponse {
            resource_permission: permissions,
            next_page_token: None,
        }))
    }

    async fn get_user_info(
        &self,
        request: Request<GetUserInfoRequest>,
    ) -> Result<Response<GetUserInfoResponse>, Status> {
        self.actor_from_request(&request).await?;
        let ids = request.get_ref().user_id.clone();
        let db = self.db.clone();
        let users = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<UserInfo>> {
            let mut result = Vec::new();
            for id in ids {
                if let Some(user) = db.find_user_by_id(&id)? {
                    result.push(as_user_info(&user));
                }
            }
            Ok(result)
        })
        .await
        .map_err(|_| Status::internal("database task failed"))?
        .map_err(internal)?;
        Ok(Response::new(GetUserInfoResponse { user_info: users }))
    }

    async fn get_user_id(
        &self,
        request: Request<GetUserIdRequest>,
    ) -> Result<Response<GetUserIdResponse>, Status> {
        self.actor_from_request(&request).await?;
        let name = request.get_ref().user_display_name.clone();
        let db = self.db.clone();
        let user = tokio::task::spawn_blocking(move || db.find_user_by_username(&name))
            .await
            .map_err(|_| Status::internal("database task failed"))?
            .map_err(internal)?;
        Ok(Response::new(GetUserIdResponse {
            user_info: user.as_ref().map(as_user_info),
        }))
    }

    async fn get_provider_user_id(
        &self,
        request: Request<GetProviderUserIdRequest>,
    ) -> Result<Response<GetProviderUserIdResponse>, Status> {
        self.actor_from_request(&request).await?;
        let user_id = request.get_ref().user_id.clone();
        Ok(Response::new(GetProviderUserIdResponse {
            provider_user_id: user_id.clone(),
            user_id,
        }))
    }
}

#[tonic::async_trait]
impl RebacApi for AuthService {
    async fn create_resource(
        &self,
        request: Request<CreateResourceRequest>,
    ) -> Result<Response<CreateResourceResponse>, Status> {
        let (_, user) = self.actor_from_request(&request).await?;
        let body = request.into_inner();
        let db = self.db.clone();
        let actor = user.clone();
        let resource_id = body.resource_id.clone();
        let name = body.resource_name;
        // A wildcard grant would also authorize every repository because Lore's
        // general interceptor currently checks resource membership, not read/write.
        // Keep repository creation restricted to explicit administrators.
        if !user.is_admin {
            return Err(Status::permission_denied(
                "repository creation is not allowed",
            ));
        }
        tokio::task::spawn_blocking(move || db.create_repository(&actor, &resource_id, &name))
            .await
            .map_err(|_| Status::internal("database task failed"))?
            .map_err(|error| {
                if error.to_string().contains("already exists") {
                    Status::already_exists("resource already exists")
                } else {
                    internal(error)
                }
            })?;
        Ok(Response::new(CreateResourceResponse {}))
    }

    async fn delete_resource(
        &self,
        request: Request<DeleteResourceRequest>,
    ) -> Result<Response<DeleteResourceResponse>, Status> {
        let (_, user) = self.actor_from_request(&request).await?;
        let resource_id = request.get_ref().resource_id.clone();
        let db = self.db.clone();
        let user_id = user.id.clone();
        let permission_resource = resource_id.clone();
        let permissions =
            tokio::task::spawn_blocking(move || db.permissions_for(&user_id, &permission_resource))
                .await
                .map_err(|_| Status::internal("database task failed"))?
                .map_err(internal)?;
        if !user.is_admin
            && !permissions
                .iter()
                .any(|value| matches!(value.as_str(), "*" | "owner" | "admin"))
        {
            return Err(Status::permission_denied(
                "repository deletion is not allowed",
            ));
        }
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || db.delete_repository(&resource_id))
            .await
            .map_err(|_| Status::internal("database task failed"))?
            .map_err(internal)?;
        Ok(Response::new(DeleteResourceResponse {}))
    }
}

fn bearer<T>(request: &Request<T>) -> Result<&str, Status> {
    let raw = request
        .metadata()
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| Status::unauthenticated("authorization header required"))?;
    raw.strip_prefix("Bearer ")
        .ok_or_else(|| Status::unauthenticated("Bearer token required"))
}

fn as_user_info(user: &User) -> UserInfo {
    UserInfo {
        user_id: user.id.clone(),
        display_name: user.display_name.clone(),
    }
}

fn as_user_token(user: &User, token: String, expires: u64) -> UserToken {
    UserToken {
        user_token: token,
        expires_at: expires as i64,
        user_id: user.id.clone(),
        user_name: user.display_name.clone(),
    }
}

fn internal(error: impl std::fmt::Display) -> Status {
    tracing::error!(error = %error, "authentication service error");
    Status::internal("authentication service error")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn browser_login_and_repository_exchange() {
        let temp = tempfile::tempdir().unwrap();
        let db = Database::open(temp.path().join("auth.db")).unwrap();
        let user = db
            .create_user("alice", "Alice", "correct horse battery staple", false)
            .unwrap();
        let resource = "urc-0123456789abcdef0123456789abcdef";
        db.set_grant("alice", resource, &["read".into(), "write".into()])
            .unwrap();
        let tokens = TokenIssuer::load_or_create(
            temp.path().join("jwt.pem"),
            "https://auth.example.test".into(),
            "lore-service".into(),
            "test".into(),
            600,
        )
        .unwrap();
        let service = AuthService::new(
            db,
            tokens.clone(),
            "https://auth.example.test".into(),
            300,
            "admin".into(),
            None,
        );

        let started = UrcAuthApi::start_auth_session(
            &service,
            Request::new(StartAuthSessionRequest {
                client_state: "state-1".into(),
            }),
        )
        .await
        .unwrap()
        .into_inner();
        service
            .complete_login(
                &started.session_code,
                "state-1",
                "alice",
                "correct horse battery staple",
            )
            .await
            .unwrap();
        let authn = UrcAuthApi::get_auth_session(
            &service,
            Request::new(GetAuthSessionRequest {
                session_code: started.session_code,
                client_state: "state-1".into(),
            }),
        )
        .await
        .unwrap()
        .into_inner()
        .user_token
        .unwrap();
        assert_eq!(authn.user_id, user.id);

        let mut request = Request::new(ExchangeUserTokenForMultiresourceTokenRequest {
            resource_id: vec![resource.into()],
        });
        request.metadata_mut().insert(
            "authorization",
            format!("Bearer {}", authn.user_token).parse().unwrap(),
        );
        let authz = UrcAuthApi::exchange_user_token_for_multiresource_token(&service, request)
            .await
            .unwrap()
            .into_inner()
            .token
            .unwrap();
        let claims = tokens.verify(&authz.user_token).unwrap();
        let resources = claims.resources.unwrap();
        assert_eq!(resources[0].resource_id, resource);
        assert_eq!(resources[0].permission, vec!["read", "write"]);
    }
}
