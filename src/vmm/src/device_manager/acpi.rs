// Copyright 2024 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

#[cfg(target_arch = "x86_64")]
use acpi_tables::{Aml, aml};
use std::sync::{Arc, Mutex};

use crate::Vm;
use crate::devices::acpi::cpu_hotplug::{CpuHotplugController, CPU_HOTPLUG_MMIO_SIZE};
use crate::devices::acpi::vmclock::{VmClock, VmClockError};
use crate::devices::acpi::vmgenid::{VmGenId, VmGenIdError};
use crate::vstate::memory::GuestMemoryMmap;

#[derive(Debug, thiserror::Error, displaydoc::Display)]
pub enum ACPIDeviceError {
    /// VMGenID: {0}
    VmGenId(#[from] VmGenIdError),
    /// VMClock: {0}
    VmClock(#[from] VmClockError),
    /// Could not register IRQ with KVM: {0}
    RegisterIrq(#[from] kvm_ioctls::Error),
}

// Although both VMGenID and VMClock devices are always present, they should be instantiated when
// they are attached to preserve the existing ordering of GSI allocation.
#[derive(Debug, Default)]
pub struct ACPIDeviceManager {
    /// VMGenID device
    vmgenid: Option<VmGenId>,
    /// VMclock device
    vmclock: Option<VmClock>,
    cpu_hotplug: Option<Arc<Mutex<CpuHotplugController>>>,
}

impl ACPIDeviceManager {
    pub fn new(vmgenid: VmGenId, vmclock: VmClock, cpu_hotplug: Option<Arc<Mutex<CpuHotplugController>>>) -> Self {
        ACPIDeviceManager {
            vmgenid: Some(vmgenid),
            vmclock: Some(vmclock),
            cpu_hotplug,
        }
    }

    pub fn attach_vmgenid(&mut self, vm: &Vm) -> Result<(), ACPIDeviceError> {
        self.vmgenid = Some(VmGenId::new(&mut vm.resource_allocator())?);
        Ok(())
    }

    pub fn attach_vmclock(&mut self, vm: &Vm) -> Result<(), ACPIDeviceError> {
        self.vmclock = Some(VmClock::new(&mut vm.resource_allocator())?);
        Ok(())
    }

    pub fn vmgenid(&self) -> &VmGenId {
        self.vmgenid.as_ref().expect("Missing VMGenID device")
    }

    pub fn vmclock(&self) -> &VmClock {
        self.vmclock.as_ref().expect("Missing VMClock device")
    }

    pub fn cpu_hotplug(&self) -> Option<&Arc<Mutex<CpuHotplugController>>> {
        self.cpu_hotplug.as_ref()
    }

    pub fn attach_cpu_hotplug(
        &mut self,
        boot_vcpus: u8,
        max_vcpus: u8,
        vm: &Vm,
    ) -> Result<Arc<Mutex<CpuHotplugController>>, ACPIDeviceError> {
        let mut ra = vm.resource_allocator();
        let gsi = ra
            .allocate_gsi_legacy(1)
            .map_err(|_| ACPIDeviceError::RegisterIrq(kvm_ioctls::Error::new(libc::ENOMEM)))?[0];
        let mmio_addr = ra
            .allocate_32bit_mmio_memory(
                CPU_HOTPLUG_MMIO_SIZE.try_into().unwrap(),
                8,
                vm_allocator::AllocPolicy::FirstMatch,
            )
            .map_err(|_| ACPIDeviceError::RegisterIrq(kvm_ioctls::Error::new(libc::ENOMEM)))?;
        let interrupt_evt = vmm_sys_util::eventfd::EventFd::new(libc::EFD_NONBLOCK)
            .map_err(|_| ACPIDeviceError::RegisterIrq(kvm_ioctls::Error::new(libc::ENOMEM)))?;
        let controller =
            CpuHotplugController::new(boot_vcpus, max_vcpus, mmio_addr, gsi, interrupt_evt);
        let shared = Arc::new(Mutex::new(controller));
        self.cpu_hotplug = Some(shared.clone());
        Ok(shared)
    }

    pub fn activate_cpu_hotplug(&self, vm: &Vm) -> Result<(), ACPIDeviceError> {
        if let Some(cpu_hp_arc) = &self.cpu_hotplug {
            let cpu_hp = cpu_hp_arc.lock().expect("Poisoned lock");
            vm.register_irq(cpu_hp.interrupt_evt(), cpu_hp.gsi())?;
        }
        Ok(())
    }

    pub fn activate_vmgenid(&self, vm: &Vm) -> Result<(), ACPIDeviceError> {
        vm.register_irq(&self.vmgenid().interrupt_evt, self.vmgenid().gsi)?;
        self.vmgenid().activate(vm.guest_memory())?;
        Ok(())
    }

    pub fn activate_vmclock(&self, vm: &Vm) -> Result<(), ACPIDeviceError> {
        vm.register_irq(&self.vmclock().interrupt_evt, self.vmclock().gsi)?;
        self.vmclock().activate(vm.guest_memory())?;
        Ok(())
    }

    pub fn do_post_restore_vmgenid(&self) -> Result<(), ACPIDeviceError> {
        self.vmgenid().do_post_restore()?;
        Ok(())
    }

    pub fn do_post_restore_vmclock(
        &mut self,
        mem: &GuestMemoryMmap,
    ) -> Result<(), ACPIDeviceError> {
        self.vmclock
            .as_mut()
            .expect("Missing VMClock device")
            .do_post_restore(mem)?;
        Ok(())
    }
}

#[cfg(target_arch = "x86_64")]
impl Aml for ACPIDeviceManager {
    fn append_aml_bytes(&self, v: &mut Vec<u8>) -> Result<(), aml::AmlError> {
        // AML for [`VmGenId`] device.
        self.vmgenid().append_aml_bytes(v)?;
        // AML for [`VmClock`] device.
        self.vmclock().append_aml_bytes(v)?;

        let cpu_hp_locked = self.cpu_hotplug.as_ref().map(|arc| arc.lock().expect("Poisoned lock"));
        if let Some(ref cpu_hp) = cpu_hp_locked {
            cpu_hp.append_aml_bytes(v)?;
        }

        let vmgenid_interrupt = aml::Interrupt::new(true, true, false, false, self.vmgenid().gsi);
        let vmclock_interrupt = aml::Interrupt::new(true, true, false, false, self.vmclock().gsi);
        let mut ged_interrupts: Vec<&dyn Aml> = vec![&vmgenid_interrupt, &vmclock_interrupt];

        // Pre-compute CPU hotplug GSI as u8 and interrupt (if configured)
        #[allow(clippy::cast_possible_truncation)]
        let cpu_hp_gsi_u8: Option<u8> = cpu_hp_locked.as_ref().map(|c| c.gsi() as u8);
        let cpu_hp_interrupt = cpu_hp_locked
            .as_ref()
            .map(|cpu_hp| aml::Interrupt::new(true, true, false, false, cpu_hp.gsi()));
        if let Some(ref intr) = cpu_hp_interrupt {
            ged_interrupts.push(intr);
        }

        // Build _EVT method body — bind all temporaries to named variables
        #[allow(clippy::cast_possible_truncation)]
        let vmgenid_gsi_u8: u8 = self.vmgenid().gsi as u8;
        #[allow(clippy::cast_possible_truncation)]
        let vmclock_gsi_u8: u8 = self.vmclock().gsi as u8;

        let vgen_path = aml::Path::new("\\_SB_.VGEN")?;
        let vgen_notify_val = 0x80usize;
        let vgen_notify = aml::Notify::new(&vgen_path, &vgen_notify_val);
        let vgen_eq = aml::Equal::new(&aml::Arg(0), &vmgenid_gsi_u8);
        let vmgenid_evt = aml::If::new(&vgen_eq, vec![&vgen_notify]);

        let vclk_path = aml::Path::new("\\_SB_.VCLK")?;
        let vclk_notify_val = 0x80usize;
        let vclk_notify = aml::Notify::new(&vclk_path, &vclk_notify_val);
        let vclk_eq = aml::Equal::new(&aml::Arg(0), &vmclock_gsi_u8);
        let vmclock_evt = aml::If::new(&vclk_eq, vec![&vclk_notify]);

        let mut evt_body: Vec<&dyn Aml> = vec![&vmgenid_evt, &vmclock_evt];

        let cscn_path = "\\_SB_.CPUS.CSCN".try_into()?;
        let cscn_call = aml::MethodCall::new(cscn_path, vec![]);
        if cpu_hp_gsi_u8.is_some() {
            evt_body.push(&cscn_call);
        }

        // Create the AML for the GED interrupt handler
        let ged_hid = aml::Name::new("_HID".try_into()?, &"ACPI0013")?;
        let ged_crs = aml::Name::new(
            "_CRS".try_into()?,
            &aml::ResourceTemplate::new(ged_interrupts),
        )?;
        let ged_evt = aml::Method::new("_EVT".try_into()?, 1, true, evt_body);

        aml::Device::new("_SB_.GED_".try_into()?, vec![&ged_hid, &ged_crs, &ged_evt])
            .append_aml_bytes(v)
    }
}
