fn main() {
    println!("cargo:rerun-if-changed=../../assets/images/spoon.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let icon = std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap())
            .join("../../assets/images/spoon.ico");
        let mut res = winres::WindowsResource::new();
        res.set_icon(icon.to_str().expect("icon path must be valid UTF-8"));
        res.set("ProductName", "Spoon");
        res.set("FileDescription", "Spoon Launcher");
        res.set("CompanyName", "Spoon");
        res.set("LegalCopyright", "Copyright (c) Spoon contributors");
        res.set("InternalName", "spoon-gui");
        res.set("OriginalFilename", "spoon-gui.exe");
        res.compile().expect("failed to compile Windows resources");
    }
}
