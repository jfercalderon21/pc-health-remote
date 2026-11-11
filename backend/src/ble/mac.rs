//! macOS Bluetooth LE transport via CoreBluetooth (btleplug).
//! Auto-discovers a PCHealth* peripheral, verifies the Flipper serial GATT
//! service, and reuses the existing HELLO/telemetry session implementation.
//! No Bluetooth MAC addresses are assumed: CoreBluetooth uses UUID identifiers.

use super::{Found, NAME_PREFIX, RX_CHAR_UUID, SERIAL_SERVICE_UUID, TX_CHAR_UUID};
use crate::session::{self, Link, SessionEnd};
use crate::shared::{LinkKind, Shared};
use anyhow::{anyhow, bail, Context, Result};
use btleplug::api::{Central, CharPropFlags, Manager as _, Peripheral as _, ScanFilter, WriteType};
use btleplug::platform::{Adapter, Manager, Peripheral};
use futures::{Stream, StreamExt};
use std::pin::Pin;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tokio::runtime::{Builder, Runtime};
use uuid::Uuid;

const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
const BACKOFF_MIN: Duration = Duration::from_secs(2);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
const HEALTHY_AFTER: Duration = Duration::from_secs(15);
const SCAN_TIMEOUT: Duration = Duration::from_secs(12);
const SCAN_POLL: Duration = Duration::from_millis(400);

type Notifications = Pin<Box<dyn Stream<Item = btleplug::api::ValueNotification> + Send>>;

fn runtime() -> Result<Runtime> {
    Builder::new_current_thread().enable_all().build().context("creating CoreBluetooth async runtime")
}

async fn adapter() -> Result<Adapter> {
    let manager = Manager::new().await.context("initialising CoreBluetooth (check Terminal Bluetooth permission)")?;
    manager.adapters().await?.into_iter().next().ok_or_else(|| anyhow!("no CoreBluetooth adapter available"))
}

fn peripheral_name(properties: &btleplug::api::PeripheralProperties) -> String {
    properties.local_name.clone().or_else(|| properties.advertisement_name.clone()).unwrap_or_default()
}

/// Discovery is supported on macOS. There is no public Bluetooth MAC address,
/// so `Found.address` is zero; do not persist it as a paired device address.
pub fn scan(duration: Duration) -> Result<Vec<Found>> {
    let rt = runtime()?;
    let _guard = rt.enter();
    rt.block_on(async {
        let adapter = adapter().await?;
        adapter.start_scan(ScanFilter::default()).await?;
        tokio::time::sleep(duration).await;
        let mut devices = Vec::new();
        let result = adapter.peripherals().await;
        let _ = adapter.stop_scan().await;
        for peripheral in result? {
            if let Some(properties) = peripheral.properties().await? {
                let name = peripheral_name(&properties);
                if name.starts_with(NAME_PREFIX) {
                    devices.push(Found { address: 0, name, rssi: properties.rssi.unwrap_or(-127) });
                }
            }
        }
        Ok(devices)
    })
}

/// Pairing/remembering CoreBluetooth UUIDs will be implemented separately.
/// The BLE worker does not need pairing: it auto-discovers PCHealth* devices.
pub fn pair_device(_address: u64, _force: bool) -> Result<bool> {
    bail!("macOS BLE uses automatic CoreBluetooth discovery; run with --transport ble instead of 'pair'")
}

pub fn unpair_device(_address: u64) -> Result<()> {
    bail!("macOS BLE does not use MAC-address pairing; there is no BLE bond to remove here")
}

pub fn paired_state(_address: u64) -> Option<bool> {
    None
}

struct BleLink<'a> {
    rt: &'a Runtime,
    peripheral: Peripheral,
    rx: btleplug::api::Characteristic,
    write_type: WriteType,
    notifications: Notifications,
}

impl Link for BleLink<'_> {
    fn recv(&mut self, timeout: Duration) -> Result<Vec<u8>> {
        let next = self.rt.block_on(async { tokio::time::timeout(timeout, self.notifications.next()).await });
        match next {
            Ok(Some(notification)) if notification.uuid == Uuid::from_u128(TX_CHAR_UUID) => Ok(notification.value),
            Ok(Some(_)) => Ok(Vec::new()),
            Ok(None) => bail!("CoreBluetooth indication stream closed"),
            Err(_) => {
                if self.rt.block_on(self.peripheral.is_connected())? {
                    Ok(Vec::new())
                } else {
                    bail!("BLE link disconnected")
                }
            }
        }
    }

    fn send(&mut self, frame: &[u8]) -> Result<()> {
        self.rt
            .block_on(self.peripheral.write(&self.rx, frame, self.write_type))
            .context("CoreBluetooth GATT write failed")
    }
}

async fn find_target(adapter: &Adapter) -> Result<Option<(Peripheral, String)>> {
    adapter.start_scan(ScanFilter::default()).await.context("starting CoreBluetooth scan")?;
    let start = Instant::now();
    let mut found = None;
    while start.elapsed() < SCAN_TIMEOUT {
        for peripheral in adapter.peripherals().await? {
            if let Some(properties) = peripheral.properties().await? {
                let name = peripheral_name(&properties);
                if name.starts_with(NAME_PREFIX) {
                    found = Some((peripheral, name));
                    break;
                }
            }
        }
        if found.is_some() {
            break;
        }
        tokio::time::sleep(SCAN_POLL).await;
    }
    let _ = adapter.stop_scan().await;
    Ok(found)
}

async fn connect(
    adapter: &Adapter,
) -> Result<Option<(Peripheral, btleplug::api::Characteristic, Notifications, String)>> {
    let Some((peripheral, name)) = find_target(adapter).await? else {
        return Ok(None);
    };
    log::info!("CoreBluetooth discovered {name} ({:?})", peripheral.id());
    if !peripheral.is_connected().await? {
        peripheral.connect().await.context("connecting to Flipper via BLE")?;
    }
    peripheral.discover_services().await.context("discovering Flipper GATT services")?;

    let service = Uuid::from_u128(SERIAL_SERVICE_UUID);
    let characteristics = peripheral.characteristics();
    let by_uuid = |id| characteristics.iter().find(|c| c.service_uuid == service && c.uuid == id).cloned();
    let rx =
        by_uuid(Uuid::from_u128(RX_CHAR_UUID)).ok_or_else(|| anyhow!("Flipper serial RX characteristic missing"))?;
    let tx =
        by_uuid(Uuid::from_u128(TX_CHAR_UUID)).ok_or_else(|| anyhow!("Flipper serial TX characteristic missing"))?;
    let notifications = peripheral.notifications().await.context("opening Flipper indication stream")?;
    peripheral.subscribe(&tx).await.context("subscribing to Flipper TX indications")?;
    Ok(Some((peripheral, rx, notifications, name)))
}

pub fn spawn(shared: Arc<Shared>) -> Option<JoinHandle<()>> {
    Some(
        std::thread::Builder::new()
            .name("ble-macos".into())
            .spawn(move || match runtime() {
                Ok(rt) => worker(&shared, &rt),
                Err(error) => log::error!("macOS BLE disabled: {error:#}"),
            })
            .expect("spawn macOS BLE thread"),
    )
}

fn worker(shared: &Shared, rt: &Runtime) {
    let _guard = rt.enter();
    let mut backoff = BACKOFF_MIN;
    while !shared.stopping() {
        if !shared.transport_allows(LinkKind::Ble) || shared.pairing() || shared.owns(LinkKind::Usb) {
            shared.sleep(Duration::from_secs(1));
            continue;
        }
        let started = Instant::now();
        let result: Result<SessionEnd> = (|| {
            let adapter = rt.block_on(adapter())?;
            let Some((peripheral, rx, notifications, name)) = rt.block_on(connect(&adapter))? else {
                bail!("PCHealth* Flipper not advertising");
            };
            let write_type = if rx.properties.contains(CharPropFlags::WRITE_WITHOUT_RESPONSE) {
                WriteType::WithoutResponse
            } else {
                WriteType::WithResponse
            };
            let mut link = BleLink { rt, peripheral: peripheral.clone(), rx, write_type, notifications };
            log::info!("CoreBluetooth GATT connected to {name}; waiting for Flipper HELLO");
            let result = session::run(&mut link, LinkKind::Ble, &name, shared, HELLO_TIMEOUT);
            let _ = rt.block_on(peripheral.disconnect());
            result
        })();
        match result {
            Ok(SessionEnd::Stopped) => break,
            Ok(end) => log::info!("macOS BLE session ended: {end:?}"),
            Err(error) => log::warn!("macOS BLE attempt failed: {error:#}"),
        }
        if started.elapsed() >= HEALTHY_AFTER {
            backoff = BACKOFF_MIN;
        } else {
            shared.sleep(backoff);
            backoff = (backoff * 2).min(BACKOFF_MAX);
        }
    }
}
