//! Bounded file observations for assertions on dataset-sized outputs.
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read};
use std::path::Path;

/// Compare lengths first, then stream through the standard reader buffers.
/// Consume only the common prefix: the readers may refill with different
/// chunk lengths even when the underlying files have identical bytes.
pub(crate) fn files_equal(a: &Path, b: &Path) -> io::Result<bool> {
    let a = File::open(a)?;
    let b = File::open(b)?;
    if a.metadata()?.len() != b.metadata()?.len() {
        return Ok(false);
    }
    let mut a = BufReader::new(a);
    let mut b = BufReader::new(b);
    loop {
        let a_buf = a.fill_buf()?;
        let b_buf = b.fill_buf()?;
        if a_buf.is_empty() || b_buf.is_empty() {
            return Ok(a_buf.is_empty() && b_buf.is_empty());
        }
        let count = a_buf.len().min(b_buf.len());
        if a_buf[..count] != b_buf[..count] {
            return Ok(false);
        }
        a.consume(count);
        b.consume(count);
    }
}

/// Check the exact small seed without allocating based on the file size.
/// A missing or unreadable destination is an observation error, not a
/// successful replacement of the seed.
pub(crate) fn holds_seed(path: &Path, role: &str) -> io::Result<bool> {
    let seed = crate::protocol::seed(role);
    let mut file = File::open(path)?;
    if file.metadata()?.len() != seed.len() as u64 {
        return Ok(false);
    }
    let mut actual = vec![0; seed.len()];
    file.read_exact(&mut actual)?;
    let mut extra = [0];
    Ok(actual == seed && file.read(&mut extra)? == 0)
}

#[cfg(test)]
mod tests {
    use super::{files_equal, holds_seed};

    #[test]
    fn bounded_comparison_edges() {
        let scratch = pbfhogg_test_scratch::scratch();
        let a = scratch.child("a");
        let b = scratch.child("b");
        assert!(files_equal(&a, &b).is_err());
        for size in [0, 1, 8191, 8192, 8193, 20000] {
            let bytes = vec![42; size];
            std::fs::write(&a, &bytes).expect("write a");
            std::fs::write(&b, &bytes).expect("write b");
            assert!(files_equal(&a, &b).expect("equal"));
            if size != 0 {
                let mut changed = bytes.clone();
                changed[size - 1] = 43;
                std::fs::write(&b, changed).expect("write differing tail");
                assert!(!files_equal(&a, &b).expect("different tail"));
            }
            std::fs::write(&b, vec![42; size + 1]).expect("write different length");
            assert!(!files_equal(&a, &b).expect("different length"));
        }
    }

    #[test]
    fn seed_requires_exact_bytes_and_eof() {
        let scratch = pbfhogg_test_scratch::scratch();
        let path = scratch.child("seed");
        assert!(holds_seed(&path, "out").is_err());
        let seed = crate::protocol::seed("out");
        std::fs::write(&path, &seed).expect("seed");
        assert!(holds_seed(&path, "out").expect("holds"));
        assert!(!holds_seed(&path, "other").expect("other seed"));
        for bytes in [
            Vec::new(),
            seed[..seed.len() - 1].to_vec(),
            [seed.as_slice(), b"x"].concat(),
            vec![0; seed.len()],
        ] {
            std::fs::write(&path, bytes).expect("replace seed");
            assert!(!holds_seed(&path, "out").expect("changed seed"));
        }
    }
}
