use btleplug::api::{
    Central, CentralEvent, Manager as _, Peripheral as _, ScanFilter, WriteType,
};
use btleplug::platform::{Adapter, Manager, Peripheral};
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};
use uuid::Uuid;

use crate::error::MeshGuardError;

/// Meshtastic BLE service UUID.
const MESHTASTIC_SERVICE_UUID: &str = "6ba1b218-15a8-461f-9fa8-5dcae273eafd";

/// Meshtastic "toRadio" characteristic — phone writes to device.
const TO_RADIO_UUID: &str = "f75c76d2-129e-4dad-a1dd-7866124401e7";

/// Meshtastic "fromRadio" characteristic — phone reads from device.
const FROM_RADIO_UUID: &str = "2c55e69e-4993-11ed-b878-0242ac120002";

/// Meshtastic "fromNum" notify characteristic — signals new data available.
const FROM_NUM_UUID: &str = "ed9da18c-a800-4f66-a670-aa7547e34453";

/// Manages BLE connections to Sensecap P1000 Meshtastic devices.
pub struct BleManager {
    adapter: Adapter,
    connected_device: Arc<Mutex<Option<Peripheral>>>,
    incoming_tx: mpsc::Sender<Vec<u8>>,
    incoming_rx: Arc<Mutex<mpsc::Receiver<Vec<u8>>>>,
}

/// Summary info about a discovered Meshtastic device.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DeviceInfo {
    pub name: String,
    pub address: String,
    pub rssi: Option<i16>,
}

impl BleManager {
    /// Initialize the BLE manager with the system's first Bluetooth adapter.
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

        let (tx, rx) = mpsc::channel(256);

        Ok(Self {
            adapter,
            connected_device: Arc::new(Mutex::new(None)),
            incoming_tx: tx,
            incoming_rx: Arc::new(Mutex::new(rx)),
        })
    }

    /// Scan for nearby Meshtastic devices (Sensecap P1000).
    pub async fn scan(&self, duration_secs: u64) -> Result<Vec<DeviceInfo>, MeshGuardError> {
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

        let service_uuid = Uuid::parse_str(MESHTASTIC_SERVICE_UUID).unwrap();
        let mut devices = Vec::new();

        for p in peripherals {
            if let Ok(Some(props)) = p.properties().await {
                // Filter to only Meshtastic devices
                let is_meshtastic = props.services.contains(&service_uuid)
                    || props
                        .local_name
                        .as_ref()
                        .map(|n| n.contains("Meshtastic") || n.contains("P1000"))
                        .unwrap_or(false);

                if is_meshtastic {
                    devices.push(DeviceInfo {
                        name: props
                            .local_name
                            .unwrap_or_else(|| "Unknown Meshtastic".into()),
                        address: props.address.to_string(),
                        rssi: props.rssi,
                    });
                }
            }
        }

        Ok(devices)
    }

    /// Connect to a Meshtastic device by address.
    pub async fn connect(&self, address: &str) -> Result<(), MeshGuardError> {
        let peripherals = self
            .adapter
            .peripherals()
            .await
            .map_err(|e| MeshGuardError::Ble(e.to_string()))?;

        let device = peripherals
            .into_iter()
            .filter(|p| {
                futures::executor::block_on(async {
                    p.properties()
                        .await
                        .ok()
                        .flatten()
                        .map(|props| props.address.to_string() == address)
                        .unwrap_or(false)
                })
            })
            .next()
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
        Ok(())
    }

    /// Send raw bytes to the Meshtastic device via the toRadio characteristic.
    pub async fn send_to_radio(&self, data: &[u8]) -> Result<(), MeshGuardError> {
        let guard = self.connected_device.lock().await;
        let device = guard.as_ref().ok_or(MeshGuardError::NotConnected)?;

        let to_radio_uuid = Uuid::parse_str(TO_RADIO_UUID).unwrap();

        let chars = device.characteristics();
        let to_radio = chars
            .iter()
            .find(|c| c.uuid == to_radio_uuid)
            .ok_or_else(|| MeshGuardError::Ble("toRadio characteristic not found".into()))?;

        // Meshtastic BLE protocol: write in chunks of up to 512 bytes
        for chunk in data.chunks(512) {
            device
                .write(to_radio, chunk, WriteType::WithResponse)
                .await
                .map_err(|e| MeshGuardError::Ble(e.to_string()))?;
        }

        Ok(())
    }

    /// Read raw bytes from the Meshtastic device via the fromRadio characteristic.
    pub async fn read_from_radio(&self) -> Result<Vec<u8>, MeshGuardError> {
        let guard = self.connected_device.lock().await;
        let device = guard.as_ref().ok_or(MeshGuardError::NotConnected)?;

        let from_radio_uuid = Uuid::parse_str(FROM_RADIO_UUID).unwrap();

        let chars = device.characteristics();
        let from_radio = chars
            .iter()
            .find(|c| c.uuid == from_radio_uuid)
            .ok_or_else(|| MeshGuardError::Ble("fromRadio characteristic not found".into()))?;

        device
            .read(from_radio)
            .await
            .map_err(|e| MeshGuardError::Ble(e.to_string()))
    }

    /// Disconnect from the currently connected device.
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

    /// Check if a device is currently connected.
    pub async fn is_connected(&self) -> bool {
        let guard = self.connected_device.lock().await;
        if let Some(device) = guard.as_ref() {
            device.is_connected().await.unwrap_or(false)
        } else {
            false
        }
    }
}
