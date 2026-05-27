//! `hiker-syncd` — the standalone zero-knowledge sync hub.
//!
//! Runs the same `hiker-sync` crate in hub topology: a Noise-authenticated,
//! enrollment-gated libp2p endpoint that store-and-forwards opaque encrypted
//! blobs between a user's enrolled devices. It never holds the vault content
//! key and never decrypts. [sync-decoupled-server, sync-zero-knowledge-server]
//!
//! # Usage
//!
//! ```text
//! hiker-syncd <listen-multiaddr> --data-dir <dir> [--device <fp>]... [--device-file <path>]
//!
//! hiker-syncd /ip4/0.0.0.0/tcp/4090 --data-dir /var/lib/hiker-syncd \
//!     --device DEV-AAA...-xx --device DEV-BBB...-yy
//! hiker-syncd /ip4/0.0.0.0/tcp/4090 --data-dir ./hub --device-file ./devices.txt
//! ```
//!
//! Enrolled device fingerprints come from repeated `--device` flags and/or a
//! `--device-file` (one fingerprint per line, `#` comments and blanks ignored).
//! The server's own [`DeviceKeypair`] is generated on first run and persisted as
//! `<data-dir>/server.key` (protobuf), so the hub keeps a stable identity.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use hiker_sync::crypto::DeviceKeypair;
use hiker_sync::identity::DeviceFingerprint;
use hiker_sync::server::Hub;

/// Parsed command line.
struct Args {
    listen: String,
    data_dir: PathBuf,
    devices: Vec<String>,
}

fn print_usage() {
    eprintln!(
        "usage: hiker-syncd <listen-multiaddr> --data-dir <dir> \
         [--device <fingerprint>]... [--device-file <path>]"
    );
}

/// Hand-roll the arg parse — no arg-parsing dependency. `--help`/`-h` prints
/// usage; an unknown flag or a missing value is a hard error.
fn parse_args() -> Result<Args, String> {
    let mut listen: Option<String> = None;
    let mut data_dir: Option<PathBuf> = None;
    let mut devices: Vec<String> = Vec::new();

    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print_usage();
                std::process::exit(0);
            }
            "--data-dir" => {
                let v = it.next().ok_or("--data-dir needs a value")?;
                data_dir = Some(PathBuf::from(v));
            }
            "--device" => {
                let v = it.next().ok_or("--device needs a value")?;
                devices.push(v);
            }
            "--device-file" => {
                let v = it.next().ok_or("--device-file needs a value")?;
                devices.extend(read_device_file(Path::new(&v))?);
            }
            flag if flag.starts_with('-') => {
                return Err(format!("unknown flag: {flag}"));
            }
            positional => {
                if listen.is_some() {
                    return Err(format!("unexpected extra argument: {positional}"));
                }
                listen = Some(positional.to_string());
            }
        }
    }

    Ok(Args {
        listen: listen.ok_or("missing <listen-multiaddr>")?,
        data_dir: data_dir.ok_or("missing --data-dir")?,
        devices,
    })
}

/// Read enrolled fingerprints from a file: one per line, `#` comments and blank
/// lines ignored.
fn read_device_file(path: &Path) -> Result<Vec<String>, String> {
    let contents =
        fs::read_to_string(path).map_err(|e| format!("read device file {path:?}: {e}"))?;
    Ok(contents
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_string)
        .collect())
}

/// Load the server's persisted [`DeviceKeypair`] from `<data-dir>/server.key`,
/// generating and persisting a fresh one (protobuf) on first run.
fn load_or_generate_keypair(data_dir: &Path) -> Result<DeviceKeypair, String> {
    let key_path = data_dir.join("server.key");
    if key_path.exists() {
        let bytes = fs::read(&key_path).map_err(|e| format!("read {key_path:?}: {e}"))?;
        DeviceKeypair::from_protobuf(&bytes).map_err(|e| format!("decode server key: {e}"))
    } else {
        let kp = DeviceKeypair::generate();
        let bytes = kp
            .to_protobuf()
            .map_err(|e| format!("encode server key: {e}"))?;
        fs::create_dir_all(data_dir).map_err(|e| format!("create data dir: {e}"))?;
        fs::write(&key_path, &bytes).map_err(|e| format!("write {key_path:?}: {e}"))?;
        Ok(kp)
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> ExitCode {
    // Default to INFO; raise with `HIKER_SYNCD_LOG=debug` (a coarse level knob
    // that needs no env-filter feature). The fmt subscriber writes to stderr.
    let level = match std::env::var("HIKER_SYNCD_LOG").as_deref() {
        Ok("trace") => tracing::Level::TRACE,
        Ok("debug") => tracing::Level::DEBUG,
        Ok("warn") => tracing::Level::WARN,
        Ok("error") => tracing::Level::ERROR,
        _ => tracing::Level::INFO,
    };
    tracing_subscriber::fmt().with_max_level(level).init();

    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("hiker-syncd: {e}");
            print_usage();
            return ExitCode::FAILURE;
        }
    };

    if let Err(e) = run(args).await {
        tracing::error!(error = %e, "hiker-syncd failed");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

async fn run(args: Args) -> Result<(), String> {
    let keypair = load_or_generate_keypair(&args.data_dir)?;
    tracing::info!(
        fingerprint = %keypair.fingerprint().0,
        data_dir = %args.data_dir.display(),
        enrolled = args.devices.len(),
        "hiker-syncd starting"
    );

    let enrolled: Vec<DeviceFingerprint> =
        args.devices.into_iter().map(DeviceFingerprint).collect();
    let mut server = Hub::new(keypair, &args.data_dir, enrolled)
        .map_err(|e| format!("open store: {e}"))?;

    let bound = server
        .listen(&args.listen)
        .await
        .map_err(|e| format!("listen on {}: {e}", args.listen))?;
    tracing::info!(address = %bound, "hiker-syncd listening");

    server.run_forever().await.map_err(|e| format!("serve: {e}"))
}
