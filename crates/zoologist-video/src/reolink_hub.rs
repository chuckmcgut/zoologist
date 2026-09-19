//! A small blocking client for the Reolink Home Hub's HTTP API (plan Step 7.2).
//!
//! Every call is a POST to `/cgi-bin/api.cgi?cmd=<Cmd>&token=<token>` with a JSON array
//! `[{"cmd": "<Cmd>", "action": 0, "param": {…}}]`, answered by an array
//! `[{"cmd": …, "code": 0, "value": {…}}]` or, on failure, `{"code": 1, "error": {"rspCode": -6,
//! "detail": "please login first"}}`. Recordings are downloaded with a GET of `cmd=Download`.
//!
//! Newer firmware (the Home Hub's included) uses a digest login and encrypts every command
//! after it; see [`crypto`]. The client picks that login when the Hub offers it.
//!
//! The Hub reports times in its own local time zone (set in the Reolink app). They are converted
//! with the station time zone, which must match. The password and token are never logged.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use chrono::{DateTime, Datelike, NaiveDate, NaiveDateTime, TimeZone, Timelike, Utc};
use chrono_tz::Tz;
use serde_json::{Value, json};

/// Timeout for one API call. Downloads may take longer in total, but no single read may stall this long.
const TIMEOUT: Duration = Duration::from_secs(30);
/// Renew the token this long before its lease ends.
const RENEW_EARLY: Duration = Duration::from_secs(300);
/// `rspCode` of "please login first" (token missing or expired).
const RSP_LOGIN_FIRST: i64 = -6;

#[derive(Debug, thiserror::Error)]
pub enum HubError {
    #[error("cannot reach the Hub: {0}")]
    Http(String),
    #[error("the Hub refused {cmd}: {detail} (rspCode {code})")]
    Api {
        cmd: String,
        code: i64,
        detail: String,
    },
    #[error("unexpected answer to {cmd}: {why}")]
    Protocol { cmd: String, why: String },
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, HubError>;

/// One recording on the Hub.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HubFile {
    /// The Hub's file name, used to download it.
    pub name: String,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    /// As the Hub reports it: rounded to whole MiB.
    pub size: u64,
    /// `main` or `sub`.
    pub stream: String,
    pub width: u32,
    pub height: u32,
}

/// One camera slot of the Hub.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HubChannel {
    pub channel: u8,
    pub name: String,
    pub online: bool,
    /// A battery camera that is asleep (the Hub reports `sleep` per channel).
    pub sleeping: bool,
}

/// Blocking Hub client. Not shared between threads: one per Hub, used from one task at a time.
pub struct HubClient {
    base: String,
    user: String,
    password: String,
    agent: ureq::Agent,
    session: Option<Session>,
    /// When set, each JSON answer is saved there as `<cmd>.json` with the token removed
    /// (`zoologist hub-test` uses this to capture test fixtures).
    record_dir: Option<PathBuf>,
}

impl std::fmt::Debug for HubClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HubClient")
            .field("base", &self.base)
            .field("user", &self.user)
            .finish_non_exhaustive()
    }
}

/// Converts the Hub's `{"year":…, "mon":…, "day":…, "hour":…, "min":…, "sec":…}` in time zone `tz`.
pub fn parse_hub_time(v: &Value, tz: Tz) -> Option<DateTime<Utc>> {
    let n = |k: &str| v.get(k).and_then(Value::as_i64);
    let date = NaiveDate::from_ymd_opt(n("year")? as i32, n("mon")? as u32, n("day")? as u32)?;
    let naive = date.and_hms_opt(n("hour")? as u32, n("min")? as u32, n("sec")? as u32)?;
    // In the repeated hour of a DST change, `earliest` picks the first; a recording then shows
    // up to an hour early, which is harmless.
    tz.from_local_datetime(&naive)
        .earliest()
        .map(|t| t.with_timezone(&Utc))
}

/// The Hub's time format for `t`, in time zone `tz`.
pub fn hub_time(t: DateTime<Utc>, tz: Tz) -> Value {
    let l = t.with_timezone(&tz);
    json!({
        "year": l.year(), "mon": l.month(), "day": l.day(),
        "hour": l.hour(), "min": l.minute(), "sec": l.second(),
    })
}

/// Splits `[from, to]` at local midnights: the Hub searches one day at a time.
fn split_days(
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    tz: Tz,
) -> Vec<(DateTime<Utc>, DateTime<Utc>)> {
    let mut out = Vec::new();
    let mut start = from;
    while start < to {
        let next_midnight: Option<NaiveDateTime> = start
            .with_timezone(&tz)
            .date_naive()
            .succ_opt()
            .and_then(|d| d.and_hms_opt(0, 0, 0));
        let day_end = next_midnight
            .and_then(|m| tz.from_local_datetime(&m).earliest())
            .map_or(to, |t| t.with_timezone(&Utc))
            .min(to);
        // The Hub's search end is inclusive, to the second.
        out.push((start, (day_end - chrono::Duration::seconds(1)).max(start)));
        start = day_end;
    }
    out
}

/// Removes the token (and any password) from a JSON answer before it is saved.
fn redact(mut v: Value) -> Value {
    fn walk(v: &mut Value) {
        match v {
            Value::Object(map) => {
                for (k, val) in map.iter_mut() {
                    if k == "Token" {
                        if let Some(obj) = val.as_object_mut() {
                            obj.insert("name".into(), json!("REDACTED"));
                        }
                    } else if k.eq_ignore_ascii_case("password") {
                        *val = json!("REDACTED");
                    } else {
                        walk(val);
                    }
                }
            }
            Value::Array(items) => items.iter_mut().for_each(walk),
            _ => {}
        }
    }
    walk(&mut v);
    v
}

/// A logged-in session.
struct Session {
    token: String,
    renew_at: Instant,
    /// Set by the digest ("Version 1") login: bodies, answers and counters are encrypted.
    cipher: Option<crypto::Cipher>,
    /// Anti-replay counters `(countId, checkNum)`. Ids 0–2 are reserved by the Hub's apps
    /// (preview, playback, upload), so only ids from 3 up are used.
    counters: Vec<(u64, u64)>,
    next: usize,
}

impl Session {
    /// The next `countId=…&checkNum=…`, incrementing that counter.
    fn count(&mut self) -> String {
        if self.counters.is_empty() {
            return String::new();
        }
        let i = self.next % self.counters.len();
        self.next = self.next.wrapping_add(1);
        let (id, val) = &mut self.counters[i];
        *val += 1;
        format!("countId={id}&checkNum={val}")
    }
}

/// Parses a JSON answer array and returns its first entry's `value`, or the Hub's error.
fn first_value(cmd: &str, answer: &Value) -> Result<Value> {
    let first = answer
        .as_array()
        .and_then(|a| a.first())
        .ok_or_else(|| HubError::Protocol {
            cmd: cmd.into(),
            why: "empty answer".into(),
        })?;
    if first.get("code").and_then(Value::as_i64) == Some(0) {
        return Ok(first.get("value").cloned().unwrap_or(Value::Null));
    }
    let err = first.get("error").cloned().unwrap_or(Value::Null);
    let mut detail = err
        .get("detail")
        .and_then(Value::as_str)
        .unwrap_or("unknown error")
        .to_string();
    // Failed logins report how close the account is to being locked.
    let warn = &err["auth_warning_info"];
    if let Some(unlock) = warn["unlock_time"].as_u64().filter(|t| *t > 0) {
        detail += &format!(
            "; the account is locked for {} more minutes",
            unlock.div_ceil(60)
        );
    } else if let Some(left) = warn["remain_times"].as_u64() {
        detail += &format!("; {left} attempts left before the Hub locks the account");
    }
    Err(HubError::Api {
        cmd: cmd.into(),
        code: err.get("rspCode").and_then(Value::as_i64).unwrap_or(0),
        detail,
    })
}

impl HubClient {
    /// `base` is the Hub's `http://` address.
    pub fn new(base: &str, user: &str, password: &str) -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(None)
            .timeout_connect(Some(Duration::from_secs(5)))
            .timeout_recv_response(Some(TIMEOUT))
            .timeout_recv_body(None)
            .http_status_as_error(false)
            .build();
        HubClient {
            base: base.trim_end_matches('/').to_string(),
            user: user.to_string(),
            password: password.to_string(),
            agent: config.into(),
            session: None,
            record_dir: None,
        }
    }

    /// Saves each JSON answer (token removed) under `dir`.
    pub fn record_to(&mut self, dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        self.record_dir = Some(dir.to_path_buf());
        Ok(())
    }

    /// True when the Hub uses the digest login and encrypted commands.
    pub fn is_encrypted(&self) -> bool {
        self.session.as_ref().is_some_and(|s| s.cipher.is_some())
    }

    fn record(&self, name: &str, v: &Value) {
        if let Some(dir) = &self.record_dir {
            let text = serde_json::to_string_pretty(&redact(v.clone())).unwrap_or_default();
            if let Err(e) = std::fs::write(dir.join(format!("{name}.json")), text) {
                tracing::warn!("cannot save {name}.json: {e}");
            }
        }
    }

    /// POSTs `body` to `url`; returns the status, any `WWW-Authenticate` header and the text.
    fn send(&self, cmd: &str, url: &str, body: String) -> Result<(u16, Option<String>, String)> {
        let mut resp = self
            .agent
            .post(url)
            .header("content-type", "application/json")
            .send(body)
            .map_err(|e| HubError::Http(scrub(&e.to_string())))?;
        let status = resp.status().as_u16();
        let www = resp
            .headers()
            .get("www-authenticate")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let text = resp
            .body_mut()
            .read_to_string()
            .map_err(|e| HubError::Protocol {
                cmd: cmd.into(),
                why: format!("HTTP {status}: {}", scrub(&e.to_string())),
            })?;
        Ok((status, www, text))
    }

    /// Parses an answer that may be encrypted with `cipher` (errors often come back plain).
    fn parse_answer(cmd: &str, text: &str, cipher: Option<&crypto::Cipher>) -> Result<Value> {
        let plain = cipher
            .and_then(|c| c.decrypt(text))
            .filter(|t| t.trim_start().starts_with('['));
        let text = plain.as_deref().unwrap_or(text);
        serde_json::from_str(text).map_err(|_| HubError::Protocol {
            cmd: cmd.into(),
            why: format!("not JSON: {:.120}", scrub(text)),
        })
    }

    /// Logs in, using the digest login when the Hub offers it and the classic one otherwise.
    pub fn login(&mut self) -> Result<()> {
        self.session = None;
        let url = format!("{}/{}", self.base, crypto::LOGIN_URI);
        let hello = json!([{ "cmd": "Login", "action": 0, "param": { "Version": 1 } }]).to_string();
        let (_, www, _) = self.send("Login", &url, hello)?;
        let session = match www.as_deref().and_then(crypto::Challenge::parse) {
            Some(challenge) => self.digest_login(&url, &challenge)?,
            None => self.classic_login(&url)?,
        };
        self.session = Some(session);
        Ok(())
    }

    fn digest_login(&self, url: &str, c: &crypto::Challenge) -> Result<Session> {
        let cnonce = crypto::new_cnonce();
        let cipher = crypto::Cipher::new(c, &self.user, &self.password, &cnonce);
        let param = json!({ "Version": 1, "Digest": {
            "UserName": self.user,
            "Realm": c.realm,
            "Method": "POST",
            "Uri": crypto::LOGIN_URI,
            "Nonce": c.nonce,
            "Nc": c.nc,
            "Cnonce": cnonce,
            "Qop": c.qop,
            "Response": crypto::digest_response(c, &self.user, &self.password, &cnonce),
        }});
        let body = json!([{ "cmd": "Login", "action": 0, "param": param }]).to_string();
        let (_, _, text) = self.send("Login", url, body)?;
        let answer = Self::parse_answer("Login", &text, Some(&cipher))?;
        let value = first_value("Login", &answer)?;
        self.record("Login", &answer);
        let token = &value["Token"];
        let total = token["countTotal"].as_u64().unwrap_or(0);
        let basic = token["checkBasic"].as_u64().unwrap_or(0);
        Ok(Session {
            token: Self::token_name(token)?,
            renew_at: Self::renew_at(token),
            cipher: Some(cipher),
            counters: (3..total).map(|id| (id, basic)).collect(),
            next: 0,
        })
    }

    fn classic_login(&self, url: &str) -> Result<Session> {
        let param =
            json!({ "User": { "Version": "0", "userName": self.user, "password": self.password } });
        let body = json!([{ "cmd": "Login", "action": 0, "param": param }]).to_string();
        let (_, _, text) = self.send("Login", url, body)?;
        let answer = Self::parse_answer("Login", &text, None)?;
        let value = first_value("Login", &answer)?;
        self.record("Login", &answer);
        Ok(Session {
            token: Self::token_name(&value["Token"])?,
            renew_at: Self::renew_at(&value["Token"]),
            cipher: None,
            counters: Vec::new(),
            next: 0,
        })
    }

    fn token_name(token: &Value) -> Result<String> {
        token["name"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| HubError::Protocol {
                cmd: "Login".into(),
                why: "no Token.name".into(),
            })
    }

    fn renew_at(token: &Value) -> Instant {
        let lease = Duration::from_secs(token["leaseTime"].as_u64().unwrap_or(3600));
        Instant::now() + lease.saturating_sub(RENEW_EARLY)
    }

    /// The current session, logging in when there is none or it is about to expire.
    fn session(&mut self) -> Result<&mut Session> {
        if self
            .session
            .as_ref()
            .is_none_or(|s| Instant::now() >= s.renew_at)
        {
            self.login()?;
        }
        Ok(self.session.as_mut().expect("logged in"))
    }

    /// Sends one authenticated command and returns its `value`.
    fn post(&mut self, cmd: &str, param: &Value) -> Result<Value> {
        let base = self.base.clone();
        let session = self.session()?;
        let plain = json!([{ "cmd": cmd, "action": 0, "param": param }]).to_string();
        let (url, body) = match &session.cipher {
            Some(cipher) => {
                let cipher = cipher.clone();
                let query = format!("{}&cmd={cmd}", session.count());
                (
                    format!(
                        "{base}/cgi-bin/api.cgi?token={}&encrypt={}",
                        session.token,
                        cipher.encrypt(&query)
                    ),
                    cipher.encrypt(&plain),
                )
            }
            None => (
                format!("{base}/cgi-bin/api.cgi?cmd={cmd}&token={}", session.token),
                plain,
            ),
        };
        let cipher = session.cipher.clone();
        let (_, _, text) = self.send(cmd, &url, body)?;
        let answer = Self::parse_answer(cmd, &text, cipher.as_ref())?;
        let value = first_value(cmd, &answer)?;
        self.record(cmd, &answer);
        Ok(value)
    }

    /// Runs an authenticated command, logging in again once if the Hub forgot the session.
    pub fn call(&mut self, cmd: &str, param: Value) -> Result<Value> {
        match self.post(cmd, &param) {
            Err(HubError::Api {
                code: RSP_LOGIN_FIRST,
                ..
            }) => {
                self.session = None;
                self.post(cmd, &param)
            }
            other => other,
        }
    }

    /// Model, firmware, channel count, … as the Hub reports them.
    pub fn device_info(&mut self) -> Result<Value> {
        Ok(self.call("GetDevInfo", json!({}))?["DevInfo"].take())
    }

    /// The Hub's camera slots.
    pub fn channels(&mut self) -> Result<Vec<HubChannel>> {
        let value = self.call("GetChannelstatus", json!({}))?;
        let list = value["status"].as_array().cloned().unwrap_or_default();
        Ok(list
            .iter()
            .filter_map(|c| {
                Some(HubChannel {
                    channel: c["channel"].as_u64()? as u8,
                    name: c["name"].as_str().unwrap_or_default().to_string(),
                    online: c["online"].as_i64() == Some(1),
                    sleeping: c["sleep"].as_i64() == Some(1),
                })
            })
            .collect())
    }

    /// Recordings of `channel` on `stream` (`"main"` or `"sub"`) that overlap `[from, to]`,
    /// oldest first. `tz` is the Hub's time zone (the station's).
    pub fn search(
        &mut self,
        channel: u8,
        stream: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        tz: Tz,
    ) -> Result<Vec<HubFile>> {
        let mut files = Vec::new();
        for (day, (start, end)) in split_days(from, to, tz).into_iter().enumerate() {
            // As the Hub's web app sends it (iLogicChannel 1 is a dual-lens camera's second lens).
            let param = json!({ "Search": {
                "channel": channel,
                "iLogicChannel": 0,
                "onlyStatus": 0,
                "streamType": stream,
                "StartTime": hub_time(start, tz),
                "EndTime": hub_time(end, tz),
            }});
            let value = self.call("Search", param)?;
            if day == 0 {
                self.record(&format!("Search-ch{channel}-{stream}"), &value);
            }
            files.extend(parse_search(&value, stream, tz));
        }
        files.sort_by_key(|f| f.start);
        files.dedup_by(|a, b| a.name == b.name);
        Ok(files)
    }

    /// Downloads recording `file` to `dest` (through `dest.part`, renamed when complete).
    /// Returns the number of bytes.
    ///
    /// The Hub sends recordings at about real-time speed, so the time limit is twice the
    /// recording's length plus a minute.
    pub fn download(&mut self, file: &HubFile, dest: &Path) -> Result<u64> {
        let name = file.name.as_str();
        let length = (file.end - file.start).to_std().unwrap_or_default();
        let limit = length * 2 + Duration::from_secs(60);
        let encrypted = {
            self.session()?;
            self.is_encrypted()
        };
        if encrypted {
            // The Home Hub web app asks first; a `fileName` or `prompt` means the recording
            // itself is encrypted ("File Encryption" in the Reolink app).
            let check = self.call("CheckDownload", json!({ "filename": name }))?;
            let locked = ["fileName", "prompt"]
                .iter()
                .any(|k| check[*k].as_str().is_some_and(|v| !v.is_empty()));
            if locked {
                return Err(HubError::Protocol {
                    cmd: "Download".into(),
                    why: "the recording is encrypted: turn off File Encryption for the Hub in the Reolink app".into(),
                });
            }
        }
        let base = self.base.clone();
        let session = self.session()?;
        let url = match &session.cipher {
            Some(cipher) => {
                let cipher = cipher.clone();
                let count = session.count();
                format!(
                    "{base}/cgi-bin/api.cgi?cmd=download&source={name}&token={}&encrypt={}",
                    session.token,
                    cipher.encrypt(&count)
                )
            }
            None => {
                let output = name.rsplit('/').next().unwrap_or(name);
                format!(
                    "{base}/cgi-bin/api.cgi?cmd=Download&source={}&output={}&token={}",
                    urlencode(name),
                    urlencode(output),
                    session.token
                )
            }
        };
        let mut resp = self
            .agent
            .get(&url)
            .config()
            .timeout_recv_body(Some(limit))
            .build()
            .call()
            .map_err(|e| HubError::Http(scrub(&e.to_string())))?;
        let status = resp.status();
        let json_answer = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|t| t.contains("json") || t.contains("text"));
        if !status.is_success() || json_answer {
            let mut text = String::new();
            let _ = resp
                .body_mut()
                .as_reader()
                .take(2000)
                .read_to_string(&mut text);
            if text.contains("please login first") {
                self.session = None;
            }
            return Err(HubError::Protocol {
                cmd: "Download".into(),
                why: format!("HTTP {status}: {}", scrub(text.trim())),
            });
        }
        if let Some(dir) = dest.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let part = dest.with_extension("part");
        let mut out = std::fs::File::create(&part)?;
        let mut reader = resp.body_mut().with_config().limit(u64::MAX).reader();
        let copied = std::io::copy(&mut reader, &mut out);
        drop(reader);
        let bytes = match copied {
            Ok(n) => n,
            Err(e) => {
                let _ = std::fs::remove_file(&part);
                return Err(HubError::Protocol {
                    cmd: "Download".into(),
                    why: format!(
                        "stopped after {} bytes: {}",
                        file_len(&part),
                        scrub(&e.to_string())
                    ),
                });
            }
        };
        out.sync_all()?;
        drop(out);
        // The search's `size` is rounded to whole MiB, so only an empty file is known to be wrong.
        if bytes == 0 {
            let _ = std::fs::remove_file(&part);
            return Err(HubError::Protocol {
                cmd: "Download".into(),
                why: "the Hub sent an empty file".into(),
            });
        }
        std::fs::rename(&part, dest)?;
        Ok(bytes)
    }

    /// Ends the session (best effort; sessions also expire by themselves).
    pub fn logout(&mut self) {
        if self.session.is_some() {
            let _ = self.post("Logout", &json!({}));
            self.session = None;
        }
    }
}

impl Drop for HubClient {
    fn drop(&mut self) {
        self.logout();
    }
}

/// Parses a `Search` answer's `SearchResult.File` list.
pub fn parse_search(value: &Value, stream: &str, tz: Tz) -> Vec<HubFile> {
    let list = value["SearchResult"]["File"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    list.iter()
        .filter_map(|f| {
            Some(HubFile {
                name: f["name"].as_str()?.to_string(),
                start: parse_hub_time(&f["StartTime"], tz)?,
                end: parse_hub_time(&f["EndTime"], tz)?,
                size: f["size"]
                    .as_u64()
                    .or_else(|| f["size"].as_str().and_then(|s| s.parse().ok()))
                    .unwrap_or(0),
                stream: f["type"].as_str().unwrap_or(stream).to_string(),
                width: f["width"].as_u64().unwrap_or(0) as u32,
                height: f["height"].as_u64().unwrap_or(0) as u32,
            })
        })
        .collect()
}

fn file_len(path: &Path) -> u64 {
    std::fs::metadata(path).map_or(0, |m| m.len())
}

/// Removes `token=…` from a message (URLs in errors must not leak the token).
fn scrub(msg: &str) -> String {
    let mut out = String::with_capacity(msg.len());
    let mut rest = msg;
    while let Some(i) = rest.find("token=") {
        out.push_str(&rest[..i + 6]);
        out.push_str("REDACTED");
        rest = &rest[i + 6..];
        let end = rest
            .find(|c: char| c == '&' || c == '"' || c.is_whitespace())
            .unwrap_or(rest.len());
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// Percent-encodes a query value (file names contain `/`).
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

mod crypto;

#[cfg(test)]
mod tests;
