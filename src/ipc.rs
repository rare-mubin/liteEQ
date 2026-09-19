//! Unix-socket protocol between the TUI (client) and the background service.
//!
//! Line based; the client always speaks first and the service answers with one
//! `OK ...` line. Requests:
//!
//! ```text
//! INFO                -> OK <route>
//! SETTINGS            -> OK <wire settings>
//! SET <wire settings> -> OK
//! GET <last_written>  -> OK <status> <written> <nsamples> <m0>,<m1>,<m2>,<m3>,<m4>
//!                        followed by 2*nsamples i16 LE (input block, then output block)
//! QUIT                -> OK   (the service shuts down)
//! ```
//!
//! `GET` only carries samples when the audio moved on since `last_written`, so
//! an idle service costs a ~50 byte reply per poll.

use crate::audio::{Shared, Status};
use crate::dsp::BlockMeters;
use crate::settings::Settings;
use crate::viz::FFT_N;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

pub fn socket_path(name: &str) -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp"));
    if name == crate::audio::SINK_NAME {
        dir.join("liteeq.sock")
    } else {
        dir.join(format!("liteeq-{name}.sock"))
    }
}

fn encode_status(s: &Status) -> String {
    match s {
        Status::Starting => "starting".into(),
        Status::Running => "running".into(),
        Status::Stopped => "stopped".into(),
        Status::Error(e) => format!("error:{}", e.replace(char::is_whitespace, "_")),
    }
}

fn decode_status(s: &str) -> Status {
    match s {
        "starting" => Status::Starting,
        "running" => Status::Running,
        "stopped" => Status::Stopped,
        other => Status::Error(other.strip_prefix("error:").unwrap_or(other).replace('_', " ")),
    }
}

// ---------------------------------------------------------------------------
// Service side
// ---------------------------------------------------------------------------

pub enum Served {
    Continue,
    Quit,
}

/// Answer one request line.
pub fn handle_request(shared: &Shared, line: &str, out: &mut impl Write) -> io::Result<Served> {
    let (cmd, arg) = line.trim().split_once(' ').unwrap_or((line.trim(), ""));
    match cmd {
        "PING" => writeln!(out, "OK")?,
        "INFO" => writeln!(out, "OK {}", shared.route.lock().map(|r| r.clone()).unwrap_or_default())?,
        "SETTINGS" => writeln!(out, "OK {}", shared.settings.lock().map(|s| s.to_wire()).unwrap_or_default())?,
        "SET" => {
            if let Ok(mut s) = shared.settings.lock() {
                *s = Settings::from_wire(arg);
            }
            writeln!(out, "OK")?;
        }
        "GET" => {
            let last: u64 = arg.trim().parse().unwrap_or(0);
            let status = encode_status(&shared.status.lock().map(|s| s.clone()).unwrap_or(Status::Starting));
            let m = shared.meters.take();
            let (written, samples) = {
                let sc = shared.scope.lock().unwrap_or_else(|e| e.into_inner());
                (sc.written, (sc.written != last).then(|| sc.snapshot(FFT_N)))
            };
            let n = samples.as_ref().map_or(0, |(i, _)| i.len());
            writeln!(out, "OK {status} {written} {n} {},{},{},{},{}", m.in_l, m.in_r, m.out_l, m.out_r, m.harmonics)?;
            if let Some((input, output)) = samples {
                let mut bytes = Vec::with_capacity(n * 4);
                for v in input.iter().chain(output.iter()) {
                    bytes.extend_from_slice(&((v.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
                }
                out.write_all(&bytes)?;
            }
        }
        "QUIT" => {
            writeln!(out, "OK")?;
            out.flush()?;
            return Ok(Served::Quit);
        }
        _ => writeln!(out, "ERR unknown command")?,
    }
    out.flush()?;
    Ok(Served::Continue)
}

// ---------------------------------------------------------------------------
// Client side
// ---------------------------------------------------------------------------

/// One poll's worth of live data from the service.
pub struct Frame {
    pub status: Status,
    pub meters: BlockMeters,
    /// New audio for the visualizer, present only if it changed.
    pub samples: Option<(Vec<f32>, Vec<f32>)>,
}

pub struct Client {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    last_written: u64,
    pub route: String,
}

impl Client {
    pub fn connect(path: &std::path::Path) -> io::Result<Client> {
        let stream = UnixStream::connect(path)?;
        stream.set_read_timeout(Some(Duration::from_millis(1500)))?;
        stream.set_write_timeout(Some(Duration::from_millis(1500)))?;
        let mut c = Client { reader: BufReader::new(stream.try_clone()?), writer: stream, last_written: 0, route: String::new() };
        c.route = c.request("INFO")?;
        Ok(c)
    }

    fn request(&mut self, line: &str) -> io::Result<String> {
        writeln!(self.writer, "{line}")?;
        self.read_reply()
    }

    fn read_reply(&mut self) -> io::Result<String> {
        let mut reply = String::new();
        if self.reader.read_line(&mut reply)? == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "service closed the connection"));
        }
        match reply.trim_end().strip_prefix("OK") {
            Some(rest) => Ok(rest.trim_start().to_string()),
            None => Err(io::Error::other(reply.trim_end().to_string())),
        }
    }

    pub fn settings(&mut self) -> io::Result<Settings> {
        Ok(Settings::from_wire(&self.request("SETTINGS")?))
    }

    pub fn set(&mut self, s: &Settings) -> io::Result<()> {
        self.request(&format!("SET {}", s.to_wire())).map(|_| ())
    }

    pub fn quit(&mut self) {
        let _ = self.request("QUIT");
    }

    pub fn poll(&mut self) -> io::Result<Frame> {
        let head = self.request(&format!("GET {}", self.last_written))?;
        let mut it = head.split(' ');
        let bad = || io::Error::new(io::ErrorKind::InvalidData, "malformed GET reply");
        let status = decode_status(it.next().ok_or_else(bad)?);
        let written: u64 = it.next().and_then(|v| v.parse().ok()).ok_or_else(bad)?;
        let n: usize = it.next().and_then(|v| v.parse().ok()).ok_or_else(bad)?;
        let m: Vec<f32> = it.next().ok_or_else(bad)?.split(',').filter_map(|v| v.parse().ok()).collect();
        if m.len() != 5 || n > 1 << 20 {
            return Err(bad());
        }
        let samples = if n > 0 {
            let mut bytes = vec![0u8; n * 4];
            self.reader.read_exact(&mut bytes)?;
            let all: Vec<f32> = bytes.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32767.0).collect();
            let (i, o) = all.split_at(n);
            self.last_written = written;
            Some((i.to_vec(), o.to_vec()))
        } else {
            None
        };
        Ok(Frame { status, meters: BlockMeters { in_l: m[0], in_r: m[1], out_l: m[2], out_r: m[3], harmonics: m[4] }, samples })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_round_trips() {
        for s in [Status::Starting, Status::Running, Status::Stopped, Status::Error("no such device".into())] {
            assert_eq!(decode_status(&encode_status(&s)), s);
        }
    }

    #[test]
    fn service_and_client_talk_over_a_socket() {
        use std::os::unix::net::UnixListener;
        let dir = std::env::temp_dir().join(format!("liteeq-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let shared = Shared::new(Settings { amount_db: 5.0, ..Settings::default() });
        *shared.route.lock().unwrap() = "liteeq_sink → dev".into();
        *shared.status.lock().unwrap() = Status::Running;

        let server_shared = shared.clone();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut w = stream.try_clone().unwrap();
            for line in BufReader::new(stream).lines() {
                if let Served::Quit = handle_request(&server_shared, &line.unwrap(), &mut w).unwrap() {
                    break;
                }
            }
        });

        let mut c = Client::connect(&path).unwrap();
        assert_eq!(c.route, "liteeq_sink → dev");
        assert_eq!(c.settings().unwrap().amount_db, 5.0);
        c.set(&Settings { scope: 200.0, ..Settings::default() }).unwrap();
        assert_eq!(shared.settings.lock().unwrap().scope, 200.0);

        let f = c.poll().unwrap();
        assert_eq!(f.status, Status::Running);
        c.quit();
        server.join().unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }
}
