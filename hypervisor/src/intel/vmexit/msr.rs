//! Provides virtual machine management capabilities, specifically for handling MSR
//! read and write operations. It ensures that guest MSR accesses are properly
//! intercepted and handled, with support for injecting faults for unauthorized accesses.
//! Credits:
//! https://revers.engineering/patchguard-detection-of-hypervisor-based-instrospection-p2/
//! https://mellownight.github.io/AetherVisor
//! https://github.com/tandasat/MiniVisorPkg/issues/4#issuecomment-664030968
//! jessiep_

use {
    crate::{
        error::HypervisorError,
        intel::{
            bitmap::{MsrAccessType, MsrOperation},
            events::EventInjection,
            hooks::hook_manager::SHARED_HOOK_MANAGER,
            support::{rdmsr, wrmsr},
            vm::Vm,
            vmexit::ExitType,
        },
    },
    bit_field::BitField,
    core::ops::RangeInclusive,
    log::*,
    x86::msr,
};

/// Handles MSR access based on the provided access type.
///
/// This function checks if the requested MSR address is within a valid
/// range, a reserved range, or a synthetic MSR range used by Hyper-V.
/// For valid MSRs, the function will either read or write to the MSR based
/// on the access type. For reserved or synthetic MSRs, a general protection
/// fault is injected.
///
/// # Arguments
///
/// * `registers` - A mutable reference to the guest's current register state.
/// * `access_type` - The type of MSR access (read or write).
///
/// # Returns
///
/// * `ExitType::IncrementRIP` - To move past the `rdmsr` or `wrmsr` instruction in the VM.
///
/// Reference: Intel® 64 and IA-32 Architectures Software Developer's Manual: RDMSR—Read From Model Specific Register or WRMSR—Write to Model Specific Register
/// and Table C-1. Basic Exit Reasons 31 and 32.
pub fn handle_msr_access(vm: &mut Vm, access_type: MsrAccessType) -> Result<ExitType, HypervisorError> {
    debug!("Handling MSR VM exit...");

    // Define the mask for the low 32-bits of the MSR value
    const MSR_MASK_LOW: u64 = u32::MAX as u64;

    // Define the VMX lock bit for IA32_FEATURE_CONTROL MSR
    const VMX_LOCK_BIT: u64 = 0;
    const VMXON_OUTSIDE_SMX: u64 = 2;

    let msr_id = vm.guest_registers.rcx as u32;
    let msr_value = (vm.guest_registers.rdx << 32) | (vm.guest_registers.rax & MSR_MASK_LOW);

    // Determine if the MSR address is valid, reserved, or synthetic (EasyAntiCheat and Battleye invalid MSR checks)
    // by checking if the MSR address is in the Hyper-V range or outside other valid ranges

    // Define the range for valid MSR access and Hyper-V MSRs
    const MSR_VALID_RANGE_LOW: RangeInclusive<u32> = 0x00000000..=0x00001FFF;
    const MSR_VALID_RANGE_HIGH: RangeInclusive<u32> = 0xC0000000..=0xC0001FFF;
    const MSR_HYPERV_RANGE: RangeInclusive<u32> = 0x40000000..=0x400000FF;

    trace!("MSR access attempted: {:#x}", msr_id);

    #[cfg(feature = "vmware")]
    if !MSR_VALID_RANGE_LOW.contains(&msr_id) && !MSR_VALID_RANGE_HIGH.contains(&msr_id) && MSR_HYPERV_RANGE.contains(&msr_id) {
        // In VMware, do not inject #GP for MSRs within the Hyper-V range
        trace!("Invalid MSR access attempted: {:#x}", msr_id);
        EventInjection::vmentry_inject_gp(0);
        return Ok(ExitType::Continue);
    }

    #[cfg(not(feature = "vmware"))]
    if !(MSR_VALID_RANGE_LOW.contains(&msr_id) || MSR_VALID_RANGE_HIGH.contains(&msr_id)) || MSR_HYPERV_RANGE.contains(&msr_id) {
        // On real hardware, inject #GP if MSR is in the Hyper-V range or outside the valid ranges
        trace!("Invalid MSR access attempted: {:#x}", msr_id);
        EventInjection::vmentry_inject_gp(0);
        return Ok(ExitType::Continue);
    }

    trace!("Valid MSR access attempted: {:#x}", msr_id);

    match access_type {
        // Credits: jessiep_ and https://revers.engineering/patchguard-detection-of-hypervisor-based-instrospection-p2/
        MsrAccessType::Read => {
            let result_value = match msr_id {
                // When the guest reads the LSTAR MSR, the hypervisor returns the shadowed original value instead of the actual (modified) value.
                // This way, the guest OS sees what it expects, assuming no tampering has occurred.
                msr::IA32_LSTAR => {
                    trace!("IA32_LSTAR read attempted with MSR value: {:#x}", msr_value);
                    // This won't be 0 here because we intercept and populate it during MsrAccessType::Write on IA32_LSTAR which is set during the initial phase when ntoskrnl.exe
                    vm.guest_registers.original_lstar
                }

                // Simulate IA32_FEATURE_CONTROL as locked: VMX locked bit set, VMX outside SMX clear.
                // Set lock bit, indicating that feature control is locked.
                // Credits to @vmctx
                msr::IA32_FEATURE_CONTROL => {
                    trace!("IA32_FEATURE_CONTROL read attempted with MSR value: {:#x}", msr_value);
                    let mut result_value = rdmsr(msr_id as _);
                    result_value.set_bit(VMX_LOCK_BIT as usize, true);
                    result_value.set_bit(VMXON_OUTSIDE_SMX as usize, false);
                    result_value
                }
                _ => rdmsr(msr_id),
            };

            vm.guest_registers.rax = result_value & MSR_MASK_LOW;
            vm.guest_registers.rdx = result_value >> 32;
        }
        // Credits: jessiep_ and https://revers.engineering/patchguard-detection-of-hypervisor-based-instrospection-p2/
        MsrAccessType::Write => {
            if msr_id == msr::IA32_LSTAR {
                trace!("IA32_LSTAR write attempted with MSR value: {:#x}", msr_value);
                // trace!("GuestRegisters Original LSTAR value: {:#x}", vm.guest_registers.original_lstar);
                // trace!("GuestRegisters Hook LSTAR value: {:#x}", vm.guest_registers.hook_lstar);

                // Lock the shared hook manager
                let mut hook_manager = SHARED_HOOK_MANAGER.lock();

                hook_manager
                    .msr_bitmap
                    .modify_msr_interception(msr::IA32_LSTAR, MsrAccessType::Write, MsrOperation::Unhook);
                trace!("Unhooked MSR_IA32_LSTAR");

                // Get and set the ntoskrnl.exe base address and size, to be used for hooking later in `CpuidLeaf::CacheInformation` or by the guest client.
                hook_manager.set_kernel_base_and_size(msr_value)?;

                // Check if it's the first time we're intercepting a write to LSTAR.
                // If so, store the value being written as the original LSTAR value.
                if vm.guest_registers.original_lstar == 0 {
                    vm.guest_registers.original_lstar = msr_value;
                    // Optionally set a hook LSTAR value here. For now, let's assume we simply store the original value.
                    // This is a placeholder for where you would set your hook.
                    vm.guest_registers.hook_lstar = vm.guest_registers.original_lstar;
                    // This should eventually be replaced with an actual hook address.
                }

                // If the guest attempts to write back the original LSTAR value we provided,
                // it could be part of an integrity check. In such a case, we allow the write to go through
                // but actually write our hook again to maintain control.
                if msr_value == vm.guest_registers.original_lstar {
                    // Write the hook LSTAR value if it's set, otherwise write the original value.
                    // This check is necessary in case the hook_lstar is not yet implemented or set to 0.
                    let value_to_write = if vm.guest_registers.hook_lstar != 0 {
                        vm.guest_registers.hook_lstar
                    } else {
                        vm.guest_registers.original_lstar
                    };

                    wrmsr(msr_id, value_to_write);
                }
            } else if msr_id == msr::IA32_GS_BASE {
                // Credits: https://github.com/tandasat/MiniVisorPkg/issues/4#issuecomment-664030968
                //
                // Write to this MSR happens at KiSystemStartup for each processor.
                // We intercept this only once that occurs on the BSP as a trigger
                // point to initialize the guest agent. We do not emulate this
                // operation and instead, make the guest retry after returning from
                // the guest agent. At the 2nd try, VM-exit no longer occurs.
                //
                // Note that is place is too early to debug the guest agent. Move to
                // later such as KeInitAmd64SpecificState for this. This place was
                // chosen so that none of PatchGuard context is initialized.
                //
                // trace!("Unhooking MSR_IA32_GS_BASE.");
                // vm.msr_bitmap.modify_msr_interception(msr::IA32_GS_BASE, MsrAccessType::Write, MsrOperation::Unhook);
                // trace!("KiSystemStartup being executed...");
                wrmsr(msr_id, msr_value);
            } else {
                // For MSRs other than msr::IA32_LSTAR or non-original LSTAR value writes, proceed with the write operation.
                // If the guest writes any other value (which would typically only happen if the guest is attempting to modify the syscall mechanism itself),
                // the write operation proceeds.
                wrmsr(msr_id, msr_value);
            }
        }
    }

    debug!("MSR VMEXIT handled successfully.");
    Ok(ExitType::IncrementRIP)
}
