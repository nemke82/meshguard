use btleplug::api::{Central, Manager as _, Peripheral as _, WriteType};
use btleplug::platform::{Adapter, Manager, Peripheral};
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

/// fromNum — notify characteristic, signals new data available.
const FROM_NUM: &str = "ed9da18c-a800-4f66-a670-aa7547e34453";

/// Manages the BLE connection to the LOCAL Meshtastic device.
/// No scanning — connects directly to a known BLE address.
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

    /// Connect directly to a Meshtastic device by its known BLE address.
    /// The address is entered once by the user and saved in config.
    pub async fn connect_to_address(&self, address: &str) -> Result<(), MeshGuardError> {
        // Brief scan to populate the adapter's peripheral list
        use btleplug::api::ScanFilter;
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
    /// This sends a ToRadio { packet: MeshPacket { admin_message } }.
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
