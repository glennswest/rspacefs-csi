use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // proto/ lives at the workspace root, two levels up from this crate.
    let proto_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("proto");
    let csi_proto = proto_root.join("csi.proto");

    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(
            &[csi_proto.to_str().unwrap()],
            &[proto_root.to_str().unwrap()],
        )?;

    println!("cargo:rerun-if-changed={}", csi_proto.display());
    Ok(())
}
