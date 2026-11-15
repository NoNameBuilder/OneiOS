//! `oneios-tool serve` as the OneOS app uses it (README "serve"): the process is started, requests are written to
//! its standard input and its lines are read. What is checked: the lines' form, the order of a request's events,
//! the queue, progress, every way a request and the process end (a cancel request, the end of the pipe in the
//! middle of a request, SIGTERM, the idle timeout, a reader that went away), that a stop removes the temporary
//! files, that nothing is left behind (`bye`'s threads and children, and the process is gone), the refusals with
//! their codes, the heat and battery limits on stand-in files, and each command on stand-in inputs.
//!
//! No Apple file and no phone: the stage, the phone's /sys and /proc files and the inputs are made here.
//! `ONEIOS_TOOL=PATH` runs another binary than this build's.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

fn tool() -> String {
    std::env::var("ONEIOS_TOOL").ok().filter(|t| !t.is_empty()).unwrap_or_else(|| env!("CARGO_BIN_EXE_oneios-tool").to_string())
}

/// A folder of this test, with `stage` and `sys` (the stand-in of the phone's files) in it.
struct Box {
    top: PathBuf,
}

impl Box {
    fn new(name: &str) -> Box {
        let top = std::env::temp_dir().join(format!("oneios-serve-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&top);
        std::fs::create_dir_all(top.join("stage")).unwrap();
        std::fs::create_dir_all(top.join("sys")).unwrap();
        Box { top }
    }
    fn stage(&self, rel: &str) -> String {
        self.top.join("stage").join(rel).to_string_lossy().into_owned()
    }
    fn put(&self, rel: &str, data: &[u8]) -> String {
        let path = self.top.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, data).unwrap();
        path.to_string_lossy().into_owned()
    }
    /// A file of the stand-in phone (`sys/...`).
    fn sys(&self, rel: &str, text: &str) {
        self.put(&format!("sys/{}", rel.trim_start_matches('/')), text.as_bytes());
    }
}

impl Drop for Box {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            let _ = std::fs::remove_dir_all(&self.top);
        }
    }
}

struct Serve {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<Value>,
    /// Every line read so far.
    seen: Vec<Value>,
    stderr: Option<std::thread::JoinHandle<String>>,
}

const LONG: Duration = Duration::from_secs(60);

impl Serve {
    fn start(b: &Box, options: &[&str]) -> Serve {
        let mut child = Command::new(tool())
            .arg("serve")
            .args(["--stage", &b.stage(""), "--sys-root", &b.top.join("sys").to_string_lossy()])
            .args(options)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("oneios-tool serve starts");
        let (tx, lines) = channel();
        let out = child.stdout.take().unwrap();
        std::thread::spawn(move || {
            for line in BufReader::new(out).lines() {
                let Ok(line) = line else { break };
                assert!(line.starts_with("{\"ev\":"), "every line of the tool begins with {{\"ev\": : {line}");
                let value: Value = serde_json::from_str(&line).unwrap_or_else(|e| panic!("not JSON ({e}): {line}"));
                if tx.send(value).is_err() {
                    break;
                }
            }
        });
        let mut err = child.stderr.take().unwrap();
        let stderr = std::thread::spawn(move || {
            let mut text = String::new();
            let _ = err.read_to_string(&mut text);
            text
        });
        let stdin = child.stdin.take();
        let mut serve = Serve { child, stdin, lines, seen: Vec::new(), stderr: Some(stderr) };
        let hello = serve.next();
        assert_eq!(hello["ev"], "hello", "the first line");
        serve
    }

    fn send(&mut self, request: Value) {
        let stdin = self.stdin.as_mut().expect("the pipe is open");
        writeln!(stdin, "{request}").unwrap();
        stdin.flush().unwrap();
    }

    fn pid(&self) -> i32 {
        self.child.id() as i32
    }

    fn next_in(&mut self, time: Duration) -> Option<Value> {
        match self.lines.recv_timeout(time) {
            Ok(value) => {
                self.seen.push(value.clone());
                Some(value)
            }
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => None,
        }
    }

    fn next(&mut self) -> Value {
        self.next_in(LONG).unwrap_or_else(|| panic!("no line in {LONG:?}; seen: {:#?}", self.seen))
    }

    /// The lines up to and with the first one that `is` takes.
    fn until(&mut self, is: impl Fn(&Value) -> bool) -> Vec<Value> {
        let mut got = Vec::new();
        loop {
            let line = self.next();
            let hit = is(&line);
            got.push(line);
            if hit {
                return got;
            }
        }
    }

    /// The events of request `id` up to its last one (other requests' lines are skipped).
    fn request(&mut self, id: &str) -> Vec<Value> {
        self.until(|v| v["id"] == id && is_last(v)).into_iter().filter(|v| v["id"] == id).collect()
    }

    /// Send a request and return its last event.
    fn ask(&mut self, request: Value) -> Value {
        let id = request["id"].as_str().map(str::to_string).unwrap_or_else(|| request["id"].to_string());
        self.send(request);
        self.request(&id).pop().unwrap()
    }

    /// Close the pipe (or not), wait for the process: its status, the lines that still came, its stderr.
    fn end(mut self, close: bool) -> (i32, Vec<Value>, String) {
        if close {
            self.stdin = None;
        }
        let until = Instant::now() + LONG;
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < until, "the process did not end; seen: {:#?}", self.seen);
            std::thread::sleep(Duration::from_millis(20));
        };
        let mut rest = Vec::new();
        while let Some(line) = self.next_in(Duration::from_secs(5)) {
            rest.push(line);
        }
        let stderr = self.stderr.take().unwrap().join().unwrap();
        (status.code().unwrap_or(-1), rest, stderr)
    }
}

fn is_last(v: &Value) -> bool {
    matches!(v["ev"].as_str(), Some("ok" | "refused" | "failed" | "cancelled"))
}

fn names(events: &[Value]) -> Vec<&str> {
    events.iter().map(|v| v["ev"].as_str().unwrap()).collect()
}

/// `bye` says nothing is left: one thread, no child.
fn clean_bye(rest: &[Value], why: &str) -> Value {
    let bye = rest.last().unwrap_or_else(|| panic!("no bye line")).clone();
    assert_eq!((bye["ev"].as_str(), bye["why"].as_str()), (Some("bye"), Some(why)), "{bye}");
    assert_eq!(bye["children"], 0, "{bye}");
    if cfg!(any(target_os = "linux", target_os = "android")) {
        assert_eq!(bye["threads"], 1, "only the main thread is left: {bye}");
    }
    assert!(bye.get("forced").is_none(), "{bye}");
    bye
}

fn sha256(data: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(data).iter().map(|b| format!("{b:02x}")).collect()
}

fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E3779B97F4A7C15) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

// ---------------------------------------------------------------- the lines, the queue, progress

#[test]
fn hello_ping_and_the_end_of_the_pipe() {
    let b = Box::new("hello");
    let mut s = Serve::start(&b, &["--jobs", "3", "--nice", "12"]);
    let hello = s.seen[0].clone();
    assert_eq!((hello["proto"].as_u64(), hello["jobs"].as_u64(), hello["nice"].as_i64(), hello["realtime"].as_bool()), (Some(1), Some(3), Some(12), Some(false)), "{hello}");
    assert_eq!(hello["pid"].as_i64(), Some(s.pid() as i64));
    assert_eq!((hello["idle"].as_u64(), hello["battery_min"].as_u64()), (Some(300), Some(10)));
    assert_eq!(hello["thermal"], json!([44.0, 46.0, 45.0, 50.0]));
    let cmds: Vec<&str> = hello["cmds"].as_array().unwrap().iter().map(|c| c.as_str().unwrap()).collect();
    for cmd in ["ping", "health", "cancel", "wait", "download", "zip-ls", "zip-get", "fetch", "im4p", "receive", "sums", "hashes", "verify", "apfs-ls", "apfs-copy", "tweaks", "prelink", "stubs-check", "package-check", "ioslog-boots", "ioslog-show", "log-list", "log", "cpu"] {
        assert!(cmds.contains(&cmd), "hello's cmds have no {cmd}: {cmds:?}");
    }
    let ok = s.ask(json!({"id": "p-1", "cmd": "ping"}));
    assert_eq!((ok["ev"].as_str(), ok["cmd"].as_str(), ok["id"].as_str()), (Some("ok"), Some("ping"), Some("p-1")));
    // an id may be a whole number; it comes back as text
    assert_eq!(s.ask(json!({"id": 7, "cmd": "ping"}))["id"], "7");
    let (status, rest, stderr) = s.end(true);
    let bye = clean_bye(&rest, "eof");
    assert_eq!((status, bye["requests"].as_u64(), bye["cancelled"].as_u64()), (0, Some(2), Some(0)));
    assert_eq!(stderr, "", "the tool's standard error stays empty");
}

#[test]
fn a_request_has_steps_progress_and_its_own_threads() {
    let b = Box::new("progress");
    let mut s = Serve::start(&b, &["--jobs", "4"]);
    s.send(json!({"id": "w", "cmd": "wait", "seconds": 1.3}));
    let events = s.request("w");
    assert_eq!(names(&events)[..2], ["start", "step"], "{events:#?}");
    assert_eq!(events[0]["steps"], json!([{"step": "wait", "text": "Waiting"}]));
    assert_eq!((events[0]["jobs"].as_u64(), events[0]["cmd"].as_str()), (Some(4), Some("wait")));
    assert_eq!((events[1]["step"].as_str(), events[1]["n"].as_u64(), events[1]["of"].as_u64(), events[1]["text"].as_str()), (Some("wait"), Some(1), Some(1), Some("Waiting")));
    let progress: Vec<&Value> = events.iter().filter(|v| v["ev"] == "progress").collect();
    // the first and the last line of the step, and at most four a second between
    assert!((2..=9).contains(&progress.len()), "{} progress lines in 1.3 s", progress.len());
    assert_eq!((progress[0]["done"].as_u64(), progress[0]["total"].as_u64(), progress[0]["unit"].as_str(), progress[0]["percent"].as_f64()), (Some(0), Some(1300), Some("ms"), Some(0.0)));
    let last = progress.last().unwrap();
    assert_eq!((last["done"].as_u64(), last["percent"].as_f64()), (Some(1300), Some(100.0)), "{last}");
    let done: Vec<u64> = progress.iter().map(|p| p["done"].as_u64().unwrap()).collect();
    assert!(done.windows(2).all(|w| w[0] <= w[1]), "progress never goes back: {done:?}");
    // after a second of work a rate and an ETA are known
    let late = progress.iter().find(|p| p["done"].as_u64().unwrap() >= 1000 && p["done"].as_u64().unwrap() < 1300);
    if let Some(late) = late {
        assert!(late["rate"].as_u64().is_some_and(|r| (500..=2000).contains(&r)) && late["eta"].as_u64().is_some_and(|e| e <= 2), "{late}");
    }
    let ok = events.last().unwrap();
    assert_eq!((ok["ev"].as_str(), ok["threads"].as_u64(), ok["realtime"].as_bool()), (Some("ok"), Some(4), Some(false)), "{ok}");
    assert!(ok["nice"].as_i64().unwrap() >= 10 && ok["seconds"].as_f64().unwrap() >= 1.2, "the workers run at the process's nice: {ok}");
    // a request's own "jobs" lowers the count, and cannot raise it
    assert_eq!(s.ask(json!({"id": "w2", "cmd": "wait", "seconds": 0.05, "jobs": 2}))["threads"], 2);
    assert_eq!(s.ask(json!({"id": "w3", "cmd": "wait", "seconds": 0.05, "jobs": 8}))["threads"], 4);
    assert_eq!(s.ask(json!({"id": "w4", "cmd": "wait", "seconds": 0.05, "jobs": 9}))["code"], "usage");
    let (status, rest, _) = s.end(true);
    clean_bye(&rest, "eof");
    assert_eq!(status, 0);
}

#[test]
fn requests_wait_in_a_queue_and_ping_does_not() {
    let b = Box::new("queue");
    let mut s = Serve::start(&b, &[]);
    s.send(json!({"id": "a", "cmd": "wait", "seconds": 0.8}));
    s.send(json!({"id": "b", "cmd": "wait", "seconds": 0.1}));
    s.send(json!({"id": "c", "cmd": "wait", "seconds": 0.1}));
    s.send(json!({"id": "p", "cmd": "ping"}));
    let lines = s.until(|v| v["id"] == "c" && is_last(v));
    let at = |id: &str, ev: &str| lines.iter().position(|v| v["id"] == id && v["ev"] == ev).unwrap_or_else(|| panic!("no {ev} of {id}: {lines:#?}"));
    assert_eq!((lines[at("b", "queued")]["ahead"].as_u64(), lines[at("c", "queued")]["ahead"].as_u64()), (Some(1), Some(2)));
    assert!(lines.iter().all(|v| !(v["id"] == "a" && v["ev"] == "queued")), "the first request does not wait");
    // ping is answered while a runs; b starts only after a's last event, c after b's
    assert!(at("p", "ok") < at("a", "ok"), "{lines:#?}");
    assert!(at("a", "ok") < at("b", "start") && at("b", "ok") < at("c", "start"), "{lines:#?}");
    // a second request with a live id is not taken, and the first goes on
    s.send(json!({"id": "d", "cmd": "wait", "seconds": 0.4}));
    s.send(json!({"id": "d", "cmd": "ping"}));
    let lines = s.until(|v| v["id"] == "d" && is_last(v));
    let error = lines.iter().find(|v| v["ev"] == "error").expect("an error line");
    assert_eq!((error["code"].as_str(), error["id"].as_str()), (Some("duplicate"), Some("d")));
    assert_eq!(lines.last().unwrap()["ev"], "ok");
    // the id is free again after its last event
    assert_eq!(s.ask(json!({"id": "d", "cmd": "ping"}))["ev"], "ok");
    let (status, rest, _) = s.end(true);
    assert_eq!((status, clean_bye(&rest, "eof")["requests"].as_u64()), (0, Some(6)));
}

#[test]
fn the_queue_is_bounded() {
    let b = Box::new("busy");
    let mut s = Serve::start(&b, &[]);
    s.send(json!({"id": "run", "cmd": "wait", "seconds": 30}));
    for i in 0..33 {
        s.send(json!({"id": format!("q{i}"), "cmd": "wait", "seconds": 30}));
    }
    let lines = s.until(|v| v["id"] == "q32" && is_last(v));
    let last = lines.last().unwrap();
    assert_eq!((last["ev"].as_str(), last["code"].as_str()), (Some("refused"), Some("busy")), "the 33rd waiting request: {last}");
    assert_eq!(lines.iter().filter(|v| v["ev"] == "queued").count(), 32);
    // the end of the pipe: the running one is cancelled, the 32 waiting ones are dropped, each with its last event
    let (status, rest, _) = s.end(true);
    let cancelled: Vec<&Value> = rest.iter().filter(|v| v["ev"] == "cancelled").collect();
    assert_eq!(cancelled.len(), 33, "{rest:#?}");
    assert!(cancelled.iter().all(|v| v["by"] == "eof"));
    assert_eq!((status, clean_bye(&rest, "eof")["cancelled"].as_u64()), (5, Some(33)));
}

// ---------------------------------------------------------------- the ways it ends

/// A `wait` of 60 s with a temporary file in the stage, started and seen to run.
fn long_wait(s: &mut Serve, b: &Box, id: &str) -> String {
    let file = b.stage(&format!("work/{id}.tmp"));
    s.send(json!({"id": id, "cmd": "wait", "seconds": 60, "file": file}));
    s.until(|v| v["id"] == id && v["ev"] == "progress");
    assert!(Path::new(&file).is_file(), "the request's temporary file is there while it runs");
    file
}

#[test]
fn a_cancel_request_stops_the_running_request_and_removes_its_file() {
    let b = Box::new("cancel");
    let mut s = Serve::start(&b, &[]);
    let file = long_wait(&mut s, &b, "long");
    s.send(json!({"id": "next", "cmd": "wait", "seconds": 0.05}));
    let began = Instant::now();
    s.send(json!({"cmd": "cancel", "id": "long"}));
    let events = s.request("long");
    let last = events.last().unwrap();
    assert_eq!((last["ev"].as_str(), last["by"].as_str(), last["removed"].as_u64(), last["cmd"].as_str()), (Some("cancelled"), Some("request"), Some(1), Some("wait")), "{last}");
    assert!(began.elapsed() < Duration::from_secs(8), "a cancel is quick: {:?}", began.elapsed());
    assert!(!Path::new(&file).exists(), "the temporary file is gone");
    // the queue goes on with the next request
    assert_eq!(s.request("next").last().unwrap()["ev"], "ok");
    // a waiting request is taken out of the queue; a cancel of an id that is not there is an error line
    let file = long_wait(&mut s, &b, "long2");
    s.send(json!({"id": "waiting", "cmd": "wait", "seconds": 60}));
    s.send(json!({"cmd": "cancel", "id": "waiting"}));
    let last = s.request("waiting").pop().unwrap();
    assert_eq!((last["ev"].as_str(), last["by"].as_str(), last["removed"].as_u64()), (Some("cancelled"), Some("request"), Some(0)), "{last}");
    assert!(last["text"].as_str().unwrap().contains("was waiting"));
    s.send(json!({"cmd": "cancel", "id": "nobody"}));
    let error = s.until(|v| v["ev"] == "error").pop().unwrap();
    assert_eq!((error["code"].as_str(), error["id"].as_str()), (Some("no-such-request"), Some("nobody")));
    s.send(json!({"cmd": "cancel", "id": "long2"}));
    assert_eq!(s.request("long2").pop().unwrap()["ev"], "cancelled");
    assert!(!Path::new(&file).exists());
    // nothing after a request's last event carries its id
    assert_eq!(s.ask(json!({"id": "after", "cmd": "ping"}))["ev"], "ok");
    let (status, rest, stderr) = s.end(true);
    let bye = clean_bye(&rest, "eof");
    assert_eq!((status, bye["cancelled"].as_u64(), stderr.as_str()), (0, Some(3), ""), "the end cancelled nothing: status 0");
    for id in ["long", "long2", "waiting"] {
        let all: Vec<&Value> = s_seen(&rest, id);
        assert!(all.is_empty(), "{id} has lines after its last event: {all:?}");
    }
}

fn s_seen<'a>(lines: &'a [Value], id: &str) -> Vec<&'a Value> {
    lines.iter().filter(|v| v["id"] == id).collect()
}

#[test]
fn the_end_of_the_pipe_in_the_middle_of_a_request() {
    let b = Box::new("eof");
    let mut s = Serve::start(&b, &[]);
    let file = long_wait(&mut s, &b, "long");
    s.send(json!({"id": "waiting", "cmd": "wait", "seconds": 60}));
    s.until(|v| v["id"] == "waiting" && v["ev"] == "queued");
    let pid = s.pid();
    let began = Instant::now();
    let (status, rest, stderr) = s.end(true);
    assert!(began.elapsed() < Duration::from_secs(9), "the process ends soon after the pipe: {:?}", began.elapsed());
    let last = |id: &str| rest.iter().find(|v| v["id"] == id && is_last(v)).unwrap_or_else(|| panic!("no last event of {id}: {rest:#?}")).clone();
    assert_eq!((last("long")["ev"].as_str(), last("long")["by"].as_str(), last("long")["removed"].as_u64()), (Some("cancelled"), Some("eof"), Some(1)));
    assert_eq!((last("waiting")["ev"].as_str(), last("waiting")["by"].as_str()), (Some("cancelled"), Some("eof")));
    let bye = clean_bye(&rest, "eof");
    assert_eq!((status, bye["cancelled"].as_u64(), stderr.as_str()), (5, Some(2), ""));
    assert!(!Path::new(&file).exists(), "the temporary file is gone");
    // the process is gone, and with it every thread
    assert!(!Path::new(&format!("/proc/{pid}")).exists() || !cfg!(target_os = "linux"));
}

#[test]
fn sigterm_in_the_middle_of_a_request() {
    let b = Box::new("sigterm");
    let mut s = Serve::start(&b, &[]);
    let file = long_wait(&mut s, &b, "long");
    let pid = s.pid();
    unsafe { libc::kill(pid, libc::SIGTERM) };
    let (status, rest, stderr) = s.end(false);
    let last = rest.iter().find(|v| v["id"] == "long" && is_last(v)).expect("the request's last event");
    assert_eq!((last["ev"].as_str(), last["by"].as_str(), last["removed"].as_u64()), (Some("cancelled"), Some("sigterm"), Some(1)), "{last}");
    clean_bye(&rest, "sigterm");
    assert_eq!((status, stderr.as_str()), (5, ""));
    assert!(!Path::new(&file).exists());
    // with nothing running: bye at once, status 0
    let s = Serve::start(&b, &[]);
    unsafe { libc::kill(s.pid(), libc::SIGTERM) };
    let (status, rest, _) = s.end(false);
    assert_eq!((status, clean_bye(&rest, "sigterm")["requests"].as_u64()), (0, Some(0)));
}

#[test]
fn the_idle_timeout() {
    let b = Box::new("idle");
    // nothing asked: the process ends after --idle seconds
    let began = Instant::now();
    let s = Serve::start(&b, &["--idle", "1"]);
    let (status, rest, _) = s.end(false);
    assert_eq!(status, 0);
    clean_bye(&rest, "idle");
    assert!((Duration::from_millis(900)..Duration::from_secs(8)).contains(&began.elapsed()), "{:?}", began.elapsed());
    // a request that runs longer than the idle time is not cut; the time counts from its end
    let mut s = Serve::start(&b, &["--idle", "1"]);
    let began = Instant::now();
    assert_eq!(s.ask(json!({"id": "w", "cmd": "wait", "seconds": 1.6}))["ev"], "ok");
    let ended = began.elapsed();
    let (status, rest, _) = s.end(false);
    assert_eq!((status, clean_bye(&rest, "idle")["requests"].as_u64()), (0, Some(1)));
    assert!(began.elapsed() >= ended + Duration::from_millis(900), "idle counts from the request's end: {:?} then {:?}", ended, began.elapsed());
    // --idle 0: never (the pipe's end ends it)
    let mut s = Serve::start(&b, &["--idle", "0"]);
    assert!(s.next_in(Duration::from_millis(1500)).is_none(), "no bye without a reason");
    let (status, rest, _) = s.end(true);
    assert_eq!(status, 0);
    clean_bye(&rest, "eof");
}

#[test]
fn a_reader_that_went_away() {
    let b = Box::new("stdout");
    let mut child = Command::new(tool()).arg("serve").args(["--stage", &b.stage("")]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap());
    let mut hello = String::new();
    out.read_line(&mut hello).unwrap();
    let file = b.stage("work/gone.tmp");
    writeln!(stdin, "{}", json!({"id": "long", "cmd": "wait", "seconds": 60, "file": file})).unwrap();
    let mut line = String::new();
    while !line.contains("\"progress\"") {
        line.clear();
        out.read_line(&mut line).unwrap();
    }
    assert!(Path::new(&file).is_file());
    // the app is gone: nobody reads; the next progress line cannot be written
    drop(out);
    let until = Instant::now() + LONG;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < until, "the process did not end when its reader went away");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(status.code(), Some(5));
    assert!(!Path::new(&file).exists(), "the temporary file is gone");
    drop(stdin);
}

// ---------------------------------------------------------------- refusals

#[test]
fn refusals_say_why_with_a_code() {
    let b = Box::new("refusals");
    let mut s = Serve::start(&b, &[]);
    let outside = b.top.join("elsewhere/x").to_string_lossy().into_owned();
    let image = b.put("in/a.img", b"x");
    for (request, code, words) in [
        (json!({"id": "1", "cmd": "format-the-phone"}), "unknown", "no command"),
        (json!({"id": "2", "cmd": "sums"}), "usage", "\"dir\" is missing"),
        (json!({"id": "3", "cmd": "wait", "secnods": 1}), "usage", "no key \"secnods\""),
        (json!({"id": "4", "cmd": "wait", "seconds": "one"}), "usage", "must be a number"),
        (json!({"id": "5", "cmd": "wait", "file": outside}), "outside", "outside the stage"),
        (json!({"id": "6", "cmd": "wait", "file": "relative.tmp"}), "outside", "absolute"),
        (json!({"id": "7", "cmd": "wait", "file": format!("{}/../x", b.stage("work"))}), "outside", "\"..\""),
        (json!({"id": "8", "cmd": "sums", "dir": b.stage("no-such-folder")}), "not-found", "is not there"),
        (json!({"id": "9", "cmd": "tweaks"}), "not-ported", "root tweaks"),
        (json!({"id": "10", "cmd": "hashes", "image": image, "out": outside}), "outside", "outside the stage"),
        (json!({"id": "11", "cmd": "hashes", "image": image, "out": b.stage("a.hashes"), "chunk": 1000}), "usage", "power of two"),
        (json!({"id": "12", "cmd": "apfs-copy", "image": image, "src": "/", "dst": b.stage("")}), "outside", "outside the stage"),
        (json!({"id": "13", "cmd": "prelink", "kernel": b.stage("no.macho")}), "not-found", "is not there"),
        (json!({"id": "14", "cmd": "log", "file": "../../etc/passwd"}), "usage", "--file"),
        (json!({"id": "15", "cmd": "log", "filters": ["everything"]}), "usage", "no filter"),
        (json!({"id": "16", "cmd": "log"}), "not-found", "log-list"),
        (json!({"id": "17", "cmd": "download", "url": "ftp://x/y", "out": b.stage("y"), "sha256": "0".repeat(64)}), "usage", "https"),
        (json!({"id": "18", "cmd": "download", "url": "https://127.0.0.1:9/y", "out": b.stage("y"), "sha256": "xyz"}), "usage", "64 lower-case hex"),
        (json!({"id": "19", "cmd": "receive", "from": image, "to": b.stage("r/a"), "bytes": 1, "sha256": "0".repeat(64), "mode": "0777"}), "usage", "0644"),
        (json!({"id": "20", "cmd": "ping", "extra": 1}), "usage", "no key \"extra\""),
    ] {
        let last = s.ask(request.clone());
        assert_eq!((last["ev"].as_str(), last["code"].as_str()), (Some("refused"), Some(code)), "{request}: {last}");
        assert!(last["text"].as_str().unwrap().contains(words), "{request}: {last}");
    }
    // lines that are not requests
    for (line, words) in [("hello", "not a JSON line"), ("[]", "not a JSON object"), ("{\"cmd\":\"ping\"}", "no \"id\""), ("{\"id\":\"a b\",\"cmd\":\"ping\"}", "1 to 64 characters")] {
        writeln!(s.stdin.as_mut().unwrap(), "{line}").unwrap();
        let error = s.until(|v| v["ev"] == "error").pop().unwrap();
        assert!(error["text"].as_str().unwrap().contains(words), "{line}: {error}");
    }
    // nothing was written outside the stage, and a refused request leaves nothing in it
    assert!(!b.top.join("elsewhere").exists());
    assert!(!Path::new(&b.stage("a.hashes")).exists() && !Path::new(&b.stage("a.hashes.part")).exists() && !Path::new(&b.stage("r")).join("a").exists());
    let (status, rest, _) = s.end(true);
    assert_eq!(status, 0);
    clean_bye(&rest, "eof");
}

#[test]
fn a_session_refuses_heavy_requests_and_lets_the_readings_run() {
    let b = Box::new("session");
    b.sys("proc/sys/kernel/random/boot_id", "boot-1\n");
    b.put("stage/control/boot", b"boot-1\n");
    b.put("stage/control/state", b"running\n");
    b.put("stage/session/xnuhost.log", b"[    1.000] xnuhost I one\n[    2.000] guest     oneios-gpu-report: two\n");
    b.put("stage/image.sha256", b"");
    std::fs::create_dir_all(b.stage("image")).unwrap();
    let mut s = Serve::start(&b, &[]);
    let health = s.ask(json!({"id": "h", "cmd": "health"}));
    assert_eq!((health["ev"].as_str(), health["session"].as_bool(), health["state"].as_str()), (Some("ok"), Some(true), Some("unknown")), "{health}");
    for request in [json!({"id": "1", "cmd": "sums", "dir": b.stage("image")}), json!({"id": "2", "cmd": "wait", "seconds": 0.1, "heavy": true}), json!({"id": "3", "cmd": "tweaks"})] {
        let last = s.ask(request);
        assert_eq!((last["ev"].as_str(), last["code"].as_str()), (Some("refused"), Some("session")), "{last}");
        assert!(last["text"].as_str().unwrap().contains("return to One UI first"));
    }
    // the readings run beside a session
    assert_eq!(s.ask(json!({"id": "4", "cmd": "log", "filters": ["gpu"]}))["matched"], 1);
    assert_eq!(s.ask(json!({"id": "5", "cmd": "log-list"}))["logs"], 1);
    assert_eq!(s.ask(json!({"id": "6", "cmd": "wait", "seconds": 0.05}))["ev"], "ok");
    // the session ended: heavy requests run again
    b.put("stage/control/state", b"returned\n");
    assert_eq!(s.ask(json!({"id": "7", "cmd": "sums", "dir": b.stage("image")}))["ev"], "ok");
    s.end(true);
}

// ---------------------------------------------------------------- heat and battery

fn heat(b: &Box, case: &str, battery: &str, level: &str, status: &str) {
    b.sys("sys/class/power_supply/battery/lrp", &format!("{case}\n"));
    b.sys("sys/class/power_supply/battery/temp", &format!("{battery}\n"));
    b.sys("sys/class/power_supply/battery/capacity", &format!("{level}\n"));
    b.sys("sys/class/power_supply/battery/status", &format!("{status}\n"));
}

#[test]
fn heat_pauses_and_stops_and_the_battery_refuses() {
    let b = Box::new("heat");
    heat(&b, "380", "330", "80", "Discharging");
    let mut s = Serve::start(&b, &[]);
    let health = s.ask(json!({"id": "h1", "cmd": "health"}));
    assert_eq!((health["case"].as_f64(), health["battery"].as_f64(), health["level"].as_u64(), health["charging"].as_bool(), health["state"].as_str()), (Some(38.0), Some(33.0), Some(80), Some(false), Some("ok")), "{health}");

    // the case at its pause limit: the work waits, and goes on when it is 2 C below
    heat(&b, "445", "330", "80", "Discharging");
    assert_eq!(s.ask(json!({"id": "h2", "cmd": "health"}))["state"], "pause");
    s.send(json!({"id": "warm", "cmd": "wait", "seconds": 0.3, "heavy": true}));
    let paused = s.until(|v| v["ev"] == "heat").pop().unwrap();
    assert_eq!((paused["state"].as_str(), paused["why"].as_str(), paused["case"].as_f64(), paused["id"].as_str()), (Some("paused"), Some("case"), Some(44.5), Some("warm")), "{paused}");
    assert!(paused["text"].as_str().unwrap().contains("44.5 C") && paused["text"].as_str().unwrap().contains("below 42.0 C"), "{paused}");
    // 43 C is not cool enough: no line for 2.5 s
    heat(&b, "430", "330", "80", "Discharging");
    assert!(s.next_in(Duration::from_millis(2500)).is_none_or(|v| v["ev"] != "heat" && !is_last(&v)), "the pause holds at 43 C");
    heat(&b, "415", "330", "80", "Discharging");
    let events = s.request("warm");
    let resumed = events.iter().find(|v| v["ev"] == "heat").expect("a resumed line");
    assert_eq!((resumed["state"].as_str(), resumed["case"].as_f64()), (Some("resumed"), Some(41.5)), "{resumed}");
    assert_eq!(events.last().unwrap()["ev"], "ok");

    // a light request is not held
    heat(&b, "445", "330", "80", "Discharging");
    assert_eq!(s.ask(json!({"id": "light", "cmd": "wait", "seconds": 0.05}))["ev"], "ok");

    // the stop limit, reached while a request runs: it ends with refused, heat, and its file is gone
    heat(&b, "380", "330", "80", "Discharging");
    let file = b.stage("work/hot.tmp");
    s.send(json!({"id": "hot", "cmd": "wait", "seconds": 60, "heavy": true, "file": file}));
    s.until(|v| v["id"] == "hot" && v["ev"] == "progress");
    heat(&b, "380", "505", "80", "Discharging");
    let last = s.request("hot").pop().unwrap();
    assert_eq!((last["ev"].as_str(), last["code"].as_str()), (Some("refused"), Some("heat")), "{last}");
    assert!(last["text"].as_str().unwrap().starts_with("The battery is at 50.5 C"), "{last}");
    assert!(!Path::new(&file).exists());
    // at the stop limit a heavy request does not start
    let last = s.ask(json!({"id": "hot2", "cmd": "wait", "seconds": 0.1, "heavy": true}));
    assert_eq!((last["code"].as_str(), s.ask(json!({"id": "h3", "cmd": "health"}))["state"].as_str()), (Some("heat"), Some("stop")));

    // the battery's level counts when it is not charging
    heat(&b, "380", "330", "7", "Discharging");
    let last = s.ask(json!({"id": "low", "cmd": "wait", "seconds": 0.1, "heavy": true}));
    assert_eq!((last["ev"].as_str(), last["code"].as_str()), (Some("refused"), Some("battery")), "{last}");
    assert!(last["text"].as_str().unwrap().contains("7 %") && last["text"].as_str().unwrap().contains("charger"));
    heat(&b, "380", "330", "7", "Charging");
    assert_eq!(s.ask(json!({"id": "charging", "cmd": "wait", "seconds": 0.1, "heavy": true}))["ev"], "ok");
    let (status, rest, _) = s.end(true);
    assert_eq!(status, 0);
    clean_bye(&rest, "eof");

    // --thermal off and --battery-min 0: no check; other limits are the options'
    heat(&b, "600", "600", "1", "Discharging");
    let mut s = Serve::start(&b, &["--thermal", "off", "--battery-min", "0"]);
    assert_eq!(s.seen[0]["thermal"], Value::Null);
    assert_eq!(s.ask(json!({"id": "w", "cmd": "wait", "seconds": 0.05, "heavy": true}))["ev"], "ok");
    s.end(true);
    heat(&b, "410", "330", "80", "Discharging");
    let mut s = Serve::start(&b, &["--thermal", "38,40,45,50"]);
    assert_eq!(s.ask(json!({"id": "w", "cmd": "wait", "seconds": 0.05, "heavy": true}))["code"], "heat");
    s.end(true);
}

// ---------------------------------------------------------------- the commands

#[test]
fn sums_checks_a_folder_against_its_list() {
    let b = Box::new("sums");
    let files: Vec<(&str, Vec<u8>)> = vec![("kernel.kc", noise(3_000_000, 1)), ("os.img", noise(5_000_000, 2)), ("overlay/userland/xnuhost.conf", b"scale=3\n".to_vec()), ("empty", Vec::new())];
    let mut list = String::new();
    for (path, data) in &files {
        b.put(&format!("stage/image/{path}"), data);
        list += &format!("{}  {path}\n", sha256(data));
    }
    b.put("stage/image.sha256", list.as_bytes());
    let mut s = Serve::start(&b, &["--jobs", "4"]);
    s.send(json!({"id": "s", "cmd": "sums", "dir": b.stage("image")}));
    let events = s.request("s");
    assert_eq!(events[0]["steps"], json!([{"step": "scan", "text": "Reading the list"}, {"step": "hash", "text": "Checking the files"}]));
    let steps: Vec<&str> = events.iter().filter(|v| v["ev"] == "step").map(|v| v["step"].as_str().unwrap()).collect();
    assert_eq!(steps, ["scan", "hash"]);
    let rows: Vec<&Value> = events.iter().filter(|v| v["ev"] == "row").collect();
    assert_eq!(rows.len(), 4);
    for (path, data) in &files {
        let row = rows.iter().find(|r| r["path"] == *path).unwrap_or_else(|| panic!("no row of {path}"));
        assert_eq!((row["kind"].as_str(), row["sha256"].as_str(), row["bytes"].as_u64(), row["ok"].as_bool()), (Some("file"), Some(sha256(data).as_str()), Some(data.len() as u64), Some(true)));
    }
    let total: u64 = files.iter().map(|(_, d)| d.len() as u64).sum();
    let last_progress = events.iter().filter(|v| v["ev"] == "progress").next_back().unwrap();
    assert_eq!((last_progress["done"].as_u64(), last_progress["total"].as_u64(), last_progress["unit"].as_str(), last_progress["items_done"].as_u64(), last_progress["items_total"].as_u64()), (Some(total), Some(total), Some("bytes"), Some(4), Some(4)), "{last_progress}");
    let ok = events.last().unwrap();
    assert_eq!((ok["ev"].as_str(), ok["files"].as_u64(), ok["bytes"].as_u64()), (Some("ok"), Some(4), Some(total)), "{ok}");

    // a changed byte, a missing file, and (strict) a file the list does not name
    let mut changed = files[1].1.clone();
    changed[4_000_000] ^= 1;
    b.put("stage/image/os.img", &changed);
    std::fs::remove_file(b.stage("image/empty")).unwrap();
    b.put("stage/image/stray.bin", b"x");
    let last = s.ask(json!({"id": "s2", "cmd": "sums", "dir": b.stage("image"), "list": b.stage("image.sha256")}));
    assert_eq!((last["ev"].as_str(), last["code"].as_str()), (Some("refused"), Some("mismatch")), "{last}");
    let text = last["text"].as_str().unwrap();
    assert!(text.contains("2 of 4 files") && text.contains("os.img (another SHA-256)") && text.contains("empty (missing)") && !text.contains("stray"), "{text}");
    let text = s.ask(json!({"id": "s3", "cmd": "sums", "dir": b.stage("image"), "strict": true}))["text"].as_str().unwrap().to_string();
    assert!(text.contains("1 files of") && text.contains("stray.bin"), "{text}");
    // a list that names a path outside the folder is not followed
    b.put("stage/bad.sha256", format!("{}  ../image.sha256\n", "0".repeat(64)).as_bytes());
    let last = s.ask(json!({"id": "s4", "cmd": "sums", "dir": b.stage("image"), "list": b.stage("bad.sha256")}));
    assert_eq!(last["code"], "mismatch");
    assert!(last["text"].as_str().unwrap().contains("line 1"));
    s.end(true);
}

#[test]
fn hashes_writes_the_list_the_image_command_writes() {
    let b = Box::new("hashes");
    let data = noise(3 * (1 << 20) + 777, 9);
    let image = b.put("in/os.img", &data);
    let mut s = Serve::start(&b, &[]);
    let out = b.stage("set/os.img.hashes");
    let ok = s.ask(json!({"id": "h", "cmd": "hashes", "image": image, "out": out}));
    assert_eq!((ok["ev"].as_str(), ok["bytes"].as_u64(), ok["sha256"].as_str(), ok["chunks"].as_u64()), (Some("ok"), Some(data.len() as u64), Some(sha256(&data).as_str()), Some(4)), "{ok}");
    // the same bytes as `oneios-tool image hashes` (one thread and all)
    for jobs in ["1", "4"] {
        let other = b.top.join(format!("cli-{jobs}.hashes"));
        let done = Command::new(tool()).args(["--jobs", jobs, "image", "hashes", &image]).arg(&other).output().unwrap();
        assert!(done.status.success(), "{}", String::from_utf8_lossy(&done.stderr));
        assert_eq!(std::fs::read(&out).unwrap(), std::fs::read(&other).unwrap(), "--jobs {jobs}");
    }
    assert!(!Path::new(&format!("{out}.part")).exists());
    s.end(true);
}

#[test]
fn receive_checks_size_and_sum_before_the_file_has_its_name() {
    let b = Box::new("receive");
    let data = noise(2_500_000, 5);
    let from = b.put("app/os.img", &data);
    let mut s = Serve::start(&b, &[]);
    let (to, sums, ids) = (b.stage("image.new/sub/os.img"), b.stage("image.new.sha256"), b.stage("image.new.ids"));
    let request = |id: &str, from: &str, to: &str, bytes: usize, sum: &str| json!({"id": id, "cmd": "receive", "from": from, "to": to, "bytes": bytes, "sha256": sum, "mode": "0755", "name": "sub/os.img", "sums_list": sums, "ids_list": ids});
    s.send(request("r", &from, &to, data.len(), &sha256(&data)));
    let events = s.request("r");
    let steps: Vec<&str> = events.iter().filter(|v| v["ev"] == "step").map(|v| v["step"].as_str().unwrap()).collect();
    assert_eq!(steps, ["copy", "check", "place"]);
    let space = events.iter().find(|v| v["ev"] == "space").expect("a space line");
    assert_eq!((space["need"].as_u64(), space["reserve"].as_u64()), (Some(data.len() as u64), Some(268_435_456)));
    assert!(space["free"].as_u64().unwrap() > 0 && space["path"].as_str().unwrap().contains("stage"));
    assert_eq!(events.last().unwrap()["ev"], "ok", "{events:#?}");
    assert_eq!(std::fs::read(&to).unwrap(), data);
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let meta = std::fs::metadata(&to).unwrap();
    assert_eq!(meta.permissions().mode() & 0o7777, 0o755);
    // the lists, as oneios_root.sh receive writes them
    assert_eq!(std::fs::read_to_string(&sums).unwrap(), format!("{}  sub/os.img\n", sha256(&data)));
    assert_eq!(std::fs::read_to_string(&ids).unwrap(), format!("{}:{}:{}:{} sub/os.img\n", meta.dev(), meta.ino(), meta.size(), meta.mtime()));

    // received already; another size; another sum: refused, and nothing is left
    assert_eq!(s.ask(request("r2", &from, &to, data.len(), &sha256(&data)))["code"], "exists");
    let to2 = b.stage("image.new/second.img");
    let last = s.ask(request("r3", &from, &to2, data.len() - 1, &sha256(&data)));
    assert_eq!(last["code"], "mismatch");
    assert!(last["text"].as_str().unwrap().contains("were announced"), "{last}");
    let last = s.ask(request("r4", &from, &to2, data.len(), &sha256(b"other")));
    assert_eq!((last["ev"].as_str(), last["code"].as_str()), (Some("refused"), Some("mismatch")));
    assert!(last["text"].as_str().unwrap().contains("did not arrive intact (SHA-256 differs)"), "{last}");
    assert!(!Path::new(&to2).exists() && !Path::new(&format!("{to2}.part")).exists());
    assert_eq!(std::fs::read_to_string(&sums).unwrap().lines().count(), 1, "a refused file is not listed");

    // from a FIFO the app writes: the bytes as they come; fewer than announced is refused
    let fifo = b.top.join("app/pipe").to_string_lossy().into_owned();
    let make = |path: &str| {
        let c = std::ffi::CString::new(path).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    };
    make(&fifo);
    let (bytes, sum) = (data.len(), sha256(&data));
    s.send(json!({"id": "f", "cmd": "receive", "from": fifo, "to": to2, "bytes": bytes, "sha256": sum, "mode": "0644"}));
    let writer = {
        let (fifo, data) = (fifo.clone(), data.clone());
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            let mut pipe = std::fs::OpenOptions::new().write(true).open(fifo).unwrap();
            for part in data.chunks(100_000) {
                pipe.write_all(part).unwrap();
            }
        })
    };
    assert_eq!(s.request("f").pop().unwrap()["ev"], "ok");
    writer.join().unwrap();
    assert_eq!(std::fs::read(&to2).unwrap(), data);
    assert_eq!(std::fs::metadata(&to2).unwrap().permissions().mode() & 0o7777, 0o644);
    let to3 = b.stage("image.new/third.img");
    s.send(json!({"id": "short", "cmd": "receive", "from": fifo, "to": to3, "bytes": bytes, "sha256": sum, "mode": "0644"}));
    {
        let mut pipe = std::fs::OpenOptions::new().write(true).open(&fifo).unwrap();
        pipe.write_all(&data[..1000]).unwrap();
    }
    let last = s.request("short").pop().unwrap();
    assert_eq!(last["code"], "mismatch");
    assert!(last["text"].as_str().unwrap().contains(&format!("1000 of {bytes} bytes arrived")), "{last}");
    // a FIFO nobody writes: the request waits, and a cancel ends it
    s.send(json!({"id": "nobody", "cmd": "receive", "from": fifo, "to": to3, "bytes": bytes, "sha256": sum, "mode": "0644"}));
    s.until(|v| v["id"] == "nobody" && v["ev"] == "progress");
    s.send(json!({"cmd": "cancel", "id": "nobody"}));
    let last = s.request("nobody").pop().unwrap();
    assert_eq!((last["ev"].as_str(), last["removed"].as_u64()), (Some("cancelled"), Some(1)), "{last}");
    assert!(!Path::new(&to3).exists() && !Path::new(&format!("{to3}.part")).exists());
    let (status, rest, _) = s.end(true);
    assert_eq!(status, 0);
    clean_bye(&rest, "eof");
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for byte in data {
        crc ^= *byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xedb8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

/// A ZIP of stored members: local headers, the directory, the end record.
fn zip_of(members: &[(&str, &[u8])]) -> Vec<u8> {
    let (mut out, mut directory) = (Vec::new(), Vec::new());
    for (name, data) in members {
        let mut fields = Vec::new();
        for (value, width) in [(20u32, 2), (0, 2), (0, 2), (0, 2), (0x21, 2), (crc32(data), 4), (data.len() as u32, 4), (data.len() as u32, 4), (name.len() as u32, 2), (0, 2)] {
            fields.extend_from_slice(&value.to_le_bytes()[..width]);
        }
        let offset = out.len() as u32;
        out.extend_from_slice(b"PK\x03\x04");
        out.extend_from_slice(&fields);
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(data);
        directory.extend_from_slice(b"PK\x01\x02\x14\x00");
        directory.extend_from_slice(&fields);
        directory.extend_from_slice(&[0u8; 10]);
        directory.extend_from_slice(&offset.to_le_bytes());
        directory.extend_from_slice(name.as_bytes());
    }
    let (start, count) = (out.len() as u32, members.len() as u16);
    out.extend_from_slice(&directory);
    out.extend_from_slice(b"PK\x05\x06\0\0\0\0");
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&(directory.len() as u32).to_le_bytes());
    out.extend_from_slice(&start.to_le_bytes());
    out.extend_from_slice(&[0, 0]);
    out
}

/// `oneios-tool rangeserve DIR PORT` on a free port of 127.0.0.1, ended when dropped.
struct Web {
    child: Child,
    port: u16,
}

impl Web {
    fn start(dir: &Path) -> Web {
        for _ in 0..20 {
            let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
            let mut child = Command::new(tool()).arg("rangeserve").arg(dir).arg(port.to_string()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
            for _ in 0..100 {
                if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                    return Web { child, port };
                }
                if child.try_wait().unwrap().is_some() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            let _ = child.kill();
            let _ = child.wait();
        }
        panic!("rangeserve did not start");
    }
}

impl Drop for Web {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn downloads_over_http_ranges_checked_and_placed() {
    let b = Box::new("download");
    let (kernel, other) = (noise(20 << 20, 11), noise(300_000, 12));
    b.put("web/xnu.macho", &kernel);
    b.put("web/set.zip", &zip_of(&[("Firmware/a.im4p", &other), ("kernelcache.release.iphone18", &kernel[..5 << 20]), ("BuildManifest.plist", b"<plist/>")]));
    let web = Web::start(&b.top.join("web"));
    let url = |name: &str| format!("http://127.0.0.1:{}/{name}", web.port);
    let mut s = Serve::start(&b, &["--jobs", "4"]);

    // a file: parallel range requests, the SHA-256 checked before the name is given
    let out = b.stage("downloads/xnu.macho");
    s.send(json!({"id": "d", "cmd": "download", "url": url("xnu.macho"), "out": out, "sha256": sha256(&kernel), "bytes": kernel.len()}));
    let events = s.request("d");
    let steps: Vec<&str> = events.iter().filter(|v| v["ev"] == "step").map(|v| v["step"].as_str().unwrap()).collect();
    assert_eq!(steps, ["download", "check", "place"], "{events:#?}");
    assert_eq!(events.iter().find(|v| v["ev"] == "space").expect("a space line")["need"].as_u64(), Some(kernel.len() as u64));
    let fetched: Vec<&Value> = events.iter().filter(|v| v["ev"] == "progress" && v["step"] == "download").collect();
    assert_eq!((fetched.last().unwrap()["done"].as_u64(), fetched.last().unwrap()["total"].as_u64()), (Some(kernel.len() as u64), Some(kernel.len() as u64)));
    let ok = events.last().unwrap();
    assert_eq!((ok["ev"].as_str(), ok["bytes"].as_u64(), ok["sha256"].as_str()), (Some("ok"), Some(kernel.len() as u64), Some(sha256(&kernel).as_str())), "{ok}");
    assert_eq!(std::fs::read(&out).unwrap(), kernel);
    let left: Vec<String> = std::fs::read_dir(b.stage("downloads")).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    assert_eq!(left, ["xnu.macho"], "no part is left");
    // there already with this sum: nothing is fetched
    assert!(s.ask(json!({"id": "d2", "cmd": "download", "url": url("xnu.macho"), "out": out, "sha256": sha256(&kernel)}))["text"].as_str().unwrap().contains("there already"));
    // another sum than expected, another size than expected, a file the server does not have: nothing is kept
    let out2 = b.stage("downloads/other.macho");
    let last = s.ask(json!({"id": "d3", "cmd": "download", "url": url("xnu.macho"), "out": out2, "sha256": sha256(b"x")}));
    assert_eq!((last["ev"].as_str(), last["code"].as_str()), (Some("refused"), Some("mismatch")), "{last}");
    let last = s.ask(json!({"id": "d4", "cmd": "download", "url": url("xnu.macho"), "out": out2, "sha256": sha256(&kernel), "bytes": 5}));
    assert_eq!(last["code"], "mismatch");
    assert!(last["text"].as_str().unwrap().contains("5 were expected"));
    assert_eq!(s.ask(json!({"id": "d5", "cmd": "download", "url": url("no-such-file"), "out": out2, "sha256": sha256(&kernel)}))["ev"], "failed");
    let left: Vec<String> = std::fs::read_dir(b.stage("downloads")).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    assert_eq!(left, ["xnu.macho"], "a refused or failed download leaves nothing");

    // members of a ZIP without downloading it
    s.send(json!({"id": "ls", "cmd": "zip-ls", "source": url("set.zip"), "match": "kernelcache|\\.plist$"}));
    let events = s.request("ls");
    let rows: Vec<(&str, u64, &str)> = events.iter().filter(|v| v["ev"] == "row").map(|v| (v["name"].as_str().unwrap(), v["bytes"].as_u64().unwrap(), v["method"].as_str().unwrap())).collect();
    assert_eq!(rows, [("kernelcache.release.iphone18", 5 << 20, "stored"), ("BuildManifest.plist", 8, "stored")]);
    assert_eq!(events.last().unwrap()["members"], 2);
    let outdir = b.stage("ipsw");
    s.send(json!({"id": "get", "cmd": "zip-get", "source": url("set.zip"), "outdir": outdir, "members": [{"member": "kernelcache.release.iphone18", "name": "kernelcache"}, {"member": "Firmware/a.im4p"}]}));
    let events = s.request("get");
    assert_eq!(events.last().unwrap()["ev"], "ok", "{events:#?}");
    let steps: Vec<&str> = events.iter().filter(|v| v["ev"] == "step").map(|v| v["step"].as_str().unwrap()).collect();
    assert_eq!(steps, ["directory", "download"]);
    assert_eq!(std::fs::read(format!("{outdir}/kernelcache")).unwrap(), &kernel[..5 << 20]);
    assert_eq!(std::fs::read(format!("{outdir}/a.im4p")).unwrap(), other);
    assert_eq!(std::fs::read_dir(&outdir).unwrap().count(), 2, "no part is left");
    let rows: Vec<&Value> = events.iter().filter(|v| v["ev"] == "row").collect();
    assert_eq!((rows[0]["name"].as_str(), rows[0]["crc32"].as_str().map(str::len), rows[0]["bytes"].as_u64()), (Some("kernelcache.release.iphone18"), Some(8), Some(5 << 20)));
    // a member the archive does not have; a name that is a path
    assert_eq!(s.ask(json!({"id": "g2", "cmd": "zip-get", "source": url("set.zip"), "outdir": outdir, "members": [{"member": "nothing"}]}))["code"], "not-found");
    assert_eq!(s.ask(json!({"id": "g3", "cmd": "zip-get", "source": url("set.zip"), "outdir": outdir, "members": [{"member": "Firmware/a.im4p", "name": "../a"}]}))["code"], "usage");
    // the same from a file the user gave
    let outdir2 = b.stage("ipsw2");
    assert_eq!(s.ask(json!({"id": "g4", "cmd": "zip-get", "source": b.top.join("web/set.zip").to_string_lossy(), "outdir": outdir2, "members": [{"member": "BuildManifest.plist"}]}))["ev"], "ok");
    assert_eq!(std::fs::read(format!("{outdir2}/BuildManifest.plist")).unwrap(), b"<plist/>");
    let (status, rest, stderr) = s.end(true);
    assert_eq!((status, stderr.as_str()), (0, ""));
    clean_bye(&rest, "eof");
}

#[test]
fn a_stopped_download_removes_its_part_or_keeps_it_for_resume() {
    let b = Box::new("download-stop");
    // a server that answers the size and then never a byte: the download stays in its first range request
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            let mut request = [0u8; 2048];
            let n = stream.read(&mut request).unwrap_or(0);
            if request[..n].starts_with(b"HEAD") {
                let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 50000000\r\nConnection: close\r\n\r\n");
            } else {
                // (kept open, silent)
                held.push(stream);
            }
        }
    });
    let url = format!("http://127.0.0.1:{port}/big.img");
    let mut s = Serve::start(&b, &[]);
    for (id, resume) in [("plain", false), ("resume", true)] {
        let out = b.stage(&format!("dl/{id}.img"));
        s.send(json!({"id": id, "cmd": "download", "url": url, "out": out, "sha256": "0".repeat(64), "resume": resume}));
        s.until(|v| v["id"] == id && v["ev"] == "progress");
        let until = Instant::now() + Duration::from_secs(10);
        while !Path::new(&format!("{out}.part.done")).exists() {
            assert!(Instant::now() < until, "the part is there while the download runs");
            std::thread::sleep(Duration::from_millis(20));
        }
        let began = Instant::now();
        s.send(json!({"cmd": "cancel", "id": id}));
        let last = s.request(id).pop().unwrap();
        assert_eq!(last["ev"], "cancelled", "{last}");
        // (a range request that gets no byte is given up after 5 s: the stop is seen then at the latest)
        assert!(began.elapsed() < Duration::from_secs(9), "{:?}", began.elapsed());
        assert_eq!(Path::new(&format!("{out}.part")).exists(), resume, "the part of {id}");
        assert_eq!(Path::new(&format!("{out}.part.done")).exists(), resume);
        assert!(!Path::new(&out).exists());
    }
    s.end(true);
}

#[test]
fn the_logs_and_the_cpu_check_from_named_files() {
    let b = Box::new("readings");
    let log: String = (0..40).map(|i| format!("[{:>9}.000] guest     {}\n", i, if i % 4 == 0 { "oneios-gpu-report: frame" } else { "launchd: a job" })).collect();
    b.put("stage/session/xnuhost.log", log.as_bytes());
    b.put("stage/switch.log", b"=== OneOS app switch ===\n");
    b.put("stage/logs/xnuhost-1794000000.log", b"an earlier session\n");
    for (n, midr, max, smax) in [(0, "0x00000000410fd801", "2265600", "2265600"), (7, "0x00000000411fd821", "3052800", "2400000")] {
        let d = format!("sys/devices/system/cpu/cpu{n}");
        b.sys(&format!("{d}/regs/identification/midr_el1"), midr);
        b.sys(&format!("{d}/cpu_capacity"), "1024");
        b.sys(&format!("{d}/cpufreq/cpuinfo_max_freq"), max);
        b.sys(&format!("{d}/cpufreq/scaling_max_freq"), smax);
        b.sys(&format!("{d}/cpufreq/scaling_governor"), "schedutil");
        b.sys(&format!("{d}/cpufreq/related_cpus"), &n.to_string());
    }
    b.sys("dev/cpuset/top-app/cpus", "0-7");
    let mut s = Serve::start(&b, &[]);

    s.send(json!({"id": "list", "cmd": "log-list"}));
    let events = s.request("list");
    let rows: Vec<&Value> = events.iter().filter(|v| v["ev"] == "row").collect();
    let row = |name: &str| rows.iter().find(|r| r["name"] == name).unwrap_or_else(|| panic!("no row {name}"));
    assert_eq!((row("xnuhost")["bytes"].as_u64(), row("xnuhost")["path"].as_str()), (Some(log.len() as u64), Some("session/xnuhost.log")));
    assert_eq!((row("return")["missing"].as_bool(), row("logs/xnuhost-1794000000.log")["bytes"].as_u64()), (Some(true), Some(19)));
    assert_eq!(events.last().unwrap()["logs"], 3);

    // the lines as `line` events; the filters; the last lines; into a file
    s.send(json!({"id": "log", "cmd": "log", "filters": ["gpu"], "since": 20.0}));
    let events = s.request("log");
    let lines: Vec<&str> = events.iter().filter(|v| v["ev"] == "line").map(|v| v["text"].as_str().unwrap()).collect();
    assert_eq!(lines.len(), 5, "{lines:?}");
    assert!(lines.iter().all(|l| l.contains("oneios-gpu-report")) && events.iter().filter(|v| v["ev"] == "line").all(|v| v["stream"] == "out"));
    assert!(lines[0].starts_with("[       20.000]"));
    let ok = events.last().unwrap();
    assert_eq!((ok["lines"].as_u64(), ok["matched"].as_u64(), ok["bytes"].as_u64()), (Some(40), Some(5), Some(log.len() as u64)), "{ok}");
    let ok = s.ask(json!({"id": "tail", "cmd": "log", "tail": 3, "grep": ["launchd"]}));
    assert_eq!((ok["lines"].as_u64(), ok["matched"].as_u64()), (Some(3), Some(3)));
    let out = b.stage("work/log.txt");
    assert_eq!(s.ask(json!({"id": "file", "cmd": "log", "file": "logs/xnuhost-1794000000.log", "out": out}))["out"], out.as_str());
    assert_eq!(std::fs::read_to_string(&out).unwrap(), "an earlier session\n");
    // "lines": at most that many line events; the rest are counted
    s.send(json!({"id": "few", "cmd": "log", "lines": 4}));
    let events = s.request("few");
    assert_eq!(events.iter().filter(|v| v["ev"] == "line").count(), 4);
    assert_eq!(events.last().unwrap()["lines_dropped"], 36);

    s.send(json!({"id": "cpu", "cmd": "cpu"}));
    let events = s.request("cpu");
    let rows: Vec<&Value> = events.iter().filter(|v| v["ev"] == "row").collect();
    assert_eq!(rows.len(), 2);
    assert_eq!((rows[1]["n"].as_u64(), rows[1]["core"].as_str(), rows[1]["max_khz"].as_u64(), rows[1]["scaling_max_khz"].as_u64(), rows[1]["capped"].as_bool()), (Some(7), Some("Cortex-X4"), Some(3052800), Some(2400000), Some(true)), "{}", rows[1]);
    assert_eq!((rows[0]["core"].as_str(), rows[0]["capped"].as_bool()), (Some("Cortex-A520"), Some(false)));
    let text: Vec<&str> = events.iter().filter(|v| v["ev"] == "line").map(|v| v["text"].as_str().unwrap()).collect();
    assert!(text.iter().any(|l| l.contains("cpu7 Cortex-X4") && l.contains("CAPPED")) && text.iter().any(|l| l.contains("xnuhost: no session")), "{text:?}");
    assert_eq!(events.last().unwrap()["cpus"], 2);
    let (status, rest, stderr) = s.end(true);
    assert_eq!((status, stderr.as_str()), (0, ""));
    clean_bye(&rest, "eof");
}

#[test]
fn the_runtime_check_refuses_a_file_of_apples() {
    let b = Box::new("package");
    b.put("stage/runtime/bin/xnuhost", b"\x7fELF not really");
    b.put("stage/runtime/runtime.json", b"{}");
    let mut s = Serve::start(&b, &[]);
    let first = s.ask(json!({"id": "ok", "cmd": "package-check", "dir": b.stage("runtime")}));
    // a Mach-O (Apple's format) in the runtime: refused, with the tool's own reason
    b.put("stage/runtime/lib/kernel.kc", &[0xcf, 0xfa, 0xed, 0xfe, 0x0c, 0, 0, 1, 0, 0, 0, 0, 0xc, 0, 0, 0]);
    let last = s.ask(json!({"id": "apple", "cmd": "package-check", "dir": b.stage("runtime")}));
    assert_eq!((last["ev"].as_str(), last["code"].as_str()), (Some("refused"), Some("apple-file")), "first: {first}; then: {last}");
    assert!(last["text"].as_str().unwrap().contains("kernel.kc"), "{last}");
    s.end(true);
}

// ---------------------------------------------------------------- the one-shot form

#[test]
fn the_one_shot_form_and_its_exit_status() {
    let b = Box::new("oneshot");
    let run = |requests: &[Value], options: &[&str]| {
        let mut command = Command::new(tool());
        command.arg("serve").args(["--stage", &b.stage("")]).args(options);
        for request in requests {
            command.args(["--request", &request.to_string()]);
        }
        let done = command.stdin(Stdio::null()).output().unwrap();
        let lines: Vec<Value> = String::from_utf8_lossy(&done.stdout).lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        (done.status.code().unwrap(), lines, String::from_utf8_lossy(&done.stderr).into_owned())
    };
    let (status, lines, stderr) = run(&[json!({"id": "1", "cmd": "wait", "seconds": 0.2}), json!({"id": "2", "cmd": "ping"})], &[]);
    assert_eq!((status, stderr.as_str()), (0, ""));
    assert_eq!(lines.first().unwrap()["ev"], "hello");
    assert_eq!(clean_bye(&lines, "requests")["requests"], 2);
    // the status is the last request's: refused 2, a wrong request 3, failed 4
    assert_eq!(run(&[json!({"id": "1", "cmd": "tweaks"})], &[]).0, 2);
    assert_eq!(run(&[json!({"id": "1", "cmd": "wait", "seconds": -1})], &[]).0, 3);
    assert_eq!(run(&[json!({"id": "1", "cmd": "ioslog-boots", "source": b.put("stage/not-a-store.img", &[0u8; 8192])})], &[]).0, 4);
    assert_eq!(run(&[json!({"id": "1", "cmd": "tweaks"}), json!({"id": "2", "cmd": "ping"})], &[]).0, 0);
    // a wrong command line of serve itself: 3, said on stderr before any line
    for options in [&["--request", "nonsense"][..], &["--request", "{\"id\":\"1\",\"cmd\":\"nothing\"}"], &["--nice", "5"], &["--nice", "20"], &["--jobs", "9"], &["--jobs", "0"], &["--thermal", "50,40,45,50"], &["--battery-min", "101"], &["--no-such-option"]] {
        let (status, lines, stderr) = run(&[], options);
        assert_eq!((status, lines.len()), (3, 0), "{options:?}: {stderr}");
        assert!(!stderr.is_empty());
    }
}

#[test]
fn the_one_shot_commands_take_jobs_and_nice() {
    let b = Box::new("options");
    let image = b.put("a.img", &noise(100_000, 3));
    let out = b.top.join("a.hashes");
    let run = |words: &[&str]| Command::new(tool()).args(words).args(["image", "hashes", &image]).arg(&out).output().unwrap();
    assert!(run(&["--jobs", "2", "--nice", "15"]).status.success());
    assert!(run(&["--nice", "19", "--jobs", "8"]).status.success());
    for (words, said) in [(&["--jobs", "0"][..], "--jobs 0: 1 to 8"), (&["--jobs", "9"], "1 to 8"), (&["--nice", "5"], "10 to 19"), (&["--nice", "x"], "10 to 19"), (&["--jobs"], "--jobs image: 1 to 8")] {
        let done = run(words);
        assert_eq!(done.status.code(), Some(2), "{words:?}");
        assert!(String::from_utf8_lossy(&done.stderr).contains(said), "{words:?}: {}", String::from_utf8_lossy(&done.stderr));
    }
}
