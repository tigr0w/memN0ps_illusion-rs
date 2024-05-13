use {
    crate::{
        error::HypervisorError,
        intel::{
            addresses::PhysicalAddress,
            ept::AccessType,
            hooks::{
                inline::{InlineHook, InlineHookType},
                memory_manager::MemoryManager,
            },
            invept::invept_all_contexts,
            invvpid::invvpid_all_contexts,
            vm::Vm,
        },
        windows::kernel::KernelHook,
    },
    alloc::boxed::Box,
    core::intrinsics::copy_nonoverlapping,
    log::trace,
    x86::bits64::paging::{PAddr, BASE_PAGE_SIZE},
};

/// The maximum number of hooks supported by the hypervisor. Change this value as needed
pub const MAX_HOOKS: usize = 64;

/// Enum representing different types of hooks that can be applied.
#[derive(Debug, Clone, Copy)]
pub enum EptHookType {
    /// Hook for intercepting and possibly modifying function execution.
    /// Requires specifying the type of inline hook to use.
    Function(InlineHookType),

    /// Hook for hiding or monitoring access to a specific page.
    /// No inline hook type is required for page hooks.
    Page,
}

/// Represents hook manager structures for hypervisor operations.
#[repr(C)]
#[derive(Debug, Clone)]
pub struct HookManager {
    /// The memory manager instance for the pre-allocated shadow pages and page tables.
    pub memory_manager: Box<MemoryManager>,

    /// The current index of the hook being installed.
    current_hook_index: u64,

    /// The hook instance for the Windows kernel, storing the VA and PA of ntoskrnl.exe. This is retrieved from the first LSTAR_MSR write operation, intercepted by the hypervisor.
    pub kernel_hook: KernelHook,

    /// A flag indicating whether the CPUID cache information has been called. This will be used to perform hooks at boot time when SSDT has been initialized.
    /// KiSetCacheInformation -> KiSetCacheInformationIntel -> KiSetStandardizedCacheInformation -> __cpuid(4, 0)
    pub has_cpuid_cache_info_been_called: bool,

    /// The old RFLAGS value before turning off the interrupt flag.
    /// Used for restoring the RFLAGS register after handling the Monitor Trap Flag (MTF) VM exit.
    pub old_rflags: Option<u64>,

    /// The number of times the MTF (Monitor Trap Flag) should be triggered before disabling it for restoring overwritten instructions.
    pub mtf_counter: Option<u64>,
}

impl HookManager {
    /// Creates a new instance of `HookManager`.
    ///
    /// # Arguments
    ///
    /// * `primary_ept_pre_alloc_pts` - A mutable reference to a vector of pre-allocated page tables.
    ///
    /// # Returns
    /// A result containing a boxed `HookManager` instance or an error of type `HypervisorError`.
    pub fn new() -> Result<Box<Self>, HypervisorError> {
        trace!("Initializing hook manager");

        let memory_manager = Box::new(MemoryManager::new(MAX_HOOKS)?);

        Ok(Box::new(Self {
            memory_manager,
            current_hook_index: 0,
            has_cpuid_cache_info_been_called: false,
            kernel_hook: Default::default(),
            old_rflags: None,
            mtf_counter: None,
        }))
    }

    /// Installs an EPT hook for a function.
    ///
    /// # Arguments
    ///
    /// * `vm` - The virtual machine instance of the hypervisor.
    /// * `guest_va` - The virtual address of the function or page to be hooked.
    /// * `ept_hook_type` - The type of EPT hook to be installed.
    ///
    /// # Returns
    ///
    /// * Returns `Ok(())` if the hook was successfully installed, `Err(HypervisorError)` otherwise.
    pub fn ept_hook_function(vm: &mut Vm, guest_function_va: u64, ept_hook_type: EptHookType) -> Result<(), HypervisorError> {
        trace!("Creating EPT hook for function at VA: {:#x}", guest_function_va);

        let guest_function_pa = PAddr::from(PhysicalAddress::pa_from_va(guest_function_va));
        trace!("Guest function PA: {:#x}", guest_function_pa.as_u64());

        let guest_page_pa = guest_function_pa.align_down_to_base_page();
        trace!("Guest page PA: {:#x}", guest_page_pa.as_u64());

        let guest_large_page_pa = guest_function_pa.align_down_to_large_page();
        trace!("Guest large page PA: {:#x}", guest_large_page_pa.as_u64());

        // Check and possibly split the page before fetching the shadow page
        if !vm.hook_manager.memory_manager.is_page_split(guest_page_pa.as_u64()) {
            trace!("Splitting 2MB page to 4KB pages for Primary EPT: {:#x}", guest_large_page_pa);
            let mut pt_ptr = vm
                .hook_manager
                .memory_manager
                .get_or_create_page_table(guest_page_pa.as_u64(), vm.hook_manager.current_hook_index)?;
            vm.primary_ept
                .split_2mb_to_4kb(guest_large_page_pa.as_u64(), unsafe { pt_ptr.as_mut() })?;
        }

        // Check and possibly copy the page before setting up the shadow function
        if !vm.hook_manager.memory_manager.is_page_copied(guest_page_pa.as_u64()) {
            trace!("Copying guest page to shadow page: {:#x}", guest_page_pa.as_u64());
            let shadow_page = vm
                .hook_manager
                .memory_manager
                .get_or_create_shadow_page(guest_page_pa.as_u64(), vm.hook_manager.current_hook_index)?;
            Self::unsafe_copy_guest_to_shadow(guest_page_pa, PAddr::from(shadow_page.as_ptr() as u64));
        }

        // Retrieve shadow page and page table after ensuring they are set up
        let shadow_page_pa = PAddr::from(vm.hook_manager.memory_manager.get_shadow_page(guest_page_pa.as_u64()).unwrap().as_ptr() as u64);
        let mut pt_ptr = vm.hook_manager.memory_manager.get_page_table(guest_page_pa.as_u64()).unwrap();

        match ept_hook_type {
            EptHookType::Function(inline_hook_type) => {
                let shadow_function_pa = PAddr::from(Self::calculate_function_offset_in_host_shadow_page(shadow_page_pa, guest_function_pa));
                trace!("Shadow Function PA: {:#x}", shadow_function_pa);

                trace!("Installing inline hook at shadow function PA: {:#x}", shadow_function_pa.as_u64());
                InlineHook::new(shadow_function_pa.as_u64() as *mut u8, inline_hook_type).detour64();
            }
            EptHookType::Page => {
                unimplemented!("Page hooks are not yet implemented");
            }
        }

        trace!("Changing Primary EPT permissions for page to Read-Write (RW) only: {:#x}", guest_page_pa);
        vm.primary_ept
            .modify_page_permissions(guest_page_pa.as_u64(), AccessType::READ_WRITE, unsafe { pt_ptr.as_mut() })?;

        invept_all_contexts();
        invvpid_all_contexts();

        vm.hook_manager.current_hook_index += 1;

        trace!("EPT hook created and enabled successfully");

        Ok(())
    }

    /// Copies the guest page to the pre-allocated host shadow page.
    ///
    /// # Arguments
    ///
    /// * `guest_page_pa` - The physical address of the guest page.
    /// * `host_shadow_page_pa` - The physical address of the host shadow page.
    ///
    /// # Safety
    ///
    /// This function is unsafe because it performs a raw memory copy from the guest page to the shadow page.
    pub fn unsafe_copy_guest_to_shadow(guest_page_pa: PAddr, host_shadow_page_pa: PAddr) {
        unsafe { copy_nonoverlapping(guest_page_pa.as_u64() as *mut u8, host_shadow_page_pa.as_u64() as *mut u8, BASE_PAGE_SIZE) };
    }

    /// Calculates the address of the function within the host shadow page.
    ///
    /// # Arguments
    ///
    /// * `host_shadow_page_pa` - The physical address of the host shadow page.
    /// * `guest_function_pa` - The physical address of the guest function.
    ///
    /// # Returns
    ///
    /// * `u64` - The adjusted address of the function within the new page.
    fn calculate_function_offset_in_host_shadow_page(host_shadow_page_pa: PAddr, guest_function_pa: PAddr) -> u64 {
        host_shadow_page_pa.as_u64() + guest_function_pa.base_page_offset()
    }
}
