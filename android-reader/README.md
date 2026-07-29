# C 无线读表器 Android App

比赛专用 Android 客户端，使用 BLE 与 HC-42 透明传输模块通信。

## 页面

- **设备连接**：扫描 BLE 设备，选择 HC-42 并建立持续连接。
- **对话模式**：以 ASCII 或 HEX 查看原始收发数据，支持手动发送。
- **显示界面**：显示各地址电流、告警、最近 30 条记录和趋势图；支持一键自动读表。
- **设置**：配置低电流和高电流告警阈值，查看固定通信参数。

原蓝牙调试器中的“专业调试”和“按钮控制”页面不保留。

## 固定通信参数

| 项目 | 值 |
| --- | --- |
| BLE Service | `0000FFE0-0000-1000-8000-00805F9B34FB` |
| Notify/Write Characteristic | `0000FFE1-0000-1000-8000-00805F9B34FB` |
| CCCD | `00002902-0000-1000-8000-00805F9B34FB` |
| 自动轮询周期 | 120 秒 |
| 单轮扫描窗口 | 8 秒 |

当前 M0 测试帧：

```text
METER_TEST,ADDR=01,CURRENT_MA=1234,STATUS=NORMAL\r\n
```

App 以 `CURRENT_MA` 为准重新判定告警，不依赖帧中的 `STATUS`：

- `< 200 mA`：低电流；
- `> 2000 mA`：高电流；
- 其余：正常；
- 自动轮询时，已登记但本轮未成功读取：离线。

阈值可在设置页修改。

## 构建

需要 JDK 17、Android SDK Platform 35 和 Build Tools 35：

```powershell
cd android-reader
gradle assembleDebug
```

调试 APK 输出到：

```text
app/build/outputs/apk/debug/app-debug.apk
```

## 视觉

界面采用 iOS 风格的低饱和蓝灰背景、平滑大圆角、柔和阴影和半透明玻璃卡片。Android 12
及以上使用 `RenderEffect` 模糊背景光斑，旧版本使用软件模糊降级，不影响 BLE 功能。
