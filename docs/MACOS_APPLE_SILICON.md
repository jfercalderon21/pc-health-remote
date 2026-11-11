# PC Health Remote on macOS Apple Silicon

This guide covers the experimental Apple Silicon backend port in the `apple-silicon-macos` branch. It works with the **original PC Health Remote Flipper Zero application v1.00**; no Flipper firmware or wire-protocol changes are required.

## Tested setup

- MacBook Pro with Apple M2, macOS 15.x.
- Rust 1.99.1 (project minimum: Rust 1.90).
- Flipper Zero running PC Health Remote v1.00, advertising as `PCHealth Oletol`.
- USB CDC and Bluetooth LE telemetry both tested successfully; BLE reconnection after reopening the Flipper application tested successfully.
- 51 backend unit tests passed.

This is a hardware-specific validation, **not** a guarantee of support across all Apple Silicon models or macOS versions.

## Build

From the repository root:

```sh
cd backend
cargo fmt --all -- --check
cargo test
cargo build --release
```

The executable is at `backend/target/release/pc-health-remote`. The macOS backend is headless (no Windows system-tray UI).

## Run over USB

Open **PC Health Remote** on the Flipper, connect it to the Mac with a data-capable USB cable, then run:

```sh
./target/release/pc-health-remote --transport usb --no-tray --console run
```

macOS should expose a serial device such as `/dev/cu.usbmodemflip_...`.

## Run over Bluetooth LE

Enable Bluetooth in macOS and grant Bluetooth access to your terminal application when prompted. Open **PC Health Remote** on the Flipper and leave it on **Waiting for PC**. Disconnect USB if you want to test BLE alone.

```sh
./target/release/pc-health-remote --transport ble --no-tray --console run
```

The macOS transport scans for a nearby `PCHealth*` advertiser, discovers the Flipper serial GATT service, subscribes to TX indications and uses the existing HELLO/telemetry protocol. It reconnects when the Flipper application is reopened. The macOS implementation does **not** require manually entering the peripheral's Bluetooth MAC address; CoreBluetooth uses a platform-specific peripheral identifier.

**Do not use the Windows-oriented `pair`, `unpair` or `install` instructions from the main README on macOS.** macOS BLE discovery is automatic in this experimental port.

## Automatic transport mode: known limitation

```sh
./target/release/pc-health-remote --transport auto --no-tray --console run
```

Both USB and BLE work independently. However, **automatic switching from an active BLE session to USB when a cable is attached has not been demonstrated and did not occur in local testing**. The USB worker may intentionally leave the port alone while BLE is active. For predictable behavior, explicitly choose `--transport usb` or `--transport ble`. Automatic switching should not be advertised as functional for this port.

## Metrics and sources

| Metric | macOS implementation | Notes |
| --- | --- | --- |
| CPU utilization, frequency, RAM, disk, uptime and top processes | `sysinfo` | Frequency is reported from available system information. |
| CPU temperature | IOHID PMU `tdie` | Chip-die temperature proxy; **not necessarily CPU package temperature**. |
| GPU utilization | Apple IOReport | Needs two samples; the one-shot `metrics` command may show GPU as `null`. |
| GPU temperature | AppleSMC `Tg**` | Mean of readable GPU temperature sensors. |
| Fan speed | AppleSMC `F*Ac` | `0 RPM` is a valid stopped-fan reading; unavailable is distinct. |
| Battery percentage and charging state | IOKit `AppleSmartBattery` | Validated on a MacBook with a battery. |

AppleSMC and IOReport include undocumented/private Apple interfaces and could change in future macOS versions.

## Inspect readings

```sh
./target/release/pc-health-remote metrics
./target/release/pc-health-remote status
```

Configuration and logs are normally at `~/.config/pc-health-remote/config.toml` and `~/.config/pc-health-remote/log.txt`. `PHR_CONFIG_DIR` overrides the configuration directory.

## Troubleshooting

- **BLE device not found:** Keep the Flipper's PC Health Remote app open at *Waiting for PC*, check Bluetooth is on, and grant Bluetooth permission to Terminal in **System Settings > Privacy & Security > Bluetooth**.
- **No GPU in `metrics`:** The one-shot command can end before the IOReport delta is available; inspect continuous streaming.
- **No fan speed:** SMC fan keys vary by model. A reported `0 RPM` may be normal when the fan is stopped.
- **USB port not detected:** Use a data-capable cable and check `/dev/cu.usbmodem*`. For testing, select `--transport usb` explicitly.
- **BLE active but USB does not take over:** This is the known automatic-switching limitation above.

## Upstream and license

This branch adapts [ataman-lab/pc-health-remote](https://github.com/ataman-lab/pc-health-remote), originally by its contributors, and retains the repository's MIT license and notices. The SMC-key approach was informed by [vladkens/macmon](https://github.com/vladkens/macmon) (MIT). This port is not an official Apple or Flipper Devices project.
