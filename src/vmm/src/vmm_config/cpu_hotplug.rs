// Copyright 2025 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use serde::{Deserialize, Serialize};

/// Configuration for CPU hotplug. Set before boot.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CpuHotplugConfig {
    /// Maximum number of vCPUs that can be hotplugged (must be > boot vCPUs and <= 32)
    pub max_vcpus: u8,
}

/// Request to update the number of active vCPUs (post-boot).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CpuHotplugUpdate {
    /// Desired number of active vCPUs
    pub desired_vcpus: u8,
}

/// Status of the CPU hotplug device.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct CpuHotplugStatus {
    /// Number of boot vCPUs
    pub boot_vcpus: u8,
    /// Maximum number of vCPUs
    pub max_vcpus: u8,
    /// Current number of active vCPUs
    pub active_vcpus: u8,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cpu_hotplug_config_serde() {
        let config = CpuHotplugConfig { max_vcpus: 8 };
        let json = serde_json::to_string(&config).unwrap();
        let deserialized: CpuHotplugConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config, deserialized);
    }

    #[test]
    fn test_cpu_hotplug_update_serde() {
        let update = CpuHotplugUpdate { desired_vcpus: 4 };
        let json = serde_json::to_string(&update).unwrap();
        let deserialized: CpuHotplugUpdate = serde_json::from_str(&json).unwrap();
        assert_eq!(update, deserialized);
    }

    #[test]
    fn test_cpu_hotplug_status_serde() {
        let status = CpuHotplugStatus {
            boot_vcpus: 2,
            max_vcpus: 8,
            active_vcpus: 4,
        };
        let json = serde_json::to_string(&status).unwrap();
        let deserialized: CpuHotplugStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(status, deserialized);
    }

    #[test]
    fn test_cpu_hotplug_config_denies_unknown_fields() {
        let json = r#"{"max_vcpus": 8, "unknown": true}"#;
        serde_json::from_str::<CpuHotplugConfig>(json).unwrap_err();
    }

    #[test]
    fn test_cpu_hotplug_update_denies_unknown_fields() {
        let json = r#"{"desired_vcpus": 2, "unknown": true}"#;
        serde_json::from_str::<CpuHotplugUpdate>(json).unwrap_err();
    }
}
