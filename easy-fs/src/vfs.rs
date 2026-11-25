use super::{
    block_cache_sync_all, get_block_cache, BlockDevice, DirEntry, DiskInode, DiskInodeType,
    EasyFileSystem, DIRENT_SZ,
};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use spin::{Mutex, MutexGuard};
/// Virtual filesystem layer over easy-fs
pub struct Inode {
    block_id: usize,
    block_offset: usize,
    fs: Arc<Mutex<EasyFileSystem>>,
    block_device: Arc<dyn BlockDevice>,
}

impl Inode {
    /// Create a vfs inode
    pub fn new(
        block_id: u32,
        block_offset: usize,
        fs: Arc<Mutex<EasyFileSystem>>,
        block_device: Arc<dyn BlockDevice>,
    ) -> Self {
        Self {
            block_id: block_id as usize,
            block_offset,
            fs,
            block_device,
        }
    }
    /// Call a function over a disk inode to read it
    fn read_disk_inode<V>(&self, f: impl FnOnce(&DiskInode) -> V) -> V {
        get_block_cache(self.block_id, Arc::clone(&self.block_device))
            .lock()
            .read(self.block_offset, f)
    }
    /// Call a function over a disk inode to modify it
    fn modify_disk_inode<V>(&self, f: impl FnOnce(&mut DiskInode) -> V) -> V {
        get_block_cache(self.block_id, Arc::clone(&self.block_device))
            .lock()
            .modify(self.block_offset, f)
    }
    /// Find inode under a disk inode by name
    fn find_inode_id(&self, name: &str, disk_inode: &DiskInode) -> Option<u32> {
        // assert it is a directory
        assert!(disk_inode.is_dir());
        let file_count = (disk_inode.size as usize) / DIRENT_SZ;
        let mut dirent = DirEntry::empty();
        for i in 0..file_count {
            assert_eq!(
                disk_inode.read_at(DIRENT_SZ * i, dirent.as_bytes_mut(), &self.block_device,),
                DIRENT_SZ,
            );
            if dirent.name() == name {
                return Some(dirent.inode_id() as u32);
            }
        }
        None
    }
    /// Find inode under current inode by name
    pub fn find(&self, name: &str) -> Option<Arc<Inode>> {
        let fs = self.fs.lock();
        self.read_disk_inode(|disk_inode| {
            self.find_inode_id(name, disk_inode).map(|inode_id| {
                let (block_id, block_offset) = fs.get_disk_inode_pos(inode_id);
                Arc::new(Self::new(
                    block_id,
                    block_offset,
                    self.fs.clone(),
                    self.block_device.clone(),
                ))
            })
        })
    }
    /// Increase the size of a disk inode
    fn increase_size(
        &self,
        new_size: u32,
        disk_inode: &mut DiskInode,
        fs: &mut MutexGuard<EasyFileSystem>,
    ) {
        if new_size < disk_inode.size {
            return;
        }
        let blocks_needed = disk_inode.blocks_num_needed(new_size);
        let mut v: Vec<u32> = Vec::new();
        for _ in 0..blocks_needed {
            v.push(fs.alloc_data());
        }
        disk_inode.increase_size(new_size, v, &self.block_device);
    }
    /// The inode number this handle points at, i.e. its index in the inode area.
    ///
    /// `fstat` reports it as `stat.ino`, which is how two names that were linked
    /// to each other are recognised as the *same* file.
    pub fn inode_id(&self) -> u32 {
        // The inode number is a property of *where this handle points*, not of the
        // on-disk struct: `DiskInode` has no back-pointer to its own slot. Only the
        // superblock knows the inode area's first block, so this takes the fs lock
        // for the arithmetic and does not read the disk inode (which would lock it
        // a second time — the tutorial's inode lock is not reentrant).
        let fs = self.fs.lock();
        fs.inode_id_of(self.block_id as u32, self.block_offset)
    }
    /// Read this inode's hard-link count without touching any other lock.
    pub fn nlink(&self) -> u32 {
        let _fs = self.fs.lock();
        self.read_disk_inode(|disk_inode| disk_inode.nlink())
    }
    /// Bump this inode's hard-link count (a new dirent now points at it).
    pub fn inc_nlink(&self) {
        let _fs = self.fs.lock();
        self.modify_disk_inode(|disk_inode| disk_inode.inc_nlink());
        block_cache_sync_all();
    }
    /// Drop one hard-link reference; returns the count that remains.
    pub fn dec_nlink(&self) -> u32 {
        let _fs = self.fs.lock();
        let left = self.modify_disk_inode(|disk_inode| disk_inode.dec_nlink());
        block_cache_sync_all();
        left
    }
    /// Append a dirent `(name, inode_id)` to this directory inode.
    ///
    /// This is `create`'s second half, split out so `link` can reuse it without
    /// allocating an inode.
    pub fn add_dirent(&self, name: &str, inode_id: u32) {
        let mut fs = self.fs.lock();
        self.modify_disk_inode(|root_inode| {
            let file_count = (root_inode.size as usize) / DIRENT_SZ;
            let new_size = (file_count + 1) * DIRENT_SZ;
            self.increase_size(new_size as u32, root_inode, &mut fs);
            let dirent = DirEntry::new(name, inode_id);
            root_inode.write_at(
                file_count * DIRENT_SZ,
                dirent.as_bytes(),
                &self.block_device,
            );
        });
        block_cache_sync_all();
    }
    /// Erase every dirent of this directory inode that names `name`.
    ///
    /// The slot is zeroed rather than compacted: entries are fixed-width and
    /// `find_inode_id` walks them by index, so shrinking the directory would mean
    /// moving every later name down. A zeroed dirent reads back as the empty name
    /// (`DirEntry::name()` stops at the first NUL) and inode number 0, which no
    /// path argument in this chapter can ask for.
    pub fn remove_dirent(&self, name: &str) -> bool {
        let _fs = self.fs.lock();
        let mut found = false;
        self.modify_disk_inode(|root_inode| {
            let file_count = (root_inode.size as usize) / DIRENT_SZ;
            for i in 0..file_count {
                let mut dirent = DirEntry::empty();
                assert_eq!(
                    root_inode.read_at(
                        i * DIRENT_SZ,
                        dirent.as_bytes_mut(),
                        &self.block_device,
                    ),
                    DIRENT_SZ,
                );
                if dirent.name() == name {
                    // Rewrite the slot as an empty dirent pointing at no file. The
                    // directory keeps its size (entries are fixed-width), and both
                    // `find_inode_id` and `ls` skip it because the name is now the
                    // all-zero placeholder that no path argument can ever equal.
                    let dead = DirEntry::new("", 0);
                    root_inode.write_at(i * DIRENT_SZ, dead.as_bytes(), &self.block_device);
                    found = true;
                }
            }
        });
        block_cache_sync_all();
        found
    }
    /// Free every data block this inode owns (including the indirect blocks) and
    /// return the inode itself to the inode bitmap. Only legal once nlink is 0.
    pub fn destroy(&self) {
        let inode_id = self.inode_id();
        let mut fs = self.fs.lock();
        let blocks_dealloc = self.modify_disk_inode(|disk_inode| {
            let size = disk_inode.size;
            let blocks = disk_inode.clear_size(&self.block_device);
            assert!(blocks.len() == DiskInode::total_blocks(size) as usize);
            blocks
        });
        // `clear_size` reports *absolute* block ids (it reads them straight out
        // of the direct/indirect slots), so they go to `dealloc_data` untranslated
        // — which is also exactly what `EasyFileSystem::dealloc_data` expects.
        for block in blocks_dealloc.into_iter() {
            fs.dealloc_data(block);
        }
        fs.dealloc_inode(inode_id);
        block_cache_sync_all();
    }
    /// Create inode under current inode by name
    pub fn create(&self, name: &str) -> Option<Arc<Inode>> {
        let mut fs = self.fs.lock();
        let op = |root_inode: &DiskInode| {
            // assert it is a directory
            assert!(root_inode.is_dir());
            // has the file been created?
            self.find_inode_id(name, root_inode)
        };
        if self.read_disk_inode(op).is_some() {
            return None;
        }
        // create a new file
        // alloc a inode with an indirect block
        let new_inode_id = fs.alloc_inode();
        // initialize inode
        let (new_inode_block_id, new_inode_block_offset) = fs.get_disk_inode_pos(new_inode_id);
        get_block_cache(new_inode_block_id as usize, Arc::clone(&self.block_device))
            .lock()
            .modify(new_inode_block_offset, |new_inode: &mut DiskInode| {
                new_inode.initialize(DiskInodeType::File);
            });
        // append the dirent through the same helper `link` uses
        drop(fs);
        self.add_dirent(name, new_inode_id);
        let fs = self.fs.lock();
        let (block_id, block_offset) = fs.get_disk_inode_pos(new_inode_id);
        block_cache_sync_all();
        // return inode
        Some(Arc::new(Self::new(
            block_id,
            block_offset,
            self.fs.clone(),
            self.block_device.clone(),
        )))
        // release efs lock automatically by compiler
    }
    /// Make the file at least `new_size` bytes long, allocating data blocks.
    ///
    /// `DiskInode::write_at` clamps to the current size, so a kernel that wants
    /// POSIX "write past EOF grows the file" has to grow it first. Taking the EFS
    /// lock here (rather than inside a disk-inode closure) is what keeps this
    /// callable while the caller still holds its own inode handle.
    pub fn resize_to(&self, new_size: usize) {
        let mut fs = self.fs.lock();
        self.modify_disk_inode(|disk_inode| {
            if (new_size as u32) > disk_inode.size {
                self.increase_size(new_size as u32, disk_inode, &mut fs);
            }
        });
        block_cache_sync_all();
    }
    /// Add a second name for an existing file living in this directory.
    ///
    /// Returns `false` when `old` is absent or `new` already exists, `-1`-wise
    /// matching POSIX `link`. The target's nlink is bumped *outside* the root
    /// inode's lock: `add_dirent` takes that lock itself, and easy-fs' disk-inode
    /// accessors are not reentrant spinlocks.
    pub fn link(&self, old: &str, new: &str) -> bool {
        let inode_id = {
            let _fs = self.fs.lock();
            self.read_disk_inode(|disk_inode| {
                assert!(disk_inode.is_dir());
                self.find_inode_id(old, disk_inode)
            })
        };
        let inode_id = match inode_id {
            Some(id) => id,
            None => return false,
        };
        {
            let _fs = self.fs.lock();
            if self
                .read_disk_inode(|disk_inode| self.find_inode_id(new, disk_inode))
                .is_some()
            {
                return false;
            }
        }
        self.add_dirent(new, inode_id);
        let (block_id, block_offset) = {
            let fs = self.fs.lock();
            fs.get_disk_inode_pos(inode_id)
        };
        get_block_cache(block_id as usize, Arc::clone(&self.block_device))
            .lock()
            .modify(block_offset, |disk_inode: &mut DiskInode| {
                disk_inode.inc_nlink();
            });
        block_cache_sync_all();
        true
    }
    /// Remove one name from this directory and drop one reference to the file.
    ///
    /// When the last dirent goes away the inode and all of its data blocks are
    /// released here, because nothing else in easy-fs ever revisits the bitmap
    /// once a slot is marked used. That release is what `Test mass open/unlink`
    /// exercises: 10 create/write/close/unlink rounds against a 4095-slot inode
    /// bitmap would otherwise die in `alloc_inode().unwrap()` on round 2.
    pub fn unlink(&self, name: &str) -> bool {
        let inode_id = {
            let _fs = self.fs.lock();
            self.read_disk_inode(|disk_inode| {
                assert!(disk_inode.is_dir());
                self.find_inode_id(name, disk_inode)
            })
        };
        let inode_id = match inode_id {
            Some(id) => id,
            None => return false,
        };
        self.remove_dirent(name);
        let (block_id, block_offset) = {
            let fs = self.fs.lock();
            fs.get_disk_inode_pos(inode_id)
        };
        let left = get_block_cache(block_id as usize, Arc::clone(&self.block_device))
            .lock()
            .modify(block_offset, |disk_inode: &mut DiskInode| {
                disk_inode.dec_nlink()
            });
        block_cache_sync_all();
        if left == 0 {
            // `destroy()` re-takes the EFS lock and the root-inode lock, so it
            // must run with nothing of ours held.
            let inode = Arc::new(Inode::new(
                block_id,
                block_offset,
                self.fs.clone(),
                self.block_device.clone(),
            ));
            inode.destroy();
        }
        true
    }
    /// List inodes under current inode
    pub fn ls(&self) -> Vec<String> {
        let _fs = self.fs.lock();
        self.read_disk_inode(|disk_inode| {
            let file_count = (disk_inode.size as usize) / DIRENT_SZ;
            let mut v: Vec<String> = Vec::new();
            for i in 0..file_count {
                let mut dirent = DirEntry::empty();
                assert_eq!(
                    disk_inode.read_at(i * DIRENT_SZ, dirent.as_bytes_mut(), &self.block_device,),
                    DIRENT_SZ,
                );
                v.push(String::from(dirent.name()));
            }
            v
        })
    }
    /// Read data from current inode
    pub fn read_at(&self, offset: usize, buf: &mut [u8]) -> usize {
        let _fs = self.fs.lock();
        self.read_disk_inode(|disk_inode| disk_inode.read_at(offset, buf, &self.block_device))
    }
    /// Write data to current inode
    pub fn write_at(&self, offset: usize, buf: &[u8]) -> usize {
        let mut fs = self.fs.lock();
        let size = self.modify_disk_inode(|disk_inode| {
            self.increase_size((offset + buf.len()) as u32, disk_inode, &mut fs);
            disk_inode.write_at(offset, buf, &self.block_device)
        });
        block_cache_sync_all();
        size
    }
    /// Clear the data in current inode
    pub fn clear(&self) {
        let mut fs = self.fs.lock();
        self.modify_disk_inode(|disk_inode| {
            let size = disk_inode.size;
            let data_blocks_dealloc = disk_inode.clear_size(&self.block_device);
            assert!(data_blocks_dealloc.len() == DiskInode::total_blocks(size) as usize);
            for data_block in data_blocks_dealloc.into_iter() {
                fs.dealloc_data(data_block);
            }
        });
        block_cache_sync_all();
    }
}
