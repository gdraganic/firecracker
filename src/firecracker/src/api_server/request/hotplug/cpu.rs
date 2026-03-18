// Copyright 2025 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use micro_http::Body;
use vmm::logger::{IncMetric, METRICS};
use vmm::rpc_interface::VmmAction;
use vmm::vmm_config::cpu_hotplug::{CpuHotplugConfig, CpuHotplugUpdate};

use crate::api_server::parsed_request::{ParsedRequest, RequestError};

pub(crate) fn parse_put_cpu_hotplug(body: &Body) -> Result<ParsedRequest, RequestError> {
    METRICS.put_api_requests.hotplug_cpu_count.inc();
    let config = serde_json::from_slice::<CpuHotplugConfig>(body.raw()).inspect_err(|_| {
        METRICS.put_api_requests.hotplug_cpu_fails.inc();
    })?;
    Ok(ParsedRequest::new_sync(VmmAction::SetCpuHotplugConfig(config)))
}

pub(crate) fn parse_patch_cpu_hotplug(body: &Body) -> Result<ParsedRequest, RequestError> {
    METRICS.patch_api_requests.hotplug_cpu_count.inc();
    let config = serde_json::from_slice::<CpuHotplugUpdate>(body.raw()).inspect_err(|_| {
        METRICS.patch_api_requests.hotplug_cpu_fails.inc();
    })?;
    Ok(ParsedRequest::new_sync(VmmAction::HotplugVcpu(config)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api_server::parsed_request::tests::vmm_action_from_request;

    #[test]
    fn test_parse_patch_cpu_hotplug_request() {
        parse_patch_cpu_hotplug(&Body::new("invalid_payload")).unwrap_err();

        let body = r#"{
            "desired_vcpus": "bar"
        }"#;
        parse_patch_cpu_hotplug(&Body::new(body)).unwrap_err();

        let body = r#"{
            "desired_vcpus": 4
        }"#;
        let expected_config = CpuHotplugUpdate { desired_vcpus: 4 };
        assert_eq!(
            vmm_action_from_request(parse_patch_cpu_hotplug(&Body::new(body)).unwrap()),
            VmmAction::HotplugVcpu(expected_config)
        );
    }
}
