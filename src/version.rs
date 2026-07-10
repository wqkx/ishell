//! 应用版本号：唯一来源为 `Cargo.toml`，编译期内嵌。

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
