//! Where a radio can be reached (saved with the settings).

use serde::{Deserialize, Serialize};

/// Where to find the radio again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum LinkTarget {
    Ble {
        name: String,
        /// Required on Android; desktop scans by name.
        #[serde(default)]
        address: Option<String>,
    },
    Tcp {
        address: String,
    },
    Serial {
        port: String,
    },
}

impl LinkTarget {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Ble { .. } => "ble",
            Self::Tcp { .. } => "tcp",
            Self::Serial { .. } => "serial",
        }
    }

    pub fn label(&self) -> String {
        match self {
            Self::Ble { name, .. } => name.clone(),
            Self::Tcp { address } => address.clone(),
            Self::Serial { port } => port.clone(),
        }
    }
}
