//! Launches the stock `scrcpy-server` on the device and connects to it.
//!
//! The installed `scrcpy` is only used to locate the server jar and to learn
//! its exact version: the server refuses to start if the client announces a
//! different one.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read};
use std::net::{Ipv4Addr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};

const DEVICE_SERVER_PATH: &str = "/data/local/tmp/scrcpy-server.jar";
const DEVICE_NAME_LEN: usize = 64;
const LOG_LINES: usize = 50;

pub struct Options {
    pub serial: Option<String>,
    pub select_usb: bool,
    pub select_emulator: bool,
    pub max_size: u16,
    pub max_fps: Option<String>,
}

pub struct Server {
    adb: String,
    serial: String,
    process: Child,
    /// `adb wait-for-disconnect`, which exits when the device goes away
    watcher: Option<Child>,
    log: Arc<Mutex<VecDeque<String>>>,
}

pub struct Connection {
    pub device_name: String,
    pub video: TcpStream,
    pub control: TcpStream,
}

impl Server {
    pub fn start(opts: &Options) -> Result<(Server, Connection)> {
        let adb = std::env::var("ADB").unwrap_or_else(|_| "adb".into());
        let (server_path, version) = locate_scrcpy()?;
        let serial = match &opts.serial {
            Some(s) => s.clone(),
            None if opts.select_usb => adb_select(&adb, "-d")?,
            None if opts.select_emulator => adb_select(&adb, "-e")?,
            None => single_device(&adb)?,
        };

        adb_run(&adb, &serial, &["push", path_str(&server_path)?, DEVICE_SERVER_PATH])
            .context("could not push scrcpy-server")?;

        let scid = random_scid();
        let socket_name = format!("localabstract:scrcpy_{scid:08x}");
        let port_out = adb_run(&adb, &serial, &["forward", "tcp:0", &socket_name])
            .context("could not set up adb forward tunnel")?;
        let port: u16 =
            port_out.trim().parse().with_context(|| format!("unexpected adb forward output: {port_out:?}"))?;

        let mut args = vec![
            "-s".to_string(),
            serial.clone(),
            "shell".into(),
            format!("CLASSPATH={DEVICE_SERVER_PATH}"),
            "app_process".into(),
            "/".into(),
            "com.genymobile.scrcpy.Server".into(),
            version,
            format!("scid={scid:08x}"),
            "log_level=info".into(),
            "audio=false".into(),
            "tunnel_forward=true".into(),
        ];
        if opts.max_size > 0 {
            args.push(format!("max_size={}", opts.max_size));
        }
        if let Some(fps) = &opts.max_fps {
            args.push(format!("max_fps={fps}"));
        }

        let mut process = Command::new(&adb)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("could not execute adb shell")?;

        let log = Arc::new(Mutex::new(VecDeque::new()));
        capture_log(process.stdout.take().unwrap(), log.clone());
        capture_log(process.stderr.take().unwrap(), log.clone());

        let mut server = Server { adb, serial, process, watcher: None, log };
        let conn = server.connect(port);
        // The tunnel is only needed to establish the connections
        let _ = adb_run(&server.adb, &server.serial, &["forward", "--remove", &format!("tcp:{port}")]);
        match conn {
            Ok(conn) => {
                // The sockets alone may stay open and silent when the device is
                // unplugged or powered off
                server.watcher = Command::new(&server.adb)
                    .args(["-s", &server.serial, "wait-for-disconnect"])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .ok();
                Ok((server, conn))
            }
            Err(e) => {
                let log = server.log().join("\n");
                bail!("{e:#}\n\nscrcpy-server output:\n{log}")
            }
        }
    }

    fn connect(&mut self, port: u16) -> Result<Connection> {
        let mut video = None;
        for _ in 0..100 {
            if let Some(status) = self.process.try_wait()? {
                bail!("scrcpy-server exited ({status})");
            }
            // adb accepts the connection even if nothing listens on the device
            // side yet, so wait for the dummy byte the server sends first
            if let Ok(mut s) = TcpStream::connect((Ipv4Addr::LOCALHOST, port)) {
                let mut byte = [0u8; 1];
                if s.read_exact(&mut byte).is_ok() {
                    video = Some(s);
                    break;
                }
            }
            thread::sleep(Duration::from_millis(100));
        }
        let mut video = video.context("could not connect to scrcpy-server")?;
        let control = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).context("could not open control socket")?;
        control.set_nodelay(true)?;
        // Never block input handling (and Ctrl+q) on a device that stopped reading
        control.set_write_timeout(Some(Duration::from_secs(2)))?;

        let mut name = [0u8; DEVICE_NAME_LEN];
        video.read_exact(&mut name).context("could not read device info")?;
        let end = name.iter().position(|&b| b == 0).unwrap_or(DEVICE_NAME_LEN);
        let device_name = String::from_utf8_lossy(&name[..end]).into_owned();

        Ok(Connection { device_name, video, control })
    }

    pub fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().iter().cloned().collect()
    }

    /// Fails once the device is disconnected or the server has stopped.
    pub fn check(&mut self) -> Result<()> {
        if let Some(w) = &mut self.watcher
            && w.try_wait()?.is_some()
        {
            bail!("device disconnected");
        }
        if let Some(status) = self.process.try_wait()? {
            bail!("scrcpy-server exited ({status})");
        }
        Ok(())
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        for child in std::iter::once(&mut self.process).chain(&mut self.watcher) {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Finds the server jar shipped with the installed scrcpy and its version.
fn locate_scrcpy() -> Result<(PathBuf, String)> {
    let out = Command::new("scrcpy").arg("--version").output().context("scrcpy not found in PATH")?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let version = stdout
        .lines()
        .next()
        .and_then(|l| l.strip_prefix("scrcpy "))
        .and_then(|l| l.split_whitespace().next())
        .context("could not parse `scrcpy --version` output")?
        .to_string();

    if let Ok(p) = std::env::var("SCRCPY_SERVER_PATH") {
        return Ok((p.into(), version));
    }

    let mut candidates = Vec::new();
    if let Some(bin) = find_in_path("scrcpy") {
        let bin = std::fs::canonicalize(&bin).unwrap_or(bin);
        if let Some(prefix) = bin.parent().and_then(Path::parent) {
            candidates.push(prefix.join("share/scrcpy/scrcpy-server"));
        }
    }
    candidates.extend(
        ["/opt/homebrew", "/usr/local", "/usr"].iter().map(|p| Path::new(p).join("share/scrcpy/scrcpy-server")),
    );
    let path =
        candidates.into_iter().find(|p| p.is_file()).context("scrcpy-server not found, set SCRCPY_SERVER_PATH")?;
    Ok((path, version))
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?).map(|dir| dir.join(name)).find(|p| p.is_file())
}

fn single_device(adb: &str) -> Result<String> {
    let out = Command::new(adb).arg("devices").output().context("could not execute adb")?;
    let devices: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .skip(1)
        .filter_map(|l| l.strip_suffix("\tdevice"))
        .map(str::to_string)
        .collect();
    match devices.as_slice() {
        [one] => Ok(one.clone()),
        [] => bail!("no adb device connected"),
        many => bail!("multiple devices connected, select one with -s:\n  {}", many.join("\n  ")),
    }
}

/// Resolves `adb -d` or `adb -e` to a serial, so that adb picks and reports
/// errors itself.
fn adb_select(adb: &str, flag: &str) -> Result<String> {
    let out = Command::new(adb).args([flag, "get-serialno"]).output().context("could not execute adb")?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn adb_run(adb: &str, serial: &str, args: &[&str]) -> Result<String> {
    let out = Command::new(adb).arg("-s").arg(serial).args(args).output()?;
    if !out.status.success() {
        bail!("adb {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn path_str(p: &Path) -> Result<&str> {
    p.to_str().context("non UTF-8 server path")
}

fn capture_log(stream: impl Read + Send + 'static, log: Arc<Mutex<VecDeque<String>>>) {
    thread::spawn(move || {
        for line in BufReader::new(stream).lines().map_while(Result::ok) {
            let mut log = log.lock().unwrap();
            if log.len() == LOG_LINES {
                log.pop_front();
            }
            log.push_back(line);
        }
    });
}

fn random_scid() -> u32 {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().subsec_nanos();
    (nanos ^ std::process::id().rotate_left(16)) & 0x7fff_ffff
}
