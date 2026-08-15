//! 打包示例：`cargo run --example pack`
//! 生成可分发/安装的 .zip 插件包。

fn main() {
    winisland_plugin_api::packager::PluginPackager::from_cargo()
        .unwrap()
        .build()
        .unwrap();
}
