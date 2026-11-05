//! Release builds download yt-dlp into the state folder on first launch; source builds use PATH.
//! It stays there across app updates and updates itself to nightly once a day.
use sha2::{Digest, Sha256};
use std::error::Error;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

const YTDLP: &str = "https://github.com/yt-dlp/yt-dlp-nightly-builds/releases/latest/download";
const UPDATE_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
static YTDLP_PATH: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Called by mpv's worker, never by a view: the first launch's download holds up only playback.
pub fn configure(init: &libmpv2::MpvInitializer) -> libmpv2::Result<()> {
    // With a JS runtime yt-dlp picks YouTube formats that mpv gets 403s for (October 2026),
    // so keep it off even when Deno is on PATH.
    init.set_option("ytdl-raw-options", "no-js-runtimes=")?;
    let Some(ytdlp) = YTDLP_PATH.get_or_init(discover) else {
        return Ok(());
    };
    log::info!("using yt-dlp {}", ytdlp.display());
    set_path(init, ytdlp)
}

fn set_path(init: &libmpv2::MpvInitializer, ytdlp: &Path) -> libmpv2::Result<()> {
    // mpv key/value options support byte-length quoting, including commas in home folders.
    let path = ytdlp.to_string_lossy();
    init.set_option(
        "script-opts",
        format!("ytdl_hook-ytdl_path=%{}%{path}", path.len()),
    )
}

fn executable(name: &str) -> String {
    format!("{name}{}", std::env::consts::EXE_SUFFIX)
}

fn discover() -> Option<PathBuf> {
    if cfg!(debug_assertions) {
        return None;
    }
    let dir = fastframe_dirs::Dirs::platform("org", "radio4000", "radio4000")?
        .state
        .join("desktop/tools");
    let ytdlp = dir.join(executable("yt-dlp"));
    if !ytdlp.is_file()
        && let Err(error) = fetch(&dir, &ytdlp)
    {
        log::warn!("could not download yt-dlp; trying PATH: {error}");
        return None;
    }
    let path = ytdlp.clone();
    std::thread::spawn(move || update(&path, &dir));
    Some(ytdlp)
}

/// This platform's yt-dlp release asset.
fn asset() -> Option<&'static str> {
    Some(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "yt-dlp_linux",
        ("linux", "aarch64") => "yt-dlp_linux_aarch64",
        ("macos", _) => "yt-dlp_macos",
        ("windows", "x86_64") => "yt-dlp.exe",
        _ => return None,
    })
}

fn fetch(dir: &Path, ytdlp: &Path) -> Result<(), Box<dyn Error>> {
    let name = asset().ok_or("no yt-dlp build for this platform")?;
    log::info!("downloading yt-dlp");
    fs::create_dir_all(dir)?;
    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(5 * 60)))
        .user_agent("radio4000")
        .build()
        .new_agent();
    let get = |url: &str| -> Result<Vec<u8>, Box<dyn Error>> {
        let mut body = Vec::new();
        agent
            .get(url)
            .call()?
            .body_mut()
            .as_reader()
            .read_to_end(&mut body)?;
        Ok(body)
    };
    let sums = get(&format!("{YTDLP}/SHA2-256SUMS"))?;
    let body = get(&format!("{YTDLP}/{name}"))?;
    verify(&body, &sums, name)?;
    save(ytdlp, &body)?;
    // Fresh from nightly, so the first update check can wait a day.
    let _ = fs::write(dir.join("last-check"), []);
    Ok(())
}

/// Lowercase hex, as `sha256sum` prints it.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Checks `body` against `name`'s line in a sha256sum-style listing.
fn verify(body: &[u8], sums: &[u8], name: &str) -> Result<(), Box<dyn Error>> {
    let sums = String::from_utf8_lossy(sums);
    let expected = sums
        .lines()
        .find_map(|line| {
            let (hash, file) = line.split_once(char::is_whitespace)?;
            (file.trim_start().trim_start_matches('*') == name).then_some(hash)
        })
        .ok_or_else(|| format!("{name} is not in its checksums"))?;
    if hex(&Sha256::digest(body)) != expected.to_ascii_lowercase() {
        return Err(format!("{name} does not match its checksum").into());
    }
    Ok(())
}

/// Writes beside `path` and renames, so a half-written download never looks installed.
fn save(path: &Path, body: &[u8]) -> io::Result<()> {
    let partial = path.with_extension(format!("partial-{}", std::process::id()));
    fs::write(&partial, body)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&partial, fs::Permissions::from_mode(0o755))?;
    }
    fs::rename(&partial, path)
}

/// Failed updates leave the working copy alone. A new version plays from the next launch.
fn update(ytdlp: &Path, dir: &Path) {
    let checked = dir.join("last-check");
    let recent = fs::metadata(&checked)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|time| time.elapsed().ok())
        .is_some_and(|age| age < UPDATE_INTERVAL);
    if recent {
        return;
    }
    // yt-dlp checks the download against its release's SHA2-256SUMS and swaps itself in.
    match std::process::Command::new(ytdlp)
        .args(["--update-to", "nightly"])
        .output()
    {
        Ok(output) if output.status.success() => {
            let _ = fs::write(&checked, []);
            log::info!("yt-dlp: {}", String::from_utf8_lossy(&output.stdout).trim());
        }
        Ok(output) => log::warn!(
            "yt-dlp update: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
        Err(error) => log::warn!("yt-dlp update: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mpv_accepts_paths_with_spaces_and_commas() {
        let ytdlp = Path::new("/home/a, b/tools/yt-dlp");
        let mpv = libmpv2::Mpv::with_initializer(|init| set_path(&init, ytdlp)).unwrap();
        let opts: String = mpv.get_property("options/script-opts").unwrap();
        assert!(
            opts.contains(&format!("ytdl_hook-ytdl_path={}", ytdlp.display())),
            "{opts}"
        );
    }

    #[test]
    fn downloads_must_match_their_listed_checksum() {
        let hash = hex(&Sha256::digest(b"good"));
        let sums = format!("{}  yt-dlp\n{hash}  yt-dlp_linux\n", "0".repeat(64));
        assert!(verify(b"good", sums.as_bytes(), "yt-dlp_linux").is_ok());
        assert!(verify(b"bad", sums.as_bytes(), "yt-dlp_linux").is_err());
        assert!(verify(b"good", sums.as_bytes(), "yt-dlp_macos").is_err());
    }
}
