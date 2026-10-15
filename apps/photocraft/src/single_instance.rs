//! One window per user on Windows and Linux, as Photoshop does: opening a file from Explorer (or
//! `photocraft a.psd`) while PhotoCraft is already running opens it as a tab there instead of
//! starting a second window. macOS gets this from the OS (Apple events, `apple_events`).
//!
//! The first instance holds an exclusive lock on `instance.lock` in the settings folder for its
//! whole life and listens on a loopback port it writes to `instance`, with a random token. A later
//! launch that can't take the lock sends its (absolute) paths there as one JSON line,
//! `{"token":…,"paths":[…]}`, waits for `ok` and exits. The running app then opens them through
//! [`OsEvent::Open`], like a Finder double-click on macOS, and comes to the front. The lock is
//! released by the OS when the first instance exits, crashed or not, so a stale `instance` file
//! never blocks a start. Anything that goes wrong just opens a window of its own, as before.
//!
//! `--new-instance` or `PHOTOCRAFT_NEW_INSTANCE=1` opt out; a control-server start (`--control`)
//! always runs on its own, so agents and tests get the window they launched.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use photocraft_ui_egui::OsEvent;
use serde_json::{Value, json};

/// How long a later launch keeps trying to reach the first instance (which may itself still be
/// starting, e.g. when Explorer launches one process per selected file at once).
const HANDOFF_WAIT: Duration = Duration::from_secs(10);
/// The most a handoff message may be: plenty for thousands of paths.
const MAX_MESSAGE: u64 = 1 << 20;
const MAX_PATHS: usize = 4096;

/// What `main` does after [`start`].
pub enum Start {
    /// The running instance took the files: exit now.
    HandedOff,
    /// Open a window. `Some`: this is the first instance, receiving later launches' files.
    Run(Option<Primary>),
}

/// The first instance: holds the lock and receives later launches' paths until dropped.
pub struct Primary {
    _lock: File,
    inbox: Arc<Inbox>,
}

#[derive(Default)]
struct Inbox {
    closed: AtomicBool,
    state: Mutex<InboxState>,
}

#[derive(Default)]
struct InboxState {
    opens: Vec<Vec<String>>,
    wake: Option<Box<dyn Fn() + Send>>,
}

impl Inbox {
    fn push(&self, paths: Vec<String>) {
        let mut s = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        s.opens.push(paths);
        if let Some(wake) = &s.wake {
            wake();
        }
    }
}

impl Primary {
    /// The queue the UI drains (see `Services::os_events`); a later launch's files wake `ctx`.
    pub fn connect(&self, ctx: &egui::Context) -> photocraft_ui_egui::OsEventsFn {
        let ctx = ctx.clone();
        let inbox = self.inbox.clone();
        {
            let mut s = inbox.state.lock().unwrap_or_else(PoisonError::into_inner);
            s.wake = Some(Box::new(move || ctx.request_repaint()));
        }
        Box::new(move || {
            let opens = std::mem::take(&mut inbox.state.lock().unwrap_or_else(PoisonError::into_inner).opens);
            opens.into_iter().map(OsEvent::Open).collect()
        })
    }
}

impl Drop for Primary {
    /// Stops accepting: a launch arriving now must open its own window, not hand off to an
    /// instance on its way out (the CPU-fallback relaunch in `main` drops this first).
    fn drop(&mut self) {
        self.inbox.closed.store(true, Ordering::SeqCst);
    }
}

/// Hands `files` to a running instance, or becomes the instance later launches hand off to.
pub fn start(dir: Option<&Path>, files: &[String]) -> Start {
    let Some(dir) = dir else { return Start::Run(None) };
    if let Err(e) = std::fs::create_dir_all(dir) {
        log::warn!("single instance: {e}");
        return Start::Run(None);
    }
    let lock_path = dir.join("instance.lock");
    let info_path = dir.join("instance");
    let paths: Vec<String> = files.iter().map(|f| absolute(f)).collect();
    let deadline = Instant::now() + HANDOFF_WAIT;
    loop {
        let lock = match OpenOptions::new().create(true).truncate(false).write(true).open(&lock_path) {
            Ok(f) => f,
            Err(e) => {
                log::warn!("single instance: {}: {e}", lock_path.display());
                return Start::Run(None);
            }
        };
        match lock.try_lock() {
            Ok(()) => return Start::Run(become_primary(lock, &info_path)),
            Err(std::fs::TryLockError::WouldBlock) => {}
            Err(std::fs::TryLockError::Error(e)) => {
                log::warn!("single instance: can't lock {}: {e}", lock_path.display());
                return Start::Run(None);
            }
        }
        drop(lock);
        match hand_off(&info_path, &paths) {
            Ok(()) => {
                log::info!("handed {} file(s) to the running PhotoCraft", paths.len());
                return Start::HandedOff;
            }
            Err(e) if Instant::now() >= deadline => {
                log::warn!("single instance: the running PhotoCraft didn't answer ({e}); opening a new window");
                return Start::Run(None);
            }
            // Not listening yet (still starting) or on its way out (then the lock frees up).
            Err(_) => std::thread::sleep(Duration::from_millis(100)),
        }
    }
}

fn absolute(file: &str) -> String {
    std::path::absolute(file).map_or_else(|_| file.to_owned(), |p| p.to_string_lossy().into_owned())
}

/// Listens for later launches; `None` (no handoffs, but still a window) if that can't be set up.
fn become_primary(lock: File, info_path: &Path) -> Option<Primary> {
    let listener = match TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))) {
        Ok(l) => l,
        Err(e) => {
            log::warn!("single instance: can't listen: {e}");
            return None;
        }
    };
    let port = listener.local_addr().ok()?.port();
    let token = match new_token() {
        Ok(t) => t,
        Err(e) => {
            log::warn!("single instance: {e}");
            return None;
        }
    };
    if let Err(e) = write_info(info_path, port, &token) {
        log::warn!("single instance: {}: {e}", info_path.display());
        return None;
    }
    let inbox = Arc::new(Inbox::default());
    let receiving = inbox.clone();
    let spawned = std::thread::Builder::new().name("single-instance".into()).spawn(move || {
        for stream in listener.incoming() {
            if receiving.closed.load(Ordering::SeqCst) {
                break;
            }
            let Ok(stream) = stream else { continue };
            // Accepted only while still open: on the way out, no `ok`, so the sender opens a
            // window of its own rather than handing its files to an instance about to exit.
            match receive(stream, &token, &receiving.closed) {
                Ok(paths) => receiving.push(paths),
                Err(e) => log::warn!("single instance: rejected a handoff: {e}"),
            }
        }
    });
    if let Err(e) = spawned {
        log::warn!("single instance: {e}");
        return None;
    }
    Some(Primary { _lock: lock, inbox })
}

fn new_token() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| format!("no random token: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Writes `port` and `token` beside the lock, replacing the file whole.
fn write_info(path: &Path, port: u16, token: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, json!({"port": port, "token": token, "pid": std::process::id()}).to_string())?;
    std::fs::rename(&tmp, path)
}

/// The running instance's port, token and process id (`None` in a file written before it was kept).
fn read_info(path: &Path) -> Result<(u16, String, Option<u32>), String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let v: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    let port = v.get("port").and_then(Value::as_u64).and_then(|p| u16::try_from(p).ok()).ok_or("no port")?;
    let token = v.get("token").and_then(Value::as_str).ok_or("no token")?;
    let pid = v.get("pid").and_then(Value::as_u64).and_then(|p| u32::try_from(p).ok());
    Ok((port, token.to_owned(), pid))
}

/// Sends `paths` to the running instance; `Ok` once it has accepted them.
fn hand_off(info_path: &Path, paths: &[String]) -> Result<(), String> {
    let (port, token, pid) = read_info(info_path)?;
    // Windows only lets the app the user just launched take the foreground: pass that right on,
    // so the running window can come to the front instead of just flashing in the taskbar.
    #[cfg(not(windows))]
    let _ = pid;
    #[cfg(windows)]
    let _ = winsafe::AllowSetForegroundWindow(Some(pid.unwrap_or(u32::MAX)));
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(500)).map_err(|e| e.to_string())?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).map_err(|e| e.to_string())?;
    stream.set_write_timeout(Some(Duration::from_secs(5))).map_err(|e| e.to_string())?;
    writeln!(stream, "{}", json!({"token": token, "paths": paths})).map_err(|e| e.to_string())?;
    let mut reply = String::new();
    BufReader::new(stream.take(16)).read_line(&mut reply).map_err(|e| e.to_string())?;
    if reply.trim() == "ok" { Ok(()) } else { Err(format!("unexpected reply {reply:?}")) }
}

/// Reads one handoff from `stream`, answering `ok` if it carries `token`.
fn receive(mut stream: TcpStream, token: &str, closed: &AtomicBool) -> Result<Vec<String>, String> {
    stream.set_read_timeout(Some(Duration::from_secs(2))).map_err(|e| e.to_string())?;
    stream.set_write_timeout(Some(Duration::from_secs(2))).map_err(|e| e.to_string())?;
    let mut line = String::new();
    BufReader::new((&stream).take(MAX_MESSAGE)).read_line(&mut line).map_err(|e| e.to_string())?;
    let paths = parse_message(&line, token)?;
    if closed.load(Ordering::SeqCst) {
        return Err("closing".into());
    }
    stream.write_all(b"ok\n").map_err(|e| e.to_string())?;
    Ok(paths)
}

/// The paths in a handoff line, if it carries `token`.
fn parse_message(line: &str, token: &str) -> Result<Vec<String>, String> {
    let v: Value = serde_json::from_str(line.trim()).map_err(|e| format!("not JSON: {e}"))?;
    if v.get("token").and_then(Value::as_str) != Some(token) {
        return Err("wrong token".into());
    }
    let paths = v.get("paths").and_then(Value::as_array).ok_or("no paths")?;
    Ok(paths.iter().filter_map(Value::as_str).take(MAX_PATHS).map(str::to_owned).collect())
}

/// Whether this start should take part (see the module docs).
pub fn wanted(control: bool, new_instance_flag: bool) -> bool {
    !cfg!(target_os = "macos")
        && !control
        && !new_instance_flag
        && std::env::var_os("PHOTOCRAFT_NEW_INSTANCE").is_none_or(|v| v.is_empty() || v == "0")
}

/// The settings folder the lock lives in.
pub fn dir() -> Option<PathBuf> {
    crate::services::config_dir()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("photocraft-single-instance-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_second_start_hands_its_files_to_the_first() {
        let dir = temp_dir("handoff");
        let Start::Run(Some(primary)) = start(Some(&dir), &[]) else { panic!("the first start runs and listens") };
        let ctx = egui::Context::default();
        let mut drain = primary.connect(&ctx);
        let file = dir.join("a.psd").to_string_lossy().into_owned();
        assert!(matches!(start(Some(&dir), std::slice::from_ref(&file)), Start::HandedOff));
        assert_eq!(drain(), vec![OsEvent::Open(vec![file])]);
        assert!(drain().is_empty());
        // No files: just brings the running window forward.
        assert!(matches!(start(Some(&dir), &[]), Start::HandedOff));
        assert_eq!(drain(), vec![OsEvent::Open(vec![])]);
        drop(primary);
        // Gone: the next start is the first instance again (the stale `instance` file is ignored).
        assert!(matches!(start(Some(&dir), &[]), Start::Run(Some(_))));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn relative_paths_are_made_absolute() {
        let p = absolute("pic.png");
        assert!(Path::new(&p).is_absolute(), "{p}");
    }

    #[test]
    fn hostile_handoffs_are_rejected_without_panicking() {
        for line in ["", "garbage", "{}", r#"{"token":"x","paths":[]}"#, r#"{"token":"t"}"#, r#"{"token":"t","paths":"a"}"#, "\u{0}\u{ffff}"] {
            assert!(parse_message(line, "t").is_err(), "{line:?}");
        }
        assert_eq!(parse_message(r#"{"token":"t","paths":["a",1,null,"b"]}"#, "t"), Ok(vec!["a".into(), "b".into()]));
        let many = json!({"token": "t", "paths": vec!["p"; MAX_PATHS + 10]}).to_string();
        assert_eq!(parse_message(&many, "t").map(|p| p.len()), Ok(MAX_PATHS));
    }

    #[test]
    fn a_corrupt_instance_file_is_an_error_not_a_crash() {
        let dir = temp_dir("corrupt");
        std::fs::create_dir_all(&dir).unwrap();
        let info = dir.join("instance");
        for text in ["", "{", r#"{"port":99999,"token":"t"}"#, r#"{"port":1}"#] {
            std::fs::write(&info, text).unwrap();
            assert!(read_info(&info).is_err(), "{text:?}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
