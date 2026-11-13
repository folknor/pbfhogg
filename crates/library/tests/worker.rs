//! Rust-only hostile worker for the driver's integration contracts.
use std::io::Write;
use std::path::Path;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
fn descendant() -> Result<std::process::Child> {
    let exe = std::env::current_exe()?;
    let mut child = std::process::Command::new(exe);
    child
        .env_clear()
        .arg("--parked")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    Ok(child.spawn()?)
}
fn marker_path() -> Result<std::path::PathBuf> {
    Ok(std::env::var_os("BROKKR_MARKER_FIFO")
        .ok_or("missing FIFO")?
        .into())
}
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args_os().collect();
    if args.get(1).is_some_and(|a| a == "--parked") {
        std::thread::park();
        return Ok(());
    }
    if args.len() != 7
        || args[1] != "--test-harness"
        || args[3] != "--private-dir"
        || args[5] != "--artefact-dir"
    {
        return Err("wrong worker argv".into());
    }
    let request = Path::new(&args[2]);
    let root = request.parent().ok_or("request has no parent")?;
    let artefacts = Path::new(&args[6]);
    if std::env::current_dir()? != root.join("cwd") || Path::new(&args[4]) != root.join("cwd") {
        return Err("wrong cwd/private dir".into());
    }
    let req: serde_json::Value = serde_json::from_slice(&std::fs::read(request)?)?;
    let mode = req["scenario"].as_str().ok_or("missing scenario")?;
    if mode == "deadline" {
        std::thread::park();
    }
    if mode == "orphan" {
        let _child = descendant()?;
    }
    if mode == "flood" {
        for _ in 0..256 {
            std::io::stdout().write_all(&[b'x'; 8192])?;
        }
    }
    if mode == "stderr" {
        std::io::stderr().write_all(b"diagnostic")?;
    }
    if mode == "refusal" {
        std::fs::write(
            artefacts.join("refusal.json"),
            br#"{"protocol":1,"reason":"fixture refusal"}"#,
        )?;
        std::process::exit(2);
    }
    if mode == "layout" {
        std::fs::write(root.join("extra"), b"bad")?;
    }
    if mode == "request" {
        std::fs::write(request, b"changed")?;
    }
    if mode == "cwd" {
        std::fs::write(root.join("cwd/extra"), b"bad")?;
    }
    if mode == "seeded"
        && std::fs::read(root.join("roles/out-a"))? != b"brokkr-library-seed-v1 out-a\n"
    {
        return Err("wrong seed bytes".into());
    }
    if mode != "missing-output" {
        std::fs::write(root.join("roles/out-a"), b"pbf")?;
    }
    documents(mode, root, artefacts)?;
    let mut fifo = std::fs::OpenOptions::new()
        .write(true)
        .open(marker_path()?)?;
    let stream = match mode {
        "stream" => "15 %a CAT_START\n25 %a @@writer_blocks=4\n26 %a !sealed drops=0\n",
        "unknown-call" => "15 %unknown CAT_START\n25 %a @@writer_blocks=3\n26 %a !sealed drops=0\n",
        "fragment" => "15 %a CAT_START",
        _ => "15 %a CAT_START\n25 %a @@writer_blocks=3\n26 %a !sealed drops=0\n",
    };
    fifo.write_all(stream.as_bytes())?;
    if mode == "fifo-flood" {
        for _ in 0..256 {
            fifo.write_all(&[b'x'; 8192])?;
        }
    }
    if mode == "exit-one" {
        std::process::exit(1);
    }
    if mode == "exit-invalid" {
        std::process::exit(9);
    }
    Ok(())
}

fn documents(mode: &str, root: &Path, artefacts: &Path) -> Result<()> {
    let mut scenario: serde_json::Value =
        serde_json::from_str(include_str!("../fixtures/scenario.json"))?;
    scenario["scenario"] = mode.into();
    let mut call: serde_json::Value = serde_json::from_str(include_str!("../fixtures/call.json"))?;
    if mode == "assertion" || mode == "exit-one" {
        call["assertions"][0]["status"] = "fail".into();
    }
    if mode == "seal" {
        call["seal"] = serde_json::json!({"state":"refused","reason":"fixture seal"});
        call["snapshot"] = serde_json::Value::Null;
        call["stream"]["state"] = "open".into();
    }
    if mode == "path" {
        call["context"]["leak"] = root.to_str().ok_or("path")?.into();
    }
    if mode == "timestamp" {
        call["seal_us"] = 9_000_000_000_u64.into();
    }
    std::fs::create_dir(artefacts.join("calls"))?;
    std::fs::write(
        artefacts.join("scenario.json"),
        serde_json::to_vec(&scenario)?,
    )?;
    std::fs::write(artefacts.join("calls/a.json"), serde_json::to_vec(&call)?)?;
    if mode == "symlink" {
        std::fs::remove_file(artefacts.join("calls/a.json"))?;
        std::os::unix::fs::symlink("../scenario.json", artefacts.join("calls/a.json"))?;
    }
    Ok(())
}
