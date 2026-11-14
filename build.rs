fn main() {
    // 编译 ui/ 下的 Slint 代码（入口为 main_window.slint，其余经 import 引入）。
    slint_build::compile("ui/main_window.slint").unwrap();

    // exe 图标与版本信息。build.rs 面向 host 编译，无法用 cfg(windows) 判定
    // target，因此在运行时检查 CARGO_CFG_TARGET_OS（与 stools 相同处理）。
    // 图标换了要重新嵌入资源：winresource 自己不会登记依赖，
    // 不写这行的话改了 assets/icon.ico 也得先手动 touch 本文件才会生效。
    println!("cargo:rerun-if-changed=assets/icon.ico");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        res.set("ProductName", "Rove");
        res.set("FileDescription", "Rove File Manager");
        // 文件版本 0.1.0.1。
        res.set_version_info(winresource::VersionInfo::FILEVERSION, 0x0001_0000_0000_0001);
        res.set_version_info(winresource::VersionInfo::PRODUCTVERSION, 0x0001_0000_0000_0001);
        if let Err(err) = res.compile() {
            println!("cargo:warning=failed to embed windows resource: {err}");
        }
    }
}
