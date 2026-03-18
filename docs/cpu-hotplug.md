# CPU Hotplugging with ACPI

## What is ACPI CPU hotplug

ACPI CPU hotplug is an industry-standard mechanism for dynamically adding and
removing vCPUs from a running virtual machine. Unlike memory hotplug, which uses
a paravirtualized `virtio-mem` device, CPU hotplug uses standard ACPI tables
that any ACPI-capable guest kernel understands without a special driver.

At boot, Firecracker populates the MADT (Multiple APIC Description Table) with
up to `max_vcpus` APIC entries. Only `vcpu_count` of those entries are enabled;
the rest are marked as "online capable" so the guest knows they may appear later.
A GED (Generic Event Device) is registered to deliver interrupts when the CPU
topology changes.

On hotplug, Firecracker creates new KVM vCPUs, marks them as inserting in the
ACPI controller, and fires a GED interrupt. The guest kernel receives the
interrupt, scans the ACPI CPU topology, and onlines the new CPUs. On unplug, the
reverse happens: the controller marks CPUs as removing, fires GED, and the guest
offlines them. KVM vCPU threads cannot be destroyed, so unplugged vCPUs are
paused at the KVM level and can be resumed on a subsequent hotplug.

## Prerequisites

To support CPU hotplugging, you must use a guest kernel with the appropriate
version and configuration options enabled as follows:

#### Kernel Version Requirements

- `x86_64`: minimal kernel version is 4.15
  - Recommended: 5.10 or later for full ACPI CPU hotplug support
- `aarch64`: not supported. ACPI CPU hotplug is x86-specific.

For more information about officially supported guest kernels, refer to the
[kernel policy documentation](kernel-policy.md).

#### Kernel Config

The following options must be enabled in the guest kernel:

- `CONFIG_ACPI_HOTPLUG_CPU=y`: enables ACPI-driven CPU hotplug
- `CONFIG_ACPI_PROCESSOR=y`: required for ACPI processor enumeration

Both options are enabled by default in most distribution kernels.

## Configuring CPU hotplug

CPU hotplug must be configured before the VM starts. This can be done through a
`PUT` request on `/hotplug/cpu` or by including the configuration in the JSON
configuration file.

> [!Note] If no CPU hotplug configuration is set, `max_vcpus` defaults to
> `vcpu_count` from the machine config. No hotplug slots will be available.

### Configuration Parameters

- `max_vcpus` (required): The maximum number of vCPUs the VM can have. Must be
  greater than `vcpu_count` from the machine config and at most 32.

### API Configuration

Here is an example of how to configure CPU hotplug via the API. In this example,
the VM is configured to allow up to 8 vCPUs.

```console
socket_location=/run/firecracker.socket

curl --unix-socket $socket_location -i \
    -X PUT 'http://localhost/hotplug/cpu' \
    -H 'Accept: application/json' \
    -H 'Content-Type: application/json' \
    -d "{
        \"max_vcpus\": 8
    }"
```

> [!Note] This is only allowed before the `InstanceStart` action and not on
> snapshot-restored VMs (which will use the configuration saved in the
> snapshot).

### JSON Configuration

To configure via JSON, add the following to your VM configuration file:

```json
{
    "cpu-hotplug": {
        "max_vcpus": 8
    }
}
```

### Checking Device Status

After configuration, you can query the CPU hotplug status at any time:

```console
socket_location=/run/firecracker.socket

curl --unix-socket $socket_location -i \
    -X GET 'http://localhost/hotplug/cpu' \
    -H 'Accept: application/json'
```

This returns information about the current CPU hotplug state, including:

- `boot_vcpus`: Number of vCPUs the VM started with
- `max_vcpus`: Maximum number of vCPUs allowed
- `active_vcpus`: Number of vCPUs currently active

## Operating CPU hotplug

Once configured and the VM is running, you can dynamically adjust the number of
vCPUs available to the guest by sending a `PATCH` request with the desired vCPU
count.

### Hotplugging CPUs

To add vCPUs to a running VM, set `desired_vcpus` to a higher value:

```console
socket_location=/run/firecracker.socket

curl --unix-socket $socket_location -i \
    -X PATCH 'http://localhost/hotplug/cpu' \
    -H 'Accept: application/json' \
    -H 'Content-Type: application/json' \
    -d "{
        \"desired_vcpus\": 4
    }"
```

The VMM side of this operation is synchronous: new KVM vCPUs are created and the
GED interrupt is fired before the API call returns. The guest kernel then onlines
the new CPUs asynchronously. Use the `GET` API to monitor `active_vcpus` and
confirm the guest has onlined the new CPUs.

Depending on the guest kernel configuration, newly hotplugged CPUs may need
manual onlining. See [Configuring the guest](#configuring-the-guest) below.

### Hot-removing CPUs

To remove vCPUs from a running VM, set `desired_vcpus` to a lower value:

```console
socket_location=/run/firecracker.socket

curl --unix-socket $socket_location -i \
    -X PATCH 'http://localhost/hotplug/cpu' \
    -H 'Accept: application/json' \
    -H 'Content-Type: application/json' \
    -d "{
        \"desired_vcpus\": 2
    }"
```

Unplug requires guest cooperation. The ACPI controller marks the target CPUs as
removing and fires a GED interrupt; the guest must offline those CPUs in
response. Once the guest offlines a CPU, the corresponding KVM vCPU thread is
paused. Paused vCPU threads are not destroyed and can be resumed on a subsequent
hotplug.

> [!Note] `desired_vcpus` cannot be set below `boot_vcpus`. The CPUs the VM
> started with cannot be removed.

## Configuring the guest

Most kernels with `CONFIG_HOTPLUG_CPU=y` (the default in distribution kernels)
will automatically online hotplugged CPUs. No special boot parameters are
required for CPU hotplug.

If the guest does not auto-online new CPUs, you can online them manually from
inside the VM:

```console
echo 1 > /sys/devices/system/cpu/cpuN/online
```

Replace `N` with the CPU index reported by the kernel after the hotplug event.
To online all offline CPUs at once:

```console
for cpu in /sys/devices/system/cpu/cpu[0-9]*/online; do
    echo 1 > "$cpu"
done
```

## Security Considerations

**ACPI CPU hotplug requires cooperation from the guest kernel.**

### Trust Model

A compromised guest can ignore hotplug and unplug ACPI events. The VMM fires the
GED interrupt and marks CPUs as inserting or removing, but it cannot force the
guest to online or offline CPUs.

Users should:

- Monitor `active_vcpus` via the `GET` API to verify the guest has responded to
  hotplug or unplug requests.
- Be prepared to handle cases where the guest does not cooperate with CPU
  operations.

Unlike memory hotplug, there is no "protection" mechanism for unplugged CPUs.
Paused vCPU threads are managed at the KVM level, which prevents execution, but
this is enforced by the hypervisor rather than userspace memory protection.

## Compatibility with Other Features

CPU hotplug is compatible with all Firecracker features. Below are specific
notes for features that interact with CPU hotplug state.

### Snapshots

Full and diff snapshots preserve the CPU hotplug state. The
`CpuHotplugController` state, including `boot_vcpus`, `max_vcpus`, and the set
of enabled CPUs, is saved in the snapshot. On restore, the MMIO device and
interrupt are re-registered automatically.

Only active (online) vCPU states are saved in the snapshot. Paused (unplugged)
vCPUs are not included. Snapshots created without CPU hotplug state load
successfully; the feature is simply disabled on restore.

### Userfaultfd

No special handling is needed for CPU hotplug. The userfaultfd handler operates
on memory regions, not CPU state, so CPU hotplug events do not affect it.

### Metrics

API requests to `/hotplug/cpu` are tracked via the following metrics:

- `put_api_requests.hotplug_cpu_count`: successful `PUT` requests
- `put_api_requests.hotplug_cpu_fails`: failed `PUT` requests
- `patch_api_requests.hotplug_cpu_count`: successful `PATCH` requests
- `patch_api_requests.hotplug_cpu_fails`: failed `PATCH` requests

## Limitations

- `x86_64` only. aarch64 would require a PSCI-based approach and is not
  currently supported.
- Maximum 32 vCPUs, limited by `MAX_SUPPORTED_VCPUS` and the size of the MADT
  table.
- KVM vCPU threads cannot be destroyed, only paused. Unplugged vCPUs still
  consume a small amount of host resources.
- Guest cooperation is required for both plug and unplug operations. The VMM
  cannot force a guest to online or offline CPUs.
