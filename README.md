# AirPods Battery Watch

WinIsland 插件：在动态岛上实时显示 AirPods 电量，任一设备电量低时高优先级提示「电量低」。

作者：C130AIR · [GitHub](https://github.com/C130AIR/airpods-battery-watch) · MIT License

## 功能

- 直接监听 AirPods 的 BLE 广播（Apple Continuity Protocol，companyId=76），**不依赖 AirPodsDesktop**。
- 显示左耳 / 右耳 / 充电盒电量（4bit 精度，10% 一档）。
- 任一设备电量 ≤ 阈值（默认 20%）时，以 `PRIORITY_HIGH` 在岛上提示「AirPods 电量低」。
- 电量正常时以 `PRIORITY_MEDIUM` 显示实时电量。

## 构建

```powershell
cargo build --release
```

产物：`target\release\airpods_battery_watch.dll`

## 安装

1. 将 `airpods_battery_watch.dll` 放入 `%APPDATA%\WinIsland\plugins\airpods-battery-watch\`。
2. 重启 WinIsland，在 **Settings > Plugins** 中启用。

或打包成 zip 后通过 WinIsland 插件市场/手动安装：

```powershell
cargo run --example pack
```

## 配置

- 低电量阈值：环境变量 `AIRPODS_LOW_THRESHOLD`（默认 `20`，单位 %）。

## 发布

推送 `v*` 标签会自动触发 [release.yml](.github/workflows/release.yml) 构建带来源证明的 GitHub Release；首次发布后向 [WinIslandProject/PluginMarketplace](https://github.com/WinIslandProject/PluginMarketplace) 提交 `plugins/airpods-battery-watch.toml` 即可上架插件市场。

## 说明

- AirPods 广播电量精度为 10%（4bit 值 ×10）。
- 耳机在充电盒内时可能不广播电量，显示 `--`。
- 蓝牙适配器需支持 BLE（Windows 10/11 自带驱动即可）。
- 插件为原生代码，在 WinIsland 进程内执行，无沙箱。
