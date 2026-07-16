use anyhow::{Context, Result, bail};
use tonic::metadata::{Ascii, Binary, MetadataValue};
use tonic::transport::Channel;
use tonic::{Request, Status};

use crate::proto::lore::model::v1::RevisionIdentifier;
use crate::proto::lore::repository::v1::repository_service_client::RepositoryServiceClient;
use crate::proto::lore::repository::v1::{RepositoryDeleteRequest, RepositoryListRequest};
use crate::proto::lore::revision::v1::revision_list_request;
use crate::proto::lore::revision::v1::revision_service_client::RevisionServiceClient;
use crate::proto::lore::revision::v1::{BranchListRequest, RevisionListRequest};
use crate::proto::lore::thin_client::v1::thin_client_service_client::ThinClientServiceClient;
use crate::proto::lore::thin_client::v1::{RevisionInfoRequest, revision_info_request};

const REPOSITORY_ID_KEY: &str = "urc-repository-id-bin";
const HISTORY_LIMIT: usize = 50;

#[derive(Clone, Debug)]
pub struct LiveRepository {
    pub resource_id: String,
    pub name: String,
    pub description: String,
    pub default_branch_id: Vec<u8>,
    pub default_branch_name: String,
    pub creator: String,
    pub created: u64,
}

#[derive(Clone, Debug)]
pub struct RevisionHistoryEntry {
    pub branch_name: String,
    pub number: u64,
    pub signature: String,
    pub message: String,
    pub timestamp: u64,
    pub committed_by: String,
}

#[derive(Clone)]
pub struct LoreAdminClient {
    endpoint: String,
}

impl LoreAdminClient {
    pub fn new(endpoint: &str) -> Self {
        Self {
            endpoint: endpoint.trim_end_matches('/').to_string(),
        }
    }

    async fn channel(&self) -> Result<Channel> {
        Channel::from_shared(self.endpoint.clone())
            .context("Lore gRPC 地址无效")?
            .connect()
            .await
            .context("无法连接 Lore Server gRPC")
    }

    pub async fn list_repositories(
        &self,
        authentication_token: &str,
    ) -> Result<Vec<LiveRepository>> {
        let mut client = RepositoryServiceClient::new(self.channel().await?);
        let mut request = Request::new(RepositoryListRequest { creator: None });
        set_bearer(&mut request, authentication_token)?;
        let mut stream = client
            .repository_list(request)
            .await
            .context("Lore Server 拒绝读取仓库列表")?
            .into_inner();
        let mut repositories = Vec::new();
        while let Some(item) = stream.message().await.context("读取仓库列表流失败")? {
            if let Some(repository) = item.repository {
                repositories.push(LiveRepository {
                    resource_id: format!("urc-{}", encode_hex(&repository.id)),
                    name: repository.name,
                    description: repository.description,
                    default_branch_id: repository.default_branch_id,
                    default_branch_name: repository.default_branch_name,
                    creator: repository.creator,
                    created: repository.created,
                });
            }
        }
        repositories.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(repositories)
    }

    pub async fn delete_repository(
        &self,
        authentication_token: &str,
        resource_id: &str,
    ) -> Result<()> {
        let id = decode_resource_id(resource_id)?;
        let mut client = RepositoryServiceClient::new(self.channel().await?);
        let mut request = Request::new(RepositoryDeleteRequest { id });
        set_bearer(&mut request, authentication_token)?;
        client
            .repository_delete(request)
            .await
            .context("Lore Server 删除仓库失败")?;
        Ok(())
    }

    pub async fn history(
        &self,
        authorization_token: &str,
        repository: &LiveRepository,
    ) -> Result<Vec<RevisionHistoryEntry>> {
        let repository_id = decode_resource_id(&repository.resource_id)?;
        let channel = self.channel().await?;
        let mut revision_client = RevisionServiceClient::new(channel.clone());
        let mut branch_request = Request::new(BranchListRequest {
            creator: None,
            include_deleted: false,
        });
        set_authorized_repository(&mut branch_request, authorization_token, &repository_id)?;
        let mut branches = revision_client
            .branch_list(branch_request)
            .await
            .context("读取 Lore 分支列表失败")?
            .into_inner();
        let mut entries = Vec::new();
        while let Some(item) = branches.message().await.context("读取分支列表流失败")? {
            let Some(branch) = item.branch else { continue };
            let mut list_request = Request::new(RevisionListRequest {
                start: Some(revision_list_request::Start::Identifier(
                    RevisionIdentifier {
                        branch_id: branch.id.clone(),
                        number: 0,
                    },
                )),
            });
            set_authorized_repository(&mut list_request, authorization_token, &repository_id)?;
            let page = revision_client
                .revision_list(list_request)
                .await
                .with_context(|| format!("读取分支 {} 的提交列表失败", branch.name))?
                .into_inner();
            for item in page
                .items
                .into_iter()
                .take(HISTORY_LIMIT.saturating_sub(entries.len()))
            {
                let mut info_client = ThinClientServiceClient::new(channel.clone());
                let mut info_request = Request::new(RevisionInfoRequest {
                    query: Some(revision_info_request::Query::Signature(
                        item.signature.clone(),
                    )),
                });
                set_authorized_repository(&mut info_request, authorization_token, &repository_id)?;
                let info = info_client
                    .revision_info(info_request)
                    .await
                    .with_context(|| format!("读取修订 #{} 详情失败", item.number))?
                    .into_inner()
                    .revision;
                let (message, timestamp, committed_by, number) = info
                    .map(|revision| {
                        (
                            revision.commit_message,
                            revision.timestamp,
                            revision.committed_by,
                            revision.number,
                        )
                    })
                    .unwrap_or_default();
                entries.push(RevisionHistoryEntry {
                    branch_name: branch.name.clone(),
                    number: if number == 0 { item.number } else { number },
                    signature: encode_hex(&item.signature),
                    message,
                    timestamp,
                    committed_by,
                });
                if entries.len() >= HISTORY_LIMIT {
                    break;
                }
            }
            if entries.len() >= HISTORY_LIMIT {
                break;
            }
        }
        entries.sort_by_key(|entry| std::cmp::Reverse(entry.timestamp));
        Ok(entries)
    }
}

fn set_bearer<T>(request: &mut Request<T>, token: &str) -> Result<()> {
    let mut value: MetadataValue<Ascii> = format!("Bearer {token}")
        .parse()
        .context("认证令牌不能写入 gRPC 请求头")?;
    value.set_sensitive(true);
    request.metadata_mut().insert("authorization", value);
    Ok(())
}

fn set_authorized_repository<T>(
    request: &mut Request<T>,
    token: &str,
    repository_id: &[u8],
) -> Result<()> {
    set_bearer(request, token)?;
    let value = MetadataValue::<Binary>::from_bytes(repository_id);
    request.metadata_mut().insert_bin(REPOSITORY_ID_KEY, value);
    Ok(())
}

fn decode_resource_id(resource_id: &str) -> Result<Vec<u8>> {
    let value = resource_id
        .strip_prefix("urc-")
        .context("仓库 ID 缺少 urc- 前缀")?;
    if value.len() != 32 {
        bail!("仓库 ID 必须包含 32 位十六进制字符");
    }
    (0..value.len())
        .step_by(2)
        .map(|index| {
            u8::from_str_radix(&value[index..index + 2], 16).context("仓库 ID 不是十六进制")
        })
        .collect()
}

fn encode_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn grpc_error_message(error: &anyhow::Error) -> String {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<Status>())
        .map(|status| format!("{}：{}", status.code(), status.message()))
        .unwrap_or_else(|| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_id_round_trip() {
        let resource = "urc-0123456789abcdef0123456789abcdef";
        let decoded = decode_resource_id(resource).unwrap();
        assert_eq!(format!("urc-{}", encode_hex(&decoded)), resource);
    }
}
