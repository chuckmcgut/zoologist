//! A minimal ONVIF client: lists a camera's media profiles (codec, resolution, frame rate,
//! bitrate), their RTSP addresses and the resolutions each encoder allows. Used by
//! `zoologist onvif` to find a camera's streams.
//!
//! Requests are SOAP 1.2 over plain HTTP, authenticated with a WS-Security UsernameToken
//! digest (the password itself is never sent). The token's time is taken from the camera's
//! clock, which ONVIF gives without a login, so a wrong clock on either side does not matter.

use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use chrono::{DateTime, TimeZone, Utc};
use sha1::{Digest, Sha1};

const DEVICE_NS: &str = "http://www.onvif.org/ver10/device/wsdl";
const MEDIA_NS: &str = "http://www.onvif.org/ver10/media/wsdl";
const SCHEMA_NS: &str = "http://www.onvif.org/ver10/schema";

#[derive(Debug, thiserror::Error)]
pub enum OnvifError {
    #[error("cannot reach the camera: {0}")]
    Http(String),
    #[error("the camera refused {call}: {reason}")]
    Fault { call: String, reason: String },
    #[error("unexpected answer to {0}")]
    Protocol(String),
}

pub type Result<T> = std::result::Result<T, OnvifError>;

/// Resolutions an encoder allows, per codec: `("H264", [(1920, 1080), …])`.
pub type AllowedResolutions = Vec<(String, Vec<(u32, u32)>)>;

/// One media profile: a stream the camera offers.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Profile {
    pub token: String,
    pub name: String,
    /// The video source (sensor) this profile encodes.
    pub source: String,
    pub encoding: String,
    pub width: u32,
    pub height: u32,
    pub fps: Option<f32>,
    pub bitrate_kbps: Option<u32>,
    /// Encoder configuration token (for the allowed resolutions).
    pub encoder: String,
}

/// Every element with local name `name` (any namespace prefix): `(attributes, inner text)`.
pub fn elements<'a>(xml: &'a str, name: &str) -> Vec<(&'a str, &'a str)> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(lt) = rest.find('<') {
        rest = &rest[lt + 1..];
        let tag_end = rest
            .find(['>', ' ', '/', '\t', '\n', '\r'])
            .unwrap_or(rest.len());
        let full = &rest[..tag_end];
        if full.starts_with('/') || full.starts_with('?') || full.starts_with('!') {
            continue;
        }
        let local = full.rsplit(':').next().unwrap_or(full);
        if local != name {
            continue;
        }
        let Some(gt) = rest.find('>') else { break };
        let attrs = &rest[tag_end..gt];
        if attrs.ends_with('/') {
            out.push((attrs.trim_end_matches('/'), ""));
            rest = &rest[gt + 1..];
            continue;
        }
        let body = &rest[gt + 1..];
        let close = format!("</{full}>");
        let Some(end) = body.find(&close) else { break };
        out.push((attrs, &body[..end]));
        rest = &body[end + close.len()..];
    }
    out
}

/// The inner text of the first element named `name`, trimmed.
pub fn text<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    elements(xml, name).first().map(|(_, t)| t.trim())
}

/// The value of attribute `name` in an attribute string.
pub fn attr<'a>(attrs: &'a str, name: &str) -> Option<&'a str> {
    let key = format!("{name}=\"");
    let i = attrs.find(&key)? + key.len();
    let end = attrs[i..].find('"')?;
    Some(&attrs[i..i + end])
}

/// The WS-Security password digest: Base64(SHA1(nonce + created + password)).
pub fn password_digest(nonce: &[u8], created: &str, password: &str) -> String {
    let mut h = Sha1::new();
    h.update(nonce);
    h.update(created.as_bytes());
    h.update(password.as_bytes());
    B64.encode(h.finalize())
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

pub struct OnvifClient {
    device_url: String,
    user: String,
    password: String,
    agent: ureq::Agent,
    /// Camera time minus local time.
    clock_offset: chrono::Duration,
}

impl OnvifClient {
    /// `device_url` is the device service, e.g. `http://192.168.1.40:9520/onvif/device_service`.
    pub fn new(device_url: &str, user: &str, password: &str) -> Self {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(10)))
            .http_status_as_error(false)
            .build()
            .into();
        OnvifClient {
            device_url: device_url.to_string(),
            user: user.to_string(),
            password: password.to_string(),
            agent,
            clock_offset: chrono::Duration::zero(),
        }
    }

    fn security_header(&self) -> String {
        if self.user.is_empty() {
            return String::new();
        }
        let mut nonce = [0u8; 16];
        let _ = getrandom::fill(&mut nonce);
        let created = (Utc::now() + self.clock_offset)
            .format("%Y-%m-%dT%H:%M:%S%.3fZ")
            .to_string();
        let digest = password_digest(&nonce, &created, &self.password);
        format!(
            r#"<s:Header><Security s:mustUnderstand="1" xmlns="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-secext-1.0.xsd"><UsernameToken><Username>{}</Username><Password Type="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-username-token-profile-1.0#PasswordDigest">{digest}</Password><Nonce EncodingType="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-soap-message-security-1.0#Base64Binary">{}</Nonce><Created xmlns="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-utility-1.0.xsd">{created}</Created></UsernameToken></Security></s:Header>"#,
            escape(&self.user),
            B64.encode(nonce)
        )
    }

    /// Sends one SOAP call and returns the response body.
    fn call(&self, url: &str, call: &str, body: &str, auth: bool) -> Result<String> {
        let header = if auth {
            self.security_header()
        } else {
            String::new()
        };
        let envelope = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?><s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope">{header}<s:Body>{body}</s:Body></s:Envelope>"#
        );
        let mut resp = self
            .agent
            .post(url)
            .header("content-type", "application/soap+xml; charset=utf-8")
            .send(envelope)
            .map_err(|e| OnvifError::Http(e.to_string()))?;
        let status = resp.status().as_u16();
        let text = resp
            .body_mut()
            .read_to_string()
            .map_err(|e| OnvifError::Http(e.to_string()))?;
        if status >= 400 || text.contains(":Fault>") {
            let reason = text_of_fault(&text).unwrap_or_else(|| format!("HTTP {status}"));
            return Err(OnvifError::Fault {
                call: call.into(),
                reason,
            });
        }
        Ok(text)
    }

    /// Reads the camera's clock (no login needed) so the security token's time matches it.
    pub fn sync_clock(&mut self) -> Result<DateTime<Utc>> {
        let xml = self.call(
            &self.device_url.clone(),
            "GetSystemDateAndTime",
            &format!(r#"<GetSystemDateAndTime xmlns="{DEVICE_NS}"/>"#),
            false,
        )?;
        let utc = elements(&xml, "UTCDateTime")
            .first()
            .map(|(_, t)| *t)
            .ok_or_else(|| OnvifError::Protocol("GetSystemDateAndTime".into()))?;
        let n = |k: &str| text(utc, k).and_then(|v| v.parse::<u32>().ok());
        let camera = camera_time(
            n("Year").unwrap_or(1970),
            n("Month").unwrap_or(1),
            n("Day").unwrap_or(1),
            n("Hour").unwrap_or(0),
            n("Minute").unwrap_or(0),
            n("Second").unwrap_or(0),
        )
        .ok_or_else(|| OnvifError::Protocol("GetSystemDateAndTime".into()))?;
        self.clock_offset = camera - Utc::now();
        Ok(camera)
    }

    /// Device information: manufacturer, model, firmware (needs a login on most cameras).
    pub fn device_information(&self) -> Result<Vec<(String, String)>> {
        let xml = self.call(
            &self.device_url.clone(),
            "GetDeviceInformation",
            &format!(r#"<GetDeviceInformation xmlns="{DEVICE_NS}"/>"#),
            true,
        )?;
        Ok(["Manufacturer", "Model", "FirmwareVersion", "HardwareId"]
            .iter()
            .filter_map(|k| text(&xml, k).map(|v| (k.to_string(), v.to_string())))
            .collect())
    }

    /// The media service address.
    pub fn media_url(&self) -> Result<String> {
        let xml = self.call(
            &self.device_url.clone(),
            "GetServices",
            &format!(
                r#"<GetServices xmlns="{DEVICE_NS}"><IncludeCapability>false</IncludeCapability></GetServices>"#
            ),
            false,
        )?;
        for (_, service) in elements(&xml, "Service") {
            if text(service, "Namespace") == Some(MEDIA_NS)
                && let Some(addr) = text(service, "XAddr")
            {
                return Ok(addr.to_string());
            }
        }
        Err(OnvifError::Protocol("GetServices: no media service".into()))
    }

    /// Uses local time for the security token instead of the camera's clock.
    pub fn use_local_clock(&mut self) {
        self.clock_offset = chrono::Duration::zero();
    }

    pub fn profiles(&self, media: &str) -> Result<Vec<Profile>> {
        let xml = self.call(
            media,
            "GetProfiles",
            &format!(r#"<GetProfiles xmlns="{MEDIA_NS}"/>"#),
            true,
        )?;
        Ok(elements(&xml, "Profiles")
            .into_iter()
            .map(|(attrs, body)| {
                let enc = elements(body, "VideoEncoderConfiguration");
                let enc_body = enc.first().map_or("", |(_, b)| *b);
                let src = elements(body, "VideoSourceConfiguration");
                Profile {
                    token: attr(attrs, "token").unwrap_or_default().to_string(),
                    name: text(body, "Name").unwrap_or_default().to_string(),
                    source: src
                        .first()
                        .and_then(|(_, b)| text(b, "SourceToken"))
                        .unwrap_or_default()
                        .to_string(),
                    encoding: text(enc_body, "Encoding").unwrap_or_default().to_string(),
                    width: text(enc_body, "Width")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0),
                    height: text(enc_body, "Height")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0),
                    fps: text(enc_body, "FrameRateLimit").and_then(|v| v.parse().ok()),
                    bitrate_kbps: text(enc_body, "BitrateLimit").and_then(|v| v.parse().ok()),
                    encoder: enc
                        .first()
                        .and_then(|(a, _)| attr(a, "token"))
                        .unwrap_or_default()
                        .to_string(),
                }
            })
            .collect())
    }

    pub fn stream_uri(&self, media: &str, profile: &str) -> Result<String> {
        let body = format!(
            r#"<GetStreamUri xmlns="{MEDIA_NS}"><StreamSetup><Stream xmlns="{SCHEMA_NS}">RTP-Unicast</Stream><Transport xmlns="{SCHEMA_NS}"><Protocol>RTSP</Protocol></Transport></StreamSetup><ProfileToken>{}</ProfileToken></GetStreamUri>"#,
            escape(profile)
        );
        let xml = self.call(media, "GetStreamUri", &body, true)?;
        text(&xml, "Uri")
            .map(|u| u.replace("&amp;", "&"))
            .ok_or_else(|| OnvifError::Protocol("GetStreamUri".into()))
    }

    /// Resolutions the profile's encoder allows, per codec: `("H264", [(1920, 1080), …])`.
    pub fn allowed_resolutions(
        &self,
        media: &str,
        profile: &Profile,
    ) -> Result<AllowedResolutions> {
        let body = format!(
            r#"<GetVideoEncoderConfigurationOptions xmlns="{MEDIA_NS}"><ConfigurationToken>{}</ConfigurationToken><ProfileToken>{}</ProfileToken></GetVideoEncoderConfigurationOptions>"#,
            escape(&profile.encoder),
            escape(&profile.token)
        );
        let xml = self.call(media, "GetVideoEncoderConfigurationOptions", &body, true)?;
        let mut out = Vec::new();
        for codec in ["JPEG", "MPEG4", "H264"] {
            if let Some((_, block)) = elements(&xml, codec).first() {
                let sizes: Vec<(u32, u32)> = elements(block, "ResolutionsAvailable")
                    .iter()
                    .filter_map(|(_, r)| {
                        Some((
                            text(r, "Width")?.parse().ok()?,
                            text(r, "Height")?.parse().ok()?,
                        ))
                    })
                    .collect();
                if !sizes.is_empty() {
                    out.push((codec.to_string(), sizes));
                }
            }
        }
        Ok(out)
    }
}

/// Builds the camera's UTC time. Some cameras (the Guide NC200 among them) report C `struct tm`
/// values: years since 1900 and months from 0.
pub fn camera_time(
    year: u32,
    month: u32,
    day: u32,
    h: u32,
    m: u32,
    s: u32,
) -> Option<DateTime<Utc>> {
    let (year, month) = if year < 1900 {
        (year + 1900, month + 1)
    } else {
        (year, month)
    };
    Utc.with_ymd_and_hms(year as i32, month, day, h, m, s)
        .single()
}

fn text_of_fault(xml: &str) -> Option<String> {
    let reason = elements(xml, "Reason").first().map(|(_, b)| *b)?;
    Some(text(reason, "Text").unwrap_or(reason).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_digest_matches_the_spec_example() {
        // Reference computed independently with Python's hashlib.
        let nonce = B64.decode("LKqI6G/AikKCQrN0zqZFlg==").unwrap();
        assert_eq!(
            password_digest(&nonce, "2010-09-16T07:50:45Z", "userpassword"),
            "tuOSpGlFlIXsozq4HFNeeGeFLEI="
        );
    }

    #[test]
    fn struct_tm_style_camera_times_are_understood() {
        let t = Utc.with_ymd_and_hms(2026, 9, 19, 18, 53, 37).unwrap();
        assert_eq!(camera_time(126, 8, 19, 18, 53, 37), Some(t));
        assert_eq!(camera_time(2026, 9, 19, 18, 53, 37), Some(t));
    }

    #[test]
    fn finds_elements_whatever_their_prefix() {
        let xml = r#"<trt:GetProfilesResponse><trt:Profiles token="p0" fixed="true"><tt:Name>main</tt:Name>
            <tt:VideoEncoderConfiguration token="e0"><tt:Encoding>H264</tt:Encoding>
            <tt:Resolution><tt:Width>1920</tt:Width><tt:Height>1080</tt:Height></tt:Resolution>
            </tt:VideoEncoderConfiguration></trt:Profiles><trt:Profiles token="p1"><tt:Name>ir</tt:Name>
            <tt:Empty/></trt:Profiles></trt:GetProfilesResponse>"#;
        let profiles = elements(xml, "Profiles");
        assert_eq!(profiles.len(), 2);
        assert_eq!(attr(profiles[0].0, "token"), Some("p0"));
        assert_eq!(text(profiles[0].1, "Name"), Some("main"));
        assert_eq!(text(profiles[0].1, "Width"), Some("1920"));
        assert_eq!(text(profiles[1].1, "Name"), Some("ir"));
        assert_eq!(elements(profiles[1].1, "Empty"), vec![("", "")]);
    }
}
