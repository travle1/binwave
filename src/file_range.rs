//! File positions are not memory indices. Convert only bounded chunk lengths.
pub const READ_LIMIT: usize = 1024 * 1024;

pub fn bounded_length(offset: u64, requested: usize, file_len: u64) -> Result<usize, &'static str> {
    if requested > READ_LIMIT {
        return Err("read exceeds 1 MiB chunk limit");
    }
    let remaining = file_len
        .checked_sub(offset)
        .ok_or("byte offset is outside the file")?;
    Ok(remaining.min(requested as u64) as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn offsets_are_independent_of_address_size() {
        let size = 40 * 1024 * 1024 * 1024u64;
        assert_eq!(bounded_length(size - 4, 65536, size), Ok(4));
        assert_eq!(bounded_length(u64::MAX - 3, 65536, u64::MAX), Ok(3));
        assert_eq!(bounded_length(size, 65536, size), Ok(0));
        assert!(bounded_length(size + 1, 1, size).is_err());
        assert!(bounded_length(0, READ_LIMIT + 1, size).is_err());
    }
}
