//! Guest RAM: one or two anonymous-mmap regions registered with KVM.
//!
//! Guests whose RAM fits below the 32-bit MMIO gap (3 GiB) use exactly one
//! region starting at guest-physical 0. Larger guests split into a low region
//! `[0, 3 GiB)` and a high region based at 4 GiB ([`FIRST_ADDR_PAST_32BITS`]),
//! so the `[3 GiB, 4 GiB)` gap (LAPIC / IO-APIC / KVM-TSS) is never backed.

use kvm_bindings::kvm_userspace_memory_region;
use kvm_ioctls::VmFd;
use vm_memory::{GuestAddress, GuestMemoryBackend, GuestMemoryRegion};

use crate::arch::{FIRST_ADDR_PAST_32BITS, MMIO_MEM_START};

/// Concrete guest-memory type used throughout the VMM.
pub type GuestMemoryMmap = vm_memory::GuestMemoryMmap<()>;

/// Static sanity ceiling on requested guest RAM (1 TiB). vm-memory mmaps
/// `MAP_NORESERVE`, so this is NOT a host-RAM check — it only catches obviously
/// bogus sizes (e.g. a byte count passed where MiB was meant). Nothing
/// host-probed enters this decision.
const MAX_GUEST_MEM: u64 = 1 << 40; // 1 TiB

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
                "guest RAM {} MiB exceeds the 1 TiB sanity cap — did you pass bytes instead of MiB?",
                *n as u64 / (1024 * 1024)
            ),
            MemoryError::Mmap(e) => write!(f, "failed to mmap guest memory: {e}"),
            MemoryError::Register(e) => write!(f, "failed to register guest memory with KVM: {e}"),
        }
    }
}
impl std::error::Error for MemoryError {}

/// Allocate `size` bytes of guest RAM.
///
/// * `size <= 3 GiB` → exactly one region `[GuestAddress(0), size)`.
/// * `size > 3 GiB`  → two regions: a 3 GiB low region `[0, MMIO_MEM_START)`
///   and a high region `[FIRST_ADDR_PAST_32BITS, size - MMIO_MEM_START)` based
///   at exactly 4 GiB. The `[3 GiB, 4 GiB)` MMIO gap stays unbacked.
pub fn create_guest_memory(size: usize) -> Result<GuestMemoryMmap, MemoryError> {
    let size = size as u64;
    if size > MAX_GUEST_MEM {
        return Err(MemoryError::TooLarge(size as usize));
    }
    let ranges: Vec<(GuestAddress, usize)> = if size > MMIO_MEM_START {
        vec![
            (GuestAddress(0), MMIO_MEM_START as usize),
            (
                GuestAddress(FIRST_ADDR_PAST_32BITS),
                (size - MMIO_MEM_START) as usize,
            ),
        ]
    } else {
        vec![(GuestAddress(0), size as usize)]
    };
    GuestMemoryMmap::from_ranges(&ranges).map_err(|e| MemoryError::Mmap(format!("{e:?}")))
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

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    /// Collect (start, len) for each region, in KVM slot (iteration) order.
    fn regions(mem: &GuestMemoryMmap) -> Vec<(u64, u64)> {
        mem.iter().map(|r| (r.start_addr().0, r.len())).collect()
    }

    #[test]
    fn exactly_3gib_is_one_region() {
        let mem = create_guest_memory((3 * GIB) as usize).unwrap();
        // Sub-/at-cap guests keep today's EXACT single-region layout: no second
        // region, no zero-length region.
        assert_eq!(regions(&mem), vec![(0, 3 * GIB)]);
    }

    #[test]
    fn small_guest_is_one_region() {
        let mem = create_guest_memory((512 * (1 << 20)) as usize).unwrap();
        assert_eq!(regions(&mem), vec![(0, 512 * (1 << 20))]);
    }

    #[test]
    fn eight_gib_splits_low_and_high() {
        let mem = create_guest_memory((8 * GIB) as usize).unwrap();
        // slot 0 = low [0, 3 GiB); slot 1 = high [4 GiB, 4 GiB + (8 - 3) GiB).
        assert_eq!(
            regions(&mem),
            vec![(0, MMIO_MEM_START), (FIRST_ADDR_PAST_32BITS, 5 * GIB)]
        );
        // The [3 GiB, 4 GiB) MMIO gap is never backed.
        assert_eq!(MMIO_MEM_START, 3 * GIB);
        assert_eq!(FIRST_ADDR_PAST_32BITS, 4 * GIB);
    }

    #[test]
    fn just_over_3gib_splits() {
        // One byte over the gap → a low region plus a 1-byte high region.
        let mem = create_guest_memory((MMIO_MEM_START + 1) as usize).unwrap();
        assert_eq!(
            regions(&mem),
            vec![(0, MMIO_MEM_START), (FIRST_ADDR_PAST_32BITS, 1)]
        );
    }

    #[test]
    fn one_tib_is_accepted() {
        // Exactly at the cap is allowed (2 regions).
        let mem = create_guest_memory(MAX_GUEST_MEM as usize).unwrap();
        assert_eq!(
            regions(&mem),
            vec![
                (0, MMIO_MEM_START),
                (FIRST_ADDR_PAST_32BITS, MAX_GUEST_MEM - MMIO_MEM_START)
            ]
        );
    }

    #[test]
    fn over_1tib_is_rejected() {
        let err = create_guest_memory((MAX_GUEST_MEM + 1) as usize).unwrap_err();
        assert!(matches!(err, MemoryError::TooLarge(_)));
    }
}
