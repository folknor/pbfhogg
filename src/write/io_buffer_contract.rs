//! Source contract for the shared I/O transfer size.

use std::path::Path;

#[test]
fn io_transfer_size_has_one_owner() -> std::io::Result<()> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for directory in ["read", "write"] {
        check_directory(&root.join(directory))?;
    }
    Ok(())
}

fn check_directory(directory: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(directory)? {
        let path = entry?.path();
        if path.is_dir() {
            check_directory(&path)?;
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            check_source(&path)?;
        }
    }
    Ok(())
}

fn check_source(path: &Path) -> std::io::Result<()> {
    let source = std::fs::read_to_string(path)?;
    let product = format!("{}*{}", 256, 1024);
    let budget = format!("{product}*{}", 1024);
    let decimal = (1usize << 18).to_string();
    for line in source.lines() {
        let compact: String = line
            .chars()
            .filter(|c| !c.is_whitespace() && *c != '_')
            .collect();
        // The larger scan/cache budgets are not transfer buffers.
        let compact = compact.replace(&budget, "SCAN_BUDGET");
        if path.file_name().is_some_and(|name| name == "mod.rs")
            && compact.starts_with("pub(crate)constIOBUFBYTES:usize=")
        {
            continue;
        }
        assert!(
            !compact.contains(&product) && !compact.contains(&decimal),
            "{} duplicates the I/O transfer size; use IO_BUF_BYTES",
            path.display(),
        );
    }
    Ok(())
}
