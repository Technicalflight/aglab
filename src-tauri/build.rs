fn main() {
    // 注意：不要在这里给所有链接目标嵌 MANIFEST（rustc-link-arg 是全目标的，
    // 会和 tauri-build 给主程序嵌的清单撞成 CVT1100 资源重复；cargo 1.98 也
    // 没有 tests 作用域的变体）。测试二进制对 comctl32 v6-only 入口的依赖
    // 已在 island.rs 里用 #[cfg(not(test))] 规避——测试不编译岛这块 UI
    tauri_build::build()
}
