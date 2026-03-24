use btleplug::api::{Central, Manager as _, Peripheral as _, ScanFilter, WriteType};
use btleplug::platform::{Adapter, Manager, Peripheral};
use serde::Serialize;
use std::sync::Arc;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::error::MeshGuardError;

/// Meshtastic BLE service UUID.
const MESHTASTIC_SERVICE: &str = "6ba1b218-15a8-461f-9fa8-5dcae273eafd";

/// toRadio — phone writes to device.
const TO_RADIO: &str = "f75c76d2-129e-4dad-a1dd-7866124401e7";

/// fromRadio — phone reads from device.
const FROM_RADIO: &str = "2c55e69e-4993-11ed-b878-0242ac120002";

/// Info about a discovered BLE device.
#[derive(Debug, Clone, Serialize)]
pub struct ScannedDevice {
    pub name: String,
    pub address: String,
    pub rssi: Option<i16>,
    pub is_meshtastic: bool,
}

/// Manages the BLE connection to the LOCAL Meshtastic device.
pub struct BleManager {
    adapter: Adapter,
    connected_device: Arc<Mutex<Option<Peripheral>>>,
}

impl BleManager {
    pub async fn new() -> Result<Self, MeshGuardError> {
        let manager = Manager::new()
            .await
            .map_err(|e| MeshGuardError::Ble(e.to_string()))?;

        let adapters = manager
            .adapters()
            .await
            .map_err(|e| MeshGuardError::Ble(e.to_string()))?;

        let adapter = adapters
            .into_iter()
            .next()
            .ok_or_else(|| MeshGuardError::Ble("no Bluetooth adapter found".into()))?;

        Ok(Self {
            adapter,
            connected_device: Arc::new(Mutex::new(None)),
        })
    }

    /// Scan for nearby Meshtastic BLE devices.
    /// Returns all discovered devices, with `is_meshtastic` flagged for those
    /// advertising the Meshtastic service UUID or matching known device names.
    pub async fn scan(&self, duration_secs: u64) -> Result<Vec<ScannedDevice>, MeshGuardError> {
        self.adapter
            .start_scan(ScanFilter::default())
            .await
            .map_err(|e| MeshGuardError::Ble(e.to_string()))?;

        tokio::time::sleep(std::time::Duration::from_secs(duration_secs)).await;

        self.adapter
            .stop_scan()
            .await
            .map_err(|e| MeshGuardError::Ble(e.to_string()))?;

        let peripherals = self
            .adapter
            .peripherals()
            .await
            .map_err(|e| MeshGuardError::Ble(e.to_string()))?;

        let service_uuid = Uuid::parse_str(MESHTASTIC_SERVICE).unwrap();
        let mut devices = Vec::new();

        for p in peripherals {
            if let Ok(Some(props)) = p.properties().await {
                let name = props
                    .local_name
                    .clone()
                    .unwrap_or_default();

                // Skip devices with no name and no services
                if name.is_empty() && props.services.is_empty() {
                    continue;
                }

                let is_meshtastic = props.services.contains(&service_uuid)
                    || name.to_lowercase().contains("meshtastic")
                    || name.to_lowercase().contains("p1000")
                    || name.to_lowercase().contains("t-beam")
                    || name.to_lowercase().contains("heltec")
                    || name.to_lowercase().contains("rak")
                    || name.to_lowercase().contains("sensecap");

                devices.push(ScannedDevice {
                    name: if name.is_empty() {
                        "Unknown Device".to_string()
                    } else {
                        name
                    },
                    address: props.address.to_string(),
                    rssi: props.rssi,
                    is_meshtastic,
                });
            }
        }

        // Sort: Meshtastic devices first, then by signal strength
        devices.sort_by(|a, b| {
            b.is_meshtastic
                .cmp(&a.is_meshtastic)
                .then_with(|| b.rssi.unwrap_or(-100).cmp(&a.rssi.unwrap_or(-100)))
        });

        Ok(devices)
    }

    /// Connect directly to a Meshtastic device by its known BLE address.
    pub async fn connect_to_address(&self, address: &str) -> Result<(), MeshGuardError> {
        // Brief scan to populate the adapter's peripheral list
        self.adapter
            .start_scan(ScanFilter::default())
            .await
            .map_err(|e| MeshGuardError::Ble(e.to_string()))?;

        tokio::time::sleep(std::time::Duration::from_secs(3)).await;

        self.adapter
            .stop_scan()
            .await
            .map_err(|e| MeshGuardError::Ble(e.to_string()))?;

        let peripherals = self
            .adapter
            .peripherals()
            .await
            .map_err(|e| MeshGuardError::Ble(e.to_string()))?;

        let device = peripherals
            .into_iter()
            .find(|p| {
                futures::executor::block_on(async {
                    p.properties()
                        .await
                        .ok()
                        .flatten()
                        .map(|props| props.address.to_string() == address)
                        .unwrap_or(false)
                })
            })
            .ok_or_else(|| MeshGuardError::DeviceNotFound(address.into()))?;

        device
            .connect()
            .await
            .map_err(|e| MeshGuardError::Ble(e.to_string()))?;

        device
            .discover_services()
            .await
            .map_err(|e| MeshGuardError::Ble(e.to_string()))?;

        *self.connected_device.lock().await = Some(device);
        tracing::info!("Connected to local Meshtastic device at {}", address);
        Ok(())
    }

    /// Write a Meshtastic protobuf to the toRadio characteristic.
    pub async fn write_to_radio(&self, data: &[u8]) -> Result<(), MeshGuardError> {
        let guard = self.connected_device.lock().await;
        let device = guard.as_ref().ok_or(MeshGuardError::NotConnected)?;

        let to_radio_uuid = Uuid::parse_str(TO_RADIO).unwrap();
        let chars = device.characteristics();
        let characteristic = chars
            .iter()
            .find(|c| c.uuid == to_radio_uuid)
            .ok_or_else(|| MeshGuardError::Ble("toRadio characteristic not found".into()))?;

        for chunk in data.chunks(512) {
            device
                .write(characteristic, chunk, WriteType::WithResponse)
                .await
                .map_err(|e| MeshGuardError::Ble(e.to_string()))?;
        }

        Ok(())
    }

    /// Read from the fromRadio characteristic.
    pub async fn read_from_radio(&self) -> Result<Vec<u8>, MeshGuardError> {
        let guard = self.connected_device.lock().await;
        let device = guard.as_ref().ok_or(MeshGuardError::NotConnected)?;

        let from_radio_uuid = Uuid::parse_str(FROM_RADIO).unwrap();
        let chars = device.characteristics();
        let characteristic = chars
            .iter()
            .find(|c| c.uuid == from_radio_uuid)
            .ok_or_else(|| MeshGuardError::Ble("fromRadio characteristic not found".into()))?;

        device
            .read(characteristic)
            .await
            .map_err(|e| MeshGuardError::Ble(e.to_string()))
    }

    /// Write a Meshtastic admin message to configure the device.
    pub async fn write_config(&self, config_data: &[u8]) -> Result<(), MeshGuardError> {
        self.write_to_radio(config_data).await
    }

    /// Disconnect from the local device.
    pub async fn disconnect(&self) -> Result<(), MeshGuardError> {
        let mut guard = self.connected_device.lock().await;
        if let Some(device) = guard.take() {
            device
                .disconnect()
                .await
                .map_err(|e| MeshGuardError::Ble(e.to_string()))?;
        }
        Ok(())
    }

    pub async fn is_connected(&self) -> bool {
        let guard = self.connected_device.lock().await;
        if let Some(device) = guard.as_ref() {
            device.is_connected().await.unwrap_or(false)
        } else {
            false
        }
    }
}
