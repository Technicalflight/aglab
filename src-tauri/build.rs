fn main() {
    // 测试二进制也要 comctl32 v6 清单：任何测试链接到 v6-only 的入口
    // （rfd 的消息弹窗、muda 菜单都会引 TaskDialogIndirect）时，没有清单
    // comctl32 会落到 5.82——加载即 STATUS_ENTRYPOINT_NOT_FOUND，测试全灭。
    // 主程序 exe 的清单由 tauri-build 负责，这里只补测试这半边
    let manifest = std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap())
        .join("tests-manifest.xml");
    println!("cargo:rerun-if-changed={}", manifest.display());
    // cargo 1.98 没有 tests 作用域的 link-arg 变体，基础指令打到所有链接目标：
    // 主程序的清单由 tauri-build 出，两份 /MANIFESTINPUT 会合并，comctl32 v6 重复无害
    println!("cargo::rustc-link-arg=/MANIFEST:EMBED");
    println!("cargo::rustc-link-arg=/MANIFESTINPUT:{}", manifest.display());
    tauri_build::build()
}
