fn main() -> Result<(), Box<dyn std::error::Error>> {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR")?;
    let workspace_root = std::path::Path::new(&manifest_dir)
        .parent() // crates/
        .unwrap()
        .parent(); // workspace root
    let proto_dir = workspace_root.unwrap().join("proto");

    for entry in std::fs::read_dir(&proto_dir)? {
        let entry = entry?;
        if entry.path().extension().is_some_and(|e| e == "proto") {
            println!("cargo:rerun-if-changed={}", entry.path().display());
        }
    }

    let protos = [
        proto_dir.join("transportinstruction.proto"),
        proto_dir.join("hostinput.proto"),
        proto_dir.join("userinput.proto"),
    ];
    let includes = [proto_dir.as_path()];

    prost_build::compile_protos(&protos, &includes)?;
    Ok(())
}
