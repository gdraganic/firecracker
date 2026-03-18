// Copyright 2025 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! ACPI-based CPU hotplug controller device.
//!
//! This device exposes a 12-byte MMIO region that the guest ACPI AML methods
//! use to query and acknowledge CPU hotplug events. The design follows
//! Cloud Hypervisor's CPU hotplug controller pattern.

use std::sync::{Arc, Barrier, Mutex};

use acpi_tables::Aml;
use acpi_tables::aml;
use log::warn;
use serde::{Deserialize, Serialize};
use vmm_sys_util::eventfd::EventFd;

use crate::vmm_config::machine_config::MAX_SUPPORTED_VCPUS;
use crate::vstate::bus::BusDevice;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CpuHotplugControllerState {
    pub boot_vcpus: u8,
    pub max_vcpus: u8,
    pub mmio_addr: u64,
    pub gsi: u32,
    pub cpu_enabled: Vec<bool>,
}

/// Size of the MMIO region for the CPU hotplug controller
pub const CPU_HOTPLUG_MMIO_SIZE: u64 = 12;

// MMIO register offsets
const CPU_SELECTION_OFFSET: u64 = 0; // 4 bytes: CPU selector (DWord)
const CPU_STATUS_OFFSET: u64 = 4; // 4 bytes: status bits (byte-accessible)

// Status bit positions within the status byte at offset 4
const CPU_ENABLE_FLAG: usize = 0; // Bit 0: CPU is enabled/present
const CPU_INSERTING_FLAG: usize = 1; // Bit 1: CPU was just inserted (hot-add)
const CPU_REMOVING_FLAG: usize = 2; // Bit 2: CPU is being removed
const CPU_EJECT_FLAG: usize = 3; // Bit 3: CPU eject trigger

#[derive(Debug, Default, Clone)]
struct CpuState {
    enabled: bool,
    inserting: bool,
    removing: bool,
}

#[derive(Debug)]
pub struct CpuHotplugController {
    /// MMIO base address (set during device attachment)
    mmio_addr: u64,
    /// Number of vCPUs booted with
    boot_vcpus: u8,
    /// Maximum vCPUs supported
    max_vcpus: u8,
    /// Currently selected CPU for MMIO queries
    selected_cpu: u32,
    /// Per-CPU states
    cpu_states: Vec<CpuState>,
    /// GSI number for the GED interrupt
    gsi: u32,
    /// EventFd to trigger GED interrupt to guest
    interrupt_evt: EventFd,
}

pub type SharedCpuHotplugController = Arc<Mutex<CpuHotplugController>>;

impl CpuHotplugController {
    pub fn new(boot_vcpus: u8, max_vcpus: u8, mmio_addr: u64, gsi: u32, interrupt_evt: EventFd) -> Self {
        let max_vcpus = max_vcpus.min(MAX_SUPPORTED_VCPUS);
        let boot_vcpus = boot_vcpus.min(max_vcpus);

        let mut cpu_states = vec![CpuState::default(); max_vcpus as usize];
        // Mark boot CPUs as enabled
        for state in cpu_states.iter_mut().take(boot_vcpus as usize) {
            state.enabled = true;
        }

        Self {
            mmio_addr,
            boot_vcpus,
            max_vcpus,
            selected_cpu: 0,
            cpu_states,
            gsi,
            interrupt_evt,
        }
    }

    pub fn gsi(&self) -> u32 {
        self.gsi
    }

    pub fn interrupt_evt(&self) -> &EventFd {
        &self.interrupt_evt
    }

    pub fn mmio_addr(&self) -> u64 {
        self.mmio_addr
    }

    pub fn boot_vcpus(&self) -> u8 {
        self.boot_vcpus
    }

    pub fn max_vcpus(&self) -> u8 {
        self.max_vcpus
    }

    /// Returns the number of currently active (enabled) vCPUs
    pub fn active_vcpus(&self) -> u8 {
        self.cpu_states.iter().filter(|s| s.enabled).count() as u8
    }

    /// Mark CPUs as newly inserted (called by VMM during hotplug)
    pub fn hotplug_vcpus(&mut self, from: u8, to: u8) {
        for cpu_id in from..to {
            if let Some(state) = self.cpu_states.get_mut(cpu_id as usize) {
                state.enabled = true;
                state.inserting = true;
                state.removing = false;
            }
        }
    }

    /// Mark CPUs for removal (called by VMM during unplug)
    /// Only non-boot CPUs can be removed. Marks from `to` down to `desired`.
    pub fn unplug_vcpus(&mut self, desired: u8) {
        let current = self.active_vcpus();
        for i in (desired..current).rev() {
            if i < self.boot_vcpus {
                continue;
            }
            if let Some(state) = self.cpu_states.get_mut(i as usize)
                && state.enabled
            {
                state.removing = true;
            }
        }
    }

    /// Returns list of CPU indices that have completed ejection (CEJ0 written)
    pub fn ejected_cpus(&self) -> Vec<u8> {
        self.cpu_states
            .iter()
            .enumerate()
            .filter(|(_, s)| !s.enabled && !s.inserting && !s.removing)
            .map(|(i, _)| i as u8)
            .filter(|&i| i >= self.boot_vcpus)
            .collect()
    }

    pub fn save(&self) -> CpuHotplugControllerState {
        CpuHotplugControllerState {
            boot_vcpus: self.boot_vcpus,
            max_vcpus: self.max_vcpus,
            mmio_addr: self.mmio_addr,
            gsi: self.gsi,
            cpu_enabled: self.cpu_states.iter().map(|s| s.enabled).collect(),
        }
    }

    pub fn restore(state: &CpuHotplugControllerState) -> Result<Self, std::io::Error> {
        let interrupt_evt = EventFd::new(libc::EFD_NONBLOCK)?;
        let cpu_states = state
            .cpu_enabled
            .iter()
            .map(|&enabled| CpuState {
                enabled,
                inserting: false,
                removing: false,
            })
            .collect();
        Ok(Self {
            mmio_addr: state.mmio_addr,
            boot_vcpus: state.boot_vcpus,
            max_vcpus: state.max_vcpus,
            selected_cpu: 0,
            cpu_states,
            gsi: state.gsi,
            interrupt_evt,
        })
    }

    pub fn notify_guest(&self) {
        if let Err(e) = self.interrupt_evt.write(1) {
            warn!("Failed to trigger CPU hotplug interrupt: {}", e);
        }
    }

    pub fn mmio_read(&self, offset: u64, data: &mut [u8]) {
        data.fill(0);

        match offset {
            CPU_SELECTION_OFFSET => {
                if data.len() >= core::mem::size_of::<u32>() {
                    data[..core::mem::size_of::<u32>()]
                        .copy_from_slice(&self.selected_cpu.to_le_bytes());
                }
            }
            CPU_STATUS_OFFSET => {
                if let Some(status) = data.first_mut() {
                    if let Some(state) = self.cpu_states.get(self.selected_cpu as usize) {
                        if state.enabled {
                            *status |= 1 << CPU_ENABLE_FLAG;
                        }
                        if state.inserting {
                            *status |= 1 << CPU_INSERTING_FLAG;
                        }
                        if state.removing {
                            *status |= 1 << CPU_REMOVING_FLAG;
                        }
                    }
                }
            }
            _ => {
                warn!("Unexpected CPU hotplug controller read at offset {offset:#x}");
            }
        }
    }

    pub fn mmio_write(&mut self, offset: u64, data: &[u8]) {
        match offset {
            CPU_SELECTION_OFFSET => {
                if data.len() >= core::mem::size_of::<u32>() {
                    let mut raw = [0u8; 4];
                    raw.copy_from_slice(&data[..core::mem::size_of::<u32>()]);
                    self.selected_cpu = u32::from_le_bytes(raw);
                }
            }
            CPU_STATUS_OFFSET => {
                if let Some(status) = data.first() {
                    if let Some(state) = self.cpu_states.get_mut(self.selected_cpu as usize) {
                        // Guest writes back 1 to CINS to acknowledge insertion.
                        if (*status & (1 << CPU_INSERTING_FLAG) != 0) && state.inserting {
                            state.inserting = false;
                        }
                        if (*status & (1 << CPU_REMOVING_FLAG) != 0) && state.removing {
                            state.removing = false;
                        }
                        if *status & (1 << CPU_EJECT_FLAG) != 0 {
                            state.enabled = false;
                        }
                    }
                }
            }
            _ => {
                warn!("Unexpected CPU hotplug controller write at offset {offset:#x}");
            }
        }
    }
}

impl BusDevice for CpuHotplugController {
    fn read(&mut self, _base: u64, offset: u64, data: &mut [u8]) {
        self.mmio_read(offset, data);
    }

    fn write(&mut self, _base: u64, offset: u64, data: &[u8]) -> Option<Arc<Barrier>> {
        self.mmio_write(offset, data);
        None
    }
}

#[derive(Debug)]
struct CpuNotify {
    cpu_id: u8,
}

impl Aml for CpuNotify {
    fn append_aml_bytes(&self, v: &mut Vec<u8>) -> Result<(), aml::AmlError> {
        let cpu_path = aml::Path::new(&format!("\\_SB_.CPUS.C{:03X}", self.cpu_id))?;

        aml::If::new(
            &aml::Equal::new(&aml::Arg(0), &self.cpu_id),
            vec![&aml::Notify::new(&cpu_path, &aml::Arg(1))],
        )
        .append_aml_bytes(v)
    }
}

#[derive(Debug)]
struct CstaMethod;

impl Aml for CstaMethod {
    fn append_aml_bytes(&self, v: &mut Vec<u8>) -> Result<(), aml::AmlError> {
        let arg0 = aml::Arg(0);
        let local0 = aml::Local(0);
        let acquire = aml::Acquire::new(aml::Path::new("\\_SB_.PRES.CPLK")?, 0xffff);
        let csel_path = aml::Path::new("\\_SB_.PRES.CSEL")?;
        let store_selected = aml::Store::new(&csel_path, &arg0);
        let store_zero = aml::Store::new(&local0, &aml::ZERO);
        let cpen_path = aml::Path::new("\\_SB_.PRES.CPEN")?;
        let cpen_is_one = aml::Equal::new(&cpen_path, &aml::ONE);
        let store_enabled = aml::Store::new(&local0, &0x0fu8);
        let if_enabled = aml::If::new(&cpen_is_one, vec![&store_enabled]);
        let release = aml::Release::new(aml::Path::new("\\_SB_.PRES.CPLK")?);
        let ret = aml::Return::new(&local0);

        aml::Method::new(
            "CSTA".try_into()?,
            1,
            true,
            vec![
                &acquire,
                &store_selected,
                &store_zero,
                &if_enabled,
                &release,
                &ret,
            ],
        )
        .append_aml_bytes(v)
    }
}

#[derive(Debug)]
struct CtfyMethod {
    max_vcpus: u8,
}

impl Aml for CtfyMethod {
    fn append_aml_bytes(&self, v: &mut Vec<u8>) -> Result<(), aml::AmlError> {
        let cpu_notifies: Vec<CpuNotify> = (0..self.max_vcpus)
            .map(|cpu_id| CpuNotify { cpu_id })
            .collect();
        let mut cpu_notify_refs: Vec<&dyn Aml> = Vec::with_capacity(cpu_notifies.len());
        for cpu_notify in &cpu_notifies {
            cpu_notify_refs.push(cpu_notify);
        }

        aml::Method::new("CTFY".try_into()?, 2, true, cpu_notify_refs).append_aml_bytes(v)
    }
}

#[derive(Debug)]
struct CscnMethod {
    max_vcpus: u8,
}

impl Aml for CscnMethod {
    fn append_aml_bytes(&self, v: &mut Vec<u8>) -> Result<(), aml::AmlError> {
        let local0 = aml::Local(0);
        let acquire = aml::Acquire::new(aml::Path::new("\\_SB_.PRES.CPLK")?, 0xffff);
        let init_counter = aml::Store::new(&local0, &aml::ZERO);

        let csel_path = aml::Path::new("\\_SB_.PRES.CSEL")?;
        let select_cpu = aml::Store::new(&csel_path, &local0);
        let cins_path = aml::Path::new("\\_SB_.PRES.CINS")?;
        let cins_is_set = aml::Equal::new(&cins_path, &aml::ONE);
        let notify_insert = aml::MethodCall::new("CTFY".try_into()?, vec![&local0, &aml::ONE]);
        let cins_path2 = aml::Path::new("\\_SB_.PRES.CINS")?;
        let clear_cins = aml::Store::new(&cins_path2, &aml::ONE);
        let if_insert = aml::If::new(&cins_is_set, vec![&notify_insert, &clear_cins]);
        let crmv_path = aml::Path::new("\\_SB_.PRES.CRMV")?;
        let crmv_is_set = aml::Equal::new(&crmv_path, &aml::ONE);
        let three = 3u8;
        let notify_remove = aml::MethodCall::new("CTFY".try_into()?, vec![&local0, &three]);
        let crmv_path2 = aml::Path::new("\\_SB_.PRES.CRMV")?;
        let clear_crmv = aml::Store::new(&crmv_path2, &aml::ONE);
        let if_remove = aml::If::new(&crmv_is_set, vec![&notify_remove, &clear_crmv]);
        let increment = aml::Add::new(&local0, &local0, &aml::ONE);

        let predicate = aml::LessThan::new(&local0, &self.max_vcpus);
        let scan_loop =
            aml::While::new(&predicate, vec![&select_cpu, &if_insert, &if_remove, &increment]);
        let release = aml::Release::new(aml::Path::new("\\_SB_.PRES.CPLK")?);

        aml::Method::new(
            "CSCN".try_into()?,
            0,
            true,
            vec![&acquire, &init_counter, &scan_loop, &release],
        )
        .append_aml_bytes(v)
    }
}

#[derive(Debug)]
struct CpuAml {
    cpu_id: u8,
}

impl Aml for CpuAml {
    fn append_aml_bytes(&self, v: &mut Vec<u8>) -> Result<(), aml::AmlError> {
        let csta_call = aml::MethodCall::new("CSTA".try_into()?, vec![&self.cpu_id]);
        let ej0_csel_path = aml::Path::new("\\_SB_.PRES.CSEL")?;
        let ej0_select = aml::Store::new(&ej0_csel_path, &self.cpu_id);
        let ej0_cej0_path = aml::Path::new("\\_SB_.PRES.CEJ0")?;
        let ej0_trigger = aml::Store::new(&ej0_cej0_path, &aml::ONE);
        let mat_data = vec![0, 8, self.cpu_id, self.cpu_id, 1, 0, 0, 0];

        aml::Device::new(
            format!("C{:03X}", self.cpu_id).as_str().try_into()?,
            vec![
                &aml::Name::new("_HID".try_into()?, &"ACPI0007")?,
                &aml::Name::new("_UID".try_into()?, &self.cpu_id)?,
                &aml::Method::new(
                    "_STA".try_into()?,
                    0,
                    false,
                    vec![&aml::Return::new(&csta_call)],
                ),
                &aml::Method::new(
                    "_EJ0".try_into()?,
                    1,
                    false,
                    vec![&ej0_select, &ej0_trigger],
                ),
                &aml::Name::new("_MAT".try_into()?, &aml::Buffer::new(mat_data))?,
            ],
        )
        .append_aml_bytes(v)
    }
}

impl Aml for CpuHotplugController {
    fn append_aml_bytes(&self, v: &mut Vec<u8>) -> Result<(), aml::AmlError> {
        let mmio_end = self
            .mmio_addr
            .checked_add(CPU_HOTPLUG_MMIO_SIZE - 1)
            .ok_or(aml::AmlError::AddressRange)?;
        let mmio_addr: usize = self
            .mmio_addr
            .try_into()
            .map_err(|_| aml::AmlError::AddressRange)?;
        let mmio_size: usize = CPU_HOTPLUG_MMIO_SIZE
            .try_into()
            .map_err(|_| aml::AmlError::AddressRange)?;

        aml::Device::new(
            "_SB_.PRES".try_into()?,
            vec![
                &aml::Name::new("_HID".try_into()?, &"PNP0A06")?,
                &aml::Name::new("_UID".try_into()?, &"CPU Hotplug Controller")?,
                &aml::Mutex::new("CPLK".try_into()?, 0),
                &aml::Name::new(
                    "_CRS".try_into()?,
                    &aml::ResourceTemplate::new(vec![&aml::AddressSpace::new_memory(
                        aml::AddressSpaceCacheable::NotCacheable,
                        true,
                        self.mmio_addr,
                        mmio_end,
                    )?]),
                )?,
                &aml::OpRegion::new(
                    "PRST".try_into()?,
                    aml::OpRegionSpace::SystemMemory,
                    mmio_addr,
                    mmio_size,
                ),
                &aml::Field::new(
                    "PRST".try_into()?,
                    aml::FieldAccessType::Byte,
                    aml::FieldUpdateRule::WriteAsZeroes,
                    vec![
                        aml::FieldEntry::Reserved(32),
                        aml::FieldEntry::Named(*b"CPEN", 1),
                        aml::FieldEntry::Named(*b"CINS", 1),
                        aml::FieldEntry::Named(*b"CRMV", 1),
                        aml::FieldEntry::Named(*b"CEJ0", 1),
                        aml::FieldEntry::Reserved(4),
                        aml::FieldEntry::Named(*b"CCMD", 8),
                    ],
                ),
                &aml::Field::new(
                    "PRST".try_into()?,
                    aml::FieldAccessType::DWord,
                    aml::FieldUpdateRule::Preserve,
                    vec![
                        aml::FieldEntry::Named(*b"CSEL", 32),
                        aml::FieldEntry::Reserved(32),
                        aml::FieldEntry::Named(*b"CDAT", 32),
                    ],
                ),
            ],
        )
        .append_aml_bytes(v)?;

        let csta_method = CstaMethod;
        let ctfy_method = CtfyMethod {
            max_vcpus: self.max_vcpus,
        };
        let cscn_method = CscnMethod {
            max_vcpus: self.max_vcpus,
        };

        let cpu_devices: Vec<CpuAml> = (0..self.max_vcpus)
            .map(|cpu_id| CpuAml { cpu_id })
            .collect();

        let hid = aml::Name::new("_HID".try_into()?, &"ACPI0010")?;
        let cid = aml::Name::new("_CID".try_into()?, &aml::EisaName::new("PNP0A05")?)?;
        let mut cpu_children: Vec<&dyn Aml> = Vec::with_capacity(5 + cpu_devices.len());
        cpu_children.extend([&hid as &dyn Aml, &cid, &csta_method, &ctfy_method, &cscn_method]);
        for cpu in &cpu_devices {
            cpu_children.push(cpu);
        }

        aml::Device::new("_SB_.CPUS".try_into()?, cpu_children).append_aml_bytes(v)
    }
}
