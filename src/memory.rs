//! Guest RAM: a single anonymous-mmap region registered with KVM.
//!
//! Step 1 only handles guests whose RAM fits below the 32-bit MMIO gap
//! (3 GiB), so there is exactly one region starting at guest-physical 0.

use kvm_bindings::kvm_userspace_memory_region;
use kvm_ioctls::VmFd;
use vm_memory::{GuestAddress, GuestMemoryBackend, GuestMemoryRegion};

use crate::arch::MMIO_MEM_START;

/// Concrete guest-memory type used throughout the VMM.
pub type GuestMemoryMmap = vm_memory::GuestMemoryMmap<()>;

#[derive(Debug)]
pub enum MemoryError {
    TooLarge(usize),
    Mmap(String),
    Register(String),
}

impl std::fmt::Display for MemoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MemoryError::TooLarge(n) => write!(
                f,
                "requested {n} bytes of guest RAM; Step 1 supports at most {} bytes (below the \
                 32-bit MMIO gap)",
                MMIO_MEM_START
            ),
            MemoryError::Mmap(e) => write!(f, "failed to mmap guest memory: {e}"),
            MemoryError::Register(e) => write!(f, "failed to register guest memory with KVM: {e}"),
        }
    }
}
impl std::error::Error for MemoryError {}

/// Allocate `size` bytes of guest RAM as one region at GPA 0.
pub fn create_guest_memory(size: usize) -> Result<GuestMemoryMmap, MemoryError> {
    if size as u64 > MMIO_MEM_START {
        return Err(MemoryError::TooLarge(size));
    }
    GuestMemoryMmap::from_ranges(&[(GuestAddress(0), size)])
        .map_err(|e| MemoryError::Mmap(format!("{e:?}")))
}

/// Hand each guest-memory region to KVM via KVM_SET_USER_MEMORY_REGION.
pub fn register_with_kvm(vm: &VmFd, mem: &GuestMemoryMmap) -> Result<(), MemoryError> {
    for (slot, region) in mem.iter().enumerate() {
        let mr = kvm_userspace_memory_region {
            slot: slot as u32,
            guest_phys_addr: region.start_addr().0,
            memory_size: region.len(),
            // `as_ptr` reaches the backing MmapRegion via Deref.
            userspace_addr: region.as_ptr() as u64,
            flags: 0,
        };
        // SAFETY: `region` owns a live mmap of `memory_size` bytes at
        // `userspace_addr`; it outlives the VM (both are dropped at process
        // exit), so the mapping KVM records stays valid.
        unsafe {
            vm.set_user_memory_region(mr)
                .map_err(|e| MemoryError::Register(e.to_string()))?;
        }
    }
    Ok(())
}
