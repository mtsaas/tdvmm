//! Magic guest-physical addresses and other x86_64 layout constants.
//!
//! Values cribbed from Firecracker's `arch/x86_64/layout.rs` and the Linux
//! 64-bit boot protocol. Keep these in one place so the boot-time writers
//! (page tables, GDT, zero page, MPTable, E820) all agree.

/// Where the uncompressed kernel is loaded (1 MiB). Also the start of "high" RAM.
pub const HIMEM_START: u64 = 0x0010_0000;

/// The Linux "zero page" (`boot_params`) address. `%rsi` points here at entry.
pub const ZERO_PAGE_START: u64 = 0x7000;

/// Kernel command line location and cap.
pub const CMDLINE_START: u64 = 0x0002_0000;
pub const CMDLINE_MAX_SIZE: usize = 2048;

/// Boot GDT / IDT scratch locations (below the page tables).
pub const BOOT_GDT_OFFSET: u64 = 0x500;
pub const BOOT_IDT_OFFSET: u64 = 0x520;

/// Initial identity-mapped page tables for entering 64-bit long mode.
pub const PML4_START: u64 = 0x9000;
pub const PDPTE_START: u64 = 0xa000;
pub const PDE_START: u64 = 0xb000;

/// Initial stack pointer for the boot vCPU.
pub const BOOT_STACK_POINTER: u64 = 0x8ff0;

/// Start of the "system data" region (EBDA). We drop the MPTable here; it is
/// also where the guest kernel scans for the MP floating pointer.
pub const SYSTEM_MEM_START: u64 = 0x0009_fc00;
/// RSDP scan location / top of the system-data region.
pub const RSDP_ADDR: u64 = 0x000e_0000;
/// Size of the reserved system-data region [SYSTEM_MEM_START, RSDP_ADDR).
pub const SYSTEM_MEM_SIZE: u64 = RSDP_ADDR - SYSTEM_MEM_START;

/// TSS address required by KVM on Intel (KVM_SET_TSS_ADDR) before KVM_RUN.
pub const KVM_TSS_ADDRESS: u64 = 0xfffb_d000;

/// Start of the 32-bit MMIO gap (3 GiB). Guest RAM never backs `[3 GiB, 4 GiB)`
/// (the LAPIC / IO-APIC / KVM-TSS live there). RAM up to this address is the low
/// region; anything above spills into a second region based at 4 GiB
/// ([`FIRST_ADDR_PAST_32BITS`]), leaving the gap unbacked.
pub const MMIO_MEM_START: u64 = 0xc000_0000;

/// First guest-physical address past the 32-bit range (4 GiB). The high RAM
/// region is based here, so nothing overlaps the 32-bit MMIO gap below it.
pub const FIRST_ADDR_PAST_32BITS: u64 = 0x1_0000_0000;

/// Legacy 16550 UART base port and its ISA IRQ line (COM1 / ttyS0 — the console).
pub const SERIAL_PORT_BASE: u16 = 0x3f8;
pub const SERIAL_IRQ: u32 = 4;

/// The second 16550 UART (COM2 / ttyS1) — the TEST-1a modeled control channel.
/// Standard PC wiring: base port 0x2f8, ISA IRQ3. The guest `tdvmm-agent` blocks
/// reading ttyS1 (a blocked read = no wakes = fast-forward-transparent); the VMM
/// delivers control commands here as scheduled queue events and reads the agent's
/// line-delimited JSON replies. IRQ3 is already identity-routed to IO-APIC pin 3
/// by the MP table (see `mptable::isa_irq_to_ioapic_pin`), so no routing change is
/// needed. The guest kernel must expose ttyS1: the pinned kernel gains
/// `CONFIG_SERIAL_8250_NR_UARTS=2` / `RUNTIME_UARTS=2` (see
/// `guest/kernel/test1a-com2.config`).
pub const SERIAL2_PORT_BASE: u16 = 0x2f8;
pub const SERIAL2_IRQ: u32 = 3;

/// The fourth 16550 UART (COM4 / ttyS3) — the `--allow-egress` transport.
/// Standard PC wiring: base port 0x2e8, ISA IRQ3 — SHARED with COM2 (both are
/// identity-routed to IO-APIC pin 3 by the MP table, so no routing change is
/// needed). A guest-side forwarder opens ttyS3 and the host `EgressBackend`
/// (`crate::egress`) terminates the proxied TCP. The guest kernel exposes ttyS3
/// via `CONFIG_SERIAL_8250_NR_UARTS=4` / `RUNTIME_UARTS=4` (recorded in
/// `guest/kernel/egress-com4.config`). COM3 (ttyS2) is intentionally left
/// unbacked (open bus): NR_UARTS=4 is the smallest cap that includes the COM4
/// slot. When `--allow-egress` is off, COM4 is not instantiated, 0x2e8 stays
/// open bus, and ttyS3 never registers — byte-identical to the closed world.
pub const SERIAL4_PORT_BASE: u16 = 0x2e8;
/// COM4's ISA line. IRQ3 is SHARED with COM2 (ttyS1): Linux registers IRQ3 with
/// `IRQF_SHARED` and its one 8250 handler walks the per-IRQ port chain, servicing
/// whichever UART on the line asserted. That correctness rests on IRQ3 carrying
/// ONLY 8250 ports (COM2 + COM4) — a non-8250 device parked on IRQ3 would not be
/// in the 8250 chain and its edge would be lost (or spuriously acked). IRQ3 must
/// therefore stay 8250-only; route any future device to a different line.
pub const SERIAL4_IRQ: u32 = 3;

/// POST diagnostic port (checkpoint codes). We silently swallow writes here so
/// early-boot BIOS-style probing does not fault out.
pub const POST_PORT: u16 = 0x80;
