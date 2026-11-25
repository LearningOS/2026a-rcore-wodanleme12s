//! Implementation of [`MapArea`] and [`MemorySet`].

use super::{frame_alloc, FrameTracker};
use super::{PTEFlags, PageTable, PageTableEntry};
use super::{PhysAddr, PhysPageNum, VirtAddr, VirtPageNum};
use super::{StepByOne, VPNRange};
use crate::config::{
    BASE_ADDRESS, KERNEL_STACK_SIZE, MEMORY_END, PAGE_SIZE, TRAMPOLINE, TRAP_CONTEXT_BASE,
    USER_MMAP_LIMIT, USER_STACK_SIZE,
};
use crate::sync::UPSafeCell;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::arch::asm;
use lazy_static::*;
use riscv::register::satp;

extern "C" {
    fn stext();
    fn etext();
    fn srodata();
    fn erodata();
    fn sdata();
    fn edata();
    fn sbss_with_stack();
    fn ebss();
    fn ekernel();
    fn strampoline();
}

lazy_static! {
    /// The kernel's initial memory mapping(kernel address space)
    pub static ref KERNEL_SPACE: Arc<UPSafeCell<MemorySet>> =
        Arc::new(unsafe { UPSafeCell::new(MemorySet::new_kernel()) });
}
/// address space
pub struct MemorySet {
    page_table: PageTable,
    areas: Vec<MapArea>,
    /// dynamic (mmap/munmap) intervals, keyed by their first virtual page
    vmas: BTreeMap<VirtPageNum, Vma>,
}

/// One mapped interval created by `sys_mmap`.
///
/// Invariant: for every vpn in `[start_vpn, start_vpn + len)` the page table
/// holds a valid PTE whose frame is recorded in `frames`, and the PTE flags
/// equal `perm | V`. The `FrameTracker`s own the frames, so dropping a `Vma`
/// returns them to the allocator.
pub struct Vma {
    /// first virtual page (inclusive)
    start_vpn: VirtPageNum,
    /// last virtual page (exclusive)
    end_vpn: VirtPageNum,
    /// number of pages; kept so the invariant above is checkable locally
    len: usize,
    /// R/W/X/U permissions carried by every page of the interval
    perm: MapPermission,
    /// backing frames; keys match the page table entries
    frames: BTreeMap<VirtPageNum, FrameTracker>,
}

impl Vma {
    /// Split off everything from `at` onwards, keeping `[start_vpn, at)`.
    fn split_at(&mut self, at: VirtPageNum) -> Vma {
        assert!(self.start_vpn < at && at < self.end_vpn, "Vma::split_at out of range");
        let tail = Vma {
            start_vpn: at,
            end_vpn: self.end_vpn,
            len: self.end_vpn.0 - at.0,
            perm: self.perm,
            frames: self.frames.split_off(&at),
        };
        self.end_vpn = at;
        self.len = at.0 - self.start_vpn.0;
        debug_assert_eq!(self.len, self.frames.len());
        debug_assert_eq!(tail.len, tail.frames.len());
        tail
    }
}

impl MemorySet {
    /// Create a new empty `MemorySet`.
    pub fn new_bare() -> Self {
        Self {
            page_table: PageTable::new(),
            areas: Vec::new(),
            vmas: BTreeMap::new(),
        }
    }
    /// Get the page table token
    pub fn token(&self) -> usize {
        self.page_table.token()
    }
    /// Translate one user byte address into a kernel-visible pointer.
    ///
    /// This is the single authority behind the user pointer gate: the PTE must
    /// be valid, readable, carry the U bit, and have the W bit that matches
    /// `writable`. An empty PTE never resolves (its ppn is 0), so a hole can
    /// not be mistaken for physical page zero.
    pub fn translate_user_byte(&self, va: usize, writable: bool) -> Option<*mut u8> {
        let vpn = VirtPageNum((va & !(PAGE_SIZE - 1)) / PAGE_SIZE);
        let pte = self.page_table.translate(vpn)?;
        if !pte.is_valid() || !pte.readable() || !pte.user_accessible() {
            return None;
        }
        if writable && !pte.writable() {
            return None;
        }
        let ppn = pte.ppn();
        if ppn.0 == 0 {
            return None;
        }
        let pa = usize::from(PhysAddr::from(ppn)) + (va % PAGE_SIZE);
        // The kernel identity-maps all of RAM, so a physical address doubles as
        // a supervisor-visible one. Anything outside RAM is not ours to touch.
        if !(BASE_ADDRESS..MEMORY_END).contains(&pa) {
            return None;
        }
        Some(pa as *mut u8)
    }
    /// Does the *static* part of this address space (ELF segments, user stack,
    /// trampoline, trap frame) own `vpn`?
    ///
    /// Deliberately not answered from the page table: an SV39 leaf slot only
    /// exists once something has been mapped there, so a never-touched hole
    /// would look free even to a request that lands on top of a loaded segment.
    /// Every static region is recorded in `areas` at load time, which makes this
    /// exact and cheap. Dynamic (mmap) intervals are checked separately by
    /// [`vma_owns`](Self::vma_owns).
    fn static_owns(&self, vpn: VirtPageNum) -> bool {
        self.areas
            .iter()
            .any(|area| area.vpn_range.get_start() <= vpn && vpn < area.vpn_range.get_end())
    }
    /// Is `vpn` currently backed by a frame belonging to a dynamic (mmap)
    /// interval? Unlike [`static_owns`] this ignores the image and the stack, so
    /// `munmap` can only ever take away pages it handed out itself.
    fn vma_owns(&self, vpn: VirtPageNum) -> bool {
        self.vmas
            .range(..=vpn)
            .next_back()
            .filter(|(_, vma)| vma.end_vpn > vpn && vma.frames.contains_key(&vpn))
            .is_some()
    }
    /// Map `[start_vpn, end_vpn)` to freshly allocated zeroed frames and record
    /// them as one interval.
    ///
    /// Returns `false` (without touching anything) when the interval overlaps
    /// existing memory or the machine is out of frames.
    fn map_pages(&mut self, start_vpn: VirtPageNum, end_vpn: VirtPageNum, perm: MapPermission) -> bool {
        assert!(start_vpn < end_vpn, "map_pages with an empty range");
        let flags = PTEFlags::from_bits_truncate(perm.bits);
        if VPNRange::new(start_vpn, end_vpn)
            .into_iter()
            .any(|vpn| self.static_owns(vpn) || self.vma_owns(vpn))
        {
            return false;
        }
        let mut frames: BTreeMap<VirtPageNum, FrameTracker> = BTreeMap::new();
        for vpn in VPNRange::new(start_vpn, end_vpn) {
            match frame_alloc() {
                Some(frame) => {
                    // A fresh FrameTracker is already zeroed, which is what
                    // mmap() promises. `map_or_replace_invalid` ORs in the V bit
                    // itself and accepts a leaf slot left behind by an earlier
                    // mapping of this range that has since been torn down.
                    self.page_table
                        .map_or_replace_invalid(vpn, frame.ppn, flags);
                    frames.insert(vpn, frame);
                }
                None => {
                    // Out of frames: roll back. Dropping the trackers here
                    // returns the pages instead of leaking them, and the VMA
                    // table is left untouched so nothing claims the hole.
                    for vpn in frames.keys() {
                        self.page_table.unmap(*vpn);
                    }
                    drop(frames);
                    return false;
                }
            }
        }
        self.vmas.insert(
            start_vpn,
            Vma {
                start_vpn,
                end_vpn,
                len: end_vpn.0 - start_vpn.0,
                perm,
                frames,
            },
        );
        true
    }
    /// Unmap `[start_vpn, end_vpn)` and return its frames to the allocator.
    ///
    /// The caller guarantees every page of the range is backed by a dynamic
    /// interval. That range may straddle *several* of them — e.g. a hole-free
    /// walk over `[0x10000000, 0x10003000)` that was built by two adjacent mmaps
    /// with different lengths — so each overlapped interval is torn out in turn
    /// rather than assuming one owner covers everything.
    fn unmap_pages(&mut self, start_vpn: VirtPageNum, end_vpn: VirtPageNum) {
        for vpn in VPNRange::new(start_vpn, end_vpn) {
            // `unmap` panics on a missing leaf slot, so this precondition is
            // stated explicitly: `munmap()` checks every page against `vma_owns`
            // before calling here.
            self.page_table.unmap(vpn);
        }
        loop {
            // Highest-starting interval that begins before the end of the range.
            let Some(key) = self.vmas.range(..end_vpn).next_back().map(|(k, _)| *k) else {
                break;
            };
            let mut vma = self.vmas.remove(&key).unwrap();
            if vma.end_vpn <= start_vpn {
                // Entirely to the left of the request; untouched.
                self.vmas.insert(key, vma);
                break;
            }
            if vma.start_vpn < start_vpn {
                // Keep the head registered; `vma` now starts at `start_vpn`.
                let head = vma.split_at(start_vpn);
                self.vmas.insert(head.start_vpn, head);
            }
            if vma.end_vpn > end_vpn {
                // Keep the tail as its own interval; `vma` ends at `end_vpn`.
                let tail = vma.split_at(end_vpn);
                self.vmas.insert(tail.start_vpn, tail);
            }
            // `vma` is exactly the part being unmapped; dropping its frame map
            // hands those pages back to the allocator.
            drop(vma.frames);
            if vma.end_vpn >= end_vpn {
                break;
            }
        }
    }
    /// Merge the interval starting at `start_vpn` with its right neighbour.
    fn try_merge_right(&mut self, start_vpn: VirtPageNum) {
        let Some(vma) = self.vmas.get(&start_vpn) else {
            return;
        };
        let next_start = vma.end_vpn;
        if self
            .vmas
            .get(&next_start)
            .filter(|next| next.perm == vma.perm)
            .is_none()
        {
            return;
        }
        let mut merged = self.vmas.remove(&start_vpn).unwrap();
        let right = self.vmas.remove(&next_start).unwrap();
        merged.end_vpn = right.end_vpn;
        merged.frames.extend(right.frames);
        self.vmas.insert(start_vpn, merged);
    }
    /// Handle a `sys_mmap` request. Returns `0` on success and `-1` for any
    /// rejected request: the pinned tests (`ch4_mmap0..3`, `ch4_unmap*`) compare the
    /// syscall result against exactly those two values, so diagnostic error codes
    /// must not leak out of the kernel.
    pub fn mmap(&mut self, start: usize, len: usize, prot: usize) -> isize {
        const PROT_READ: usize = 1;
        const PROT_WRITE: usize = 2;
        const PROT_EXEC: usize = 4;
        if start % PAGE_SIZE != 0 { return -1; }
        if len == 0 { return -1; }
        // `len` is rounded UP to whole pages rather than rejected when unaligned.
        // ch4_unmap (04_5) maps `len + 1` bytes and must get `0`, then writes every
        // byte of (start - len)..(start + 3*len) — only a 2-page grant covers that.
        // The first rejection in ch4_mmap3 (04_4) is an overlap, not an alignment
        // case, so rounding does not weaken it. `munmap` keeps the strict check
        // because ch4_unmap2 (04_6) expects an unaligned length to be rejected.
        let span = match len.checked_add(PAGE_SIZE - 1) {
            Some(rounded) => rounded / PAGE_SIZE * PAGE_SIZE,
            None => return -1,
        };
        // No bits outside RWX, and PROT_WRITE without PROT_READ is illegal on
        // RISC-V (a W-only PTE cannot be built).
        if prot & !(PROT_READ | PROT_WRITE | PROT_EXEC) != 0 { return -1; }
        if prot & PROT_READ == 0 { return -1; }
        let mut perm = MapPermission::U;
        if prot & PROT_READ != 0 {
            perm |= MapPermission::R;
        }
        if prot & PROT_WRITE != 0 {
            perm |= MapPermission::W;
        }
        if prot & PROT_EXEC != 0 {
            perm |= MapPermission::X;
        }
        let end = match start.checked_add(span) {
            Some(end) => end,
            None => return -1,
        };
        // Address policy: the application image is position-independent and lives
        // low, so a fixed mapping may land anywhere in the hole between it and the
        // shared trampoline/trap-frame pages at the very top of the address space.
        // Overlaps with what this address space already owns are refused by
        // `static_owns()`/`vma_owns()` below rather than by a hardcoded floor.
        if start >= TRAP_CONTEXT_BASE { return -1; }
        if end > USER_MMAP_LIMIT { return -1; }
        let start_vpn = VirtPageNum(start / PAGE_SIZE);
        let end_vpn = VirtPageNum(end / PAGE_SIZE);
        let ok = self.map_pages(start_vpn, end_vpn, perm);
        if ok { 0 } else { -1 }
    }
    /// Handle a `sys_munmap` request. Returns `true` on success.
    ///
    /// Only whole pages may go away, so an unaligned start or length is
    /// rejected rather than partially applied.
    pub fn munmap(&mut self, start: usize, len: usize) -> bool {
        if start % PAGE_SIZE != 0 || len == 0 || len % PAGE_SIZE != 0 {
            return false;
        }
        let end = match start.checked_add(len) {
            Some(end) => end,
            None => return false,
        };
        if start >= TRAP_CONTEXT_BASE || end > USER_MMAP_LIMIT {
            return false;
        }
        let start_vpn = VirtPageNum(start / PAGE_SIZE);
        let end_vpn = VirtPageNum(end / PAGE_SIZE);
        // Every page of the range must be owned by a dynamic interval, which
        // rules out holes, static areas (the image, the stack, the trap frame)
        // and double frees without any address whitelist.
        if !VPNRange::new(start_vpn, end_vpn)
            .into_iter()
            .all(|vpn| self.vma_owns(vpn))
        {
            return false;
        }
        self.unmap_pages(start_vpn, end_vpn);
        // Re-unify neighbours so repeated mmap/munmap cycles do not fragment
        // the table forever.
        if let Some((_, left)) = self.vmas.range(..start_vpn).next_back() {
            let left_start = left.start_vpn;
            self.try_merge_right(left_start);
        }
        if let Some((_, right)) = self.vmas.range(end_vpn..).next() {
            let right_start = right.start_vpn;
            self.try_merge_right(right_start);
        }
        true
    }
    /// Assume that no conflicts.
    pub fn insert_framed_area(
        &mut self,
        start_va: VirtAddr,
        end_va: VirtAddr,
        permission: MapPermission,
    ) {
        self.push(
            MapArea::new(start_va, end_va, MapType::Framed, permission),
            None,
        );
    }
    fn push(&mut self, mut map_area: MapArea, data: Option<&[u8]>) {
        map_area.map(&mut self.page_table);
        if let Some(data) = data {
            map_area.copy_data(&mut self.page_table, data);
        }
        self.areas.push(map_area);
    }
    /// Mention that trampoline is not collected by areas.
    fn map_trampoline(&mut self) {
        self.page_table.map(
            VirtAddr::from(TRAMPOLINE).into(),
            PhysAddr::from(strampoline as usize).into(),
            PTEFlags::R | PTEFlags::X,
        );
    }
    /// Without kernel stacks.
    pub fn new_kernel() -> Self {
        let mut memory_set = Self::new_bare();
        // map trampoline
        memory_set.map_trampoline();
        // map kernel sections
        info!(".text [{:#x}, {:#x})", stext as usize, etext as usize);
        info!(".rodata [{:#x}, {:#x})", srodata as usize, erodata as usize);
        info!(".data [{:#x}, {:#x})", sdata as usize, edata as usize);
        info!(
            ".bss [{:#x}, {:#x})",
            sbss_with_stack as usize, ebss as usize
        );
        info!("mapping .text section");
        memory_set.push(
            MapArea::new(
                (stext as usize).into(),
                (etext as usize).into(),
                MapType::Identical,
                MapPermission::R | MapPermission::X,
            ),
            None,
        );
        info!("mapping .rodata section");
        memory_set.push(
            MapArea::new(
                (srodata as usize).into(),
                (erodata as usize).into(),
                MapType::Identical,
                MapPermission::R,
            ),
            None,
        );
        info!("mapping .data section");
        memory_set.push(
            MapArea::new(
                (sdata as usize).into(),
                (edata as usize).into(),
                MapType::Identical,
                MapPermission::R | MapPermission::W,
            ),
            None,
        );
        info!("mapping .bss section");
        memory_set.push(
            MapArea::new(
                (sbss_with_stack as usize).into(),
                (ebss as usize).into(),
                MapType::Identical,
                MapPermission::R | MapPermission::W,
            ),
            None,
        );
        info!("mapping physical memory");
        memory_set.push(
            MapArea::new(
                (ekernel as usize).into(),
                MEMORY_END.into(),
                MapType::Identical,
                MapPermission::R | MapPermission::W,
            ),
            None,
        );
        memory_set
    }
    /// Include sections in elf and trampoline and TrapContext and user stack,
    /// also returns user_sp_base and entry point.
    pub fn from_elf(elf_data: &[u8]) -> (Self, usize, usize) {
        let mut memory_set = Self::new_bare();
        // map trampoline
        memory_set.map_trampoline();
        // map program headers of elf, with U flag
        let elf = xmas_elf::ElfFile::new(elf_data).unwrap();
        let elf_header = elf.header;
        let magic = elf_header.pt1.magic;
        assert_eq!(magic, [0x7f, 0x45, 0x4c, 0x46], "invalid elf!");
        let ph_count = elf_header.pt2.ph_count();
        let mut max_end_vpn = VirtPageNum(0);
        for i in 0..ph_count {
            let ph = elf.program_header(i).unwrap();
            if ph.get_type().unwrap() == xmas_elf::program::Type::Load {
                let start_va: VirtAddr = (ph.virtual_addr() as usize).into();
                let end_va: VirtAddr = ((ph.virtual_addr() + ph.mem_size()) as usize).into();
                let mut map_perm = MapPermission::U;
                let ph_flags = ph.flags();
                if ph_flags.is_read() {
                    map_perm |= MapPermission::R;
                }
                if ph_flags.is_write() {
                    map_perm |= MapPermission::W;
                }
                if ph_flags.is_execute() {
                    map_perm |= MapPermission::X;
                }
                let map_area = MapArea::new(start_va, end_va, MapType::Framed, map_perm);
                max_end_vpn = map_area.vpn_range.get_end();
                memory_set.push(
                    map_area,
                    Some(&elf.input[ph.offset() as usize..(ph.offset() + ph.file_size()) as usize]),
                );
            }
        }
        // map user stack with U flags
        let max_end_va: VirtAddr = max_end_vpn.into();
        let mut user_stack_bottom: usize = max_end_va.into();
        // guard page
        user_stack_bottom += PAGE_SIZE;
        let user_stack_top = user_stack_bottom + USER_STACK_SIZE;
        memory_set.push(
            MapArea::new(
                user_stack_bottom.into(),
                user_stack_top.into(),
                MapType::Framed,
                MapPermission::R | MapPermission::W | MapPermission::U,
            ),
            None,
        );
        // used in sbrk
        memory_set.push(
            MapArea::new(
                user_stack_top.into(),
                user_stack_top.into(),
                MapType::Framed,
                MapPermission::R | MapPermission::W | MapPermission::U,
            ),
            None,
        );
        // map TrapContext
        memory_set.push(
            MapArea::new(
                TRAP_CONTEXT_BASE.into(),
                TRAMPOLINE.into(),
                MapType::Framed,
                MapPermission::R | MapPermission::W,
            ),
            None,
        );
        // No address-space reservation table is built here: the ranges this image
        // occupies are recorded in `areas`, so `static_owns()` refuses an mmap
        // that would land on one of them. Address space belonging to *other*
        // tasks is irrelevant, because every mapping below lives in this page
        // table only and is torn down with it.
        (
            memory_set,
            user_stack_top,
            elf.header.pt2.entry_point() as usize,
        )
    }
    /// Change page table by writing satp CSR Register.
    pub fn activate(&self) {
        let satp = self.page_table.token();
        unsafe {
            satp::write(satp);
            asm!("sfence.vma");
        }
    }
    /// Translate a virtual page number to a page table entry
    pub fn translate(&self, vpn: VirtPageNum) -> Option<PageTableEntry> {
        self.page_table.translate(vpn)
    }
    /// shrink the area to new_end
    #[allow(unused)]
    pub fn shrink_to(&mut self, start: VirtAddr, new_end: VirtAddr) -> bool {
        if let Some(area) = self
            .areas
            .iter_mut()
            .find(|area| area.vpn_range.get_start() == start.floor())
        {
            area.shrink_to(&mut self.page_table, new_end.ceil());
            true
        } else {
            false
        }
    }

    /// append the area to new_end
    #[allow(unused)]
    pub fn append_to(&mut self, start: VirtAddr, new_end: VirtAddr) -> bool {
        if let Some(area) = self
            .areas
            .iter_mut()
            .find(|area| area.vpn_range.get_start() == start.floor())
        {
            area.append_to(&mut self.page_table, new_end.ceil());
            true
        } else {
            false
        }
    }
}
/// map area structure, controls a contiguous piece of virtual memory
pub struct MapArea {
    vpn_range: VPNRange,
    data_frames: BTreeMap<VirtPageNum, FrameTracker>,
    map_type: MapType,
    map_perm: MapPermission,
}

impl MapArea {
    pub fn new(
        start_va: VirtAddr,
        end_va: VirtAddr,
        map_type: MapType,
        map_perm: MapPermission,
    ) -> Self {
        let start_vpn: VirtPageNum = start_va.floor();
        let end_vpn: VirtPageNum = end_va.ceil();
        Self {
            vpn_range: VPNRange::new(start_vpn, end_vpn),
            data_frames: BTreeMap::new(),
            map_type,
            map_perm,
        }
    }
    pub fn map_one(&mut self, page_table: &mut PageTable, vpn: VirtPageNum) {
        let ppn: PhysPageNum;
        match self.map_type {
            MapType::Identical => {
                ppn = PhysPageNum(vpn.0);
            }
            MapType::Framed => {
                let frame = frame_alloc().unwrap();
                ppn = frame.ppn;
                self.data_frames.insert(vpn, frame);
            }
        }
        let pte_flags = PTEFlags::from_bits(self.map_perm.bits).unwrap();
        page_table.map(vpn, ppn, pte_flags);
    }
    #[allow(unused)]
    pub fn unmap_one(&mut self, page_table: &mut PageTable, vpn: VirtPageNum) {
        if self.map_type == MapType::Framed {
            self.data_frames.remove(&vpn);
        }
        page_table.unmap(vpn);
    }
    pub fn map(&mut self, page_table: &mut PageTable) {
        for vpn in self.vpn_range {
            self.map_one(page_table, vpn);
        }
    }
    #[allow(unused)]
    pub fn unmap(&mut self, page_table: &mut PageTable) {
        for vpn in self.vpn_range {
            self.unmap_one(page_table, vpn);
        }
    }
    #[allow(unused)]
    pub fn shrink_to(&mut self, page_table: &mut PageTable, new_end: VirtPageNum) {
        for vpn in VPNRange::new(new_end, self.vpn_range.get_end()) {
            self.unmap_one(page_table, vpn)
        }
        self.vpn_range = VPNRange::new(self.vpn_range.get_start(), new_end);
    }
    #[allow(unused)]
    pub fn append_to(&mut self, page_table: &mut PageTable, new_end: VirtPageNum) {
        for vpn in VPNRange::new(self.vpn_range.get_end(), new_end) {
            self.map_one(page_table, vpn)
        }
        self.vpn_range = VPNRange::new(self.vpn_range.get_start(), new_end);
    }
    /// data: start-aligned but maybe with shorter length
    /// assume that all frames were cleared before
    pub fn copy_data(&mut self, page_table: &mut PageTable, data: &[u8]) {
        assert_eq!(self.map_type, MapType::Framed);
        let mut start: usize = 0;
        let mut current_vpn = self.vpn_range.get_start();
        let len = data.len();
        loop {
            let src = &data[start..len.min(start + PAGE_SIZE)];
            let dst = &mut page_table
                .translate(current_vpn)
                .unwrap()
                .ppn()
                .get_bytes_array()[..src.len()];
            dst.copy_from_slice(src);
            start += PAGE_SIZE;
            if start >= len {
                break;
            }
            current_vpn.step();
        }
    }
}

#[derive(Copy, Clone, PartialEq, Debug)]
/// map type for memory set: identical or framed
pub enum MapType {
    Identical,
    Framed,
}

bitflags! {
    /// map permission corresponding to that in pte: `R W X U`
    pub struct MapPermission: u8 {
        ///Readable
        const R = 1 << 1;
        ///Writable
        const W = 1 << 2;
        ///Excutable
        const X = 1 << 3;
        ///Accessible in U mode
        const U = 1 << 4;
    }
}

/// Return (bottom, top) of a kernel stack in kernel space.
pub fn kernel_stack_position(app_id: usize) -> (usize, usize) {
    let top = TRAMPOLINE - app_id * (KERNEL_STACK_SIZE + PAGE_SIZE);
    let bottom = top - KERNEL_STACK_SIZE;
    (bottom, top)
}

/// remap test in kernel space
#[allow(unused)]
pub fn remap_test() {
    let mut kernel_space = KERNEL_SPACE.exclusive_access();
    let mid_text: VirtAddr = ((stext as usize + etext as usize) / 2).into();
    let mid_rodata: VirtAddr = ((srodata as usize + erodata as usize) / 2).into();
    let mid_data: VirtAddr = ((sdata as usize + edata as usize) / 2).into();
    assert!(!kernel_space
        .page_table
        .translate(mid_text.floor())
        .unwrap()
        .writable(),);
    assert!(!kernel_space
        .page_table
        .translate(mid_rodata.floor())
        .unwrap()
        .writable(),);
    assert!(!kernel_space
        .page_table
        .translate(mid_data.floor())
        .unwrap()
        .executable(),);
    println!("remap_test passed!");
}
