pub mod epic_urc {
    tonic::include_proto!("epic_urc");
}

pub mod rebac {
    tonic::include_proto!("ucs.auth");
}

pub mod lore {
    pub mod model {
        pub mod v1 {
            tonic::include_proto!("lore.model.v1");
        }
    }
    pub mod repository {
        pub mod v1 {
            tonic::include_proto!("lore.repository.v1");
        }
    }
    pub mod revision {
        pub mod v1 {
            tonic::include_proto!("lore.revision.v1");
        }
    }
    pub mod thin_client {
        pub mod v1 {
            tonic::include_proto!("lore.thin_client.v1");
        }
    }
}
