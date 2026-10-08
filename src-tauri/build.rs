fn main() {
    // 注意：不要在这里给所有链接目标嵌 MANIFEST（rustc-link-arg 是全目标的，
    // 会和 tauri-build 给主程序嵌的清单撞成 CVT1100 资源重复；实测
    // rustc-link-arg-tests 也到不了 lib unittest 的链接行）。测试二进制对
    // comctl32 v6-only 入口的依赖在 chat.rs 里用 #[cfg(test)] + .drectve
    // 链接指令注入（见 TEST_MANIFEST_DIRECTIVE）——只有测试构建带清单
    tauri_build::build()
}
