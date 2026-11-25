//! Implementation of [`PageTableEntry`] and [`PageTable`].
use super::{
    frame_alloc, FrameTracker, PhysAddr, PhysPageNum, StepByOne, VirtAddr, VirtPageNum, VPNRange,
};
use crate::config::PAGE_SIZE;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use bitflags::*;

bitflags! {
    /// page table entry flags
    pub struct PTEFlags: u8 {
        const V = 1 << 0;
        const R = 1 << 1;
        const W = 1 << 2;
        const X = 1 << 3;
        const U = 1 << 4;
        const G = 1 << 5;
        const A = 1 << 6;
        const D = 1 << 7;
    }
}

#[derive(Copy, Clone)]
#[repr(C)]
/// page table entry structure
pub struct PageTableEntry {
    /// bits of page table entry
    pub bits: usize,
}

impl PageTableEntry {
    /// Create a new page table entry
    pub fn new(ppn: PhysPageNum, flags: PTEFlags) -> Self {
        PageTableEntry {
            bits: ppn.0 << 10 | flags.bits as usize,
        }
    }
    /// Create an empty page table entry
    pub fn empty() -> Self {
        PageTableEntry { bits: 0 }
    }
    /// Get the physical page number from the page table entry
    pub fn ppn(&self) -> PhysPageNum {
        (self.bits >> 10 & ((1usize << 44) - 1)).into()
    }
    /// Get the flags from the page table entry
    pub fn flags(&self) -> PTEFlags {
        PTEFlags::from_bits(self.bits as u8).unwrap()
    }
    /// The page pointered by page table entry is valid?
    pub fn is_valid(&self) -> bool {
        (self.flags() & PTEFlags::V) != PTEFlags::empty()
    }
    /// The page pointered by page table entry is readable?
    pub fn readable(&self) -> bool {
        (self.flags() & PTEFlags::R) != PTEFlags::empty()
    }
    /// The page pointered by page table entry is writable?
    pub fn writable(&self) -> bool {
        (self.flags() & PTEFlags::W) != PTEFlags::empty()
    }
    /// The page may be reached from U mode (its U bit is set)?
    pub fn user_accessible(&self) -> bool {
        (self.flags() & PTEFlags::U) != PTEFlags::empty()
    }
    /// The page pointered by page table entry is executable?
    pub fn executable(&self) -> bool {
        (self.flags() & PTEFlags::X) != PTEFlags::empty()
    }
}

/// page table structure
pub struct PageTable {
    root_ppn: PhysPageNum,
    frames: Vec<FrameTracker>,
}

/// Assume that it won't oom when creating/mapping.
impl PageTable {
    /// Create a new page table
    pub fn new() -> Self {
        let frame = frame_alloc().unwrap();
        PageTable {
            root_ppn: frame.ppn,
            frames: vec![frame],
        }
    }
    /// Temporarily used to get arguments from user space.
    pub fn from_token(satp: usize) -> Self {
        Self {
            root_ppn: PhysPageNum::from(satp & ((1usize << 44) - 1)),
            frames: Vec::new(),
        }
    }
    /// Find PageTableEntry by VirtPageNum, create a frame for a 4KB page table if not exist
    fn find_pte_create(&mut self, vpn: VirtPageNum) -> Option<&mut PageTableEntry> {
        let idxs = vpn.indexes();
        let mut ppn = self.root_ppn;
        let mut result: Option<&mut PageTableEntry> = None;
        for (i, idx) in idxs.iter().enumerate() {
            let pte = &mut ppn.get_pte_array()[*idx];
            if i == 2 {
                result = Some(pte);
                break;
            }
            if !pte.is_valid() {
                let frame = frame_alloc().unwrap();
                *pte = PageTableEntry::new(frame.ppn, PTEFlags::V);
                self.frames.push(frame);
            }
            ppn = pte.ppn();
        }
        result
    }
    /// Find PageTableEntry by VirtPageNum
    fn find_pte(&self, vpn: VirtPageNum) -> Option<&mut PageTableEntry> {
        let idxs = vpn.indexes();
        let mut ppn = self.root_ppn;
        let mut result: Option<&mut PageTableEntry> = None;
        for (i, idx) in idxs.iter().enumerate() {
            let pte = &mut ppn.get_pte_array()[*idx];
            if i == 2 {
                result = Some(pte);
                break;
            }
            if !pte.is_valid() {
                return None;
            }
            ppn = pte.ppn();
        }
        result
    }
    /// set the map between virtual page number and physical page number
    #[allow(unused)]
    pub fn map(&mut self, vpn: VirtPageNum, ppn: PhysPageNum, flags: PTEFlags) {
        let pte = self.find_pte_create(vpn).unwrap();
        assert!(!pte.is_valid(), "vpn {:?} is mapped before mapping", vpn);
        *pte = PageTableEntry::new(ppn, flags | PTEFlags::V);
    }
    /// remove the map between virtual page number and physical page number
    #[allow(unused)]
    pub fn unmap(&mut self, vpn: VirtPageNum) {
        let pte = self.find_pte(vpn).unwrap();
        assert!(pte.is_valid(), "vpn {:?} is invalid before unmapping", vpn);
        *pte = PageTableEntry::empty();
    }
    /// get the page table entry from the virtual page number
    pub fn translate(&self, vpn: VirtPageNum) -> Option<PageTableEntry> {
        self.find_pte(vpn).map(|pte| *pte)
    }
    /// get the physical address from the virtual address
    pub fn translate_va(&self, va: VirtAddr) -> Option<PhysAddr> {
        self.find_pte(va.clone().floor()).map(|pte| {
            let aligned_pa: PhysAddr = pte.ppn().into();
            let offset = va.page_offset();
            let aligned_pa_usize: usize = aligned_pa.into();
            (aligned_pa_usize + offset).into()
        })
    }
    /// Like [`translate_va`](Self::translate_va), but refuses an empty leaf slot
    /// instead of reporting physical page zero for it.
    fn translate_va_valid(&self, va: VirtAddr) -> Option<PhysAddr> {
        let pte = self.find_pte(va.floor())?;
        if !pte.is_valid() {
            return None;
        }
        let aligned_pa_usize: usize = PhysAddr::from(pte.ppn()).into();
        Some((aligned_pa_usize + va.page_offset()).into())
    }
    /// Read one byte through this page table.
    ///
    /// Returns `None` when the leaf entry is missing or not valid, so a hole in
    /// the address space never reads back as physical page zero.
    pub fn read_byte(&self, va: VirtAddr) -> Option<u8> {
        // An invalid leaf has no frame at all (its ppn field reads as 0), which
        // is why this must not fall through to translate_va.
        let pa = self.translate_va_valid(va)?;
        // SAFETY: translate_va_valid only answers for a valid leaf slot of this
        // page table, so `pa` designates one byte inside a mapped frame; the
        // caller decides whether reading it is legal (MemorySet::
        // translate_user_byte adds the U/R/W and RAM-range checks).
        Some(unsafe { (usize::from(pa) as *const u8).read_volatile() })
    }
    /// Write one byte through this page table, same validity rule as
    /// [`read_byte`](Self::read_byte).
    pub fn write_byte(&mut self, va: VirtAddr, value: u8) -> Option<()> {
        let pa = self.translate_va_valid(va)?;
        // SAFETY: as above, and the caller checked the W bit.
        unsafe { (usize::from(pa) as *mut u8).write_volatile(value) };
        Some(())
    }
    /// Forget every leaf mapping in the range without freeing anything.
    ///
    /// `unmap` insists on a valid entry (that is how a double free gets caught),
    /// so tearing down an address space that may contain holes — pages the
    /// application `munmap`'d before exiting, or simply never touched — needs
    /// this variant. Ownership of the frames stays where it already is (`MapArea`
    /// and `Vma` trackers), which then release them exactly once.
    pub fn clear_range(&mut self, start: VirtPageNum, end: VirtPageNum) {
        for vpn in VPNRange::new(start, end) {
            // A missing intermediate table means nothing is mapped below it.
            if let Some(pte) = self.find_pte(vpn) {
                *pte = PageTableEntry::empty();
            }
        }
    }
    /// get the token from the page table
    pub fn token(&self) -> usize {
        8usize << 60 | self.root_ppn.0
    }
}

/// Translate&Copy a ptr[u8] array with LENGTH len to a mutable u8 Vec through page table
pub fn translated_byte_buffer(token: usize, ptr: *const u8, len: usize) -> Vec<&'static mut [u8]> {
    let page_table = PageTable::from_token(token);
    let mut start = ptr as usize;
    let end = start + len;
    let mut v = Vec::new();
    while start < end {
        let start_va = VirtAddr::from(start);
        let mut vpn = start_va.floor();
        // An unmapped byte is the application's bug, not the kernel's: refuse
        // (empty vector) instead of unwrapping and panicking the whole system.
        // Callers turn this into `-1`/a killed task.
        let ppn = match page_table.translate(vpn) {
            Some(pte) if pte.is_valid() => pte.ppn(),
            _ => return Vec::new(),
        };
        vpn.step();
        let mut end_va: VirtAddr = vpn.into();
        end_va = end_va.min(VirtAddr::from(end));
        if end_va.page_offset() == 0 {
            v.push(&mut ppn.get_bytes_array()[start_va.page_offset()..]);
        } else {
            v.push(&mut ppn.get_bytes_array()[start_va.page_offset()..end_va.page_offset()]);
        }
        start = end_va.into();
    }
    v
}

/// Copy a NUL-terminated string out of the *current task's* address space.
///
/// The translation goes through that task's own page table (`MemorySet::
/// read_user_str`, which demands present + R + U) rather than "whichever table
/// satp happens to hold". While handling a trap, satp is the kernel's table, so
/// an application string constant such as `ch5_usertest`'s `TESTS` entries —
/// which live in the image's `.rodata`, not on the stack — would read back as
/// unmapped and every `sys_spawn` would answer `-1`.
pub fn translated_str_opt(ptr: *const u8) -> Option<alloc::string::String> {
    let task = crate::task::current_task()?;
    let inner = task.inner_exclusive_access();
    inner.memory_set.read_user_str(ptr)
}

/// Translate&Copy a ptr[u8] array end with `\0` to a `String` Vec through page table
pub fn translated_str(_token: usize, ptr: *const u8) -> String {
    translated_str_opt(ptr).expect("invalid user string in translated_str")
}

/// Copy `len` bytes from user memory into `dst` (which must be exactly `len`
/// long), one page at a time. Returns `false` on any unreadable byte.
pub fn copy_from_user(token: usize, src: usize, dst: &mut [u8]) -> bool {
    let page_table = PageTable::from_token(token);
    let mut offset = 0;
    while offset < dst.len() {
        let va = match src.checked_add(offset) {
            Some(va) => va,
            None => return false,
        };
        let page_off = va % PAGE_SIZE;
        let take = core::cmp::min(dst.len() - offset, PAGE_SIZE - page_off);
        for i in 0..take {
            match page_table.read_byte(VirtAddr::from(va + i)) {
                Some(byte) => dst[offset + i] = byte,
                None => return false,
            }
        }
        offset += take;
    }
    true
}

/// Copy `src` into user memory, one page at a time. Returns `false` on any
/// unwritable byte.
pub fn copy_to_user(token: usize, dst: usize, src: &[u8]) -> bool {
    let mut page_table = PageTable::from_token(token);
    let mut offset = 0;
    while offset < src.len() {
        let va = match dst.checked_add(offset) {
            Some(va) => va,
            None => return false,
        };
        let page_off = va % PAGE_SIZE;
        let take = core::cmp::min(src.len() - offset, PAGE_SIZE - page_off);
        for i in 0..take {
            if page_table
                .write_byte(VirtAddr::from(va + i), src[offset + i])
                .is_none()
            {
                return false;
            }
        }
        offset += take;
    }
    true
}

/// Read a plain-old-data value out of user memory.
pub fn read_struct<T: Copy>(token: usize, addr: usize) -> Option<T> {
    let size = core::mem::size_of::<T>();
    if size == 0 || addr % core::mem::align_of::<T>() != 0 {
        return None;
    }
    let mut buf = vec![0u8; size];
    if !copy_from_user(token, addr, &mut buf) {
        return None;
    }
    // SAFETY: `buf` holds size_of::<T>() bytes and T's alignment was checked.
    Some(unsafe { core::ptr::read_unaligned(buf.as_ptr() as *const T) })
}

/// Write a plain-old-data value into user memory.
pub fn write_struct<T: Copy>(token: usize, addr: usize, value: &T) -> bool {
    let size = core::mem::size_of::<T>();
    if size == 0 || addr % core::mem::align_of::<T>() != 0 {
        return false;
    }
    // SAFETY: reinterpreting a Copy value as its own bytes is always valid.
    let bytes = unsafe { core::slice::from_raw_parts(value as *const T as *const u8, size) };
    copy_to_user(token, addr, bytes)
}
/// Translate a ptr[u8] array through page table and return a mutable reference of T
pub fn translated_refmut<T>(token: usize, ptr: *mut T) -> &'static mut T {
    //trace!("into translated_refmut!");
    let page_table = PageTable::from_token(token);
    let va = ptr as usize;
    //trace!("translated_refmut: before translate_va");
    page_table
        .translate_va(VirtAddr::from(va))
        .unwrap()
        .get_mut()
}
