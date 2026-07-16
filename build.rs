fn main() -> Result<(), Box<dyn std::error::Error>> {
    let protoc = protoc_bin_vendored::protoc_bin_path()?;
    unsafe { std::env::set_var("PROTOC", protoc) };

    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(
            &[
                "proto/auth_api.proto",
                "proto/rebac_api.proto",
                "proto/lore/repository/v1/repository.proto",
                "proto/lore/revision/v1/revision.proto",
                "proto/lore/thin_client/v1/thin_client.proto",
            ],
            &["proto"],
        )?;
    Ok(())
}
