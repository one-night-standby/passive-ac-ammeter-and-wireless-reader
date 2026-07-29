# LP-MSPM0G3507 开发板信息

本文汇总本项目所用 TI LP-MSPM0G3507 LaunchPad 的硬件资源、排针、跳线、Rust 工程配置及 `prepare-nuedc` 固定驱动引脚，作为接线、外设分配和冲突检查依据。

## 资料来源

| 资料 | 版本或快照 | 用途 |
| --- | --- | --- |
| [TI LP-MSPM0G3507 用户指南](https://www.ti.com/lit/ug/slau873d/slau873d.pdf) | SLAU873D，2024-10 修订 | 开发板资源、跳线、排针和原理图 |
| [prepare-nuedc](https://github.com/one-night-standby/prepare-nuedc) | `bc94dc62685a730ae3bde089fff23e4594207a55` | Rust 模板、固定驱动及接线 |
| 当前工程 | `passive-ac-ammeter-and-wireless-reader` | 本项目实际资源分配 |

整理日期：2026-07-29。

## 一、核心硬件

| 项目 | 参数 |
| --- | --- |
| MCU | MSPM0G3507SPM，LQFP64 |
| CPU | Arm Cortex-M0+，最高 80 MHz |
| Flash | 128 KB，地址从 `0x0000_0000` 开始 |
| SRAM | 32 KB，地址从 `0x2020_0000` 开始 |
| 工作电压 | 1.62 V - 3.6 V |
| ADC | 2 个 12 位 SAR ADC，最高 4 Msps |
| DAC | 1 个 12 位 DAC |
| 模拟资源 | 2 个零漂移斩波运放 OPA、1 个 GPAMP、比较器 |
| 通信资源 | UART、SPI、I2C、CAN-FD 等 |
| 板载时钟 | 32.768 kHz 晶振、40 MHz 晶振 |
| 默认系统时钟 | 32 MHz SYSOSC，默认精度 2.5% |
| 扩展接口 | 40 针 BoosterPack 排针、板底 MCU 引脚扩展排针 |

开发板还集成以下资源：

- XDS110-ET 调试器，可用于下载、调试、虚拟串口和 EnergyTrace 功耗分析。
- 1 个红色 LED、1 个 RGB LED。
- 用户按键、BSL 按键和复位按键。
- 光敏二极管、热敏电阻。
- 外置 OPA2365 ADC 缓冲电路。
- QEI 接口。

## 二、供电、下载和串口

### 2.1 供电

- 常规开发时由 Micro-USB 向 XDS110-ET 供电，再经 J101 的 `3V3` 跳帽给目标 MCU 供电。
- J10 提供 `5V/GND`，J11 提供 `3V3/GND`。
- 通过 J11 外部供电时，必须保证目标电压处于 MCU 的 `1.62 V - 3.6 V` 工作范围。
- 外部电源给目标侧供电时，建议断开 J101 的 `3V3` 跳帽，避免两路 3.3 V 电源并联或反向灌电。

### 2.2 下载与调试

- 板载 XDS110-ET 通过 SWD 连接 PA19、PA20。
- PA19 为 `SWDIO`，PA20 为 `SWCLK`。
- J101 隔离排针可断开目标板与 XDS110 的电源、UART、复位、SWD 和 BSL 信号。
- 当前 Rust 工程使用：

```text
pyocd flash --target mspm0g3507 --format elf
```

### 2.3 板载虚拟串口

| 信号 | MCU 引脚 | 跳线 | 默认去向 |
| --- | --- | --- | --- |
| UART0_TX | PA10 | J21 | XDS110 虚拟串口 |
| UART0_RX | PA11 | J22 | XDS110 虚拟串口 |

开发板连接电脑后会枚举出 `XDS110 Class Application/User UART` 虚拟串口。J21、J22 默认把 UART0 接到 XDS110；改作外部串口前必须调整跳线，或选择其他 UART。

## 三、40 针 BoosterPack 排针

以下编号与 TI 用户指南 Figure 2-10 一致。

| 针号 | MCU/电源 | 开发板标注或常用功能 | 注意事项 |
| ---: | --- | --- | --- |
| 1 | 3V3 | 3.3 V 电源 |  |
| 2 | PA25 | ADC0.2 |  |
| 3 | PB23 / PA9 | UART_RX | 由 J14 选择，默认 PB23 |
| 4 | PA8 | UART1_TX |  |
| 5 | PA26 | OPA0_IN0+、ADC0.1 | 板载光传感器相关，见 J18 |
| 6 | PB24 | ADC0.5 | 板载热敏电阻相关，见 J9 |
| 7 | PB9 | SPI1_CLK |  |
| 8 | PA27 | OPA0_IN0-、比较器输入 | 板载光传感器相关，见 J17 |
| 9 | PB2 | I2C1_SCL | `prepare-nuedc` SSD1306 SCL |
| 10 | PB3 | I2C1_SDA | `prepare-nuedc` SSD1306 SDA |
| 11 | PB16 | UART2_RX、TIMG1_C1 | 推荐接 HC-42 TXD |
| 12 | PB0 | SPI1_CS2 |  |
| 13 | PB6 | SPI1_CS0 | 多个固定驱动占用 |
| 14 | PB7 | SPI1_POCI | ADS131A04 使用 |
| 15 | PB8 | SPI1_PICO | ADS131A04 使用 |
| 16 | NRST | 目标 MCU 复位 | 低电平复位 |
| 17 | PB15 | UART2_TX、TIMG1_C0 | 推荐接 HC-42 RXD |
| 18 | PB17 | ADC1.4、SPI1_CS1 | 多个 DDS 固定驱动占用 |
| 19 | PB12 | TIMA0_C2、TIMA1_FAL | 多个 DDS 固定驱动占用 |
| 20 | GND | 地 |  |
| 21 | 5V | 5 V 电源 | 不可直接接 HC-42 VCC |
| 22 | GND | 地 |  |
| 23 | PB19 | OPA1_IN+、ADC1.6 | `prepare-nuedc` OPA1 输入 |
| 24 | PA22 | OPA0_OUT、ADC0.7 | 板载光传感器相关，见 J16 |
| 25 | PB18 | ADC1.5 |  |
| 26 | PA18 | OPA1_IN+、ADC1.3、GPAMP_IN- | J8 用户/BSL 按键；也受 J15 选择 |
| 27 | PA24 | OPA0_IN1-、ADC0.3 |  |
| 28 | PA17 | OPA1_IN-、ADC1.2 | 多个固定驱动占用 |
| 29 | PA16 / PA18 | ADC1.1 / ADC1.3 | 由 J15 选择，默认 PA16 |
| 30 | PA15 | DAC_OUT |  |
| 31 | PA13 | CAN_RX |  |
| 32 | PA12 | CAN_TX | 多个固定驱动占用 |
| 33 | PA11 | UART0_RX、LIN_RX | 默认经 J22 接 XDS110 |
| 34 | PA10 | UART0_TX、LIN_TX | 默认经 J21 接 XDS110 |
| 35 | PB13 | TIMA0_C3、PWM | 多个 DDS 固定驱动占用 |
| 36 | PB20 | TIMA0_C2、PWM | 多个 DDS 固定驱动占用 |
| 37 | PA31 | TIMA2_C1、PWM |  |
| 38 | PA28 | TIMA2_C0、PWM |  |
| 39 | PB1 | TIMA1_C1、PWM | ILI9341 背光固定引脚 |
| 40 | PB4 | TIMA1_C0、PWM | ILI9341 片选固定引脚 |

说明：

- 排针标注是开发板推荐功能，不代表该引脚只能用于该功能。
- PB15/PB16 在开发板原理图中分别标注为 `UART2_TX`、`UART2_RX`。
- PA10/PA11 虽然在 BoosterPack 图中标注为 LIN TX/RX，但同时也是 UART0 TX/RX。

## 四、关键跳线

| 跳线 | 默认状态 | 作用 | 本项目注意事项 |
| --- | --- | --- | --- |
| J101 | 已安装 | 隔离 XDS110 与目标侧的 GND、5V、3V3、UART、复位、SWD、BSL | 功耗测量和外部供电时重点检查 |
| J4 | 已安装 | PA0 连接红色 LED1 | 当前模板闪灯占用 PA0 |
| J5 | 已安装 | PB22 连接 RGB 蓝灯 | 不用时可断开以降低功耗 |
| J6 | 已安装 | PB26 连接 RGB 红灯 | 不用时可断开以降低功耗 |
| J7 | 已安装 | PB27 连接 RGB 绿灯 | 不用时可断开以降低功耗 |
| J8 | 已安装 | PA18 连接 S1/BSL 按键 | 使用 PA18 模拟功能时应断开 |
| J9 | 1-2 | PB24 连接热敏电阻 | 不用热敏电阻时可断开 |
| J13 | 已安装 | 给热敏电阻和外置 OPA2365 供电 | 不用这两部分时断开以降低功耗 |
| J14 | 1-2，PB23 | J1.3 在 PB23 与 PA9 之间选择 | 使用 UART1_RX/PA9 时改为 PA9 档 |
| J15 | 1-2，PA16 | J3.29 在 PA16 与 PA18 之间选择 | OPA1_OUT/PA16 默认已引出 |
| J16 | 已安装 | PA22 连接光传感器电路 | 使用独立 OPA0/ADC 通道时检查 |
| J17 | 已安装 | PA27 连接光传感器电路 | 使用独立 OPA0 输入时检查 |
| J18 | 已安装 | PA26 连接光传感器电路 | 使用独立 OPA0 输入时检查 |
| J19 | 1-2，3V3 | PA0 开漏上拉 | PA0 作普通推挽输出时可断开 |
| J20 | 1-2，3V3 | PA1 开漏上拉 | PA1 作普通推挽输出时可断开 |
| J21 | 1-2，XDS_UART | PA10 在 XDS110 与排针之间选择 | 默认保留作调试串口 |
| J22 | 1-2，XDS_UART | PA11 在 XDS110 与排针之间选择 | 默认保留作调试串口 |

## 五、`prepare-nuedc` Rust 工程配置

### 5.1 目标与存储布局

```toml
[target.thumbv6m-none-eabi]
runner = "pyocd flash --target mspm0g3507 --format elf"

[build]
target = "thumbv6m-none-eabi"
```

```text
FLASH: 0x0000_0000，128 KB
RAM:   0x2020_0000，32 KB
```

### 5.2 运行时

- `no_std`、`no_main`。
- Rust edition 2024。
- 使用 `embassy-mspm0`、`embassy-executor` 和 `embassy-time`。
- `embassy-mspm0` 启用 `mspm0g3507pm`、`rt`、`time-driver-any`。
- Embassy 固定到提交 `f6daf50db52bb90e7dbee2314a04eee77b46b03b`。
- 模板调用一次 `embassy_mspm0::init(Default::default())`，随后以 PA0 驱动板载红色 LED。

### 5.3 固定驱动引脚

`prepare-nuedc` 的 MSPM0 驱动会在内部 `steal()` 固定外设和引脚。组合使用前必须人工检查冲突，同一个引脚不得被两个存活的驱动同时占用。

| 驱动 | 固定资源 |
| --- | --- |
| ILI9341 | PB6 SCLK、PB5 MOSI、PB4 CS、PB3 DC、PB2 RST、PB1 BL |
| SSD1306 | PB2 SCL、PB3 SDA |
| AD9910 | PB6 SCLK、PB12 SDIO、PB13 CS、PB15 IO_UPDATE、PB17 RESET、PA12 PROFILE0、PA13 PROFILE1、PA17 DRCTL、PA18 DRHOLD、PB20 OSK |
| AD9959 | PB6 SCLK、PB12 SDIO0、PB13 CS、PB15 IO_UPDATE、PB17 RESET、PB20 POWER_DOWN |
| ADS131A04 | SPI1、TIMG0、PB6、PB7、PB8、PA17、PA12、PA8 |
| OPA0 外部通路 | PA26 OPA0_IN0+、PA22 OPA0_OUT |
| OPA1 外部通路 | PB19 OPA1_IN0+、PA16 OPA1_OUT |

主要已知冲突：

- ILI9341 与 SSD1306 同时占用 PB2、PB3，不能直接同时实例化。
- HC-42 若使用 UART2/PB15，会与 AD9910、AD9959 的 IO_UPDATE 冲突。
- UART1/PA8 会与 ADS131A04 的 PA8 DRDY 冲突。
- OPA0 固定通路会与板载光传感器电路的 PA26、PA22 冲突，使用前应检查 J16、J18。
- OPA1 输入若改用 PA18，会与板载 S1/BSL 按键冲突；当前固定驱动使用 PB19，不占用 PA18。

## 六、本项目建议资源分配

| 功能 | 外设/引脚 | 状态 |
| --- | --- | --- |
| 板载调试 LED | PA0 | 当前模板已使用 |
| OLED 128×64 | PB2 SCL、PB3 SDA | 计划使用 SSD1306 固定驱动 |
| 模拟前端 | OPA1，PB19 输入、PA16 输出或内部 ADC 通路 | 待模拟链路实测确认 |
| HC-42 TXD | PB16 / UART2_RX | 推荐 |
| HC-42 RXD | PB15 / UART2_TX | 推荐 |
| HC-42 电源 | 3V3、GND | 禁止接 5V |
| 地址开关 S1 / bit 0 | PB13，排针 35 | 权重 1，ON 接地 |
| 地址开关 S2 / bit 1 | PB20，排针 36 | 权重 2，ON 接地 |
| 地址开关 S3 / bit 2 | PA31，排针 37 | 权重 4，ON 接地 |
| 地址开关 S4 / bit 3 | PA28，排针 38 | 权重 8，ON 接地 |
| UART0 调试口 | PA10/PA11，经 XDS110 | 建议保留 |

选择 UART2/PB15-PB16 连接 HC-42 的理由：

1. 不占用板载 XDS110 使用的 UART0。
2. 不需要修改 J14 跳线。
3. 与当前 OLED、OPA1 规划不冲突。
4. PB15、PB16 都直接引出到 BoosterPack 排针，分别为 17、11 号针。

接线如下：

| LP-MSPM0G3507 | HC-42 |
| --- | --- |
| J2/J4-17，PB15/UART2_TX | RXD |
| J2/J4-11，PB16/UART2_RX | TXD |
| 3V3 | VCC |
| GND | GND |

4 位编码开关使用 GPIO 内部上拉，不需要外接上拉电阻。每个开关的一端分别连接 PB13、PB20、PA31、PA28，另一端全部连接 GND：

| 开关 | GPIO | BoosterPack 针号 | 位权 |
| --- | --- | ---: | ---: |
| S1 | PB13 | 35 | 1 |
| S2 | PB20 | 36 | 2 |
| S3 | PA31 | 37 | 4 |
| S4 | PA28 | 38 | 8 |

- OFF 断开，GPIO 被内部上拉，软件解释为 `0`。
- ON 闭合到 GND，GPIO 读取低电平，软件解释为 `1`。
- 四位地址范围为十进制 `00 - 15`，每秒重新读取并写入蓝牙测试帧的 `ADDR` 字段。
- 例如 S4/S3/S2/S1 为 `0/1/0/1` 时，地址为 `4 + 1 = 05`。

## 七、低功耗测量检查清单

1. 断开 J101 的 `3V3` 跳帽，在该位置串入电流表。
2. 断开不需要的 XDS110 UART、复位、SWD、BSL 等隔离跳帽，防止调试器信号产生额外灌拉电流。
3. 不使用板载热敏电阻和外置 OPA2365 时断开 J13。
4. 不使用板载 LED 时断开 J4 - J7。
5. 不使用板载光传感器时断开 J16 - J18。
6. 不使用 S1/BSL 按键时评估是否断开 J8。
7. 所有未用输入必须有确定电平，不能悬空。
8. 分别测量 MCU、OLED 和 HC-42 的工作、广播、连接及休眠电流。
9. 记录开发板测量值与最终自制 PCB 测量值；LaunchPad 板载器件较多，两者不能直接等同。

## 八、常用命令

```sh
# 检查
cargo check

# 发布构建
cargo build --release

# 按工程 runner 烧录
cargo run --release
```

若使用 `prepare-nuedc` 重新生成独立工程：

```sh
cargo run -p xtask -- new \
  --board mspm0g3507 \
  --name demo-mspm0 \
  --output ../demo-mspm0
```
