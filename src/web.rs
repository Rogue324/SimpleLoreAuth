use axum::extract::{Form, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;

use crate::db::User;
use crate::keys::ResourceClaim;
use crate::lore_admin::{LoreAdminClient, grpc_error_message};
use crate::service::{AdminSession, AuthService, LoginFailure};

const ADMIN_COOKIE: &str = "lore_auth_admin";

#[derive(Debug, Deserialize)]
struct LoginQuery {
    session_code: String,
    client_state: String,
}

#[derive(Debug, Deserialize)]
struct LoginForm {
    session_code: String,
    client_state: String,
    username: String,
    password: String,
}

#[derive(Debug, Default, Deserialize)]
struct AdminQuery {
    message: Option<String>,
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AdminLoginForm {
    username: String,
    password: String,
}

#[derive(Debug, Deserialize)]
struct CreateUserForm {
    csrf_token: String,
    username: String,
    display_name: String,
    password: String,
}

#[derive(Debug, Deserialize)]
struct UserActionForm {
    csrf_token: String,
    username: String,
}

#[derive(Debug, Deserialize)]
struct PasswordForm {
    csrf_token: String,
    username: String,
    password: String,
}

#[derive(Debug, Deserialize)]
struct GrantSetForm {
    csrf_token: String,
    username: String,
    resource_id: String,
    access_level: String,
}

#[derive(Debug, Deserialize)]
struct GrantRevokeForm {
    csrf_token: String,
    username: String,
    resource_id: String,
}

#[derive(Debug, Deserialize)]
struct RepositoryHistoryQuery {
    resource_id: String,
}

#[derive(Debug, Deserialize)]
struct RepositoryDeleteForm {
    csrf_token: String,
    resource_id: String,
    confirmation: String,
}

#[derive(Debug, Deserialize)]
struct CsrfForm {
    csrf_token: String,
}

pub fn router(state: AuthService) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/health", get(health))
        .route("/.well-known/jwks.json", get(jwks))
        .route("/login", get(login_page).post(login))
        .route("/admin", get(admin))
        .route("/admin/login", get(admin_login_get).post(admin_login))
        .route("/admin/logout", axum::routing::post(admin_logout))
        .route(
            "/admin/users/create",
            axum::routing::post(admin_create_user),
        )
        .route(
            "/admin/users/delete",
            axum::routing::post(admin_delete_user),
        )
        .route(
            "/admin/users/enable",
            axum::routing::post(admin_enable_user),
        )
        .route(
            "/admin/users/disable",
            axum::routing::post(admin_disable_user),
        )
        .route(
            "/admin/users/password",
            axum::routing::post(admin_set_password),
        )
        .route("/admin/grants/set", axum::routing::post(admin_set_grant))
        .route(
            "/admin/grants/revoke",
            axum::routing::post(admin_revoke_grant),
        )
        .route("/admin/repositories/history", get(admin_repository_history))
        .route(
            "/admin/repositories/delete",
            axum::routing::post(admin_delete_repository),
        )
        .with_state(state)
}

async fn index() -> Response {
    secured_html(
        StatusCode::OK,
        page(
            "Lore 认证服务",
            "<p>认证服务运行正常。</p><p><a href=\"/admin\">打开管理后台</a></p>",
        ),
    )
}

async fn health() -> impl IntoResponse {
    Json(serde_json::json!({"status": "ok"}))
}

async fn jwks(State(state): State<AuthService>) -> impl IntoResponse {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=300"),
    );
    (headers, Json(state.tokens.jwks()))
}

async fn login_page(State(state): State<AuthService>, Query(query): Query<LoginQuery>) -> Response {
    let valid = state
        .sessions
        .read()
        .await
        .get(&query.session_code)
        .is_some_and(|session| {
            session.client_state == query.client_state
                && session.expires_at >= chrono::Utc::now().timestamp()
        });
    if !valid {
        return secured_html(
            StatusCode::BAD_REQUEST,
            page("登录已过期", "<p>这次登录请求无效或已经过期。</p>"),
        );
    }
    secured_html(StatusCode::OK, login_form(&query, None))
}

async fn login(State(state): State<AuthService>, Form(form): Form<LoginForm>) -> Response {
    match state
        .complete_login(
            &form.session_code,
            &form.client_state,
            &form.username,
            &form.password,
        )
        .await
    {
        Ok(_) => secured_html(
            StatusCode::OK,
            page(
                "登录成功",
                "<p>身份验证成功。你可以关闭此窗口并返回 Lore。</p>",
            ),
        ),
        Err(LoginFailure::InvalidCredentials) => secured_html(
            StatusCode::UNAUTHORIZED,
            login_form(
                &LoginQuery {
                    session_code: form.session_code,
                    client_state: form.client_state,
                },
                Some("用户名或密码错误。"),
            ),
        ),
        Err(LoginFailure::Expired) => secured_html(
            StatusCode::BAD_REQUEST,
            page(
                "登录已过期",
                "<p>这次登录请求已经过期，请返回 Lore 重新发起登录。</p>",
            ),
        ),
        Err(LoginFailure::Internal) => secured_html(
            StatusCode::INTERNAL_SERVER_ERROR,
            page("登录失败", "<p>认证服务发生内部错误。</p>"),
        ),
    }
}

async fn admin(
    State(state): State<AuthService>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
) -> Response {
    let Some((_, session, user)) = admin_actor(&state, &headers).await else {
        return admin_login_page(query.error.as_deref());
    };
    render_admin(
        &state,
        &session,
        &user,
        query.message.as_deref(),
        query.error.as_deref(),
    )
    .await
}

async fn admin_login_get() -> Response {
    Redirect::to("/admin").into_response()
}

async fn admin_login(
    State(state): State<AuthService>,
    Form(form): Form<AdminLoginForm>,
) -> Response {
    let db = state.db.clone();
    let username = form.username;
    let password = form.password;
    let user = tokio::task::spawn_blocking(move || db.authenticate(&username, &password)).await;
    let Ok(Ok(Some(user))) = user else {
        return admin_login_page(Some("用户名或密码错误。"));
    };
    let Some((token, _)) = state.create_admin_session(&user).await else {
        return admin_login_page(Some("只有终极管理员可以使用此后台。"));
    };
    let mut response = Redirect::to("/admin").into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&format!(
            "{ADMIN_COOKIE}={token}; Path=/admin; HttpOnly; Secure; SameSite=Strict; Max-Age=28800"
        ))
        .expect("admin cookie is valid"),
    );
    response
}

async fn admin_logout(
    State(state): State<AuthService>,
    headers: HeaderMap,
    Form(form): Form<CsrfForm>,
) -> Response {
    let Some((token, session, _)) = admin_actor(&state, &headers).await else {
        return admin_login_page(None);
    };
    if !valid_csrf(&session, &form.csrf_token) {
        return admin_error(StatusCode::FORBIDDEN, "安全校验失败，请刷新页面后重试。 ");
    }
    state.remove_admin_session(&token).await;
    let mut response = Redirect::to("/admin").into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_static(
            "lore_auth_admin=; Path=/admin; HttpOnly; Secure; SameSite=Strict; Max-Age=0",
        ),
    );
    response
}

async fn admin_create_user(
    State(state): State<AuthService>,
    headers: HeaderMap,
    Form(form): Form<CreateUserForm>,
) -> Response {
    let Some((_, session, _)) = admin_actor(&state, &headers).await else {
        return admin_login_page(None);
    };
    if !valid_csrf(&session, &form.csrf_token) {
        return admin_error(StatusCode::FORBIDDEN, "安全校验失败，请刷新页面后重试。 ");
    }
    let db = state.db.clone();
    let username = form.username;
    let display_name = form.display_name;
    let password = form.password;
    match tokio::task::spawn_blocking(move || {
        db.create_user(&username, &display_name, &password, false)
    })
    .await
    {
        Ok(Ok(user)) => admin_redirect("message", &format!("用户 {} 创建成功。", user.username)),
        Ok(Err(error)) => admin_operation_error(error),
        Err(_) => admin_redirect("error", "数据库任务执行失败。"),
    }
}

async fn admin_delete_user(
    State(state): State<AuthService>,
    headers: HeaderMap,
    Form(form): Form<UserActionForm>,
) -> Response {
    let Some((_, session, _)) = admin_actor(&state, &headers).await else {
        return admin_login_page(None);
    };
    if !valid_csrf(&session, &form.csrf_token) {
        return admin_error(StatusCode::FORBIDDEN, "安全校验失败，请刷新页面后重试。 ");
    }
    if form
        .username
        .eq_ignore_ascii_case(&state.bootstrap_username)
    {
        return admin_redirect("error", "终极管理员不能被删除。 ");
    }
    let db = state.db.clone();
    let username = form.username;
    let successor = state.bootstrap_username.clone();
    match tokio::task::spawn_blocking(move || db.delete_user(&username, &successor)).await {
        Ok(Ok(())) => admin_redirect("message", "用户已删除，其名下仓库已转交给终极管理员。"),
        Ok(Err(error)) => admin_operation_error(error),
        Err(_) => admin_redirect("error", "数据库任务执行失败。"),
    }
}

async fn admin_enable_user(
    State(state): State<AuthService>,
    headers: HeaderMap,
    Form(form): Form<UserActionForm>,
) -> Response {
    set_user_disabled(state, headers, form, false).await
}

async fn admin_disable_user(
    State(state): State<AuthService>,
    headers: HeaderMap,
    Form(form): Form<UserActionForm>,
) -> Response {
    set_user_disabled(state, headers, form, true).await
}

async fn set_user_disabled(
    state: AuthService,
    headers: HeaderMap,
    form: UserActionForm,
    disabled: bool,
) -> Response {
    let Some((_, session, _)) = admin_actor(&state, &headers).await else {
        return admin_login_page(None);
    };
    if !valid_csrf(&session, &form.csrf_token) {
        return admin_error(StatusCode::FORBIDDEN, "安全校验失败，请刷新页面后重试。 ");
    }
    if form
        .username
        .eq_ignore_ascii_case(&state.bootstrap_username)
    {
        return admin_redirect("error", "终极管理员不能被禁用。 ");
    }
    let db = state.db.clone();
    let username = form.username;
    match tokio::task::spawn_blocking(move || db.set_disabled(&username, disabled)).await {
        Ok(Ok(())) => admin_redirect(
            "message",
            if disabled {
                "用户已禁用。"
            } else {
                "用户已启用。"
            },
        ),
        Ok(Err(error)) => admin_operation_error(error),
        Err(_) => admin_redirect("error", "数据库任务执行失败。"),
    }
}

async fn admin_set_password(
    State(state): State<AuthService>,
    headers: HeaderMap,
    Form(form): Form<PasswordForm>,
) -> Response {
    let Some((_, session, _)) = admin_actor(&state, &headers).await else {
        return admin_login_page(None);
    };
    if !valid_csrf(&session, &form.csrf_token) {
        return admin_error(StatusCode::FORBIDDEN, "安全校验失败，请刷新页面后重试。 ");
    }
    let db = state.db.clone();
    let username = form.username;
    let password = form.password;
    match tokio::task::spawn_blocking(move || db.set_password(&username, &password)).await {
        Ok(Ok(())) => admin_redirect("message", "密码已更新。"),
        Ok(Err(error)) => admin_operation_error(error),
        Err(_) => admin_redirect("error", "数据库任务执行失败。"),
    }
}

async fn admin_set_grant(
    State(state): State<AuthService>,
    headers: HeaderMap,
    Form(form): Form<GrantSetForm>,
) -> Response {
    let Some((_, session, _)) = admin_actor(&state, &headers).await else {
        return admin_login_page(None);
    };
    if !valid_csrf(&session, &form.csrf_token) {
        return admin_error(StatusCode::FORBIDDEN, "安全校验失败，请刷新页面后重试。 ");
    }
    let permissions = match form.access_level.as_str() {
        "read" => vec!["read".to_string()],
        "write" => vec!["read".to_string(), "write".to_string()],
        "admin" => ["owner", "admin", "read", "write", "obliterate", "migrate"]
            .into_iter()
            .map(str::to_string)
            .collect(),
        "all" => vec!["*".to_string()],
        _ => return admin_redirect("error", "请选择有效的权限级别。"),
    };
    let db = state.db.clone();
    let username = form.username;
    let resource_id = form.resource_id;
    match tokio::task::spawn_blocking(move || db.set_grant(&username, &resource_id, &permissions))
        .await
    {
        Ok(Ok(())) => admin_redirect("message", "仓库授权已保存。"),
        Ok(Err(error)) => admin_operation_error(error),
        Err(_) => admin_redirect("error", "数据库任务执行失败。"),
    }
}

async fn admin_revoke_grant(
    State(state): State<AuthService>,
    headers: HeaderMap,
    Form(form): Form<GrantRevokeForm>,
) -> Response {
    let Some((_, session, _)) = admin_actor(&state, &headers).await else {
        return admin_login_page(None);
    };
    if !valid_csrf(&session, &form.csrf_token) {
        return admin_error(StatusCode::FORBIDDEN, "安全校验失败，请刷新页面后重试。 ");
    }
    if form
        .username
        .eq_ignore_ascii_case(&state.bootstrap_username)
        && form.resource_id == "urc-*"
    {
        return admin_redirect("error", "不能撤销终极管理员的全局权限。 ");
    }
    let db = state.db.clone();
    let username = form.username;
    let resource_id = form.resource_id;
    match tokio::task::spawn_blocking(move || db.revoke_grant(&username, &resource_id)).await {
        Ok(Ok(())) => admin_redirect("message", "仓库授权已撤销。"),
        Ok(Err(error)) => admin_operation_error(error),
        Err(_) => admin_redirect("error", "数据库任务执行失败。"),
    }
}

async fn admin_repository_history(
    State(state): State<AuthService>,
    headers: HeaderMap,
    Query(query): Query<RepositoryHistoryQuery>,
) -> Response {
    let Some((_, _, actor)) = admin_actor(&state, &headers).await else {
        return admin_login_page(None);
    };
    let Some(endpoint) = state.lore_grpc_url.as_deref() else {
        return admin_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "尚未配置 LORE_AUTH_LORE_GRPC_URL，无法读取提交历史。",
        );
    };
    let client = LoreAdminClient::new(endpoint);
    let Ok((authentication_token, _)) = state.tokens.issue_authentication(&actor) else {
        return admin_error(StatusCode::INTERNAL_SERVER_ERROR, "无法签发管理令牌。");
    };
    let repositories = match client.list_repositories(&authentication_token).await {
        Ok(repositories) => repositories,
        Err(error) => return admin_lore_error("读取 Lore 仓库列表失败", error),
    };
    let Some(repository) = repositories
        .into_iter()
        .find(|repository| repository.resource_id == query.resource_id)
    else {
        return admin_error(StatusCode::NOT_FOUND, "仓库不存在或已被删除。");
    };
    let Ok((authorization_token, _)) = state.tokens.issue_authorization(
        &actor,
        vec![ResourceClaim {
            resource_id: repository.resource_id.clone(),
            permission: vec!["*".into()],
        }],
    ) else {
        return admin_error(StatusCode::INTERNAL_SERVER_ERROR, "无法签发仓库管理令牌。");
    };
    let history = match client.history(&authorization_token, &repository).await {
        Ok(history) => history,
        Err(error) => return admin_lore_error("读取提交历史失败", error),
    };
    let mut rows = String::new();
    for entry in history {
        rows.push_str(&format!(
            r#"<tr><td>{}</td><td>#{}</td><td>{}</td><td>{}</td><td>{}</td><td><code>{}</code></td></tr>"#,
            escape(&entry.branch_name),
            entry.number,
            escape(if entry.message.is_empty() { "（无提交说明）" } else { &entry.message }),
            escape(if entry.committed_by.is_empty() { "未知" } else { &entry.committed_by }),
            format_timestamp(entry.timestamp),
            escape(&entry.signature.chars().take(16).collect::<String>()),
        ));
    }
    if rows.is_empty() {
        rows.push_str(r#"<tr><td colspan="6" class="muted">该仓库暂无提交记录。</td></tr>"#);
    }
    let body = format!(
        r#"<p><a href="/admin">← 返回管理后台</a></p><h2>仓库提交历史</h2><p><strong>{}</strong><br><code>{}</code></p><p class="muted">最多显示最近 50 条提交，按提交时间倒序排列。</p><div style="overflow-x:auto"><table><thead><tr><th>分支</th><th>修订</th><th>提交说明</th><th>提交人</th><th>提交时间</th><th>签名</th></tr></thead><tbody>{rows}</tbody></table></div>"#,
        escape(&repository.name),
        escape(&repository.resource_id),
    );
    secured_html(StatusCode::OK, admin_page("仓库提交历史", &body))
}

async fn admin_delete_repository(
    State(state): State<AuthService>,
    headers: HeaderMap,
    Form(form): Form<RepositoryDeleteForm>,
) -> Response {
    let Some((_, session, actor)) = admin_actor(&state, &headers).await else {
        return admin_login_page(None);
    };
    if !valid_csrf(&session, &form.csrf_token) {
        return admin_error(StatusCode::FORBIDDEN, "安全校验失败，请刷新页面后重试。 ");
    }
    let Some(endpoint) = state.lore_grpc_url.as_deref() else {
        return admin_redirect("error", "尚未配置 LORE_AUTH_LORE_GRPC_URL，不能删除仓库。");
    };
    let client = LoreAdminClient::new(endpoint);
    let Ok((token, _)) = state.tokens.issue_authentication(&actor) else {
        return admin_redirect("error", "无法签发管理令牌。");
    };
    let repositories = match client.list_repositories(&token).await {
        Ok(repositories) => repositories,
        Err(error) => return admin_lore_redirect("读取 Lore 仓库列表失败", error),
    };
    let Some(repository) = repositories
        .into_iter()
        .find(|repository| repository.resource_id == form.resource_id)
    else {
        return admin_redirect("error", "仓库不存在或已被删除。");
    };
    if form.confirmation != repository.name {
        return admin_redirect("error", "确认名称不匹配，仓库未删除。");
    }
    match client
        .delete_repository(&token, &repository.resource_id)
        .await
    {
        Ok(()) => admin_redirect(
            "message",
            &format!("仓库 {} 已从 Lore Server 永久删除。", repository.name),
        ),
        Err(error) => admin_lore_redirect("Lore Server 删除仓库失败", error),
    }
}

async fn admin_actor(
    state: &AuthService,
    headers: &HeaderMap,
) -> Option<(String, AdminSession, User)> {
    let token = cookie(headers, ADMIN_COOKIE)?.to_string();
    let (session, user) = state.admin_session(&token).await?;
    Some((token, session, user))
}

async fn render_admin(
    state: &AuthService,
    session: &AdminSession,
    actor: &User,
    message: Option<&str>,
    error: Option<&str>,
) -> Response {
    let db = state.db.clone();
    let data = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        Ok((
            db.list_users()?,
            db.list_repositories("")?,
            db.list_all_user_grants()?,
        ))
    })
    .await;
    let (users, local_repositories, grants) = match data {
        Ok(Ok(data)) => data,
        _ => return admin_error(StatusCode::INTERNAL_SERVER_ERROR, "无法读取后台数据。"),
    };
    let (live_repositories, repository_notice) = if let Some(endpoint) =
        state.lore_grpc_url.as_deref()
    {
        match state.tokens.issue_authentication(actor) {
            Ok((token, _)) => match LoreAdminClient::new(endpoint)
                .list_repositories(&token)
                .await
            {
                Ok(repositories) => (repositories, String::new()),
                Err(error) => (
                    Vec::new(),
                    format!(
                        "<p class=\"notice error\">无法读取 Lore Server 仓库：{}</p>",
                        escape(&grpc_error_message(&error))
                    ),
                ),
            },
            Err(_) => (
                Vec::new(),
                "<p class=\"notice error\">无法签发仓库管理令牌。</p>".to_string(),
            ),
        }
    } else {
        (
            Vec::new(),
            "<p class=\"notice error\">未配置 LORE_AUTH_LORE_GRPC_URL；仓库列表、提交历史和删除功能不可用。</p>".to_string(),
        )
    };
    let csrf = escape(&session.csrf_token);
    let notice = message
        .map(|value| format!("<p class=\"notice success\">{}</p>", escape(value)))
        .or_else(|| error.map(|value| format!("<p class=\"notice error\">{}</p>", escape(value))))
        .unwrap_or_default();
    let mut rows = String::new();
    let mut user_options = String::new();
    for user in &users {
        if !user
            .username
            .eq_ignore_ascii_case(&state.bootstrap_username)
        {
            user_options.push_str(&format!(
                r#"<option value="{}">{}（{}）</option>"#,
                escape(&user.username),
                escape(&user.display_name),
                escape(&user.username),
            ));
        }
        let username = escape(&user.username);
        let protected = user
            .username
            .eq_ignore_ascii_case(&state.bootstrap_username);
        let role = if protected {
            "终极管理员"
        } else if user.is_admin {
            "管理员"
        } else {
            "普通用户"
        };
        let status = if user.disabled {
            "已禁用"
        } else {
            "已启用"
        };
        let account_actions = if protected {
            "<span class=\"muted\">受保护账号</span>".to_string()
        } else {
            let toggle_path = if user.disabled { "enable" } else { "disable" };
            let toggle_label = if user.disabled { "启用" } else { "禁用" };
            format!(
                r#"<form method="post" action="/admin/users/{toggle_path}" class="inline"><input type="hidden" name="csrf_token" value="{csrf}"><input type="hidden" name="username" value="{username}"><button class="secondary" type="submit">{toggle_label}</button></form>
                <form method="post" action="/admin/users/delete" class="inline"><input type="hidden" name="csrf_token" value="{csrf}"><input type="hidden" name="username" value="{username}"><button class="danger" type="submit">删除</button></form>"#
            )
        };
        rows.push_str(&format!(
            r#"<tr><td><strong>{username}</strong><br><span class="muted">{}</span></td><td>{}</td><td>{role}</td><td>{status}</td><td><form method="post" action="/admin/users/password" class="password"><input type="hidden" name="csrf_token" value="{csrf}"><input type="hidden" name="username" value="{username}"><input type="password" name="password" minlength="10" placeholder="新密码（至少 10 位）" required><button class="secondary" type="submit">重置密码</button></form>{account_actions}</td></tr>"#,
            escape(&user.display_name),
            escape(&user.id),
        ));
    }
    let mut repository_options = String::from(r#"<option value="urc-*">全部仓库</option>"#);
    let repository_ids = if live_repositories.is_empty() {
        local_repositories
    } else {
        live_repositories
            .iter()
            .map(|repository| repository.resource_id.clone())
            .collect()
    };
    for resource_id in repository_ids {
        repository_options.push_str(&format!(
            r#"<option value="{}"></option>"#,
            escape(&resource_id)
        ));
    }
    let mut grant_rows = String::new();
    for grant in grants {
        let protected = grant
            .username
            .eq_ignore_ascii_case(&state.bootstrap_username)
            && grant.resource_id == "urc-*";
        let username = escape(&grant.username);
        let resource_id = escape(&grant.resource_id);
        let actions = if protected {
            "<span class=\"muted\">终极管理员全局权限（受保护）</span>".to_string()
        } else {
            format!(
                r#"<form method="post" action="/admin/grants/revoke" class="inline"><input type="hidden" name="csrf_token" value="{csrf}"><input type="hidden" name="username" value="{username}"><input type="hidden" name="resource_id" value="{resource_id}"><button class="danger" type="submit">撤销授权</button></form>"#
            )
        };
        grant_rows.push_str(&format!(
            r#"<tr><td><strong>{username}</strong></td><td><code>{resource_id}</code></td><td>{}</td><td>{actions}</td></tr>"#,
            permission_label(&grant.permissions),
        ));
    }
    if grant_rows.is_empty() {
        grant_rows.push_str(r#"<tr><td colspan="4" class="muted">暂无仓库授权。</td></tr>"#);
    }
    let mut repository_rows = String::new();
    for repository in &live_repositories {
        let resource_id = escape(&repository.resource_id);
        let name = escape(&repository.name);
        let description = if repository.description.is_empty() {
            "<span class=\"muted\">无描述</span>".to_string()
        } else {
            escape(&repository.description)
        };
        let branch = if repository.default_branch_name.is_empty() {
            format!(
                "<code>{}</code>",
                escape(&short_hex(&repository.default_branch_id))
            )
        } else {
            escape(&repository.default_branch_name)
        };
        repository_rows.push_str(&format!(
            r#"<tr><td><strong>{name}</strong><br><span class="muted">{description}</span></td><td><code>{resource_id}</code></td><td>{branch}</td><td>{}</td><td>{}</td><td><a class="button-link" href="/admin/repositories/history?resource_id={}">提交历史</a><details><summary>删除仓库</summary><p class="muted">这是不可恢复的硬删除。请输入仓库名称 <strong>{name}</strong> 确认。</p><form method="post" action="/admin/repositories/delete" class="password"><input type="hidden" name="csrf_token" value="{csrf}"><input type="hidden" name="resource_id" value="{resource_id}"><input name="confirmation" autocomplete="off" placeholder="输入仓库名称" required><button class="danger" type="submit">永久删除</button></form></details></td></tr>"#,
            escape(if repository.creator.is_empty() { "未知" } else { &repository.creator }),
            format_timestamp(repository.created),
            urlencoding::encode(&repository.resource_id),
        ));
    }
    if repository_rows.is_empty() {
        repository_rows.push_str(
            r#"<tr><td colspan="6" class="muted">没有从 Lore Server 读取到仓库。</td></tr>"#,
        );
    }
    let body = format!(
        r#"<!doctype html><html lang="zh-CN"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>Lore 认证管理后台</title><style>
        :root{{color-scheme:dark}}*{{box-sizing:border-box}}body{{font:15px system-ui;background:#0b1120;color:#e5e7eb;margin:0}}a{{color:#93c5fd}}header{{display:flex;justify-content:space-between;align-items:center;padding:20px 28px;background:#111827;border-bottom:1px solid #334155}}main{{width:min(1280px,calc(100% - 32px));margin:28px auto}}h1,h2{{margin-top:0}}section{{background:#111827;border:1px solid #293548;border-radius:14px;padding:22px;margin-bottom:22px}}form.grid{{display:grid;grid-template-columns:1fr 1fr 1fr auto;gap:12px;align-items:end}}label{{display:grid;gap:6px}}input,select{{font:inherit;padding:10px;border:1px solid #475569;border-radius:7px;background:#0f172a;color:#fff;min-width:0}}button,.button-link{{font:inherit;font-weight:700;padding:10px 14px;border:0;border-radius:7px;background:#2563eb;color:#fff;cursor:pointer;text-decoration:none;display:inline-block}}button.secondary{{background:#475569}}button.danger{{background:#b91c1c}}.inline{{display:inline-block;margin:5px 5px 0 0}}.password{{display:flex;gap:7px;flex-wrap:wrap}}details{{margin-top:10px}}summary{{cursor:pointer;color:#fca5a5}}table{{width:100%;border-collapse:collapse}}th,td{{text-align:left;padding:12px 10px;border-bottom:1px solid #293548;vertical-align:top}}th{{color:#94a3b8}}code{{color:#bfdbfe}}.muted{{color:#94a3b8;font-size:13px}}.notice{{padding:12px 14px;border-radius:8px}}.success{{background:#064e3b}}.error{{background:#7f1d1d}}.logout{{margin:0}}@media(max-width:800px){{form.grid{{grid-template-columns:1fr}}table,thead,tbody,tr,th,td{{display:block}}thead{{display:none}}tr{{padding:12px 0;border-bottom:1px solid #334155}}td{{border:0;padding:6px 0}}}}
        </style></head><body><header><div><strong>Lore 认证管理后台</strong><div class="muted">当前账号：{}</div></div><form class="logout" method="post" action="/admin/logout"><input type="hidden" name="csrf_token" value="{csrf}"><button class="secondary" type="submit">退出登录</button></form></header><main>{notice}<section><h2>仓库管理</h2>{repository_notice}<p class="muted">仓库信息和提交历史实时读取自 Lore Server。删除是不可恢复的硬删除。</p><div style="overflow-x:auto"><table><thead><tr><th>仓库</th><th>仓库 ID</th><th>默认分支</th><th>创建者</th><th>创建时间</th><th>操作</th></tr></thead><tbody>{repository_rows}</tbody></table></div></section><section><h2>仓库授权</h2><p class="muted">选择已有仓库，或填写 <code>urc-*</code> 表示全部仓库。再次保存同一用户和仓库会覆盖原权限。</p><form class="grid" method="post" action="/admin/grants/set"><input type="hidden" name="csrf_token" value="{csrf}"><label>用户<select name="username" required><option value="">请选择用户</option>{user_options}</select></label><label>仓库 ID<input name="resource_id" list="repository-ids" placeholder="urc-..." required><datalist id="repository-ids">{repository_options}</datalist></label><label>权限级别<select name="access_level" required><option value="read">只读</option><option value="write">读写</option><option value="admin">仓库管理</option><option value="all">完全权限</option></select></label><button type="submit">保存授权</button></form><div style="overflow-x:auto;margin-top:18px"><table><thead><tr><th>用户</th><th>仓库</th><th>权限</th><th>操作</th></tr></thead><tbody>{grant_rows}</tbody></table></div></section><section><h2>创建用户</h2><form class="grid" method="post" action="/admin/users/create"><input type="hidden" name="csrf_token" value="{csrf}"><label>用户名<input name="username" minlength="3" maxlength="64" required></label><label>显示名称<input name="display_name" maxlength="128" required></label><label>初始密码<input type="password" name="password" minlength="10" required></label><button type="submit">创建用户</button></form></section><section><h2>用户列表</h2><div style="overflow-x:auto"><table><thead><tr><th>账号</th><th>用户 ID</th><th>角色</th><th>状态</th><th>操作</th></tr></thead><tbody>{rows}</tbody></table></div></section></main></body></html>"#,
        escape(&actor.username),
    );
    secured_html(StatusCode::OK, body)
}

fn admin_login_page(error: Option<&str>) -> Response {
    let error = error
        .map(|message| format!("<p class=\"error\">{}</p>", escape(message)))
        .unwrap_or_default();
    secured_html(
        if error.is_empty() {
            StatusCode::OK
        } else {
            StatusCode::UNAUTHORIZED
        },
        page(
            "Lore 认证管理后台",
            &format!(
                r#"{error}<p>请使用 .env 中配置的终极管理员账号登录。</p><form method="post" action="/admin/login"><label>用户名<input name="username" autocomplete="username" required autofocus maxlength="64"></label><label>密码<input type="password" name="password" autocomplete="current-password" required maxlength="1024"></label><button type="submit">登录</button></form>"#
            ),
        ),
    )
}

fn admin_error(status: StatusCode, message: &str) -> Response {
    secured_html(
        status,
        page(
            "管理操作失败",
            &format!(
                "<p class=\"error\">{}</p><p><a href=\"/admin\">返回管理后台</a></p>",
                escape(message)
            ),
        ),
    )
}

fn admin_redirect(kind: &str, message: &str) -> Response {
    Redirect::to(&format!("/admin?{kind}={}", urlencoding::encode(message))).into_response()
}

fn admin_lore_error(context: &str, error: anyhow::Error) -> Response {
    tracing::warn!(error = %error, operation = context, "Lore administration request failed");
    admin_error(
        StatusCode::BAD_GATEWAY,
        &format!("{context}：{}", grpc_error_message(&error)),
    )
}

fn admin_lore_redirect(context: &str, error: anyhow::Error) -> Response {
    tracing::warn!(error = %error, operation = context, "Lore administration request failed");
    admin_redirect(
        "error",
        &format!("{context}：{}", grpc_error_message(&error)),
    )
}

fn admin_operation_error(error: anyhow::Error) -> Response {
    let detail = error.to_string();
    tracing::warn!(error = %error, "web administration operation failed");
    let message = if detail.contains("username must contain") {
        "用户名长度必须为 3 到 64 个字符。"
    } else if detail.contains("username may contain") {
        "用户名只能包含英文字母、数字、点、短横线和下划线。"
    } else if detail.contains("display name must not be empty") {
        "显示名称不能为空。"
    } else if detail.contains("password must contain") {
        "密码长度不能少于 10 个字符。"
    } else if detail.contains("username may already exist") {
        "该用户名已经存在。"
    } else if detail.contains("user not found") {
        "用户不存在或已经被删除。"
    } else if detail.contains("resource must be") {
        "仓库 ID 格式无效，应为 urc- 加 32 位十六进制字符，或 urc-*。"
    } else if detail.contains("at least one permission") {
        "必须选择至少一项权限。"
    } else {
        "操作失败，请查看 lore-auth 容器日志。"
    };
    admin_redirect("error", message)
}

fn permission_label(permissions: &[String]) -> String {
    permissions
        .iter()
        .map(|permission| match permission.as_str() {
            "*" => "完全权限".to_string(),
            "read" => "读取".to_string(),
            "write" => "写入".to_string(),
            "owner" => "所有者".to_string(),
            "admin" => "管理".to_string(),
            "obliterate" => "彻底删除".to_string(),
            "migrate" => "迁移".to_string(),
            value => escape(value),
        })
        .collect::<Vec<_>>()
        .join("、")
}

fn format_timestamp(timestamp: u64) -> String {
    if timestamp == 0 {
        return "未知".to_string();
    }
    chrono::DateTime::from_timestamp(timestamp as i64, 0)
        .map(|value| {
            value
                .with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|| "无效时间".to_string())
}

fn short_hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn admin_page(title: &str, content: &str) -> String {
    format!(
        r#"<!doctype html><html lang="zh-CN"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>{}</title><style>:root{{color-scheme:dark}}*{{box-sizing:border-box}}body{{font:15px system-ui;background:#0b1120;color:#e5e7eb;margin:0}}main{{width:min(1280px,calc(100% - 32px));margin:28px auto;background:#111827;border:1px solid #293548;border-radius:14px;padding:22px}}a{{color:#93c5fd}}code{{color:#bfdbfe}}table{{width:100%;border-collapse:collapse}}th,td{{text-align:left;padding:12px 10px;border-bottom:1px solid #293548;vertical-align:top}}th{{color:#94a3b8}}.muted{{color:#94a3b8;font-size:13px}}@media(max-width:800px){{table,thead,tbody,tr,th,td{{display:block}}thead{{display:none}}tr{{padding:12px 0;border-bottom:1px solid #334155}}td{{border:0;padding:6px 0}}}}</style></head><body><main>{content}</main></body></html>"#,
        escape(title)
    )
}

fn valid_csrf(session: &AdminSession, submitted: &str) -> bool {
    !submitted.is_empty() && submitted == session.csrf_token
}

fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .map(str::trim)
        .find_map(|item| item.strip_prefix(&format!("{name}=")))
}

fn login_form(query: &LoginQuery, error: Option<&str>) -> String {
    let error = error
        .map(|message| format!("<p class=\"error\">{}</p>", escape(message)))
        .unwrap_or_default();
    page(
        "登录 Lore",
        &format!(
            r#"{error}
            <form method="post" action="/login">
              <input type="hidden" name="session_code" value="{}">
              <input type="hidden" name="client_state" value="{}">
              <label>用户名<input name="username" autocomplete="username" required autofocus maxlength="64"></label>
              <label>密码<input type="password" name="password" autocomplete="current-password" required maxlength="1024"></label>
              <button type="submit">登录</button>
            </form>"#,
            escape(&query.session_code),
            escape(&query.client_state),
        ),
    )
}

fn page(title: &str, body: &str) -> String {
    format!(
        r#"<!doctype html><html lang="zh-CN"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>{}</title><style>
        :root{{color-scheme:dark}}body{{font:16px system-ui;background:#111827;color:#e5e7eb;display:grid;place-items:center;min-height:100vh;margin:0}}main{{width:min(420px,calc(100% - 40px));background:#1f2937;padding:32px;border-radius:14px;box-shadow:0 18px 50px #0008}}h1{{margin-top:0}}label{{display:grid;gap:7px;margin:18px 0}}input{{font:inherit;padding:11px;border:1px solid #4b5563;border-radius:7px;background:#111827;color:#fff}}button{{font:inherit;font-weight:700;padding:11px 18px;border:0;border-radius:7px;background:#2563eb;color:#fff;cursor:pointer}}.error{{color:#fca5a5}}a{{color:#93c5fd}}
        </style></head><body><main><h1>{}</h1>{}</main></body></html>"#,
        escape(title),
        escape(title),
        body
    )
}

fn secured_html(status: StatusCode, body: String) -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    (status, headers, Html(body)).into_response()
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
