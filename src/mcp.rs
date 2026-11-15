//! `simpool mcp`: the pool as a Model Context Protocol server on stdio, so a
//! coding agent can lease, drive and look at its simulator as tool calls.
//! Answers both the 2026-07-28 revision and the older `initialize` handshake.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::time::Duration;

use crate::hid::{self, Hid};
use crate::pool::{self, Owner};
use crate::state::Store;
use crate::{doctor, view};

const MODERN: &str = "2026-07-28";
const LEGACY: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
const VERSION_KEY: &str = "io.modelcontextprotocol/protocolVersion";
const INSTRUCTIONS: &str = "simpool gives each owner (by default the git worktree this server was started in) its own iOS simulator. Call `lease` first, then `install` and `launch` your app. Look with `screenshot`; act with `tap`, `swipe`, `type_text`, `press_key` and `press_button`. Screen positions are fractions from the top left: x 0 to 1 across, y 0 to 1 down, so the middle is 0.5 0.5. Take a screenshot after acting to see the result. Call `release` when the work is done. When several agents share this server, pass `owner` on every call.";

pub fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let block = u32::from(chunk[0]) << 16 | u32::from(*chunk.get(1).unwrap_or(&0)) << 8 | u32::from(*chunk.get(2).unwrap_or(&0));
        for (index, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            if index <= chunk.len() {
                out.push(char::from(ALPHABET[(block >> shift & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn tool(name: &str, title: &str, description: &str, properties: Value, required: &[&str], read_only: bool) -> Value {
    let mut properties = properties;
    if let Some(fields) = properties.as_object_mut() {
        fields.insert("owner".into(), json!({ "type": "string", "description": "Who holds the simulator. Defaults to the worktree this server runs in." }));
    }
    json!({
        "name": name,
        "title": title,
        "description": description,
        "inputSchema": { "type": "object", "properties": properties, "required": required },
        "annotations": { "readOnlyHint": read_only, "openWorldHint": false },
    })
}

pub fn tools() -> Vec<Value> {
    let x = json!({ "type": "number", "minimum": 0, "maximum": 1, "description": "Fraction across the screen from the left edge" });
    let y = json!({ "type": "number", "minimum": 0, "maximum": 1, "description": "Fraction down the screen from the top edge" });
    vec![
        tool("lease", "Lease a simulator", "Get a booted iOS simulator for this owner. The same owner gets the same device every time. The first lease of a device type takes a few minutes; later ones take seconds.", json!({
            "device": { "type": "string", "description": "Device type, such as iPhone 17 Pro (the default)" },
            "runtime": { "type": "string", "description": "iOS version such as 26.5; the newest installed when omitted" },
            "ttl": { "type": "string", "description": "How long the lease lasts without use, such as 45m, 8h (the default) or 2d" },
        }), &[], false),
        tool("release", "Release the simulator", "Shut the leased simulator down and return it to the pool.", json!({ "delete": { "type": "boolean", "description": "Delete the device instead of keeping it for the next lease" } }), &[], false),
        tool("reset", "Erase the simulator", "Erase the leased simulator, including every installed app and its data, and boot it clean.", json!({}), &[], false),
        tool("status", "Pool status", "List the pool: who holds which simulator, what is booted, and disk used.", json!({}), &[], true),
        tool("doctor", "Check the machine", "Check Xcode, the license, the process limit, iOS runtimes and disk, with the fix for each problem.", json!({}), &[], true),
        tool("install", "Install an app", "Install a simulator .app build on the leased simulator. A device build is refused with the reason.", json!({ "app": { "type": "string", "description": "Absolute path to the .app folder" } }), &["app"], false),
        tool("launch", "Launch an app", "Launch an installed app, restarting it if it is running.", json!({
            "bundle_id": { "type": "string", "description": "Bundle identifier, such as com.example.app" },
            "args": { "type": "array", "items": { "type": "string" }, "description": "Launch arguments" },
        }), &["bundle_id"], false),
        tool("open_url", "Open a URL", "Open a URL or deep link on the leased simulator.", json!({ "url": { "type": "string" } }), &["url"], false),
        tool("screenshot", "Take a screenshot", "Return the leased simulator's screen as an image.", json!({}), &[], true),
        tool("tap", "Tap the screen", "Tap a point on the screen. Positions are fractions of the screen, not pixels.", json!({
            "x": x, "y": y,
            "hold_ms": { "type": "integer", "minimum": 0, "maximum": 5000, "description": "Hold this long for a long press; 60 when omitted" },
        }), &["x", "y"], false),
        tool("swipe", "Swipe the screen", "Drag a finger from one point to another. To scroll a list down, swipe from a lower point to a higher one.", json!({
            "from_x": x, "from_y": y, "to_x": x, "to_y": y,
            "duration_ms": { "type": "integer", "minimum": 50, "maximum": 5000, "description": "How long the swipe takes; 300 when omitted" },
        }), &["from_x", "from_y", "to_x", "to_y"], false),
        tool("type_text", "Type text", "Type text into the focused field as a hardware keyboard would. Tap the field first. US keyboard characters only.", json!({ "text": { "type": "string" } }), &["text"], false),
        tool("press_key", "Press a key", "Press one keyboard key.", json!({ "name": { "type": "string", "enum": ["return", "delete", "escape", "tab", "space", "up", "down", "left", "right"] } }), &["name"], false),
        tool("press_button", "Press a hardware button", "Press a hardware button on the simulator.", json!({ "name": { "type": "string", "enum": ["home", "lock", "side", "siri"] } }), &["name"], false),
    ]
}

struct Server {
    store: Store,
    owner: Owner,
    inputs: HashMap<String, Hid>,
}

fn text(args: &Value, name: &str) -> Result<String> {
    args.get(name).and_then(Value::as_str).map(str::to_string).with_context(|| format!("`{name}` is required and must be text"))
}

fn fraction(args: &Value, name: &str) -> Result<f64> {
    let value = args.get(name).and_then(Value::as_f64).with_context(|| format!("`{name}` is required and must be a number"))?;
    if !(0.0..=1.0).contains(&value) {
        bail!("`{name}` is {value}, which is not a fraction of the screen. Use 0 to 1, such as 0.5 for the middle");
    }
    Ok(value)
}

fn millis(args: &Value, name: &str, fallback: u64) -> Duration {
    Duration::from_millis(args.get(name).and_then(Value::as_u64).unwrap_or(fallback).min(5000))
}

fn words(message: impl Into<String>) -> Vec<Value> {
    vec![json!({ "type": "text", "text": message.into() })]
}

impl Server {
    fn input(&mut self, owner: &Owner) -> Result<&Hid> {
        let udid = pool::held(&self.store, owner)?;
        if !self.inputs.contains_key(&udid) {
            self.inputs.insert(udid.clone(), Hid::for_device(&udid)?);
        }
        self.inputs.get(&udid).context("the input session was lost")
    }

    fn call(&mut self, name: &str, args: &Value) -> Result<Vec<Value>> {
        let owner = match args.get("owner").and_then(Value::as_str) {
            Some(named) => Owner { name: named.to_string(), path: self.owner.path.clone() },
            None => Owner { name: self.owner.name.clone(), path: self.owner.path.clone() },
        };
        let store = &self.store;
        let report = match name {
            "lease" => {
                let device = args.get("device").and_then(Value::as_str).unwrap_or(pool::DEFAULT_DEVICE);
                let ttl = pool::parse_duration(args.get("ttl").and_then(Value::as_str).unwrap_or("8h"))?;
                pool::lease(store, &owner, device, args.get("runtime").and_then(Value::as_str), ttl)?
            }
            "release" => {
                let report = pool::release(store, &owner, args.get("delete").and_then(Value::as_bool).unwrap_or(false))?;
                self.inputs.clear();
                report
            }
            "reset" => pool::reset(store, &owner)?,
            "status" => pool::status(store)?,
            "doctor" => return Ok(words(doctor::render(&doctor::checks()))),
            "install" => pool::install(store, &owner, &text(args, "app")?)?,
            "launch" => {
                let extra: Vec<String> = args.get("args").and_then(Value::as_array).map(|list| list.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default();
                pool::launch(store, &owner, &text(args, "bundle_id")?, &extra)?
            }
            "open_url" => pool::open_url(store, &owner, &text(args, "url")?)?,
            "screenshot" => {
                let udid = pool::held(store, &owner)?;
                let path = std::env::temp_dir().join(format!("simpool-mcp-{}-{udid}.jpg", std::process::id())).to_string_lossy().into_owned();
                let image = view::capture_jpeg(&udid, &path)?;
                return Ok(vec![json!({ "type": "image", "data": base64(&image), "mimeType": "image/jpeg" }), json!({ "type": "text", "text": "Positions for tap and swipe are fractions of this image: x 0 to 1 across, y 0 to 1 down." })]);
            }
            "tap" => {
                let (x, y) = (fraction(args, "x")?, fraction(args, "y")?);
                self.input(&owner)?.tap(x, y, millis(args, "hold_ms", 60))?;
                return Ok(words(format!("tapped {x} {y}")));
            }
            "swipe" => {
                let from = (fraction(args, "from_x")?, fraction(args, "from_y")?);
                let to = (fraction(args, "to_x")?, fraction(args, "to_y")?);
                self.input(&owner)?.swipe(from, to, millis(args, "duration_ms", 300))?;
                return Ok(words(format!("swiped {} {} to {} {}", from.0, from.1, to.0, to.1)));
            }
            "type_text" => {
                let value = text(args, "text")?;
                self.input(&owner)?.type_text(&value)?;
                return Ok(words(format!("typed {} character(s)", value.chars().count())));
            }
            "press_key" => {
                let key = text(args, "name")?;
                let usage = hid::named_key(&key).with_context(|| format!("no key named `{key}`"))?;
                self.input(&owner)?.key(usage, false)?;
                return Ok(words(format!("pressed {key}")));
            }
            "press_button" => {
                let button = text(args, "name")?;
                let code = hid::named_button(&button).with_context(|| format!("no button named `{button}`"))?;
                self.input(&owner)?.button(code)?;
                return Ok(words(format!("pressed {button}")));
            }
            _ => bail!("unreachable tool `{name}`"),
        };
        Ok(words(report.text))
    }
}

struct Refusal {
    code: i64,
    message: String,
    data: Option<Value>,
}

fn refusal(code: i64, message: impl Into<String>) -> Refusal {
    Refusal { code, message: message.into(), data: None }
}

fn server_info() -> Value {
    json!({ "name": "simpool", "title": "simpool", "version": env!("CARGO_PKG_VERSION") })
}

fn answer(server: &mut Server, method: &str, params: &Value) -> Result<Value, Refusal> {
    if let Some(requested) = params.pointer(&format!("/_meta/{}", VERSION_KEY.replace('/', "~1"))).and_then(Value::as_str)
        && requested != MODERN {
            return Err(Refusal { code: -32022, message: format!("simpool does not speak MCP revision {requested}"), data: Some(json!({ "supported": [MODERN], "requested": requested })) });
        }
    match method {
        "initialize" => {
            let requested = params.get("protocolVersion").and_then(Value::as_str).unwrap_or(LEGACY[0]);
            let version = if LEGACY.contains(&requested) { requested } else { LEGACY[0] };
            Ok(json!({ "protocolVersion": version, "capabilities": { "tools": {} }, "serverInfo": server_info(), "instructions": INSTRUCTIONS }))
        }
        "server/discover" => Ok(json!({
            "resultType": "complete",
            "supportedVersions": [MODERN],
            "capabilities": { "tools": {} },
            "_meta": { "io.modelcontextprotocol/serverInfo": server_info() },
            "instructions": INSTRUCTIONS,
            "ttlMs": 3_600_000,
            "cacheScope": "public",
        })),
        "ping" => Ok(json!({ "resultType": "complete" })),
        "tools/list" => Ok(json!({ "resultType": "complete", "tools": tools() })),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).ok_or_else(|| refusal(-32602, "tools/call needs a tool `name`"))?;
            if !tools().iter().any(|known| known["name"] == name) {
                return Err(refusal(-32602, format!("Unknown tool: {name}")));
            }
            let empty = json!({});
            let args = params.get("arguments").unwrap_or(&empty);
            Ok(match server.call(name, args) {
                Ok(content) => json!({ "resultType": "complete", "content": content, "isError": false }),
                Err(error) => json!({ "resultType": "complete", "content": words(format!("{error:#}")), "isError": true }),
            })
        }
        _ => Err(refusal(-32601, format!("Method not found: {method}"))),
    }
}

fn reply(id: Value, outcome: Result<Value, Refusal>) -> Value {
    match outcome {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err(refused) => {
            let mut error = json!({ "code": refused.code, "message": refused.message });
            if let Some(data) = refused.data {
                error["data"] = data;
            }
            json!({ "jsonrpc": "2.0", "id": id, "error": error })
        }
    }
}

pub fn serve(store: Store, owner: Owner) -> Result<()> {
    let mut server = Server { store, owner, inputs: HashMap::new() };
    let mut out = std::io::stdout().lock();
    for line in std::io::stdin().lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let message = match serde_json::from_str::<Value>(&line) {
            Ok(message) => message,
            Err(error) => {
                writeln!(out, "{}", reply(Value::Null, Err(refusal(-32700, format!("Parse error: {error}")))))?;
                out.flush()?;
                continue;
            }
        };
        let (Some(id), Some(method)) = (message.get("id").filter(|id| !id.is_null()).cloned(), message.get("method").and_then(Value::as_str)) else {
            continue;
        };
        let empty = json!({});
        let outcome = answer(&mut server, method, message.get("params").unwrap_or(&empty));
        writeln!(out, "{}", reply(id, outcome))?;
        out.flush()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_base64_with_padding() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn every_tool_has_a_schema_and_an_owner() {
        let listed = tools();
        assert!(listed.len() >= 14);
        for entry in &listed {
            assert_eq!(entry["inputSchema"]["type"], "object");
            assert!(entry["inputSchema"]["properties"]["owner"].is_object(), "{} takes an owner", entry["name"]);
            assert!(!entry["description"].as_str().unwrap_or_default().is_empty());
        }
    }
}
