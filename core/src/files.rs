//! Sending files and folders to the peer.
//!
//! A transfer is a list of entries (folders, files, symbolic links) in walk order, each on
//! its own unidirectional stream at the lowest priority, so input, video and clipboard
//! always go first:
//! `STREAM_FILE: u8 | header_len: u32 LE | FileHeader (postcard) | file bytes`.
//! Permissions, modification times, symbolic links and empty folders are kept, so app
//! bundles and scripts still work after the copy. The receiver writes files into a hidden
//! partial file and renames it into place once complete.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use iroh::endpoint::{Connection, RecvStream};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::Instant;
use tracing::{info, warn};

use crate::protocol::STREAM_FILE;
use crate::session::{Event, EventTx};

const FILE_PRIORITY: i32 = -20;
const CHUNK: usize = 256 * 1024;
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);
const MAX_HEADER: usize = 64 * 1024;
/// Permission bits kept on arrival: rwx for user/group/other, never setuid/setgid/sticky.
#[cfg(unix)]
const MODE_MASK: u32 = 0o777;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum Kind {
    File,
    Dir,
    /// The header's `target` is the link's (relative) target.
    Symlink,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FileHeader {
    transfer: u64,
    index: u32,
    count: u32,
    /// Relative, '/'-separated. The first component is the item that was sent.
    path: String,
    kind: Kind,
    size: u64,
    /// Unix permission bits, 0 when unknown (e.g. sent from Windows).
    mode: u32,
    /// Modification time, seconds since the Unix epoch, 0 when unknown.
    mtime: i64,
    target: String,
    /// Bytes of this transfer before this entry, and the transfer's total, for progress.
    offset: u64,
    total: u64,
    /// Sent as clipboard contents: lands in the clipboard cache, not the download folder.
    clipboard: bool,
}

static DOWNLOAD_DIR: Mutex<Option<PathBuf>> = Mutex::new(None);
static CLIPBOARD_DIR: Mutex<Option<PathBuf>> = Mutex::new(None);
/// Per transfer: the name each sent top-level item got here (made unique on arrival).
static TOP_LEVEL_NAMES: Mutex<Option<HashMap<(u64, String), String>>> = Mutex::new(None);

/// Where received files go. Defaults to ~/Downloads.
pub fn set_download_dir(dir: PathBuf) {
    *DOWNLOAD_DIR.lock().unwrap() = Some(dir);
}

/// Where files copied on the peer are cached until pasted; each transfer gets a
/// subfolder named after its id. Defaults to a temporary folder.
pub fn set_clipboard_dir(dir: PathBuf) {
    *CLIPBOARD_DIR.lock().unwrap() = Some(dir);
}

fn clipboard_dir(transfer: u64) -> PathBuf {
    CLIPBOARD_DIR
        .lock()
        .unwrap()
        .clone()
        .unwrap_or_else(|| std::env::temp_dir().join("kvm-clipboard"))
        .join(transfer.to_string())
}

fn download_dir() -> PathBuf {
    DOWNLOAD_DIR.lock().unwrap().clone().unwrap_or_else(|| {
        std::env::var_os("HOME").map_or_else(std::env::temp_dir, |h| PathBuf::from(h).join("Downloads"))
    })
}

struct Entry {
    abs: PathBuf,
    rel: String,
    kind: Kind,
    size: u64,
    mode: u32,
    mtime: i64,
    target: String,
}

#[cfg(unix)]
fn mode_of(meta: &std::fs::Metadata) -> u32 {
    std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & MODE_MASK
}

#[cfg(not(unix))]
fn mode_of(_: &std::fs::Metadata) -> u32 {
    0
}

fn mtime_of(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs() as i64)
}

/// Expands files and folders into entries, folders before their contents.
fn collect(paths: &[PathBuf]) -> Result<Vec<Entry>> {
    fn walk(abs: &Path, rel: String, out: &mut Vec<Entry>) -> Result<()> {
        let meta = std::fs::symlink_metadata(abs).with_context(|| format!("reading {}", abs.display()))?;
        let (mode, mtime) = (mode_of(&meta), mtime_of(&meta));
        let entry = |kind, size, target| Entry { abs: abs.to_path_buf(), rel: rel.clone(), kind, size, mode, mtime, target };
        if meta.file_type().is_symlink() {
            let target = std::fs::read_link(abs)?.to_string_lossy().into_owned();
            out.push(entry(Kind::Symlink, 0, target));
        } else if meta.is_dir() {
            out.push(entry(Kind::Dir, 0, String::new()));
            let mut children: Vec<_> = std::fs::read_dir(abs)?.filter_map(|e| e.ok()).collect();
            children.sort_by_key(|e| e.file_name());
            for child in children {
                let name = child.file_name().to_string_lossy().into_owned();
                if name == ".DS_Store" {
                    continue;
                }
                walk(&child.path(), format!("{rel}/{name}"), out)?;
            }
        } else if meta.is_file() {
            out.push(entry(Kind::File, meta.len(), String::new()));
        } // sockets, devices, pipes: skipped
        Ok(())
    }
    let mut out = Vec::new();
    for path in paths {
        let name = path
            .file_name()
            .with_context(|| format!("{} has no name", path.display()))?
            .to_string_lossy()
            .into_owned();
        walk(path, name, &mut out)?;
    }
    Ok(out)
}

/// Sends `paths` (files or folders), reporting progress, then [`Event::FileSendDone`].
pub(crate) async fn send(conn: Connection, transfer: u64, paths: Vec<PathBuf>, clipboard: bool, events: EventTx) {
    let result = send_all(&conn, transfer, paths, clipboard, &events).await;
    if let Err(err) = &result {
        warn!("sending files: {err:#}");
    }
    let _ = events.send(Event::FileSendDone { transfer, error: result.err().map(|e| format!("{e:#}")) });
}

async fn send_all(conn: &Connection, transfer: u64, paths: Vec<PathBuf>, clipboard: bool, events: &EventTx) -> Result<()> {
    let entries = tokio::task::spawn_blocking(move || collect(&paths)).await??;
    if entries.is_empty() {
        bail!("nothing to send");
    }
    let total: u64 = entries.iter().map(|e| e.size).sum();
    let count = entries.len() as u32;
    let mut sent = 0u64;
    let mut last_report = Instant::now();
    let _ = events.send(Event::FileSendProgress { transfer, sent, total });
    for (index, entry) in entries.into_iter().enumerate() {
        let header = FileHeader {
            transfer,
            index: index as u32,
            count,
            path: entry.rel,
            kind: entry.kind,
            size: entry.size,
            mode: entry.mode,
            mtime: entry.mtime,
            target: entry.target,
            offset: sent,
            total,
            clipboard,
        };
        let mut file = match entry.kind {
            Kind::File => Some(
                tokio::fs::File::open(&entry.abs)
                    .await
                    .with_context(|| format!("opening {}", entry.abs.display()))?,
            ),
            Kind::Dir | Kind::Symlink => None,
        };
        let mut stream = conn.open_uni().await?;
        stream.set_priority(FILE_PRIORITY).ok();
        let header_bytes = postcard::to_stdvec(&header)?;
        stream.write_all(&[STREAM_FILE]).await?;
        stream.write_all(&(header_bytes.len() as u32).to_le_bytes()).await?;
        stream.write_all(&header_bytes).await?;
        if let Some(file) = file.as_mut() {
            let mut buf = vec![0u8; CHUNK];
            let mut remaining = entry.size;
            while remaining > 0 {
                let n = file.read(&mut buf).await?;
                if n == 0 {
                    bail!("{} shrank while sending", entry.abs.display());
                }
                let n = n.min(remaining as usize);
                stream.write_all(&buf[..n]).await?;
                remaining -= n as u64;
                sent += n as u64;
                if last_report.elapsed() >= PROGRESS_INTERVAL {
                    last_report = Instant::now();
                    let _ = events.send(Event::FileSendProgress { transfer, sent, total });
                }
            }
        }
        stream.finish()?;
        // Wait until the peer has the entry: folders must exist before their contents,
        // and "done" should mean delivered. Code 0 is a stream it read what it needed of;
        // any other means it didn't take the entry.
        if let Ok(Some(code)) = stream.stopped().await
            && code != 0u32.into()
        {
            bail!("{} wasn't taken by the other machine", header.path);
        }
    }
    let _ = events.send(Event::FileSendProgress { transfer, sent, total });
    info!("sent {count} entries, {total} bytes");
    Ok(())
}

/// Receives one entry stream (after its kind byte).
pub(crate) async fn receive(mut recv: RecvStream, events: EventTx) {
    let mut len = [0u8; 4];
    if recv.read_exact(&mut len).await.is_err() {
        return;
    }
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_HEADER {
        warn!("file header too large");
        return;
    }
    let mut header = vec![0u8; len];
    if recv.read_exact(&mut header).await.is_err() {
        return;
    }
    let Ok(header) = postcard::from_bytes::<FileHeader>(&header) else {
        warn!("bad file header");
        return;
    };
    let (transfer, name, clipboard) = (header.transfer, header.path.clone(), header.clipboard);
    match receive_entry(&mut recv, &header, &events).await {
        Ok(path) => {
            let _ = events.send(Event::FileReceived { transfer, path, index: header.index, count: header.count, clipboard });
        }
        Err(err) => {
            warn!("receiving {name}: {err:#}");
            let _ = recv.stop(1u32.into());
            let _ = events.send(Event::FileReceiveFailed { transfer, name, error: format!("{err:#}"), clipboard });
        }
    }
    if header.index + 1 == header.count
        && let Some(map) = TOP_LEVEL_NAMES.lock().unwrap().as_mut()
    {
        map.retain(|(t, _), _| *t != transfer);
    }
}

async fn receive_entry(recv: &mut RecvStream, header: &FileHeader, events: &EventTx) -> Result<PathBuf> {
    let parts = sanitize(&header.path)?;
    let root = if header.clipboard { clipboard_dir(header.transfer) } else { download_dir() };
    tokio::fs::create_dir_all(&root).await?;
    // A clipboard transfer has a fresh folder of its own: names can't clash.
    let top = if header.clipboard { parts[0].clone() } else { top_level_name(header.transfer, &parts[0], &root) };
    let mut rel = vec![top];
    rel.extend_from_slice(&parts[1..]);
    let display = parts.join("/");
    let progress = |received: u64| Event::FileReceiveProgress {
        transfer: header.transfer,
        name: display.clone(),
        received: header.offset + received,
        total: header.total,
        clipboard: header.clipboard,
    };

    match header.kind {
        Kind::Dir => {
            let dest = tokio::task::spawn_blocking(move || real_dirs(&root, &rel)).await??;
            // Keep the folder writable for its contents, which arrive next.
            set_mode(&dest, header.mode | 0o700);
            return Ok(dest);
        }
        Kind::Symlink => {
            check_link_target(&parts, &header.target)?;
            let dir = real_dirs(&root, &rel[..rel.len() - 1])?;
            let dest = dir.join(&rel[rel.len() - 1]);
            make_symlink(&header.target, &dest)?;
            quarantine(&dest);
            return Ok(dest);
        }
        Kind::File => {}
    }

    // Entries arrive in parallel, so a link may appear at any moment; but nothing that
    // exists is ever replaced, so folders checked to be real stay real.
    let dir = {
        let (root, rel) = (root.clone(), rel[..rel.len() - 1].to_vec());
        tokio::task::spawn_blocking(move || real_dirs(&root, &rel)).await??
    };
    let file_name = &rel[rel.len() - 1];
    let dest = dir.join(file_name);
    let partial = dir.join(format!(".{file_name}.kvmpart"));

    let mut file = open_partial(&partial).await?;
    quarantine(&partial);
    let mut buf = vec![0u8; CHUNK];
    let mut received = 0u64;
    let mut last_report = Instant::now();
    loop {
        match recv.read(&mut buf).await {
            Ok(Some(n)) => {
                received += n as u64;
                if received > header.size {
                    let _ = tokio::fs::remove_file(&partial).await;
                    bail!("more data than announced");
                }
                file.write_all(&buf[..n]).await?;
                if last_report.elapsed() >= PROGRESS_INTERVAL {
                    last_report = Instant::now();
                    let _ = events.send(progress(received));
                }
            }
            Ok(None) => break,
            Err(err) => {
                drop(file);
                let _ = tokio::fs::remove_file(&partial).await;
                return Err(err.into());
            }
        }
    }
    file.flush().await?;
    drop(file);
    if received != header.size {
        let _ = tokio::fs::remove_file(&partial).await;
        bail!("incomplete: {received} of {} bytes", header.size);
    }
    set_mode(&partial, header.mode);
    if header.mtime > 0 {
        let mtime = UNIX_EPOCH + Duration::from_secs(header.mtime as u64);
        if let Ok(f) = std::fs::File::options().write(true).open(&partial) {
            let _ = f.set_modified(mtime.min(SystemTime::now() + Duration::from_secs(86_400)));
        }
    }
    // Nested files live in a folder that is already unique; only top-level files need it.
    let final_path = if parts.len() == 1 { dest } else { unique(&dest) };
    tokio::fs::rename(&partial, &final_path).await?;
    let _ = events.send(progress(received));
    Ok(final_path)
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    if mode != 0 {
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode & MODE_MASK));
    }
}

#[cfg(not(unix))]
fn set_mode(_: &Path, _: u32) {}

#[cfg(unix)]
fn make_symlink(target: &str, dest: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, dest).with_context(|| format!("creating link {}", dest.display()))
}

#[cfg(not(unix))]
fn make_symlink(_: &str, dest: &Path) -> Result<()> {
    bail!("symbolic links are not supported here ({})", dest.display())
}

/// Makes the folders `rel` below `root`, one at a time, and returns the last. Fails if any
/// of them is a link (or not a folder): writing through a link received earlier could
/// land anywhere. Nothing received replaces what exists, so the check holds afterwards.
fn real_dirs(root: &Path, rel: &[String]) -> Result<PathBuf> {
    let mut path = root.to_path_buf();
    for part in rel {
        path.push(part);
        match std::fs::create_dir(&path) {
            Ok(()) => quarantine(&path),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(err).with_context(|| format!("creating {}", path.display())),
        }
        let meta = std::fs::symlink_metadata(&path)?;
        if !meta.is_dir() {
            bail!("{} is not a folder; not writing through it", path.display());
        }
    }
    Ok(path)
}

/// Creates (or truncates) a partial file, never through a link.
async fn open_partial(path: &Path) -> Result<tokio::fs::File> {
    let mut opts = tokio::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    opts.custom_flags(libc::O_NOFOLLOW);
    opts.open(path).await.with_context(|| format!("creating {}", path.display()))
}

/// Marks a received item as downloaded, like AirDrop and browsers do, so Gatekeeper checks
/// an app or script before it first runs.
#[cfg(target_os = "macos")]
fn quarantine(path: &Path) {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else { return };
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let value = format!("0081;{now:08x};KVM;");
    let ok = unsafe {
        libc::setxattr(
            c_path.as_ptr(),
            c"com.apple.quarantine".as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            0,
            libc::XATTR_NOFOLLOW,
        )
    } == 0;
    if !ok {
        warn!("quarantining {}: {}", path.display(), std::io::Error::last_os_error());
    }
}

#[cfg(not(target_os = "macos"))]
fn quarantine(_: &Path) {}

/// A received link may only point inside the item that was sent (as app bundles'
/// `Versions/Current -> A` do), never to an absolute path or above the item. Any `..`
/// must come first: after a name, which may itself be a link, it could go anywhere.
fn check_link_target(parts: &[String], target: &str) -> Result<()> {
    let target_path = Path::new(target);
    if target.is_empty() || target_path.is_absolute() {
        bail!("link to {target:?} not allowed");
    }
    if parts.len() < 2 {
        bail!("a link on its own isn't received, only inside a folder");
    }
    // Depth below the sent item of the folder holding the link.
    let mut depth = parts.len() - 2;
    let mut descended = false;
    for component in target_path.components() {
        match component {
            Component::ParentDir if descended || depth == 0 => bail!("link to {target:?} leaves the sent item"),
            Component::ParentDir => depth -= 1,
            Component::Normal(_) => descended = true,
            Component::CurDir => {}
            _ => bail!("link to {target:?} not allowed"),
        }
    }
    Ok(())
}

/// Keeps only plain path components, so a peer can't write outside the download folder.
fn sanitize(path: &str) -> Result<Vec<String>> {
    let mut parts = Vec::new();
    for component in Path::new(path).components() {
        match component {
            Component::Normal(part) => {
                let part = part.to_string_lossy();
                if part.contains(['/', '\\', '\0']) || part.starts_with('.') && part.ends_with(".kvmpart") {
                    bail!("unsafe file name {path:?}");
                }
                parts.push(part.into_owned());
            }
            _ => bail!("unsafe file path {path:?}"),
        }
    }
    if parts.is_empty() {
        bail!("empty file path");
    }
    Ok(parts)
}

/// The name a sent top-level item gets here: its own, or "name 2", "name 3", ... if taken.
fn top_level_name(transfer: u64, sent_name: &str, root: &Path) -> String {
    let mut names = TOP_LEVEL_NAMES.lock().unwrap();
    let map = names.get_or_insert_with(HashMap::new);
    map.entry((transfer, sent_name.to_string()))
        .or_insert_with(|| {
            unique(&root.join(sent_name))
                .file_name()
                .map_or_else(|| sent_name.to_string(), |n| n.to_string_lossy().into_owned())
        })
        .clone()
}

/// `path`, or the first of "stem 2.ext", "stem 3.ext", ... that doesn't exist yet.
fn unique(path: &Path) -> PathBuf {
    if std::fs::symlink_metadata(path).is_err() {
        return path.to_path_buf();
    }
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = path.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    (2..)
        .map(|i| path.with_file_name(format!("{stem} {i}{ext}")))
        .find(|p| std::fs::symlink_metadata(p).is_err())
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_rejects_escapes() {
        assert_eq!(sanitize("Photos/a.jpg").unwrap(), vec!["Photos", "a.jpg"]);
        assert!(sanitize("../etc/passwd").is_err());
        assert!(sanitize("/etc/passwd").is_err());
        assert!(sanitize("a/../../b").is_err());
        assert!(sanitize("").is_err());
        assert!(sanitize(".x.kvmpart").is_err());
    }

    #[test]
    fn unique_names_like_finder() {
        let dir = std::env::temp_dir().join(format!("kvm-files-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let taken = dir.join("report.pdf");
        std::fs::write(&taken, b"x").unwrap();
        assert_eq!(unique(&taken), dir.join("report 2.pdf"));
        assert_eq!(unique(&dir.join("new.txt")), dir.join("new.txt"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn parts(p: &str) -> Vec<String> {
        p.split('/').map(String::from).collect()
    }

    #[test]
    fn link_targets_must_stay_inside_the_sent_item() {
        // Framework-style links inside an app bundle.
        let link = parts("My.app/Contents/Frameworks/X.framework/Versions/Current");
        assert!(check_link_target(&link, "A").is_ok());
        let link = parts("My.app/Contents/Frameworks/X.framework/X");
        assert!(check_link_target(&link, "Versions/Current/X").is_ok());
        assert!(check_link_target(&parts("Folder/Sub/link"), "../file").is_ok());
        assert!(check_link_target(&parts("Folder/link"), "./file").is_ok());
        // Escapes.
        assert!(check_link_target(&parts("Folder/link"), "/etc/passwd").is_err());
        assert!(check_link_target(&parts("Folder/link"), "../../.ssh").is_err());
        assert!(check_link_target(&parts("Folder/link"), "../Other").is_err());
        assert!(check_link_target(&parts("Folder/link"), "").is_err());
        assert!(check_link_target(&parts("link"), "x").is_err());
        // Through another link: Folder/a -> .. would make "a/.." the home folder.
        assert!(check_link_target(&parts("Folder/b"), "a/..").is_err());
        assert!(check_link_target(&parts("Folder/Sub/b"), "../a/../..").is_err());
    }

    #[test]
    fn never_writes_through_a_received_link() {
        let root = std::env::temp_dir().join(format!("kvm-links-test-{}", std::process::id()));
        std::fs::create_dir_all(root.join("Item")).unwrap();
        std::os::unix::fs::symlink("/tmp", root.join("Item/sneaky")).unwrap();
        let rel = |p: &str| p.split('/').map(String::from).collect::<Vec<_>>();
        assert!(real_dirs(&root, &rel("Item/sneaky/evil")).is_err());
        assert!(real_dirs(&root, &rel("Item/sneaky")).is_err());
        assert!(!Path::new("/tmp/evil").exists());
        assert_eq!(real_dirs(&root, &rel("Item/fine/deeper")).unwrap(), root.join("Item/fine/deeper"));
        assert!(root.join("Item/fine/deeper").is_dir());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn received_items_are_quarantined() {
        let root = std::env::temp_dir().join(format!("kvm-quarantine-test-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        real_dirs(&root, &["My.app".to_string()]).unwrap();
        let out = std::process::Command::new("xattr")
            .args(["-p", "com.apple.quarantine"])
            .arg(root.join("My.app"))
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&out.stdout).starts_with("0081;"), "{out:?}");
        std::fs::remove_dir_all(&root).unwrap();
    }
}
