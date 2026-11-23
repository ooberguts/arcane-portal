fn main() {
    println!("cargo:rerun-if-changed=assets/arcane-portal.ico");

    #[cfg(windows)]
    {
        let mut resources = winresource::WindowsResource::new();
        resources
            .set_icon("assets/arcane-portal.ico")
            .set("ProductName", "Arcane Portal")
            .set("FileDescription", "Arcane Portal");
        resources.compile().expect("embed the Windows app icon");
    }
}
