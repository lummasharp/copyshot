fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut resource = winresource::WindowsResource::new();
        resource.set_icon("src/copyshot.ico");
        resource.set("FileDescription", "Copyshot");
        resource.set("ProductName", "Copyshot");
        resource
            .compile()
            .expect("failed to compile Windows resources");
    }
}
