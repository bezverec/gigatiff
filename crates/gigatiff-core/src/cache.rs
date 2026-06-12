use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScanlineKey {
    pub(crate) path: PathBuf,
    pub(crate) y: u32,
    pub(crate) x: u32,
    pub(crate) width: u32,
    pub(crate) bytes_per_pixel: usize,
}

pub struct ScanlineCache {
    pub(crate) entries: VecDeque<(ScanlineKey, Arc<Vec<u8>>)>,
    pub(crate) byte_limit: usize,
    pub(crate) bytes: usize,
}

impl ScanlineCache {
    pub fn new(byte_limit: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            byte_limit,
            bytes: 0,
        }
    }

    pub(crate) fn get(&mut self, key: &ScanlineKey) -> Option<Arc<Vec<u8>>> {
        let index = self
            .entries
            .iter()
            .position(|(cached_key, _)| cached_key == key)?;
        let entry = self.entries.remove(index)?;
        let row = Arc::clone(&entry.1);
        self.entries.push_back(entry);
        Some(row)
    }

    pub(crate) fn insert(&mut self, key: ScanlineKey, row: Arc<Vec<u8>>) {
        if row.len() > self.byte_limit {
            return;
        }

        if let Some(index) = self
            .entries
            .iter()
            .position(|(cached_key, _)| cached_key == &key)
        {
            if let Some((_, old)) = self.entries.remove(index) {
                self.bytes = self.bytes.saturating_sub(old.len());
            }
        }

        self.bytes += row.len();
        self.entries.push_back((key, row));

        while self.bytes > self.byte_limit {
            if let Some((_, old)) = self.entries.pop_front() {
                self.bytes = self.bytes.saturating_sub(old.len());
            } else {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(y: u32) -> ScanlineKey {
        ScanlineKey {
            path: PathBuf::from("image.tif"),
            y,
            x: 0,
            width: 16,
            bytes_per_pixel: 3,
        }
    }

    #[test]
    fn scanline_cache_refreshes_recently_read_rows() {
        let mut cache = ScanlineCache::new(6);
        cache.insert(key(1), Arc::new(vec![1, 1, 1]));
        cache.insert(key(2), Arc::new(vec![2, 2, 2]));

        assert!(cache.get(&key(1)).is_some());
        cache.insert(key(3), Arc::new(vec![3, 3, 3]));

        assert!(cache.get(&key(1)).is_some());
        assert!(cache.get(&key(2)).is_none());
        assert!(cache.get(&key(3)).is_some());
        assert_eq!(cache.bytes, 6);
    }

    #[test]
    fn scanline_cache_replaces_rows_and_ignores_oversized_rows() {
        let mut cache = ScanlineCache::new(5);
        cache.insert(key(1), Arc::new(vec![1, 1, 1]));
        cache.insert(key(1), Arc::new(vec![2, 2]));
        cache.insert(key(2), Arc::new(vec![9; 6]));

        let row = cache.get(&key(1)).unwrap();
        assert_eq!(row.as_slice(), &[2, 2]);
        assert!(cache.get(&key(2)).is_none());
        assert_eq!(cache.bytes, 2);
    }
}
