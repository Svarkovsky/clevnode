//! The BlueZ half of the Columba BLE interface: everything that talks to
//! bluer. Thin by design — protocol decisions live in [`super::links`],
//! orchestration in [`super`]; this module performs advertising, the
//! GATT server, windowed scanning and the central-role connections, and
//! reports what happened as [`Ev`] events.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use bluer::adv::{Advertisement, AdvertisementHandle, Type as AdvType};
use bluer::gatt::local::{
    Application, ApplicationHandle, Characteristic, CharacteristicNotify,
    CharacteristicNotifyMethod, CharacteristicRead, CharacteristicWrite, CharacteristicWriteMethod,
    Service,
};
use bluer::gatt::remote::CharacteristicWriteRequest;
use bluer::gatt::WriteOp;
use bluer::{Adapter, AdapterEvent, Address, DiscoveryFilter, DiscoveryTransport, Uuid};
use futures::{FutureExt, StreamExt};
use tokio::sync::{mpsc, oneshot};

use super::links::{IdentityHash, SERVICE_UUID_U128};
use super::{Ev, LINK_QUEUE_DEPTH};
use leviculum_ble_tx::{manufacturer_data, COMPANY_ID};

/// The Columba GATT service and its three characteristics
/// (BLE_PROTOCOL_v2.2 §GATT Service Structure).
pub(crate) const SERVICE_UUID: Uuid = Uuid::from_u128(SERVICE_UUID_U128);
/// TX: peripheral → central, read + notify.
pub(crate) const TX_UUID: Uuid = Uuid::from_u128(0x37145b00_442d_4a94_917f_8f42c5da28e4);
/// RX: central → peripheral, write + write-without-response.
pub(crate) const RX_UUID: Uuid = Uuid::from_u128(0x37145b00_442d_4a94_917f_8f42c5da28e5);
/// Identity: the 16-byte identity hash, read-only.
pub(crate) const IDENTITY_UUID: Uuid = Uuid::from_u128(0x37145b00_442d_4a94_917f_8f42c5da28e6);

/// One scan window per discovery cycle. The reference scans in windows
/// too (its `balanced` mode: 1 s scan, `discovery_interval` pause);
/// windowed discovery also keeps BlueZ's scanner out of the way of our
/// own connection attempts, which mostly land in the pauses.
const SCAN_WINDOW: Duration = Duration::from_secs(2);

/// Budget for one central-role connection setup: connect, resolve,
/// identity read, subscribe, handshake.
const SETUP_TIMEOUT: Duration = Duration::from_secs(20);

/// Register the Columba advertisement.
///
/// Separate from [`serve_gatt`] because the two have different
/// lifetimes: the GATT application serves live sessions for the whole
/// BlueZ session, while the advertisement is gated on link-table
/// occupancy (`LinkTable::should_advertise`, the firmware's `ADV_LOCK`
/// policy) — deregistered when the table fills, re-registered when a
/// slot frees. Dropping the returned handle deregisters it.
///
/// The advertisement carries the service UUID and the v0.3.0 capability
/// record with flags 0x00 — dual-role, and deliberately *present* rather
/// than omitted, so peers sort us by the confirmed rule instead of the
/// assumed one. BlueZ places the `LN-<hex8>` local name in the scan
/// response, where it does not compete with the 31 advertisement bytes
/// (same layout as the firmware's).
pub(crate) async fn register_advertisement(
    adapter: &Adapter,
    identity: IdentityHash,
    iface: &str,
) -> bluer::Result<AdvertisementHandle> {
    let name = super::links::local_name(&identity);
    // The manufacturer-record payload minus the company ID: BlueZ keys
    // the record by CID and prepends it on the wire.
    let record = manufacturer_data(super::links::LOCAL_CAPS)[2..].to_vec();

    let advertisement = Advertisement {
        advertisement_type: AdvType::Peripheral,
        service_uuids: BTreeSet::from([SERVICE_UUID]),
        manufacturer_data: BTreeMap::from([(COMPANY_ID, record)]),
        discoverable: Some(true),
        local_name: Some(name.clone()),
        ..Default::default()
    };
    let adv = adapter.advertise(advertisement).await?;
    tracing::info!(
        "BLE {iface}: advertising as {name} (service {SERVICE_UUID}, dual-role record present)"
    );
    Ok(adv)
}

/// Register the Columba GATT application; it serves every
/// peripheral-role session for the whole BlueZ session. Dropping the
/// returned handle deregisters it.
pub(crate) async fn serve_gatt(
    adapter: &Adapter,
    identity: IdentityHash,
    ev_tx: mpsc::Sender<Ev>,
) -> bluer::Result<ApplicationHandle> {
    let write_tx = ev_tx.clone();
    let notify_tx = ev_tx;
    let app = Application {
        services: vec![Service {
            uuid: SERVICE_UUID,
            primary: true,
            characteristics: vec![
                Characteristic {
                    uuid: TX_UUID,
                    read: Some(CharacteristicRead {
                        read: true,
                        // The TX value is only ever pushed as
                        // notifications; a read returns nothing. The
                        // read flag exists because the reference
                        // declares it (`ble-reticulum@07d94130` `linux_bluetooth_driver.py`, TX char flags).
                        fun: Box::new(|_req| async move { Ok(Vec::new()) }.boxed()),
                        ..Default::default()
                    }),
                    notify: Some(CharacteristicNotify {
                        notify: true,
                        method: CharacteristicNotifyMethod::Fun(Box::new(move |notifier| {
                            let tx = notify_tx.clone();
                            async move {
                                let _ = tx.send(Ev::PeriphNotify(notifier)).await;
                            }
                            .boxed()
                        })),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                Characteristic {
                    uuid: RX_UUID,
                    write: Some(CharacteristicWrite {
                        write: true,
                        write_without_response: true,
                        method: CharacteristicWriteMethod::Fun(Box::new(move |data, req| {
                            let tx = write_tx.clone();
                            async move {
                                let _ = tx
                                    .send(Ev::PeriphWrite {
                                        addr: req.device_address,
                                        mtu: usize::from(req.mtu),
                                        data,
                                    })
                                    .await;
                                Ok(())
                            }
                            .boxed()
                        })),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                Characteristic {
                    uuid: IDENTITY_UUID,
                    read: Some(CharacteristicRead {
                        read: true,
                        fun: Box::new(move |_req| async move { Ok(identity.to_vec()) }.boxed()),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
            ],
            ..Default::default()
        }],
        ..Default::default()
    };
    adapter.serve_gatt_application(app).await
}

/// Windowed discovery: scan for [`SCAN_WINDOW`], report every sighting,
/// pause for the configured interval, repeat. Dropping the discovery
/// stream stops the BlueZ scan between windows.
pub(crate) struct ScanTask {
    pub adapter: Adapter,
    pub ev_tx: mpsc::Sender<Ev>,
    pub interval: Duration,
    pub iface: String,
}

impl ScanTask {
    pub(crate) async fn run(self) {
        let filter = DiscoveryFilter {
            uuids: [SERVICE_UUID].into_iter().collect(),
            transport: DiscoveryTransport::Le,
            ..Default::default()
        };
        if let Err(e) = self.adapter.set_discovery_filter(filter).await {
            tracing::warn!("BLE {}: discovery filter rejected: {e}", self.iface);
        }
        loop {
            // `discover_devices_with_changes`, not `discover_devices`:
            // plain `DeviceAdded` fires only when bluetoothd CREATES a
            // device object. For a device it already holds (stored after
            // an earlier connect, or created by a previous window), the
            // stream opens with a synthetic `DeviceAdded` carrying no
            // RSSI — BlueZ drops RSSI outside discovery, so the receiver
            // discards it as a cache entry — and the adv reports that
            // then land in the window only update properties, which the
            // plain stream never surfaces. Every window after the first
            // was blind to every known peer (BLE path hunt round 2,
            // 2026-08-31). The `_with_changes` stream re-emits
            // `DeviceAdded` on each property change, so the first adv
            // report of the window re-offers the device with its RSSI
            // set.
            match self.adapter.discover_devices_with_changes().await {
                Ok(events) => {
                    let mut events = std::pin::pin!(events);
                    let window_end = tokio::time::Instant::now() + SCAN_WINDOW;
                    loop {
                        let event = tokio::select! {
                            ev = events.next() => ev,
                            _ = tokio::time::sleep_until(window_end) => break,
                        };
                        match event {
                            Some(AdapterEvent::DeviceAdded(addr)) => {
                                if let Some(ev) = self.probe(addr).await {
                                    if self.ev_tx.send(ev).await.is_err() {
                                        return;
                                    }
                                }
                            }
                            Some(_) => {}
                            None => break,
                        }
                    }
                }
                Err(e) => {
                    tracing::debug!("BLE {}: discovery failed: {e}", self.iface);
                }
            }
            tokio::time::sleep(self.interval).await;
        }
    }

    /// Read the sighting's decision inputs from BlueZ's parsed properties.
    async fn probe(&self, addr: Address) -> Option<Ev> {
        let device = self.adapter.device(addr).ok()?;
        let rssi = device.rssi().await.ok()?;
        let offers_service = device
            .uuids()
            .await
            .ok()?
            .is_some_and(|uuids| uuids.contains(&SERVICE_UUID));
        let record = device
            .manufacturer_data()
            .await
            .ok()
            .flatten()
            .and_then(|mut m| m.remove(&COMPANY_ID));
        Some(Ev::Scan {
            addr,
            rssi,
            offers_service,
            record,
        })
    }
}

/// One central-role connection: connect, read the peer's identity, ask
/// the orchestrator for admission, subscribe, handshake, then pump
/// frames in both directions until either side ends the session.
/// Always reports [`Ev::CentralGone`] as its last event.
pub(crate) struct CentralTask {
    pub adapter: Adapter,
    pub addr: Address,
    pub own_identity: IdentityHash,
    pub ev_tx: mpsc::Sender<Ev>,
    pub iface: String,
}

impl CentralTask {
    pub(crate) async fn run(self) {
        match tokio::time::timeout(SETUP_TIMEOUT, self.establish()).await {
            Ok(Ok(Some(session))) => self.session_loop(session).await,
            Ok(Ok(None)) => {
                // Admission refused; the orchestrator logged why.
            }
            Ok(Err(e)) => {
                tracing::debug!("BLE {}: central {} failed: {e}", self.iface, self.addr);
            }
            Err(_) => {
                tracing::debug!("BLE {}: central {} setup timed out", self.iface, self.addr);
            }
        }
        if let Ok(device) = self.adapter.device(self.addr) {
            let _ = device.disconnect().await;
        }
        let _ = self.ev_tx.send(Ev::CentralGone { addr: self.addr }).await;
    }

    async fn establish(&self) -> bluer::Result<Option<CentralSession>> {
        let device = self.adapter.device(self.addr)?;
        if !device.is_connected().await? {
            device.connect().await?;
        }

        // `Device::services` waits for BlueZ's ServicesResolved itself.
        let mut tx_char = None;
        let mut rx_char = None;
        let mut id_char = None;
        for service in device.services().await? {
            if service.uuid().await? != SERVICE_UUID {
                continue;
            }
            for characteristic in service.characteristics().await? {
                match characteristic.uuid().await? {
                    u if u == TX_UUID => tx_char = Some(characteristic),
                    u if u == RX_UUID => rx_char = Some(characteristic),
                    u if u == IDENTITY_UUID => id_char = Some(characteristic),
                    _ => {}
                }
            }
        }
        let (Some(tx_char), Some(rx_char), Some(id_char)) = (tx_char, rx_char, id_char) else {
            return Err(missing_gatt());
        };

        // Identity first: admission is decided before any subscription
        // or handshake touches the peer's session state (the firmware
        // orders it the same way).
        let identity_bytes = id_char.read().await?;
        let identity: IdentityHash = identity_bytes
            .as_slice()
            .try_into()
            .map_err(|_| missing_gatt())?;
        // BlueZ ≥ 5.62 exposes the exchanged ATT MTU as a characteristic
        // property; on anything older fall back to the BLE 4.0 floor,
        // the reference's final fallback too.
        let mtu = tx_char
            .mtu()
            .await
            .unwrap_or(leviculum_core::framing::ble::MIN_MTU);

        let (frames_tx, frames_rx) = mpsc::channel(LINK_QUEUE_DEPTH);
        let (ack_tx, ack_rx) = oneshot::channel();
        let sent = self
            .ev_tx
            .send(Ev::CentralIdentity {
                addr: self.addr,
                identity,
                mtu,
                frames: frames_tx,
                ack: ack_tx,
            })
            .await;
        if sent.is_err() || !ack_rx.await.unwrap_or(false) {
            return Ok(None);
        }

        // Subscribe before the handshake so nothing the peer sends lands
        // in an unsubscribed gap.
        let notify_stream = tx_char.notify().await?;

        // The 16-byte identity handshake, written WITH response
        // (BLE_PROTOCOL_v2.2 §Identity Handshake: a Write Request).
        rx_char
            .write_ext(
                &self.own_identity,
                &CharacteristicWriteRequest {
                    op_type: WriteOp::Request,
                    ..Default::default()
                },
            )
            .await?;

        Ok(Some(CentralSession {
            notify_stream: Box::pin(notify_stream),
            frames_rx,
            rx_char,
        }))
    }

    async fn session_loop(&self, mut session: CentralSession) {
        let write_req = CharacteristicWriteRequest {
            op_type: WriteOp::Command,
            ..Default::default()
        };
        // This link's inter-packet gap (#376): the queue carries one
        // packet's fragments per unit, so pacing packet-to-packet is a
        // sleep between units. Keepalives bypass the gap and never
        // slide its window, same rule as the firmware pumps.
        let start = tokio::time::Instant::now();
        let mut pacer = super::links::LinkPacer::new();
        loop {
            tokio::select! {
                frame = session.notify_stream.next() => {
                    let Some(data) = frame else { break };
                    let ev = Ev::CentralFrame { addr: self.addr, data };
                    if self.ev_tx.send(ev).await.is_err() {
                        break;
                    }
                }
                unit = session.frames_rx.recv() => {
                    let Some(frames) = unit else { break };
                    let keepalive = frames.len() == 1
                        && frames[0].len()
                            < leviculum_core::framing::ble::FRAGMENT_HEADER_SIZE;
                    if !keepalive {
                        let now_ms = tokio::time::Instant::now()
                            .duration_since(start)
                            .as_millis() as u64;
                        let wait = pacer.wait_ms(now_ms);
                        if wait > 0 {
                            tracing::info!(
                                event = "BLE_TX_GAP",
                                iface = %self.iface,
                                addr = %self.addr,
                                waited_ms = wait,
                            );
                            tokio::time::sleep(Duration::from_millis(wait)).await;
                        }
                    }
                    let mut failed = false;
                    for payload in &frames {
                        if let Err(e) = session.rx_char.write_ext(payload, &write_req).await {
                            tracing::debug!(
                                "BLE {}: write to {} failed: {e}",
                                self.iface,
                                self.addr
                            );
                            failed = true;
                            break;
                        }
                    }
                    if !keepalive {
                        pacer.packet_done(
                            tokio::time::Instant::now()
                                .duration_since(start)
                                .as_millis() as u64,
                        );
                    }
                    if failed {
                        break;
                    }
                }
            }
        }
    }
}

struct CentralSession {
    notify_stream: std::pin::Pin<Box<dyn futures::Stream<Item = Vec<u8>> + Send>>,
    frames_rx: mpsc::Receiver<Vec<Vec<u8>>>,
    rx_char: bluer::gatt::remote::Characteristic,
}

fn missing_gatt() -> bluer::Error {
    bluer::Error {
        kind: bluer::ErrorKind::ServicesUnresolved,
        message: "Columba GATT layout not found on peer".to_string(),
    }
}
