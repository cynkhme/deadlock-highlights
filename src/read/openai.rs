//! The `OpenAI` chat-completions reader.

use std::time::Duration;

use base64::Engine as _;

use super::{HudReader, ReadError, ReaderIdentity};
use crate::error::{Error, Result};

/// The clock-only prompt. It holds no sample value: models echo an example on frames
/// without a HUD.
pub(crate) const PROMPT: &str = "A HUD strip from the game Deadlock. It may show the game clock as minutes:seconds. Copy the clock exactly as shown. If the strip does not show it, use an empty string. Answer with JSON only: {\"clock\": \"<clock or empty>\"}.";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

pub(crate) struct OpenAiChat {
    agent: ureq::Agent,
    server: String,
    model: String,
    auth: Option<String>,
}

/// A key goes only over https or to a loopback server, so it never crosses a network in
/// clear text.
pub(crate) fn check_key_policy(server: &str, key: Option<&str>) -> Result<()> {
    if key.is_none() {
        return Ok(());
    }
    let lower = server.to_ascii_lowercase();
    if lower.starts_with("https://") {
        return Ok(());
    }
    let loopback = lower.strip_prefix("http://").is_some_and(|rest| {
        let authority = rest.split('/').next().unwrap_or("");
        let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
        let host = match host.strip_prefix('[') {
            Some(v6) => v6.split(']').next().unwrap_or(""),
            None => host.split(':').next().unwrap_or(""),
        };
        host == "localhost"
            || host == "::1"
            || host
                .parse::<std::net::Ipv4Addr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    if loopback {
        Ok(())
    } else {
        Err(Error::Usage(format!(
            "DLHL_OPENAI_API_KEY is only sent over https or to a loopback server, not to {server}"
        )))
    }
}

impl OpenAiChat {
    /// Checks the key policy, then that `server` serves `model`.
    pub(crate) fn new(server: &str, model: &str, key: Option<&str>) -> Result<OpenAiChat> {
        check_key_policy(server, key)?;
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(REQUEST_TIMEOUT))
            .build()
            .into();
        let reader = OpenAiChat {
            agent,
            server: server.trim_end_matches('/').to_owned(),
            model: model.to_owned(),
            auth: key.map(|k| format!("Bearer {k}")),
        };
        reader.check_model()?;
        Ok(reader)
    }

    fn check_model(&self) -> Result<()> {
        let url = format!("{}/models", self.server);
        let mut req = self.agent.get(&url);
        if let Some(a) = &self.auth {
            req = req.header("authorization", a);
        }
        let mut resp = req
            .call()
            .map_err(|e| Error::Reader(format!("model server {} unreachable: {e}", self.server)))?;
        let status = resp.status().as_u16();
        let body = resp.body_mut().read_to_string().unwrap_or_default();
        if status != 200 {
            return Err(Error::Reader(format!(
                "GET {url}: {status}: {}",
                clip(&body)
            )));
        }
        let ids: Vec<String> = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v["data"].as_array().cloned())
            .unwrap_or_default()
            .iter()
            .filter_map(|m| m["id"].as_str().map(str::to_owned))
            .collect();
        if ids.contains(&self.model) {
            Ok(())
        } else {
            Err(Error::Reader(format!(
                "model {} is not served by {} (it serves: {})",
                self.model,
                self.server,
                ids.join(", ")
            )))
        }
    }
}

fn clip(s: &str) -> &str {
    let end = s.char_indices().nth(300).map_or(s.len(), |(i, _)| i);
    &s[..end]
}

impl HudReader for OpenAiChat {
    fn identity(&self) -> ReaderIdentity {
        ReaderIdentity {
            protocol: "openai-chat",
            server: self.server.clone(),
            model: self.model.clone(),
            prompt: PROMPT,
        }
    }

    fn read(&self, png: &[u8]) -> std::result::Result<String, ReadError> {
        let image = format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(png)
        );
        // No response_format: Ollama's MLX runner answers any with 501. reasoning_effort
        // "none": without it gemma4:e4b spends its token budget on hidden thinking.
        let body = serde_json::json!({
            "model": self.model,
            "temperature": 0,
            "max_tokens": 64,
            "reasoning_effort": "none",
            "messages": [{"role": "user", "content": [
                {"type": "image_url", "image_url": {"url": image}},
                {"type": "text", "text": PROMPT},
            ]}],
        });
        let bytes = serde_json::to_vec(&body).expect("a JSON value serialises");
        let mut req = self
            .agent
            .post(format!("{}/chat/completions", self.server))
            .header("content-type", "application/json");
        if let Some(a) = &self.auth {
            req = req.header("authorization", a);
        }
        let mut resp = req
            .send(&bytes[..])
            .map_err(|e| ReadError::Retryable(e.to_string()))?;
        let status = resp.status().as_u16();
        let retry_after = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok()?.trim().parse::<u64>().ok())
            .map(Duration::from_secs);
        let text = resp
            .body_mut()
            .read_to_string()
            .map_err(|e| ReadError::Retryable(e.to_string()))?;
        match status {
            200 => {}
            429 => return Err(ReadError::RateLimited(retry_after)),
            500 | 502..=504 => {
                return Err(ReadError::Retryable(format!("{status}: {}", clip(&text))));
            }
            _ => return Err(ReadError::Fatal(format!("{status}: {}", clip(&text)))),
        }
        let content = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| {
                v.get("choices")?
                    .get(0)?
                    .get("message")?
                    .get("content")
                    .cloned()
            });
        Ok(match content {
            Some(serde_json::Value::String(c)) => c,
            Some(serde_json::Value::Null) => String::new(),
            // Not the chat shape: the body itself is the answer, kept raw and validated empty.
            _ => text,
        })
    }
}

#[cfg(test)]
#[allow(clippy::many_single_char_names, reason = "short-lived test values")]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    #[derive(Debug, Clone)]
    struct Req {
        line: String,
        headers: Vec<(String, String)>,
        body: String,
    }

    impl Req {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.as_str())
        }
    }

    /// Answers each request with the next scripted (status, extra headers, body).
    fn server(script: Vec<(u16, &'static str, String)>) -> (String, Arc<Mutex<Vec<Req>>>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/v1", l.local_addr().unwrap());
        let log = Arc::new(Mutex::new(Vec::new()));
        let seen = log.clone();
        std::thread::spawn(move || {
            for (status, extra, body) in script {
                let (mut s, _) = l.accept().unwrap();
                let mut r = BufReader::new(s.try_clone().unwrap());
                let mut line = String::new();
                r.read_line(&mut line).unwrap();
                let mut headers = Vec::new();
                loop {
                    let mut h = String::new();
                    r.read_line(&mut h).unwrap();
                    let h = h.trim_end();
                    if h.is_empty() {
                        break;
                    }
                    let (k, v) = h.split_once(':').unwrap();
                    headers.push((k.trim().to_owned(), v.trim().to_owned()));
                }
                let len = headers
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                    .map_or(0, |(_, v)| v.parse().unwrap());
                let mut b = vec![0; len];
                r.read_exact(&mut b).unwrap();
                seen.lock().unwrap().push(Req {
                    line: line.trim_end().to_owned(),
                    headers,
                    body: String::from_utf8(b).unwrap(),
                });
                write!(
                    s,
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\n{extra}content-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
        });
        (base, log)
    }

    fn models() -> (u16, &'static str, String) {
        (
            200,
            "",
            r#"{"object":"list","data":[{"id":"qwen3-vl:4b-instruct"},{"id":"gemma4:e4b"}]}"#
                .into(),
        )
    }

    fn chat(content: &str) -> (u16, &'static str, String) {
        (200, "", serde_json::json!({"choices": [{"message": {"role": "assistant", "content": content}}]}).to_string())
    }

    #[test]
    fn models_check_lists_model() {
        let (base, log) = server(vec![models()]);
        OpenAiChat::new(&base, "gemma4:e4b", None).unwrap();
        assert_eq!(log.lock().unwrap()[0].line, "GET /v1/models HTTP/1.1");
    }

    #[test]
    fn model_not_served_is_reader_error() {
        let (base, _) = server(vec![models()]);
        let e = OpenAiChat::new(&base, "llava", None).err().unwrap();
        assert!(
            matches!(&e, Error::Reader(m) if m.contains("llava") && m.contains("gemma4:e4b")),
            "{e}"
        );
    }

    #[test]
    fn unreachable_server_is_reader_error() {
        let e = OpenAiChat::new("http://127.0.0.1:9/v1", "gemma4:e4b", None)
            .err()
            .unwrap();
        assert!(
            matches!(&e, Error::Reader(m) if m.contains("127.0.0.1:9")),
            "{e}"
        );
    }

    #[test]
    fn request_body_shape() {
        let (base, log) = server(vec![models(), chat(r#"{"clock":"5:17"}"#)]);
        let r = OpenAiChat::new(&base, "gemma4:e4b", None).unwrap();
        assert_eq!(r.read(b"PNGDATA").unwrap(), r#"{"clock":"5:17"}"#);
        let req = log.lock().unwrap()[1].clone();
        assert_eq!(req.line, "POST /v1/chat/completions HTTP/1.1");
        assert!(req.header("authorization").is_none());
        let b: serde_json::Value = serde_json::from_str(&req.body).unwrap();
        assert_eq!(b["model"], "gemma4:e4b");
        assert_eq!(b["temperature"], 0);
        assert_eq!(b["max_tokens"], 64);
        assert_eq!(b["reasoning_effort"], "none");
        assert!(b.get("response_format").is_none());
        let content = &b["messages"][0]["content"];
        let url = content[0]["image_url"]["url"].as_str().unwrap();
        assert_eq!(
            url,
            format!(
                "data:image/png;base64,{}",
                base64::engine::general_purpose::STANDARD.encode(b"PNGDATA")
            )
        );
        assert_eq!(content[1]["text"], PROMPT);
        assert_eq!(r.identity().protocol, "openai-chat");
    }

    #[test]
    fn fenced_and_unfenced_answers() {
        let fenced = "```json\n{\"clock\": \"5:17\"}\n```";
        let (base, _) = server(vec![models(), chat(fenced), chat(r#"{"clock":"5:17"}"#)]);
        let r = OpenAiChat::new(&base, "gemma4:e4b", None).unwrap();
        for _ in 0..2 {
            assert_eq!(
                crate::clock::parse_answer(&r.read(b"x").unwrap()),
                Some(317)
            );
        }
    }

    #[test]
    fn openai_prose_answer_is_kept_raw() {
        let (base, _) = server(vec![
            models(),
            chat("I see 5:17"),
            (200, "", "not json".into()),
            (
                200,
                "",
                r#"{"choices":[{"message":{"content":null}}]}"#.into(),
            ),
        ]);
        let r = OpenAiChat::new(&base, "gemma4:e4b", None).unwrap();
        assert_eq!(r.read(b"x").unwrap(), "I see 5:17");
        assert_eq!(r.read(b"x").unwrap(), "not json");
        assert_eq!(r.read(b"x").unwrap(), "");
    }

    #[test]
    fn statuses_map_to_read_errors() {
        let (base, _) = server(vec![
            models(),
            (
                400,
                "",
                r#"{"error":"unknown field reasoning_effort"}"#.into(),
            ),
            (429, "retry-after: 7\r\n", String::new()),
            (429, "", String::new()),
            (503, "", String::new()),
            (
                501,
                "",
                r#"{"error":"structured output is unavailable"}"#.into(),
            ),
        ]);
        let r = OpenAiChat::new(&base, "gemma4:e4b", None).unwrap();
        assert!(
            matches!(r.read(b"x"), Err(ReadError::Fatal(m)) if m.starts_with("400") && m.contains("unknown field"))
        );
        assert_eq!(
            r.read(b"x"),
            Err(ReadError::RateLimited(Some(Duration::from_secs(7))))
        );
        assert_eq!(r.read(b"x"), Err(ReadError::RateLimited(None)));
        assert!(matches!(r.read(b"x"), Err(ReadError::Retryable(m)) if m.starts_with("503")));
        assert!(
            matches!(r.read(b"x"), Err(ReadError::Fatal(m)) if m.contains("structured output"))
        );
        assert!(
            matches!(r.read(b"x"), Err(ReadError::Retryable(_))),
            "server gone: a connection error"
        );
    }

    #[test]
    fn key_sent_only_as_bearer() {
        let (base, log) = server(vec![models(), chat("{}")]);
        let r = OpenAiChat::new(&base, "gemma4:e4b", Some("sk-test")).unwrap();
        r.read(b"x").unwrap();
        for req in log.lock().unwrap().iter() {
            assert_eq!(req.header("authorization"), Some("Bearer sk-test"));
            assert!(!req.body.contains("sk-test"));
        }
    }

    #[test]
    fn key_policy() {
        let k = Some("k");
        for ok in [
            "http://127.0.0.1:11434/v1",
            "http://localhost:1/v1",
            "http://[::1]:11434/v1",
            "https://api.example.com/v1",
            "HTTP://LOCALHOST/v1",
            "http://127.8.9.1/v1",
        ] {
            assert!(check_key_policy(ok, k).is_ok(), "{ok}");
        }
        for bad in [
            "http://10.0.0.5:11434/v1",
            "http://localhost.evil.com/v1",
            "http://192.168.1.2/v1",
            "ftp://127.0.0.1/",
            "http://127.0.0.1.nip.io/v1",
        ] {
            let e = check_key_policy(bad, k).unwrap_err();
            assert!(
                matches!(&e, Error::Usage(m) if m.contains("DLHL_OPENAI_API_KEY")),
                "{bad}: {e}"
            );
        }
        assert!(check_key_policy("http://10.0.0.5:11434/v1", None).is_ok());
    }
}
