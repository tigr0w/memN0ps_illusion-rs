//! This module provides utility functions for processor-related operations in UEFI,
//! facilitating the initialization of virtualization across multiple processors.

use {
    crate::virtualize::virtualize_system,
    alloc::boxed::Box,
    core::ffi::c_void,
    hypervisor::intel::{
        capture::{capture_registers, GuestRegisters},
        ept::paging::Ept,
        shared::SharedData,
    },
    log::*,
    uefi::{prelude::*, proto::pi::mp::MpServices},
};

/// Starts the hypervisor on all processors.
///
/// # Arguments
///
/// * `system_table` - A reference to the UEFI System Table.
///
/// # Returns
///
/// A result indicating the success or failure of starting the hypervisor.
pub fn start_hypervisor_on_all_processors(
    boot_services: &BootServices,
    primary_ept: Box<Ept>,
    secondary_ept: Box<Ept>,
) -> uefi::Result<()> {
    debug!("Creating Shared Data");
    let mut shared_data =
        SharedData::new(primary_ept, secondary_ept).expect("Failed to create shared data");

    let handle = boot_services.get_handle_for_protocol::<MpServices>()?;
    let mp_services = boot_services.open_protocol_exclusive::<MpServices>(handle)?;
    let processor_count = mp_services.get_number_of_processors()?;

    info!("Total processors: {}", processor_count.total);
    info!("Enabled processors: {}", processor_count.enabled);

    if processor_count.enabled == 1 {
        info!("Found only one processor, virtualizing it");
        start_hypervisor(shared_data.as_mut());
    } else {
        info!("Found multiple processors, virtualizing all of them");
        mp_services
            .startup_all_aps(
                true,
                start_hypervisor_on_ap as _,
                shared_data.as_mut() as *mut _ as *mut _,
                None,
                None,
            )
            .expect("Failed to start APs")
    }

    info!("The hypervisor has been installed successfully!");

    Ok(())
}

/// Hypervisor initialization procedure for Application Processors (APs).
///
/// # Arguments
///
/// * `procedure_argument` - A pointer to the `SharedData` instance.
extern "efiapi" fn start_hypervisor_on_ap(procedure_argument: *mut c_void) {
    let shared_data = unsafe { &mut *(procedure_argument as *mut SharedData) };
    start_hypervisor(shared_data);
}

/// Initiates the virtualization process.
///
/// # Arguments
///
/// * `shared_data` - A reference to the `SharedData` instance.
fn start_hypervisor(shared_data: &mut SharedData) {
    let mut guest_registers = GuestRegisters::default();
    // Unsafe block to capture the current CPU's register state.
    let mut is_virtualized = unsafe { capture_registers(&mut guest_registers) };

    // The guest will return here and it will have it's value of rax set to 1, meaning the logical core is virtualized.
    guest_registers.rax = 1;

    // After `vmlaunch`, Guest execution will begin here. We then check for an existing hypervisor:
    // if absent, proceed with installation; otherwise, no further action is needed.

    // Proceed with virtualization only if the current processor is not yet virtualized.
    debug!("Is virtualized: {}", is_virtualized);
    if !is_virtualized {
        debug!("Virtualizing the system");
        is_virtualized = true;
        debug!("Is virtualized: {}", is_virtualized);
        virtualize_system(&guest_registers, shared_data);
    }
}
