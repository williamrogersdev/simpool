//! A live view of one simulator in the browser: a stream of screenshots,
//! with clicks, drags and keys sent back as input. Local connections only.

use anyhow::{Context, Result, bail};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

use crate::hid::{self, Hid};
use crate::simctl;

const PAGE: &str = include_str!("view.html");
const BOUNDARY: &str = "simpoolframe";

pub fn token() -> Result<String> {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom").and_then(|mut source| source.read_exact(&mut bytes)).context("could not read random bytes for the viewer's key")?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

pub fn capture_jpeg(udid: &str, path: &str) -> Result<Vec<u8>> {
    simctl::simctl(&["io", udid, "screenshot", "--type=jpeg", path], simctl::QUICK)?;
    let bytes = std::fs::read(path).with_context(|| format!("could not read the screenshot at {path}"));
    let _ = std::fs::remove_file(path);
    bytes
}

pub fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let hex = bytes.get(index + 1..index + 3).and_then(|pair| std::str::from_utf8(pair).ok()).and_then(|pair| u8::from_str_radix(pair, 16).ok());
        match (bytes[index], hex) {
            (b'%', Some(value)) => {
                out.push(value);
                index += 3;
            }
            (b'+', _) => {
                out.push(b' ');
                index += 1;
            }
            (other, _) => {
                out.push(other);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn query(text: &str) -> HashMap<String, String> {
    text.split('&').filter_map(|pair| pair.split_once('=')).map(|(name, value)| (decode(name), decode(value))).collect()
}

fn respond(stream: &mut TcpStream, status: &str, kind: &str, body: &[u8]) {
    let head = format!("HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n", body.len());
    let _ = stream.write_all(head.as_bytes()).and_then(|()| stream.write_all(body));
}

fn stream_frames(mut stream: TcpStream, udid: String, key: String) {
    let head = format!("HTTP/1.1 200 OK\r\nContent-Type: multipart/x-mixed-replace; boundary={BOUNDARY}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n");
    if stream.write_all(head.as_bytes()).is_err() {
        return;
    }
    let path = std::env::temp_dir().join(format!("simpool-frame-{key}-{:?}.jpg", std::thread::current().id())).to_string_lossy().into_owned();
    let mut failures = 0;
    while failures < 5 {
        let Ok(frame) = capture_jpeg(&udid, &path) else {
            failures += 1;
            std::thread::sleep(Duration::from_secs(1));
            continue;
        };
        failures = 0;
        let part = format!("--{BOUNDARY}\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n", frame.len());
        if stream.write_all(part.as_bytes()).and_then(|()| stream.write_all(&frame)).and_then(|()| stream.write_all(b"\r\n")).and_then(|()| stream.flush()).is_err() {
            return;
        }
    }
}

fn number(fields: &HashMap<String, String>, name: &str) -> Result<f64> {
    let value: f64 = fields.get(name).and_then(|text| text.parse().ok()).with_context(|| format!("`{name}` is missing or not a number"))?;
    if !(0.0..=1.0).contains(&value) {
        bail!("`{name}` must be between 0 and 1");
    }
    Ok(value)
}

fn act(hid: &Hid, action: &str, fields: &HashMap<String, String>) -> Result<()> {
    let millis = |name: &str, fallback: u64| Duration::from_millis(fields.get(name).and_then(|text| text.parse().ok()).unwrap_or(fallback).min(5000));
    match action {
        "/tap" => hid.tap(number(fields, "x")?, number(fields, "y")?, millis("hold", 60)),
        "/swipe" => hid.swipe((number(fields, "x")?, number(fields, "y")?), (number(fields, "x2")?, number(fields, "y2")?), millis("ms", 300)),
        "/type" => hid.type_text(fields.get("t").map(String::as_str).unwrap_or_default()),
        "/key" => {
            let name = fields.get("n").map(String::as_str).unwrap_or_default();
            hid.key(hid::named_key(name).with_context(|| format!("no key named `{name}`"))?, false)
        }
        "/button" => {
            let name = fields.get("n").map(String::as_str).unwrap_or_default();
            hid.button(hid::named_button(name).with_context(|| format!("no button named `{name}`"))?)
        }
        _ => bail!("no such action"),
    }
}

fn handle(mut stream: TcpStream, udid: &str, hid: &Hid, key: &str) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let Ok(clone) = stream.try_clone() else { return };
    let mut reader = BufReader::new(clone);
    let mut first = String::new();
    if reader.read_line(&mut first).is_err() {
        return;
    }
    let mut header = String::new();
    while reader.read_line(&mut header).is_ok_and(|read| read > 2) {
        header.clear();
    }
    let mut parts = first.split_whitespace();
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else { return };
    let (path, fields) = match target.split_once('?') {
        Some((path, rest)) => (path, query(rest)),
        None => (target, HashMap::new()),
    };
    if fields.get("k").map(String::as_str) != Some(key) {
        respond(&mut stream, "403 Forbidden", "text/plain", b"Open the address `simpool view` printed; it carries the key.");
        return;
    }
    match (method, path) {
        ("GET", "/") => respond(&mut stream, "200 OK", "text/html; charset=utf-8", PAGE.replace("__KEY__", key).as_bytes()),
        ("GET", "/stream") => {
            let (udid, key) = (udid.to_string(), key.to_string());
            std::thread::spawn(move || stream_frames(stream, udid, key));
        }
        ("POST", action) => match act(hid, action, &fields) {
            Ok(()) => respond(&mut stream, "200 OK", "text/plain", b"ok"),
            Err(error) => respond(&mut stream, "400 Bad Request", "text/plain", format!("{error:#}").as_bytes()),
        },
        _ => respond(&mut stream, "404 Not Found", "text/plain", b"not found"),
    }
}

pub fn serve(udid: &str, hid: &Hid, port: u16, announce: impl FnOnce(&str)) -> Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", port)).with_context(|| format!("could not listen on 127.0.0.1:{port}"))?;
    let key = token()?;
    announce(&format!("http://127.0.0.1:{}/?k={key}", listener.local_addr()?.port()));
    for stream in listener.incoming().flatten() {
        handle(stream, udid, hid, &key);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_query_text() {
        assert_eq!(decode("a%20b+c%3F"), "a b c?");
        assert_eq!(decode("100%"), "100%");
        let fields = query("k=abc&t=hi%26bye");
        assert_eq!(fields.get("t").map(String::as_str), Some("hi&bye"));
    }
}
