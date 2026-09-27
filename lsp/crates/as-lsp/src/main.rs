//! as-lsp：LSP server 壳（M0 占位）。
//!
//! 真正的 server（tower-lsp-server、增量同步、overlay、请求路由）随 M2 就位
//! （LSP实现规划 §8/§9）。本文件只占住 crate 位与依赖边
//! `as-lsp → as-core → as-syntax`，保证依赖图从第一天就是可编译的整体。

fn main() {
    println!(
        "as-lsp {} (M0 stub: server shell lands in M2)",
        env!("CARGO_PKG_VERSION")
    );
}
