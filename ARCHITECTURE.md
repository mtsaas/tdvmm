# Architecture, from first principles

This document explains what `tdvmm` is, how it works, and why it is
built the way it is — starting from zero. It assumes you can read code but does
**not** assume you know anything about hypervisors, KVM, or how a CPU keeps time.
Every piece of jargon is defined the first time it appears; there is also a
[glossary](#glossary) at the end.

The companion `README.md` is a step-by-step build log. This document is the
conceptual map: the *ideas*, the *techniques*, and the *challenges*.

---

## 1. What this is, in one paragraph

`tdvmm` is a small **hypervisor** — a program that runs a whole other
computer (a "virtual machine") inside itself. It's about 4,500 lines of Rust. It
boots one Linux virtual machine, runs a real workload inside it (a PostgreSQL
database plus a little service container that writes a row every hour), and — the
interesting part — it can **fast-forward the virtual machine's clock**. When the
guest goes idle waiting for its next hourly task, instead of waiting a real hour,
the hypervisor jumps the guest's clock straight to that moment. The result: the
guest lives through **24 hours in about 90 real seconds** (~950× faster than
wall-clock), while believing each hour genuinely passed.

This is a **time-dilation VMM**: it bakes a Docker Compose–style service stack
into one self-contained file and runs it in a single Linux VM, fast-forwarding
virtual time whenever the guest is idle — so a multi-service test spanning hours
of service time finishes in seconds to minutes of real time. The guest lives in a
**closed world** (no network uplink, no disk, fixed start time), and that
closedness is what lets tdvmm collapse the idle stretches safely.

---

## 2. First principles: how a virtual machine actually runs

### 2.1 A CPU is a loop

At the lowest level, a CPU does one thing forever: read the instruction at some
address, execute it, move to the next. Registers hold its working state. Memory
holds code and data. "Running a program" is just pointing the CPU at some code
and letting the loop turn.

### 2.2 A virtual machine borrows the real CPU

You could *emulate* a CPU in software — read each guest instruction and interpret
it — but that's slow (10–20× slower than native). Modern Intel/AMD chips instead
have **hardware virtualization** (Intel calls it VT-x). It lets the real CPU run
the guest's instructions *directly, at native speed*, in a special "guest mode."

The trick is the **trap**. Most guest instructions (add, move, jump) just run.
But certain instructions — the ones that would touch real hardware or read
privileged state — cause the CPU to *stop guest mode and hand control back* to
the supervisor. This handover is called a **VM exit**. The supervisor inspects
what the guest was trying to do, emulates it, and resumes the guest. The guest
never knows it was interrupted.

So a virtual machine is: real CPU runs guest code → guest does something special →
**exit** → supervisor handles it → resume. That cycle is the beating heart of
every hypervisor, including this one.

### 2.3 The three layers

On Linux, the machinery is split across three layers, and it's essential to keep
them straight:

```
  ┌──────────────────────────────────────────────┐
  │  The guest: a whole Linux OS + our workload   │   runs in "guest mode"
  └──────────────────────────────────────────────┘
                     ▲  │ VM exit
          KVM_RUN    │  ▼
  ┌──────────────────────────────────────────────┐
  │  KVM  — a Linux *kernel* module               │   drives VT-x, handles the
  │  (part of the host kernel; we do not modify   │   lowest-level details
  │   it — Phase 1 is a zero-kernel-change goal)  │
  └──────────────────────────────────────────────┘
                     ▲  │ ioctl()
                     │  ▼
  ┌──────────────────────────────────────────────┐
  │  tdvmm  — THIS program (a normal userspace     │   decides the machine's shape:
  │  process). The "VMM" / hypervisor.            │   memory, devices, interrupts,
  │                                               │   and — crucially — the clock
  └──────────────────────────────────────────────┘
```

- **KVM** (Kernel-based Virtual Machine) is a module already in the Linux kernel.
  It talks to the VT-x hardware. You use it by opening `/dev/kvm` and issuing
  `ioctl()` calls. We do **not** patch it.
- **tdvmm** is this project: a plain userspace program — the **VMM** (Virtual
  Machine Monitor), a.k.a. the hypervisor. It asks KVM to create a VM and a
  virtual CPU, hands the VM some memory, and then runs the loop. When the guest
  exits, KVM returns control to *us*, and we decide what happens.

Everything interesting in this repo lives in that bottom box.

### 2.4 The vCPU run loop — the single most important abstraction

A **vCPU** (virtual CPU) is our handle on one guest processor. We run it by
calling the `KVM_RUN` ioctl. That call blocks until the guest exits, then returns
with a reason. Our whole program is, in essence:

```
loop {
    service_timers();   // fire any guest timer whose moment has arrived
    kvm_run();          // hand the CPU to the guest until it exits
    handle_exit();      // look at why it exited and emulate it
}
```

The exit **reason** tells us what the guest wanted. The ones that matter here:

- **PIO** (port I/O) — the guest used `in`/`out` on an "I/O port" (old-style
  hardware access). E.g. the serial port and the legacy timer live at ports.
- **MMIO** (memory-mapped I/O) — the guest read/wrote a magic memory address
  that isn't real RAM but a device register. E.g. the interrupt controller
  lives at fixed addresses; touching them exits to us.
- **HLT** — the guest executed the `hlt` instruction, which means "I have nothing
  to do; wake me when an interrupt arrives." This is how an idle OS sleeps, and
  it is the hook the whole fast-forward feature hangs on.
- **SHUTDOWN** — the guest triple-faulted (crashed/rebooted); we stop.

That's the core mental model. A hypervisor is a loop that runs guest code and
services its exits. The art is in *which* devices you emulate and *how*.

### 2.5 Guest memory is just our memory

We `mmap` a big region in our own process and tell KVM "this block of my memory
*is* the guest's physical RAM" (via `KVM_SET_USER_MEMORY_REGION`). The guest's
address 0 maps to the start of that block. When the guest reads its own RAM,
the hardware reads our memory directly — no exit, full speed. (See `memory.rs`,
`arch.rs`.)

---

## 3. The goal — and why *time* is the entire problem

We want one property: **fast-forward time** — skip the boring idle stretches
instantly, so a stack that idles through hours of virtual time runs in seconds to
minutes of real time.

It comes down to controlling one thing: **the guest's sense of time.** Here's why
time is hard.

A normal VM's clock is glued to the *host's* wall clock:

- The guest reads the CPU's cycle counter (the **TSC**, Time-Stamp Counter) with
  the `rdtsc` instruction. For speed, `rdtsc` **does not cause a VM exit** — the
  guest reads the real, free-running host counter directly. We never see it.
- The guest's timers (the periodic "tick" that drives its scheduler, its
  `sleep()`s, its timeouts) are ultimately armed against that same host time.
- Interrupts arrive on the host's schedule.

So by default, time inside the guest *is* host time. You cannot skip an hour,
because the guest is reading a counter you don't control.

**The insight that makes everything work: if we control the one clock the guest
reads, every time-dependent behavior follows.** Own the clock, own time.

---

## 4. The core idea: one virtual clock (`vtsc`)

Everything in this VMM hangs off a single definition (`vtsc.rs`):

```
vtsc_now()  =  host_rdtsc()  +  tsc_offset
```

`vtsc` is the guest's virtual time, in TSC cycles. It's the real host counter
plus an **offset** we control.

The key fact: **the guest's own `rdtsc` returns the same value.** VT-x has a
built-in "TSC offset" — a number KVM adds to the host TSC before the guest sees
it. We set it via `KVM_VCPU_TSC_OFFSET`. So the guest's clock and our `vtsc_now()`
are *literally the same clock*, sharing one offset. There is no second source of
truth. (This equality is asserted in the code.)

Now watch what falls out:

- **Fast-forward = add to the offset.** Bump `tsc_offset` by Δ and the guest's
  entire sense of time — `rdtsc`, its monotonic clock, its wall clock, its next
  timer deadline — jumps forward by Δ, atomically, because *all of them are
  computed from that one offset*. Move the offset and you move time itself.

The offset is **cached** in the VMM (a shared cell every clock-reader sees), so
the hot loop never has to ask KVM for it. When we jump, we write KVM's offset
*and* the cache, in that order. The offset only ever increases (monotonic), and
is only ever written while the guest is parked at a HLT exit, on the vCPU thread,
between `KVM_RUN`s — never while the guest is running.

---

## 5. The architecture — the pieces and how they fit

```
                         tdvmm process (one vCPU thread)
  ┌───────────────────────────────────────────────────────────────────────┐
  │                                                                         │
  │   vCPU run loop  (main.rs):   service_timers();  KVM_RUN;  handle_exit  │
  │        │                                   │                            │
  │        │ on HLT (idle)                     │ on MMIO/PIO exit           │
  │        ▼                                   ▼                            │
  │   ┌─────────┐   asks "next event?"   ┌──────────────────────────────┐   │
  │   │ park.rs │ ────────────────────▶  │ device emulation:            │   │
  │   │ wait or │                        │  lapic.rs  (timer + IRQs)     │   │
  │   │  JUMP   │ ◀──── fire due ──────  │  ioapic.rs (IRQ routing)      │   │
  │   └────┬────┘                        │  pic.rs    (masked legacy)    │   │
  │        │ bump offset                 │  pit.rs    (legacy timer stub)│   │
  │        ▼                             │  serial.rs (console)          │   │
  │   ┌─────────┐                        └──────────────┬───────────────┘   │
  │   │ vtsc.rs │◀── every timer is a ──┐               │ raise IRQ         │
  │   │ THE     │    (vtsc, event)      │        ┌──────▼───────┐           │
  │   │ clock   │                  ┌────┴─────┐  │ inject into  │           │
  │   └─────────┘                  │ events.rs│  │ the guest    │           │
  │                                │ ONE queue│  │ (KVM_INTERRUPT)          │
  │                                └──────────┘  └──────────────┘           │
  │                                                                         │
  │   guest RAM (memory.rs)   ◀── KVM maps our mmap as the guest's physical │
  │                               memory; CPUID filter (cpuid.rs) shapes    │
  │                               what CPU the guest thinks it has          │
  └───────────────────────────────────────────────────────────────────────┘
```

Three ideas hold this together:

1. **One clock authority** (`vtsc.rs`). Nothing else is allowed to define "now."
2. **One event queue** (`events.rs`). *Every* future thing that should happen —
   a timer deadline, a re-armed periodic tick — is a `(vtsc, event)` entry in a
   single ordered queue. "What happens next" is always `queue.peek()`.
3. **One place where time becomes a wait-or-jump** (`park.rs`). When the guest
   is idle, exactly one function decides whether to *wait* real time for the next
   event (normal mode) or *jump* the clock to it (fast-forward mode). That single
   seam is the entire difference between a normal VM and a time-warping one.

### File map

| File | Role |
|---|---|
| `main.rs` | the vCPU run loop, exit dispatch, orchestration |
| `vtsc.rs` | **the clock**: `vtsc_now() = host TSC + cached offset`; the jump (`bump_offset`); cycles↔ns math |
| `events.rs` | **the one event queue**, ordered by vtsc |
| `park.rs` | idle handling: **wait** (normal) or **jump** (fast-forward) to the next event |
| `lapic.rs` | our userspace **Local APIC**: the per-CPU interrupt controller *and the timer*, driven off vtsc |
| `ioapic.rs` | our userspace **I/O APIC**: routes device IRQs (e.g. serial) to the LAPIC |
| `pic.rs` | a **masked** legacy 8259 interrupt controller (present so early boot is happy; delivers nothing) |
| `pit.rs` | a legacy 8254 **timer counter** stub — readable, drives calibration, generates no interrupts |
| `serial.rs` | the 16550 serial port = the console |
| `cpuid.rs` | the **CPUID filter**: shapes what CPU features the guest sees (see §9) |
| `mptable.rs` | the **MP table**: tells the guest what CPU/APICs exist, without ACPI |
| `boot.rs`, `regs.rs`, `memory.rs`, `arch.rs`, `msrs.rs` | boot-time setup: load the kernel, build boot data, set registers/page tables, map RAM |

---

## 6. How a guest comes alive — the boot path

A real PC is booted by firmware (BIOS/UEFI). We have no firmware; we set the
machine up by hand and jump straight into the Linux kernel (`boot.rs`,
`regs.rs`, `mptable.rs`):

1. **Load the kernel.** We read an uncompressed Linux kernel image (`vmlinux`, an
   ELF file) into guest RAM and set the vCPU's registers to Linux's expected
   64-bit entry state (long mode on, a flat GDT, initial page tables).
2. **Hand it boot data.** Linux expects a "zero page" (`boot_params`) describing
   the machine: the command line, and an **E820 map** (the classic list of which
   physical address ranges are usable RAM). We build these by hand.
3. **Tell it what CPUs exist.** With no ACPI (the modern hardware-description
   standard, which is heavy to emulate), the guest discovers its processor and
   interrupt controllers via an **MP table** — an older, lightweight table we
   place in low memory. (This table turned out to be the source of the project's
   nastiest bug — see §10.)
4. **Give it a root filesystem.** The guest's entire OS — Alpine Linux plus a
   container runtime — is packed into an **initramfs** (a compressed archive that
   the kernel unpacks into RAM). There is **no disk**. Everything lives in memory.
   The guest then `switch_root`s onto a `tmpfs` (a RAM filesystem) because
   container runtimes need a "real" mount to pivot into.
5. **Run the workload.** `init` sets a fixed date (so every boot starts at the
   same instant), starts PostgreSQL and the service container on a private bridge
   network, and the service begins its insert-a-row-every-hour loop.

This is a **closed world**: no network uplink, no disk, no outside input. Every
container image is baked in ahead of time and pinned by cryptographic digest.
That closedness is not incidental — it's what makes fast-forward safe (§8) and
the system simple to reason about (§11).

---

## 7. Owning the clock — the hard middle of the project

Here is the crux. To fast-forward, we must control the timer that wakes the
guest. But **by default that timer lives inside KVM, in the kernel, and runs on
the host clock** — we can't jump it.

Modern CPUs have a per-core **Local APIC** (Advanced Programmable Interrupt
Controller). It does two jobs: it receives interrupts, and it contains **the
timer** that fires the OS's scheduler tick and its `sleep()` deadlines. KVM can
emulate the APIC *in the kernel* for speed (the "in-kernel irqchip") — and that's
the normal, fast, well-trodden path. But an in-kernel APIC timer is armed against
host time. If we jump our virtual clock, that timer keeps firing on the real
schedule. Useless for us.

So we do the unusual thing: **we pull the APIC out of the kernel and emulate it
ourselves, in userspace** (`lapic.rs`, plus `ioapic.rs` for routing and a masked
`pic.rs` for legacy compatibility). The APIC's registers live at fixed physical
addresses (`0xFEE0_0000` for the LAPIC, `0xFEC0_0000` for the I/O APIC). With no
in-kernel APIC, every time the guest touches those addresses it's an **MMIO
exit** — and we handle it. Now:

- When the guest programs "wake me in N units," we don't arm a host timer. We
  compute the deadline **in vtsc** and drop a `(vtsc, event)` entry into the
  event queue.
- When that deadline arrives (in virtual time), *we* raise the timer interrupt
  and **inject** it into the guest (via `KVM_INTERRUPT`, following the standard
  "is the guest ready for an interrupt?" handshake).

The timer is now *ours*, expressed entirely in a clock we control. That is the
whole point of Step 3 in the build log, and it was by far the hardest part
(see §10 for why).

### The park

When the guest has nothing to do it executes `hlt`, which exits to us. We land in
`park.rs`. It asks the event queue for the next deadline and:

- **Normal mode (`--ff off`):** compute how long until that deadline in *real*
  nanoseconds, and sleep that long using a `timerfd` + `ppoll` (also watching the
  console for keystrokes). Classic VM behavior: idle costs real time, ~0% CPU.
- **Fast-forward mode (`--ff on`, the default):** don't sleep at all — jump
  (§8).

---

## 8. Fast-forwarding — the payoff

The entire feature is a change to that one `park.rs` decision. When the guest is
HLTed and the only thing that will wake it is a future timer:

```
Δ = next_event_vtsc − vtsc_now()   // how far to the next scheduled event
tsc_offset += Δ                    // JUMP: advance the guest's whole clock
fire all events now due            // deliver the timer interrupt(s)
// guest wakes, does a little work, HLTs again → repeat
```

Because *every* guest-visible clock is a pure function of the one offset,
`tsc_offset += Δ` moves the guest's `rdtsc`, its `CLOCK_MONOTONIC` (uptime), its
`CLOCK_REALTIME` (wall clock), and its next timer deadline **all at once,
consistently**. The guest wakes up and every clock agrees that Δ elapsed. It has
no way to tell the difference between "an hour passed" and "an hour was skipped."

Why is this *safe* to do — why won't the guest notice time was faked? Two
reasons, both from the closed-world design:

1. **No external input.** Nothing outside can send the guest a packet or a
   keystroke that "should have" arrived during the skipped hour, because there is
   no network uplink and the console is operator-only.
2. **No in-flight host work.** In a closed, RAM-only world there is no disk or
   network request that could complete "in the skipped past." The optional
   `--allow-egress` network channel is the *first* thing that can put real host
   work in flight, and it is exactly where this rule is now enforced (below).

**The phase gate (`--allow-egress`).** When you open the network, the guest can
have a TCP connection the host is mediating on its behalf — real work outstanding
in real time. Skipping the clock forward then would let the guest see a response
"arrive in the past," which is precisely the hazard reason 2 forbids. So egress
adds a **phase gate**: the host proxy owns the connection table, and a jump is
allowed *only* while that table is empty (no open session, nothing mid-resolve, no
bytes in flight). While anything is open, `park.rs` falls back to waiting at real
rate — virtual time advances 1:1, exactly like `--ff off` — and resumes skipping
the instant the connection drains. An always-on assertion sits immediately before
every `tsc_offset += Δ`: if a jump were ever about to skip real time with external
state open, the run aborts rather than fake a result. This is the general shape of
the rule §12 describes for future host I/O (a disk), made concrete for the network
channel: **never jump the clock while a host operation the guest is waiting on is
still outstanding.**

**Measured result:** the workload set to insert a row every 3600 virtual seconds
runs at **~950× real time** — 24 virtual hours in ~90 seconds of wall clock —
with each jump costing ~0.3 microseconds and the guest's row timestamps landing
exactly one hour apart. Because PostgreSQL's own background threads wake every
couple hundred milliseconds, the guest is never idle for long virtual stretches,
so the largest single jump is ~0.1 s — far below the `--max-jump-secs 300` safety
bound that aborts the run if any deadline ever looked implausibly far away.

---

## 9. Techniques catalog

The reusable tricks that make the above work:

- **TSC offsetting.** The one lever for the whole clock. VT-x adds a per-VM
  offset to the host cycle counter before the guest sees it; we own that offset.
- **CPUID filtering** (`cpuid.rs`). `CPUID` is how software asks the CPU "what
  are you and what can you do." `CPUID` always exits, so we get to *answer for a
  virtual CPU*. We use this to remove things that would sabotage the clock:
  - **Hide KVM's paravirtual clock.** KVM normally offers the guest a "kvmclock"
    that it periodically re-syncs to *host* time. If the guest used it, our jumps
    would be silently undone. We mask the entire hypervisor-leaf range so the
    guest never sees it and falls back to plain `rdtsc`.
  - **Hide MWAIT.** `MWAIT` is an alternate idle instruction that would *not*
    exit to us. Masking it forces idle to use `HLT`, which does exit — our hook.
  - **Advertise an invariant TSC** and set `tsc=reliable` on the cmdline, so
    Linux trusts the counter and skips the "is my clocksource drifting?" watchdog
    (which would panic when it saw the TSC "jump").
  - **Pass through the frequency leaves (`0x15`/`0x16`)** so the guest learns the
    true crystal frequency directly (this matters for the timer — see §10).
- **Userspace interrupt controller via MMIO exits** (§7). The mechanism for
  owning the timer.
- **One event queue as the sole time authority** (`events.rs`). No timer state
  exists anywhere else, so "what's next" is always a single, cheap query — and
  the jump target is unambiguous.
- **`timerfd` + `ppoll` park.** The precise, low-overhead way to sleep until the
  next deadline *or* a keystroke, in normal mode. Fast-forward simply replaces
  this sleep with the offset bump.
- **Integer-only time math.** Cycle↔nanosecond and APIC-count↔TSC conversions
  use exact integer ratios (e.g. `count × EBX/EAX` from CPUID `0x15`, in 128-bit
  integers) — **no floating point anywhere** in the time path. Floats round
  differently across compilers and hosts; exact integer ratios keep the timer
  math consistent run-to-run.
- **Closed-world image baking.** Container images are pulled and pinned by
  digest at *build* time and baked into the guest; nothing is fetched at runtime.
  (See the squash technique in §10.)
- **Pinned, hashed artifacts.** The kernel, the initramfs, and the effective
  guest CPUID profile are all hashed into a manifest, so any host/CPU change that
  would alter the guest surfaces as a *detected deviation*, not a silent one.

---

## 10. Challenges & war stories

The honest part. Most of the effort went into problems that were invisible until
something downstream broke.

### 10.1 The platform pivot: bhyve doesn't run on Linux

The project started aimed at forking **bhyve**, FreeBSD's hypervisor. But bhyve's
core is a FreeBSD *kernel module*; it does not run on Linux. Since the target was
a Linux machine, the right base was **KVM** with a small custom userspace VMM
(this project), which is native to Linux and — conveniently — put almost all the
interesting code in userspace where it's easy to iterate.

### 10.2 Building a 2022 kernel with a 2026 compiler

The pinned guest kernel (Linux 6.1) wouldn't compile: modern GCC defaults to the
C23 language standard, in which `bool`/`true`/`false` became reserved keywords,
and the old kernel's boot stub defines them the old way. Fix: build the kernel
with `-std=gnu11`. A one-line change, but a total blocker until found.

### 10.3 The storage explosion: 290 MiB vs 1.8 GiB

The guest runs containers from a RAM filesystem using the `vfs` storage driver,
which stores **every image layer as a full cumulative copy**. Fine for a tiny
busybox image; catastrophic for PostgreSQL, whose as-pulled layers balloon to
**1.8 GiB** — too big to even boot into RAM. Fix: repackage the digest-pinned
Postgres image into a **single flattened layer** (~290 MiB) via a pure
`FROM <digest>` rebuild, with a build-time gate asserting the flattened image's
config is byte-identical to the original so the repackage can't smuggle in a
change.

### 10.4 The three-layer timer bug (the big one)

This one took three separate discoveries, each hidden behind the last, and it's
the most instructive story in the project.

We tried to remove the old legacy timer (the **PIT**, an 8254 chip from the
original PC) so our own APIC timer would take over. The guest immediately hung —
containers wouldn't start, PostgreSQL never came up. Peeling it back:

- **Layer 1:** the guest wasn't using the modern **APIC timer** at all. It was
  ticking off the ancient **PIT** (IRQ0). Remove the PIT and the guest has *no
  clock*, so every `sleep`/timeout hangs forever. (`/proc/timer_list` showed
  `Clock Event Device: pit`; the local-APIC-timer interrupt count was zero.)
- **Layer 2:** *why* was it on the PIT? Because the guest had booted into APIC
  "**virtual wire mode**" — a fallback the kernel uses when it can't find a
  description of the interrupt controllers. Our **MP table** (§6) was never being
  consumed.
- **Layer 3:** *why* was the MP table ignored? Because the pinned kernel had been
  compiled with `CONFIG_X86_MPPARSE` **turned off** — the MP-table *parser* was
  not in the kernel at all. A perfect table would change nothing, because nothing
  could read it. This had been silently true since the very first boot; it only
  ever "worked" because the in-kernel legacy timer happened to cover for it.

The fix touched all three layers: rebuild the kernel with `CONFIG_X86_MPPARSE=y`,
fix the MP table's interrupt routing (ISA IRQ0 → I/O APIC pin 2, serial IRQ4 →
pin 4), and only *then* switch the guest onto our userspace APIC timer. The
lesson: an un-exercised code path can be broken for a long time and only reveal
it when you finally lean on it — so verify each layer, don't assume it.

### 10.5 The KVM limitation: the modern timer mode can't be intercepted

The natural design was to use the LAPIC's modern **TSC-deadline** timer mode
(program a deadline directly in TSC units) and route its control register to our
userspace APIC. On this host's KVM it's **impossible**: KVM's fast path for that
specific register write handles it *before* the userspace hook and — unlike the
neighboring case in the same function — omits the check for "is the APIC in the
kernel?" So without an in-kernel APIC the write is silently dropped and never
reaches us. (This is a genuine KVM quirk; a one-line kernel patch would fix it,
and it's recorded as future work — but Phase 1 is a zero-kernel-change effort.)

The workaround is arguably *better* for the end goal: use the LAPIC's older
**one-shot/periodic** timer mode instead, which is programmed via ordinary MMIO
that always exits to us. So *every* timer operation now flows through our code —
no reliance on any fragile kernel fast path. One subtlety: with the modern mode
gone, Linux *calibrates* the timer by reading the crystal frequency straight from
CPUID `0x15` (which we pass through) rather than measuring — so our timer must
count at *exactly that* crystal frequency (38.4 MHz on this host), or every
`sleep` lands at the wrong real time. It does, by construction, as a pure integer
function of vtsc — so calibration is consistent and identical on every run.

### 10.6 The clocksource landmine (defused early)

A related trap, handled up front so it never bit: if Linux picks a clocksource
that *doesn't* follow our offset (kvmclock, or the HPET timer, or the PIT
counter used as a clocksource), then jumping the TSC makes the guest's clocks
*disagree*, and Linux either panics or silently reverts to a clock we don't
control. The CPUID filter + `tsc=reliable` + a PIT counter that is itself a
function of vtsc together ensure the guest only ever trusts clocks that ride our
single offset.

---

## 11. Design discipline — the choices that made it tractable

A few deliberate constraints keep the whole thing simple enough to reason about,
and keep the clock jumps trustworthy:

- **Single vCPU.** With one CPU, there is no shared-memory race between cores —
  the single hardest thing to reason about in a multiprocessor VM simply doesn't
  exist. The guest's behavior follows from its inputs and its interrupt timing.
- **Closed world.** No network uplink, no disk, fixed start time. Every source of
  "the outside world" — the thing that could invalidate a clock jump — is
  removed. Inputs that *do* need to exist later (fault injection, real
  time-of-day) are modeled as *scheduled events on the queue*, not ambient I/O.
- **The single-writer invariant.** *All* effects on guest state — memory,
  interrupt raises, device registers, the TSC offset — happen on the vCPU thread,
  at loop boundaries. Host I/O may happen on other threads, but its *effects*
  land only at controlled points, so the guest never observes a half-applied
  change.
- **Chokepoints, not sprawl.** One clock (`vtsc.rs`), one event queue
  (`events.rs`), one wait-or-jump seam (`park.rs`). The whole time behavior lives
  at three small, auditable places — not scattered across the codebase.
- **Pinned + hashed everything.** Kernel, initramfs, CPUID profile: all hashed,
  so the guest is reproducible and drift is detectable.

None of these were strictly needed to make fast-forward *work* — they were chosen
so the system stays simple to reason about and the clock jumps stay trustworthy.

---

## 12. What's done, what's next

**Done:** a from-scratch KVM hypervisor that boots a Linux guest, runs a real
closed-world PostgreSQL + service workload on a container runtime, owns the
guest's clock through a userspace interrupt controller, and fast-forwards idle
time (~950× on the demo).

**How virtual time advances.** Fast-forward skips *idle* time: when the guest
halts, the clock jumps straight to the next armed event. While the guest is
actually running, virtual time advances with real host cycles at 1:1
(`vtsc = host_tsc + offset`) — execution runs at real wall-clock rate. tdvmm
compresses the gaps between work, not the work itself.

Two mechanisms couple execution to real host time. Driving virtual time from
*work done* instead would take a different machine — different costs, different
hardware:

- **The guest reads the TSC directly.** `rdtsc` doesn't exit, so during active
  execution the guest's clock is `host_TSC + offset` — real host cycles. Driving
  it from work done would mean trapping `rdtsc` (a VT-x feature) and feeding it a
  value derived from an instruction/branch count, which costs performance and
  needs the small KVM patch noted in §10.5.
- **Work is counted at event granularity, not per instruction.** Landing an
  interrupt at an exact instruction would need a hardware performance counter
  (retired branches) plus single-stepping — the technique the `rr` debugger uses.
  That needs a **real hardware PMU**, which is unreliable under nested
  virtualization, so it wants bare-metal or a dedicated box.

**Adjacent future work:**

- **virtio-blk** (a real disk device). The rule fast-forward must enforce here —
  **never jump the clock while a host operation the guest is waiting on is still
  outstanding**, or the guest would see a completion "in the past" — is now live
  for the network: the optional `--allow-egress` channel enforces it with a phase
  gate (§8), the first real host work fast-forward has had to fence against. A disk
  device would reuse the same discipline: gate the jump on outstanding I/O. In the
  default RAM-only, no-egress world there is still no host I/O, so with the flag off
  the rule has nothing to fence against and the closed-world path is unchanged.
- **Snapshot / replay**, and eventually **multiple cooperating VMs**.

---

## Glossary

- **VMM / hypervisor** — the program that runs a virtual machine. This project.
- **KVM** — the Linux kernel module that drives the CPU's virtualization
  hardware; we call it via `ioctl` on `/dev/kvm`.
- **vCPU** — a virtual CPU; our handle for running guest instructions.
- **VM exit** — the hardware handing control from guest back to us, because the
  guest did something that needs emulation.
- **`KVM_RUN`** — the ioctl that runs the vCPU until the next exit.
- **guest / host** — the virtual machine / the real machine it runs on.
- **TSC** (Time-Stamp Counter) — the CPU's cycle counter, read by `rdtsc`. The
  guest's clock foundation.
- **TSC offset** — a hardware value added to the host TSC before the guest reads
  it; our single lever for controlling guest time.
- **`vtsc`** — this project's virtual time = `host TSC + offset`.
- **HLT** — the "sleep until interrupt" instruction; an idle OS runs it, and it
  exits to us — the hook fast-forward uses.
- **MMIO / PIO** — memory-mapped / port I/O: two ways a guest touches "device
  registers," both of which exit to us so we can emulate the device.
- **(Local) APIC** — the per-CPU interrupt controller; contains the timer that
  drives the OS. We emulate it in userspace so we own that timer.
- **I/O APIC** — routes device interrupt lines (like the serial port's) to the
  Local APIC.
- **PIC (8259) / PIT (8254)** — the *legacy* interrupt controller and timer from
  the original PC. We keep a masked PIC and a non-interrupting PIT stub for
  compatibility; the APIC does the real work.
- **CPUID** — the instruction software uses to ask the CPU what it is; always
  exits, so we answer for a virtual CPU (`cpuid.rs`).
- **MP table** — a lightweight, pre-ACPI table describing the CPU and interrupt
  controllers, which we place in guest memory so Linux can discover its hardware.
- **E820** — the classic list of physical memory ranges handed to the kernel.
- **initramfs** — a compressed archive the kernel unpacks into RAM as the root
  filesystem; here it holds the entire guest OS (no disk).
- **NO_HZ / clockevent** — Linux's "tickless idle": instead of a fixed periodic
  tick, it programs the timer for exactly the next thing it needs, which is what
  makes idle jumps land on meaningful deadlines rather than every millisecond.
- **closed world** — no network, no disk, fixed start time; the property that
  makes fast-forward safe.

---

## Try it

```sh
cargo build --release

# Watch the fast-forward: >= 24 virtual hours of the hourly-insert workload,
# in seconds-to-minutes of real time, with acceptance-gate assertions.
scripts/ff_demo.sh 24

# The same workload in normal real-time mode (fast-forward off):
FF=off scripts/smoke_test_workload.sh 240

# An interactive shell in the guest. run.sh is the human entry point, so it
# boots with fast-forward OFF (real time). The BINARY default stays FF-on (an
# explicit, documented choice — core time semantics must not vary with the
# environment), but fast-forward at a console races the guest clock and pins a
# host core, so run.sh is the one place that picks real time (it passes
# `--ff off`). On startup the VMM always prints a one-line mode statement — the
# FF state and how it was chosen, e.g. `[tdvmm] fast-forward: OFF (--ff off)` —
# and, at a tty, a quit hint. Leave the guest with `poweroff` or `reboot`;
# `exit` just gives a fresh prompt (the shell is respawned by init). Override
# the FF mode by passing `--ff on` through (`./run.sh --ff on`).
./run.sh
```

See `README.md` for the full build-and-run details and the per-step history.
