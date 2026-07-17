# SimpleLoreAuth

[简体中文](README.md) | [English](README.en.md)

一个面向家庭 NAS、小型团队和私有网络部署的
[EpicGames/lore](https://github.com/EpicGames/lore) 独立认证与权限服务。

SimpleLoreAuth 实现了 Lore 客户端和 Lore Server 使用的认证 gRPC 接口，并提供中文网页管理后台、用户名密码登录、用户管理、仓库授权以及仓库管理能力。它通过标准协议接入 Lore，不需要修改 Lore 源码。

> [!IMPORTANT]
> 本项目是社区项目，不是 Epic Games 官方认证服务，也不隶属于 Epic Games。请先在可信的测试环境中验证，再用于重要数据。

## 主要功能

- 实现 Lore `UrcAuthApi` gRPC 登录、令牌交换和权限查询协议；
- 实现 Lore 创建、列出和删除仓库时使用的 `RebacApi`；
- 浏览器用户名/密码登录，兼容 Lore Desktop 和 Lore CLI 的交互式登录；
- SQLite 用户数据库，密码使用 Argon2id 哈希保存；
- RS256 JWT 和标准 `/.well-known/jwks.json` 公钥端点；
- 普通用户启用/禁用、修改密码、创建和删除；
- 按仓库授予只读、读写、仓库管理或完全权限；
- 中文网页管理后台；
- 从 Lore Server 实时读取仓库列表；
- 查看仓库最近 50 条提交历史；
- 输入仓库名称二次确认后永久删除仓库；
- Docker Compose、Caddy HTTPS 和 gRPC/h2c 部署配置；
- 命令行用户与授权管理工具。

目前未实现外部 OIDC、第三方登录和 API Key 登录，相关调用会返回 `UNIMPLEMENTED`。

## 工作方式

```mermaid
flowchart LR
    C["Lore Desktop / CLI"] -->|"Lore gRPC :41337"| L["Lore Server"]
    C -->|"HTTPS + gRPC 登录"| P["Caddy / Lucky"]
    P -->|"HTTP :18080"| A["SimpleLoreAuth"]
    P -->|"h2c gRPC :15051"| A
    L -->|"权限检查 gRPC"| P
    A -->|"仓库管理 gRPC :41337"| L
    A --> D[("SQLite + RSA 密钥")]
```

HTTP 登录网页和认证 gRPC 共用同一个公网地址。Lore Server 的仓库端口是另一项独立服务，不要将两者混淆。

## 端口说明

| 端口 | 协议 | 用途 | 是否建议公网暴露 |
|---|---|---|---|
| `18080` | HTTP/1.1 | 登录网页、管理后台、健康检查和 JWKS | 否 |
| `15051` | h2c gRPC | Lore 认证与权限接口 | 否 |
| `10443` | HTTPS + HTTP/2 | Caddy 对外统一入口 | 是，或仅提供给上级反向代理 |
| `41337` | h2c gRPC | Lore Server 仓库服务，属于 Lore Server | 按实际网络需求 |

`18080` 和 `15051` 仅绑定在一体化容器的 `127.0.0.1`，不会由镜像对外提供；宿主机只需映射 `10443`。

## 预编译 Docker 镜像

GitHub Actions 会在每次推送到 `main`、推送 `v*` 版本标签或手动运行工作流时，自动构建并发布 `linux/amd64` 和 `linux/arm64` 镜像：

```text
ghcr.io/rogue324/simpleloreauth:latest
```

可用标签：

- `latest`：`main` 分支最新成功构建的一体化 Auth + Caddy 镜像；
- `sha-xxxxxxx`：对应具体 Git 提交；
- `v1.2.3`、`1.2.3`、`1.2`：推送 `v1.2.3` 标签时自动生成。

默认 `compose.yaml` 直接拉取预编译镜像，不再在 NAS 上编译 Rust。可通过 `LORE_AUTH_IMAGE` 指定其他标签：

```env
LORE_AUTH_IMAGE=ghcr.io/rogue324/simpleloreauth:v1.2.3
```

> [!NOTE]
> 个人 GitHub 账户首次发布的 GHCR 包默认为私有。第一次 Action 成功后，在 GitHub 个人主页的 **Packages → simpleloreauth → Package settings → Change visibility** 中将其设为 **Public**，NAS 才能匿名拉取。公开后不能再改回私有；如果希望保持私有，则需要先在 NAS 上执行 `docker login ghcr.io`。

## 快速部署

### 一体化 Auth + Caddy 镜像

`latest` 是唯一发布的运行镜像，同时包含 Lore Auth 和 Caddy：

```text
ghcr.io/rogue324/simpleloreauth:latest
```

该镜像同时运行 Lore Auth 和 Caddy。两者只通过容器内部的 `127.0.0.1:18080` 和 `127.0.0.1:15051` 通信，对外只需映射容器 TCP 端口 `10443`。镜像内置进程监管；任一服务退出时整个容器都会退出并交给 Docker 的重启策略恢复。

在 NAS 图形界面中至少配置：

```env
LORE_AUTH_URL=https://lore.example.com:10443
LORE_AUTH_PASSWORD=replace-with-a-long-random-password
LORE_SERVER_URL=http://NAS-LAN-IP:41337
LORE_AUTH_TLS_MODE=manual
```

同时进行以下映射：

- NAS 数据目录 → `/data`，读写；
- NAS 证书目录 → `/certs`，只读；
- NAS HTTPS 端口（例如 `10443`）→ 容器 TCP `10443`。

手动证书固定使用 `/certs/server.pem` 和 `/certs/server.key`，因此只需按这两个名称放入映射的 NAS 证书目录。如需持久保存 Caddy 自动证书，再映射 `/caddy-data` 和 `/caddy-config`。默认 `compose.yaml` 已经是一体化单容器配置。

```bash
docker compose pull
docker compose up -d
```

### 从镜像直接创建容器

一体化的 `latest` 镜像已在镜像配置中声明运行参数。NAS 的 Docker 图形界面从镜像创建容器时，会像显示 `PATH` 一样自动列出这些环境变量。具有固定默认值的参数会预填默认值；必须按部署环境填写的参数保持为空。

镜像只列出四个启动参数：

- `LORE_AUTH_URL`：用户实际访问的完整 HTTPS 地址；同时作为 JWT issuer，并自动提取证书域名。
- `LORE_AUTH_PASSWORD`：终极管理员密码；首次启动必须填写。
- `LORE_SERVER_URL`：Lore Server 的内部 gRPC 地址；需要后台仓库管理时填写。
- `LORE_AUTH_TLS_MODE`：`manual`（默认，使用已有证书）或 `auto`（Caddy 自动申请）。

管理员用户名固定默认为 `admin`。JWT Audience 会自动取 `LORE_AUTH_URL` 的域名部分，不包含协议、端口或路径，例如 `https://auth.example.com:10443` 对应 `auth.example.com`。它必须与 Lore Server 的 `jwt_audience` 完全一致；若通过旧版高级变量显式设置了不一致的 `LORE_AUTH_AUDIENCE`，容器会拒绝启动并提示正确值。令牌时长等其他高级设置使用内置默认值，不再占用 NAS 的启动参数列表。`18080` 和 `15051` 仅供同一容器内的 Caddy 与 Auth 通信，不需要映射到宿主机。

域名、HTTPS 和证书参数也属于同一个 `latest` 容器，不需要再创建单独的 Caddy 容器。

### NAS 参数化部署（推荐）

统一使用 `compose.yaml`；重复的 `compose.nas.yaml` 已删除。Caddy 在启动时自动生成配置，不需要创建或挂载 `Caddyfile`。

在 NAS 的 Compose 项目环境变量页面填写：

```env
LORE_AUTH_DATA_DIR=/path/on/nas/lore-auth/data
LORE_AUTH_HTTPS_PORT=10443
CADDY_CERTS_DIR=/path/on/nas/lore-auth/certs

LORE_AUTH_URL=https://auth.example.com:2234
LORE_AUTH_PASSWORD=replace-with-a-long-random-password
LORE_SERVER_URL=http://NAS-LAN-IP:41337
LORE_AUTH_TLS_MODE=manual
```

参数说明：

| 参数 | 示例 | 说明 |
|---|---|---|
| `LORE_AUTH_DATA_DIR` | `/volume/lore-auth/data` | NAS 上保存数据库和 RSA 密钥的目录 |
| `LORE_AUTH_HTTPS_PORT` | `10443` | NAS 对外映射到 Caddy `10443` 的 TCP 端口 |
| `CADDY_CERTS_DIR` | `/volume/lore-auth/certs` | NAS 上的证书目录，仅手动证书模式需要 |
| `LORE_AUTH_URL` | `https://auth.example.com:2234` | 公网地址、JWT issuer 和证书域名的唯一来源 |
| `LORE_AUTH_PASSWORD` | 强密码 | 首次创建终极管理员时使用 |
| `LORE_SERVER_URL` | `http://192.168.1.10:41337` | 后台连接 Lore Server；不使用仓库管理可留空 |
| `LORE_AUTH_TLS_MODE` | `manual` | `manual` 使用已有证书；`auto` 由 Caddy 申请证书 |

启动：

```bash
docker compose pull
docker compose up -d
```

如果 NAS 图形界面支持导入 Compose，导入 `compose.yaml` 后在界面中填写上述环境变量即可，不需要上传 `Caddyfile`。

手动证书模式需要把证书文件放在 `CADDY_CERTS_DIR` 指向的 NAS 目录中，并命名为 `server.pem` 和 `server.key`。

自动证书模式设置：

```env
LORE_AUTH_TLS_MODE=auto
```

自动模式会从 `LORE_AUTH_URL` 提取域名，但公网 `443` 仍必须能够转发到 NAS 的 `LORE_AUTH_HTTPS_PORT` 并满足 ACME 验证条件。

### 1. 准备配置

```bash
cp .env.example .env
```

编辑 `.env`：

```env
LORE_AUTH_URL=https://auth.example.com:10443
LORE_AUTH_PASSWORD=请替换为至少十位的高强度密码
LORE_SERVER_URL=http://192.168.1.10:41337
LORE_AUTH_TLS_MODE=manual
```

变量说明：

| 变量 | 必填 | 说明 |
|---|---|---|
| `LORE_AUTH_URL` | 是 | 客户端公网地址；自动用作 JWT issuer，并从中提取证书域名 |
| `LORE_AUTH_PASSWORD` | 首次启动需要 | 首次创建终极管理员 `admin` 时使用的密码 |
| `LORE_SERVER_URL` | 仓库后台需要 | 管理后台连接 Lore Server 的内部 gRPC 地址 |
| `LORE_AUTH_TLS_MODE` | 否 | 默认 `manual`；也可设为 `auto` |

终极管理员每次启动都会恢复为启用状态，并获得 `urc-*` 全局权限。该账号不能在网页后台被禁用或删除。

### 2. 选择 TLS 方式

#### 方式 A：Caddy 自动申请证书

容器会根据环境变量自动生成 Caddy 配置，不需要提供 `Caddyfile`：

```env
LORE_AUTH_TLS_MODE=auto
```

域名和网络必须满足 Caddy/ACME 验证要求；公网 TCP `443` 必须能够到达映射到容器 `10443` 的 NAS 端口。

```bash
docker compose pull
docker compose up -d
```

#### 方式 B：Lucky/其他反向代理 + 已有证书

把 NAS 证书目录映射到容器 `/certs`，然后设置：

```env
LORE_AUTH_TLS_MODE=manual
```

证书必须覆盖客户端访问使用的域名并包含完整证书链，私钥必须与证书匹配。容器会自动生成 HTTPS、普通网页和 gRPC/h2c 分流配置。

如果公网使用 `https://auth.example.com:2234`，`.env` 必须相应写成：

```env
LORE_AUTH_URL=https://auth.example.com:2234
```

Lucky 后端示例：

```text
后端地址：https://NAS局域网IP:10443
忽略后端 TLS 证书验证：是
grpc 使用安全连接：是
禁用长连接：否
```

必须保留 HTTP/2。若普通网页正常但 gRPC 返回 `grpc-status: 14`，通常是 Lucky 没有启用“grpc 使用安全连接”。

常见配置错误：

- 证书目录没有映射到 `/certs`，或文件名不是 `server.pem`/`server.key`；
- Lucky 后端写成 `http://NAS:10443`，而 Caddy 的 `10443` 实际是 HTTPS；
- Lucky 没有保留 HTTP/2，导致网页正常但 Lore 登录或权限检查失败。

### 3. 验证认证服务

```bash
docker compose ps
curl https://auth.example.com:10443/health
curl https://auth.example.com:10443/.well-known/jwks.json
```

健康检查应返回：

```json
{"status":"ok"}
```

查看日志：

```bash
docker compose logs --tail=100 lore-auth
```

## 配置 Lore Server

将 `lore-server.local.toml.example` 中的内容合并到 Lore Server 的本地配置。三个公网地址必须使用完全相同的协议、域名和端口：

```toml
[environment.endpoint]
auth_url = "https://auth.example.com:10443"

[server.auth]
jwt_issuer = "https://auth.example.com:10443"
jwt_audience = ["auth.example.com"]

[server.auth.jwk]
endpoint = "https://auth.example.com:10443/.well-known/jwks.json"
```

如果认证服务实际公网端口是 `2234`，这里三处 URL 都必须改为 `2234`。`jwt_audience` 仍然只填写域名 `auth.example.com`，不包含协议和端口。修改后重启 Lore Server。

`environment.endpoint.auth_url` 会同时提供给 Lore 客户端，并被 Lore Server 用于权限查询。地址写错时，客户端调试日志会出现：

```text
starting auth session failed to connect to auth endpoint
```

## 客户端登录

CLI 示例：

```bash
lore auth login lore://your-lore-server:41337
```

Lore Desktop 添加远程地址后会自动打开登录网页。成功页面会显示“身份验证成功”，随后客户端在本机安全凭据目录中保存令牌。

管理后台登录 Cookie 与 Lore Desktop 登录令牌互相独立：登录过 `/admin` 不代表 Lore Desktop 已经登录。

## 中文管理后台

访问：

```text
https://auth.example.com:10443/admin
```

后台支持：

- 创建、启用、禁用和删除普通用户；
- 重置用户密码；
- 查看用户 ID 和状态；
- 为用户授予或撤销指定仓库权限；
- 实时查看 Lore Server 中的仓库；
- 查看仓库默认分支、创建者、创建时间和提交历史；
- 永久删除 Lore 仓库。

仓库管理要求配置：

```env
LORE_SERVER_URL=http://NAS局域网IP:41337
```

删除仓库是不可恢复的硬删除，请先备份 Lore 数据目录。

## 命令行管理

创建用户：

```bash
docker compose exec \
  -e LORE_AUTH_PASSWORD='用户的高强度密码' \
  lore-auth lore-auth user add --username alice --display-name 'Alice'
```

列出、启用和禁用用户：

```bash
docker compose exec lore-auth lore-auth user list
docker compose exec lore-auth lore-auth user disable alice
docker compose exec lore-auth lore-auth user enable alice
```

重置密码：

```bash
docker compose exec \
  -e LORE_AUTH_PASSWORD='新的高强度密码' \
  lore-auth lore-auth user password alice
```

仓库授权：

```bash
docker compose exec lore-auth lore-auth grant set alice \
  urc-0194b726b34e72b0b45550b88a967076 \
  --permissions read,write

docker compose exec lore-auth lore-auth grant list alice

docker compose exec lore-auth lore-auth grant revoke alice \
  urc-0194b726b34e72b0b45550b88a967076
```

## 数据与备份

持久数据位于：

```text
./data/lore-auth.db
./data/private-key.pem
./data/public-key.pem
```

数据库保存用户、密码哈希、仓库授权和仓库归属记录。RSA 私钥用于签发令牌。请备份整个 `data` 目录，并严格保护私钥：

- 丢失数据库会丢失账号和授权；
- 丢失私钥会使已签发令牌失效；
- 泄露私钥会导致攻击者能够伪造令牌。

`.env`、`data/`、`certs/` 和 `target/` 默认不会提交到 Git。

## 安全说明

- 管理后台仅允许终极管理员登录；
- 管理会话保存在内存中，有效期 8 小时；
- Cookie 使用 `Secure`、`HttpOnly` 和 `SameSite=Strict`；
- 所有管理表单使用 CSRF Token；
- 禁用用户会立即阻止新登录和新令牌交换；
- 已签发 JWT 在到期前仍可能有效，可缩短 `LORE_AUTH_TOKEN_TTL_SECONDS`；
- 不要把 `18080`、`15051` 或 SQLite 数据库暴露到不可信网络；
- 不建议向普通用户授予 `urc-*` 通配权限。

## 更新

使用预编译镜像更新：

```bash
docker compose pull
docker compose up -d --force-recreate
```

不要删除 `data` 目录，也不要使用 `docker compose down -v`，否则可能清除持久数据或 Caddy 状态。

仅在开发或修改源码时本地构建：

```bash
docker compose -f compose.yaml -f compose.build.yaml up -d --build
```

## 常见问题

### 网页正常，但 Lore Server 显示 `Failed to connect to lore auth service`

检查：

1. `environment.endpoint.auth_url` 是否为实际公网认证地址；
2. 反向代理是否支持 HTTP/2 gRPC；
3. Lucky 是否启用了“grpc 使用安全连接”；
4. Caddy 的 gRPC 上游是否使用 `h2c`。

### Lore Desktop 显示 `Not authenticated`

查看 `authLoginInteractive` 调试事件。确认浏览器登录成功，并确认 Lore Server 返回的 `auth_url` 端口正确。管理后台登录不能替代客户端登录。

### SQLite 报 `Unable to open the database file`

确保 `./data` 存在且容器用户有写入权限：

```bash
mkdir -p data
chmod 770 data
```

### Caddy TLS 握手失败

检查证书目录是否映射到容器 `/certs`、证书与私钥是否匹配，并确认文件名为 `server.pem` 和 `server.key`。

## 本地开发

```bash
cargo fmt --all -- --check
cargo test --locked
cargo clippy --all-targets --locked -- -D warnings
```

仅用于回环地址的明文开发启动：

```bash
cargo run --locked -- \
  --data-dir ./data \
  serve \
  --public-base-url http://127.0.0.1:18080 \
  --issuer http://127.0.0.1:18080 \
  --bootstrap-username admin \
  --bootstrap-password a-long-development-password
```

## 许可证

[MIT](LICENSE)
