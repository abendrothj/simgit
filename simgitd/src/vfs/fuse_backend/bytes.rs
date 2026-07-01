//! Small byte-manipulation helpers used by FUSE read/write handlers.

pub(super) fn file_slice(data: &[u8], offset: usize, size: usize) -> &[u8] {
    if offset >= data.len() {
        return &[];
    }
    let end = offset.saturating_add(size).min(data.len());
    &data[offset..end]
}

pub(super) fn apply_write_at_offset(buf: &mut Vec<u8>, offset: usize, data: &[u8]) {
    if buf.len() < offset {
        buf.resize(offset, 0);
    }
    let end = offset.saturating_add(data.len());
    if buf.len() < end {
        buf.resize(end, 0);
    }
    buf[offset..end].copy_from_slice(data);
}

#[cfg(test)]
mod tests {
    use super::{apply_write_at_offset, file_slice};

    #[test]
    fn file_slice_within_bounds() {
        let data = b"abcdef";
        assert_eq!(file_slice(data, 1, 3), b"bcd");
    }

    #[test]
    fn file_slice_offset_past_end() {
        let data = b"abcdef";
        assert_eq!(file_slice(data, 99, 5), b"");
    }

    #[test]
    fn file_slice_truncates_to_end() {
        let data = b"abcdef";
        assert_eq!(file_slice(data, 4, 10), b"ef");
    }

    #[test]
    fn apply_write_in_middle() {
        let mut data = b"abcdef".to_vec();
        apply_write_at_offset(&mut data, 2, b"ZZ");
        assert_eq!(data, b"abZZef");
    }

    #[test]
    fn apply_write_extends_with_zeros() {
        let mut data = b"abc".to_vec();
        apply_write_at_offset(&mut data, 5, b"xy");
        assert_eq!(data, b"abc\0\0xy");
    }
}
