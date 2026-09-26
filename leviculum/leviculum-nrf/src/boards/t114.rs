//! Heltec Mesh Node T114 pin mappings and hardware constants
//!
//! nRF52840 + SX1262 LoRa radio + L76K GPS + optional ST7789 TFT display.
//! Pin assignments verified from Heltec Rev 2.0 Pin Map, datasheet,
//! Meshtastic firmware `variants/nrf52840/heltec_mesh_node_t114/variant.h`,
//! and RNode firmware `Boards.h`.
//!
//! Reference: <https://resource.heltec.cn/download/Mesh_Node_T114/Mesh_node_t114_Pin_Map.png>

use embassy_nrf::gpio::{Level, Output, OutputDrive};
use embassy_nrf::peripherals;
use embassy_nrf::Peri;

// SX1262 LoRa Radio (SPI0)
/// SX1262 SPI clock
pub type LoRaSck = peripherals::P0_19;
/// SX1262 SPI MOSI
pub type LoRaMosi = peripherals::P0_22;
/// SX1262 SPI MISO
pub type LoRaMiso = peripherals::P0_23;
/// SX1262 SPI chip-select (active low)
pub type LoRaCs = peripherals::P0_24;
/// SX1262 reset (active low)
pub type LoRaReset = peripherals::P0_25;
/// SX1262 busy indicator (high = busy)
pub type LoRaBusy = peripherals::P0_17;
/// SX1262 DIO1 interrupt output
pub type LoRaDio1 = peripherals::P0_20;

/// SX1262 SPI frequency in Hz (Meshtastic uses 4 MHz; RNode uses 16 MHz)
pub const LORA_SPI_FREQ_HZ: u32 = 4_000_000;
/// SX1262 TCXO voltage supplied via DIO3 (volts)
pub const LORA_TCXO_VOLTAGE: f32 = 1.8;
/// SX1262 max TX power (dBm)
pub const LORA_MAX_POWER_DBM: i8 = 22;
// The OCP current limit is not here. It is written as part of the
// transmit-power sequence, which has to stay in one host-testable piece
// (`leviculum_core::sx126x::OCP_HIGH_POWER`); this constant was never read by
// anything and a second copy of the number is a second thing to keep in step.
/// SX1262 uses DIO2 as internal RF switch (no external RXEN/TXEN pins)
pub const LORA_DIO2_AS_RF_SWITCH: bool = true;
/// SX1262 BUSY polling timeout (ms, from RNode firmware)
pub const LORA_BUSY_TIMEOUT_MS: u32 = 100;

// Green LED
/// Green indicator LED (active LOW)
pub type LedPin = peripherals::P1_03;

// NeoPixel (2x SK6812)
/// NeoPixel data pin (GRB, 800 kHz WS2812 protocol)
pub type NeoPixelPin = peripherals::P0_14;
/// Number of addressable NeoPixel LEDs
pub const NEOPIXEL_COUNT: usize = 2;

// UART (NFC pins repurposed as GPIO)
/// UART RX (header P1, NFC pin repurposed)
pub type UartRx = peripherals::P0_09;
/// UART TX (header P1, NFC pin repurposed)
pub type UartTx = peripherals::P0_10;

// GPS (Quectel L76K, powered via VEXT)
/// GPS UART TX (MCU → GPS; Meshtastic `GPS_RX_PIN (32+5)`, named from
/// the module's side there)
pub type GpsTx = peripherals::P1_05;
/// GPS UART RX (GPS → MCU; Meshtastic `GPS_TX_PIN (32+7)`)
pub type GpsRx = peripherals::P1_07;
/// GPS standby control (LOW = allow sleep, HIGH = force wake;
/// `variant.h:170` with `GPS_STANDBY_ACTIVE LOW`, `GPS.h:22-23`)
pub type GpsStandby = peripherals::P1_02;
/// GPS PPS (pulse-per-second) input
pub type GpsPps = peripherals::P1_04;

// I2C0 (RTC footprint, optional PCF8563TS)
/// I2C0 SDA (RTC footprint)
pub type I2c0Sda = peripherals::P0_26;
/// I2C0 SCL (RTC footprint)
pub type I2c0Scl = peripherals::P0_27;

// I2C1 (general purpose, header P1)
/// I2C1 SDA (general purpose, header P1)
pub type I2c1Sda = peripherals::P0_16;
/// I2C1 SCL (general purpose, header P1)
pub type I2c1Scl = peripherals::P0_13;

// TFT Display (ST7789 240x135, optional, SPI1)
/// TFT SPI clock
pub type TftSck = peripherals::P1_08;
/// TFT SPI MOSI
pub type TftMosi = peripherals::P1_09;
/// TFT chip-select (active low)
pub type TftCs = peripherals::P0_11;
/// TFT data/command select
pub type TftDc = peripherals::P0_12;
/// TFT reset (active low)
pub type TftReset = peripherals::P0_02;
/// TFT backlight control (active LOW = on)
pub type TftBacklight = peripherals::P0_15;
/// TFT power enable
pub type TftPowerEn = peripherals::P0_03;

// QSPI Flash (MX25R1635F, 16 Mbit)
/// QSPI flash clock
pub type QspiClk = peripherals::P1_14;
/// QSPI flash chip-select
pub type QspiCs = peripherals::P1_15;
/// QSPI flash IO0 (MOSI)
pub type QspiIo0 = peripherals::P1_12;
/// QSPI flash IO1 (MISO)
pub type QspiIo1 = peripherals::P1_13;
/// QSPI flash IO2 (WP#)
pub type QspiIo2 = peripherals::P0_07;
/// QSPI flash IO3 (HOLD#, must be HIGH before QSPI activation)
pub type QspiIo3 = peripherals::P0_05;

// Battery / ADC
/// Battery voltage sense (AIN2)
pub type BatteryAdc = peripherals::P0_04;
/// ADC divider enable (HIGH = enabled)
pub type AdcCtrl = peripherals::P0_06;
/// ADC multiplier: converts raw ADC reading to battery voltage (mV)
pub const ADC_MULTIPLIER: f32 = 4.916;

// External peripheral power
/// VEXT enable (HIGH = on, controls GPS + display power rail, 1s warmup)
pub type VextEnable = peripherals::P0_21;
/// VEXT warmup time in milliseconds
pub const VEXT_WARMUP_MS: u32 = 1000;

// User button
/// User button
pub type UserButton = peripherals::P1_10;

/// Create the green LED output (active low)
pub fn led(pin: Peri<'static, LedPin>) -> Output<'static> {
    // LED is active LOW, start with Level::High (LED off)
    Output::new(pin, Level::High, OutputDrive::Standard)
}

/// Runtime board metadata for shared init code (USB / flash / LoRa).
pub const CONFIG: super::BoardConfig = super::BoardConfig {
    usb_vid: 0x1209,
    usb_pid: 0x0001,
    usb_manufacturer: "leviculum",
    usb_product: "leviculum T114",
    log_prefix: "T114",
    identity_flash_page: 0xEC000,
    radio_config_flash_page: 0xEB000,
    telemetry_flash_page: 0xEA000,
    lora_tcxo_voltage_reg: 0x02, // 1.8 V
    lora_spi_freq_hz: LORA_SPI_FREQ_HZ,
    lora_max_power_dbm: LORA_MAX_POWER_DBM,
    qspi_part: &crate::qspi::MX25R1635F,
};

/// Panic-LED descriptor — port, pin, active-low flag — for `set_panic_led`.
/// LED1 (green) on P1.03, active low.
pub const PANIC_LED_PORT: u8 = 1;
pub const PANIC_LED_PIN: u8 = 3;
pub const PANIC_LED_ACTIVE_LOW: bool = true;
