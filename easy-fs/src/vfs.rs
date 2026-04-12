use super::{
    block_cache_sync_all, get_block_cache, BlockDevice, DirEntry, DiskInode,
    DiskInodeType, EasyFileSystem, DIRENT_SZ,
};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use spin::{Mutex, MutexGuard};
/// Virtual filesystem layer over easy-fs
pub struct Inode {
    block_id: usize,
    block_offset: usize,
    inode_id: u32,
    fs: Arc<Mutex<EasyFileSystem>>,
    block_device: Arc<dyn BlockDevice>,
}

impl Inode {
    /// Create a vfs inode
    pub fn new(
        block_id: u32,
        block_offset: usize,
        inode_id: u32,
        fs: Arc<Mutex<EasyFileSystem>>,
        block_device: Arc<dyn BlockDevice>,
    ) -> Self {
        Self {
            block_id: block_id as usize,
            block_offset,
            inode_id,
            fs,
            block_device,
        }
    }

    fn read_disk_inode<V>(&self, f: impl FnOnce(&DiskInode) -> V) -> V {
        get_block_cache(self.block_id, Arc::clone(&self.block_device))
            .lock()
            .read(self.block_offset, f)
    }

    fn modify_disk_inode<V>(&self, f: impl FnOnce(&mut DiskInode) -> V) -> V {
        get_block_cache(self.block_id, Arc::clone(&self.block_device))
            .lock()
            .modify(self.block_offset, f)
    }

    fn find_inode_id(&self, name: &str, disk_inode: &DiskInode) -> Option<u32> {
        assert!(disk_inode.is_dir());
        let file_count = (disk_inode.size as usize) / DIRENT_SZ;
        let mut dirent = DirEntry::empty();
        for i in 0..file_count {
            assert_eq!(
                disk_inode.read_at(
                    DIRENT_SZ * i,
                    dirent.as_bytes_mut(),
                    &self.block_device,
                ),
                DIRENT_SZ,
            );
            if dirent.name() == name {
                return Some(dirent.inode_id());
            }
        }
        None
    }

    fn find_inode_id_with_index(
        &self,
        name: &str,
        disk_inode: &DiskInode,
    ) -> Option<(usize, u32)> {
        assert!(disk_inode.is_dir());
        let file_count = (disk_inode.size as usize) / DIRENT_SZ;
        let mut dirent = DirEntry::empty();
        for i in 0..file_count {
            assert_eq!(
                disk_inode.read_at(
                    DIRENT_SZ * i,
                    dirent.as_bytes_mut(),
                    &self.block_device,
                ),
                DIRENT_SZ,
            );
            if dirent.name() == name {
                return Some((i, dirent.inode_id()));
            }
        }
        None
    }
    /// Return inode number
    pub fn inode_id(&self) -> u32 {
        self.inode_id
    }
    /// Return hard link count
    pub fn nlink(&self) -> u32 {
        self.read_disk_inode(|disk_inode| disk_inode.nlink)
    }
    /// Whether this inode is a directory
    pub fn is_dir(&self) -> bool {
        self.read_disk_inode(|disk_inode| disk_inode.is_dir())
    }
    /// Whether this inode is a file
    pub fn is_file(&self) -> bool {
        self.read_disk_inode(|disk_inode| disk_inode.is_file())
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
                    inode_id,
                    self.fs.clone(),
                    self.block_device.clone(),
                ))
            })
        })
    }

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
    /// Create inode under current inode by name
    pub fn create(&self, name: &str) -> Option<Arc<Inode>> {
        let mut fs = self.fs.lock();
        let op = |root_inode: &DiskInode| {
            assert!(root_inode.is_dir());
            self.find_inode_id(name, root_inode)
        };
        if self.read_disk_inode(op).is_some() {
            return None;
        }
        let new_inode_id = fs.alloc_inode();
        let (new_inode_block_id, new_inode_block_offset) =
            fs.get_disk_inode_pos(new_inode_id);
        get_block_cache(new_inode_block_id as usize, Arc::clone(&self.block_device))
            .lock()
            .modify(new_inode_block_offset, |new_inode: &mut DiskInode| {
                new_inode.initialize(DiskInodeType::File);
            });

        self.modify_disk_inode(|root_inode| {
            let file_count = (root_inode.size as usize) / DIRENT_SZ;
            let new_size = (file_count + 1) * DIRENT_SZ;
            self.increase_size(new_size as u32, root_inode, &mut fs);
            let dirent = DirEntry::new(name, new_inode_id);
            root_inode.write_at(
                file_count * DIRENT_SZ,
                dirent.as_bytes(),
                &self.block_device,
            );
        });

        let (block_id, block_offset) = fs.get_disk_inode_pos(new_inode_id);
        block_cache_sync_all();
        Some(Arc::new(Self::new(
            block_id,
            block_offset,
            new_inode_id,
            self.fs.clone(),
            self.block_device.clone(),
        )))
    }
    /// Create a hard link in current directory
    pub fn link(&self, old_name: &str, new_name: &str) -> isize {
        if old_name == new_name {
            return -1;
        }
        let mut fs = self.fs.lock();

        let pair = self.read_disk_inode(|root_inode| {
            assert!(root_inode.is_dir());
            (
                self.find_inode_id(old_name, root_inode),
                self.find_inode_id(new_name, root_inode),
            )
        });

        let old_inode_id = match pair.0 {
            Some(id) => id,
            None => return -1,
        };
        if pair.1.is_some() {
            return -1;
        }

        let (old_block_id, old_block_offset) = fs.get_disk_inode_pos(old_inode_id);
        let is_dir = get_block_cache(old_block_id as usize, Arc::clone(&self.block_device))
            .lock()
            .read(old_block_offset, |disk_inode: &DiskInode| disk_inode.is_dir());
        if is_dir {
            return -1;
        }

        get_block_cache(old_block_id as usize, Arc::clone(&self.block_device))
            .lock()
            .modify(old_block_offset, |disk_inode: &mut DiskInode| {
                disk_inode.nlink += 1;
            });

        self.modify_disk_inode(|root_inode| {
            let file_count = (root_inode.size as usize) / DIRENT_SZ;
            let new_size = (file_count + 1) * DIRENT_SZ;
            self.increase_size(new_size as u32, root_inode, &mut fs);
            let dirent = DirEntry::new(new_name, old_inode_id);
            root_inode.write_at(
                file_count * DIRENT_SZ,
                dirent.as_bytes(),
                &self.block_device,
            );
        });

        block_cache_sync_all();
        0
    }
    /// Remove a hard link in current directory
    pub fn unlink(&self, name: &str) -> isize {
        let mut fs = self.fs.lock();

        let (target_index, target_inode_id) = match self.read_disk_inode(|root_inode| {
            assert!(root_inode.is_dir());
            self.find_inode_id_with_index(name, root_inode)
        }) {
            Some(v) => v,
            None => return -1,
        };

        let (target_block_id, target_block_offset) = fs.get_disk_inode_pos(target_inode_id);
        let is_dir = get_block_cache(target_block_id as usize, Arc::clone(&self.block_device))
            .lock()
            .read(target_block_offset, |disk_inode: &DiskInode| disk_inode.is_dir());
        if is_dir {
            return -1;
        }

        self.modify_disk_inode(|root_inode| {
            let file_count = (root_inode.size as usize) / DIRENT_SZ;
            let last_index = file_count - 1;
            if target_index != last_index {
                let mut last_dirent = DirEntry::empty();
                assert_eq!(
                    root_inode.read_at(
                        last_index * DIRENT_SZ,
                        last_dirent.as_bytes_mut(),
                        &self.block_device,
                    ),
                    DIRENT_SZ,
                );
                root_inode.write_at(
                    target_index * DIRENT_SZ,
                    last_dirent.as_bytes(),
                    &self.block_device,
                );
            }
            root_inode.size -= DIRENT_SZ as u32;
        });

        let mut need_dealloc = false;
        let mut data_blocks_dealloc: Vec<u32> = Vec::new();
        get_block_cache(target_block_id as usize, Arc::clone(&self.block_device))
            .lock()
            .modify(target_block_offset, |disk_inode: &mut DiskInode| {
                assert!(disk_inode.nlink > 0);
                disk_inode.nlink -= 1;
                if disk_inode.nlink == 0 {
                    let size = disk_inode.size;
                    data_blocks_dealloc = disk_inode.clear_size(&self.block_device);
                    assert!(
                        data_blocks_dealloc.len()
                            == DiskInode::total_blocks(size) as usize
                    );
                    need_dealloc = true;
                }
            });

        if need_dealloc {
            for data_block in data_blocks_dealloc.into_iter() {
                fs.dealloc_data(data_block);
            }
            fs.dealloc_inode(target_inode_id);
        }

        block_cache_sync_all();
        0
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
                    disk_inode.read_at(
                        i * DIRENT_SZ,
                        dirent.as_bytes_mut(),
                        &self.block_device,
                    ),
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
        self.read_disk_inode(|disk_inode| {
            disk_inode.read_at(offset, buf, &self.block_device)
        })
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