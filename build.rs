fn main() {
    // 强制 fluent-light：否则 Slint 会跟随 Windows 暗色模式，
    // 令 Text / Button / LineEdit 的默认前景色变为白色，与浅色背景形成白底白字。
    let config = || {
        slint_build::CompilerConfiguration::new().with_style("fluent-light".to_string())
    };

    // 先编译各独立窗口（由 main.rs 手动 include，生成 OUT_DIR/<stem>.rs），
    // 最后编译 main.slint —— SLINT_INCLUDE_GENERATED 指向它，供 include_modules! 使用。
    for path in ["ui/preview.slint", "ui/diff.slint", "ui/main.slint"] {
        slint_build::compile_with_config(path, config())
            .unwrap_or_else(|e| panic!("Slint 编译失败 {path}: {e}"));
    }

    for path in ["ui/main.slint", "ui/preview.slint", "ui/diff.slint"] {
        println!("cargo:rerun-if-changed={path}");
    }
}
