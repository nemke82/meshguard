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

/// Bluetooth adapter status.
#[derive(Debug, Clone, Serialize)]
pub struct BluetoothStatus {
    /// Whether a Bluetooth adapter was found on this system.
    pub adapter_found: bool,
    /// Whether the adapter appears to be powered on / functional.
    pub powered_on: bool,
    /// Human-readable status message.
    pub message: String,
}

/// Check Bluetooth status without creating a full BleManager.
pub async fn check_bluetooth() -> BluetoothStatus {
    let manager = match Manager::new().await {
        Ok(m) => m,
        Err(e) => {
            let msg = e.to_string().to_lowercase();
            if msg.contains("permission") || msg.contains("denied") || msg.contains("not authorized") {
                return BluetoothStatus {
                    adapter_found: false,
                    powered_on: false,
                    message: "Bluetooth permissions not granted. Please allow Bluetooth access in your device settings.".into(),
                };
            }
            return BluetoothStatus {
                adapter_found: false,
                powered_on: false,
                message: format!("Cannot access Bluetooth: {}", e),
            };
        }
    };

    let adapters = match manager.adapters().await {
        Ok(a) => a,
        Err(e) => {
            let msg = e.to_string().to_lowercase();
            if msg.contains("turned off") || msg.contains("disabled") || msg.contains("not powered") {
                return BluetoothStatus {
                    adapter_found: true,
                    powered_on: false,
                    message: "Bluetooth is turned off. Please enable Bluetooth in your device settings.".into(),
                };
            }
            return BluetoothStatus {
                adapter_found: false,
                powered_on: false,
                message: format!("Cannot access Bluetooth adapter: {}", e),
            };
        }
    };

    if adapters.is_empty() {
        return BluetoothStatus {
            adapter_found: false,
            powered_on: false,
            message: "No Bluetooth adapter found on this device.".into(),
        };
    }

    // Try a quick scan to verify the adapter is actually working
    let adapter = &adapters[0];
    match adapter.start_scan(ScanFilter::default()).await {
        Ok(_) => {
            let _ = adapter.stop_scan().await;
            BluetoothStatus {
                adapter_found: true,
                powered_on: true,
                message: "Bluetooth is ready.".into(),
            }
        }
        Err(e) => {
            let msg = e.to_string().to_lowercase();
            if msg.contains("turned off") || msg.contains("disabled") || msg.contains("powered off")
                || msg.contains("not powered") || msg.contains("no powered")
            {
                BluetoothStatus {
                    adapter_found: true,
                    powered_on: false,
                    message: "Bluetooth is turned off. Please enable Bluetooth in your device settings.".into(),
                }
            } else if msg.contains("permission") || msg.contains("denied") {
                BluetoothStatus {
                    adapter_found: true,
                    powered_on: false,
                    message: "Bluetooth permissions not granted. Please allow Bluetooth access in your device settings.".into(),
                }
            } else {
                BluetoothStatus {
                    adapter_found: true,
                    powered_on: false,
                    message: format!("Bluetooth adapter error: {}", e),
                }
            }
        }
    }
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
            .map_err(|e| {
                let msg = e.to_string().to_lowercase();
                if msg.contains("permission") || msg.contains("denied") {
                    MeshGuardError::BluetoothPermission
                } else {
                    MeshGuardError::Ble(e.to_string())
                }
            })?;

        let adapters = manager
            .adapters()
            .await
            .map_err(|e| {
                let msg = e.to_string().to_lowercase();
                if msg.contains("turned off") || msg.contains("disabled") || msg.contains("not powered") {
                    MeshGuardError::BluetoothDisabled
                } else {
                    MeshGuardError::Ble(e.to_string())
                }
            })?;

        let adapter = adapters
            .into_iter()
            .next()
            .ok_or(MeshGuardError::BluetoothDisabled)?;

        Ok(Self {
            adapter,
            connected_device: Arc::new(Mutex::new(None)),
        })
    }

    /// Scan for nearby Meshtastic BLE devices.
    /// Only returns devices that match Meshtastic identifiers.
    pub async fn scan(&self, duration_secs: u64) -> Result<Vec<ScannedDevice>, MeshGuardError> {
        self.adapter
            .start_scan(ScanFilter::default())
            .await
            .map_err(|e| {
                let msg = e.to_string().to_lowercase();
                if msg.contains("turned off") || msg.contains("disabled") || msg.contains("not powered") {
                    MeshGuardError::BluetoothDisabled
                } else if msg.contains("permission") || msg.contains("denied") {
                    MeshGuardError::BluetoothPermission
                } else {
                    MeshGuardError::Ble(format!("Failed to start scan: {}", e))
                }
            })?;

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

                let name_lower = name.to_lowercase();

                let is_meshtastic = props.services.contains(&service_uuid)
                    || name_lower.contains("meshtastic")
                    || name_lower.contains("p1000")
                    || name_lower.contains("t-beam")
                    || name_lower.contains("heltec")
                    || name_lower.contains("rak")
                    || name_lower.contains("sensecap")
                    || name_lower.contains("t-echo")
                    || name_lower.contains("lora")
                    || name_lower.contains("mesh");

                // Only include Meshtastic devices — filter out unrelated BLE noise
                if is_meshtastic {
                    devices.push(ScannedDevice {
                        name: if name.is_empty() {
                            "Meshtastic Device".to_string()
                        } else {
                            name
                        },
                        address: props.address.to_string(),
                        rssi: props.rssi,
                        is_meshtastic: true,
                    });
                }
            }
        }

        // Sort by signal strength (strongest first)
        devices.sort_by(|a, b| {
            b.rssi.unwrap_or(-100).cmp(&a.rssi.unwrap_or(-100))
        });

        Ok(devices)
    }

    /// Connect directly to a Meshtastic device by its known BLE address.
    pub async fn connect_to_address(&self, address: &str) -> Result<(), MeshGuardError> {
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
