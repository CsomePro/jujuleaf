use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail, ensure};
use futures_util::{SinkExt, StreamExt};
use reqwest::header::{COOKIE, SET_COOKIE};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio::time::{Instant, timeout};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{Message, Utf8Bytes};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::auth::merge_cookie;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[derive(Debug, Clone, PartialEq)]
enum Packet {
    Connected,
    Heartbeat,
    Event { name: String, args: Vec<Value> },
    Ack { id: u64, args: Vec<Value> },
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JoinedDocument {
    pub snapshot: Value,
    pub version: i64,
    pub ops: Value,
    pub ranges: Value,
    pub ot_type: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateConfirmation {
    pub confirmed: bool,
    pub version: i64,
    pub attempts: usize,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub noop: bool,
}

#[derive(Debug, Clone)]
pub struct UpdateOptions {
    pub retry_after: Duration,
    pub timeout: Duration,
}

impl Default for UpdateOptions {
    fn default() -> Self {
        Self {
            retry_after: Duration::from_secs(5),
            timeout: Duration::from_secs(45),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RealtimeEvent {
    pub name: String,
    pub args: Vec<Value>,
}

pub struct OverleafSocket {
    socket: Socket,
    ack_id: u64,
    public_id: Option<String>,
    heartbeat_timeout: Duration,
    base_url: String,
    cookie: String,
    project_id: String,
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn parse_packet(raw: &str) -> Result<Packet> {
    let Some(kind) = raw.as_bytes().first().copied() else {
        return Ok(Packet::Other);
    };
    match kind {
        b'1' => Ok(Packet::Connected),
        b'2' => Ok(Packet::Heartbeat),
        b'5' => {
            let payload = raw
                .get(4..)
                .ok_or_else(|| anyhow!("malformed Socket.IO event frame"))?;
            let value: Value = serde_json::from_str(payload)?;
            let name = value
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("Socket.IO event has no name"))?
                .to_owned();
            let args = value
                .get("args")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            Ok(Packet::Event { name, args })
        }
        b'6' => {
            let rest = raw
                .strip_prefix("6:::")
                .ok_or_else(|| anyhow!("malformed Socket.IO ack frame"))?;
            let (id, payload) = rest.split_once('+').unwrap_or((rest, ""));
            let id = id.parse().context("invalid Socket.IO ack ID")?;
            let args = if payload.is_empty() {
                vec![]
            } else {
                serde_json::from_str(payload).context("invalid Socket.IO ack JSON")?
            };
            Ok(Packet::Ack { id, args })
        }
        _ => Ok(Packet::Other),
    }
}

fn websocket_base(base_url: &str) -> Result<String> {
    if let Some(rest) = base_url.strip_prefix("https://") {
        Ok(format!("wss://{}", rest.trim_end_matches('/')))
    } else if let Some(rest) = base_url.strip_prefix("http://") {
        Ok(format!("ws://{}", rest.trim_end_matches('/')))
    } else {
        bail!("unsupported Overleaf base URL: {base_url}")
    }
}

impl OverleafSocket {
    pub async fn connect(base_url: &str, cookie: &str, project_id: &str) -> Result<Self> {
        let base_url = base_url.trim_end_matches('/');
        let client = reqwest::Client::new();
        let response = client
            .get(format!(
                "{base_url}/socket.io/1/?projectId={}&t={}",
                urlencoding::encode(project_id),
                now_ms()
            ))
            .header(COOKIE, cookie)
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            bail!("Socket.IO handshake failed ({status})");
        }
        let mut ws_cookie = cookie.to_owned();
        for value in response.headers().get_all(SET_COOKIE) {
            let value = value.to_str().unwrap_or_default();
            if value.starts_with("GCLB=") {
                ws_cookie = merge_cookie(&ws_cookie, value);
            }
        }
        let body = response.text().await?;
        let mut fields = body.split(':');
        let session_id = fields
            .next()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow!("Socket.IO handshake returned no session ID"))?;
        let heartbeat_seconds = fields
            .next()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(25);
        let url = format!(
            "{}/socket.io/1/websocket/{session_id}",
            websocket_base(base_url)?
        );
        let mut request = url.into_client_request()?;
        request.headers_mut().insert(COOKIE, ws_cookie.parse()?);
        let (socket, _) = tokio_tungstenite::connect_async(request).await?;
        let mut result = Self {
            socket,
            ack_id: 0,
            public_id: None,
            heartbeat_timeout: Duration::from_secs(heartbeat_seconds.max(1)),
            base_url: base_url.to_owned(),
            cookie: cookie.to_owned(),
            project_id: project_id.to_owned(),
        };
        let connected = timeout(Duration::from_secs(10), async {
            loop {
                if matches!(result.read_packet().await?, Packet::Connected) {
                    return Ok::<_, anyhow::Error>(());
                }
            }
        })
        .await
        .context("Socket.IO connection timed out")?;
        connected?;
        Ok(result)
    }

    async fn read_packet(&mut self) -> Result<Packet> {
        loop {
            let message = self
                .socket
                .next()
                .await
                .ok_or_else(|| anyhow!("Socket.IO connection closed"))??;
            match message {
                Message::Text(text) => {
                    let packet = parse_packet(&text)?;
                    if packet == Packet::Heartbeat {
                        self.socket
                            .send(Message::Text(Utf8Bytes::from_static("2::")))
                            .await?;
                        continue;
                    }
                    return Ok(packet);
                }
                Message::Ping(data) => self.socket.send(Message::Pong(data)).await?,
                Message::Close(_) => bail!("Socket.IO connection closed"),
                _ => {}
            }
        }
    }

    async fn emit(&mut self, name: &str, args: Vec<Value>) -> Result<u64> {
        self.ack_id += 1;
        let id = self.ack_id;
        let frame = format!("5:{id}+::{}", json!({"name": name, "args": args}));
        self.socket.send(Message::Text(frame.into())).await?;
        Ok(id)
    }

    async fn wait_for_ack(&mut self, id: u64, wait: Duration) -> Result<Vec<Value>> {
        timeout(wait, async {
            loop {
                if let Packet::Ack { id: ack_id, args } = self.read_packet().await?
                    && ack_id == id
                {
                    return Ok::<_, anyhow::Error>(args);
                }
            }
        })
        .await
        .with_context(|| format!("timed out waiting for Socket.IO ack {id}"))?
    }

    pub async fn join_project(&mut self, project_id: &str) -> Result<Vec<Value>> {
        self.emit("joinProject", vec![json!({"project_id": project_id})])
            .await?;
        let args = timeout(Duration::from_secs(30), async {
            loop {
                if let Packet::Event { name, args } = self.read_packet().await?
                    && name == "joinProjectResponse"
                {
                    return Ok::<_, anyhow::Error>(args);
                }
            }
        })
        .await
        .context("timed out waiting for joinProjectResponse")??;
        self.public_id = args
            .first()
            .and_then(|value| value.get("publicId"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        Ok(args)
    }

    pub async fn join_doc(&mut self, doc_id: &str) -> Result<JoinedDocument> {
        self.join_doc_from_version(doc_id, None, None).await
    }

    /// Rejoin from a known server version. Overleaf returns only the missing
    /// operations when this succeeds; callers apply the returned ops to their
    /// existing snapshot. A missing-ops error requires a full join.
    pub async fn join_doc_from_version(
        &mut self,
        doc_id: &str,
        from_version: Option<i64>,
        age_ms: Option<u64>,
    ) -> Result<JoinedDocument> {
        if let Some(version) = from_version {
            ensure!(version >= 0, "fromVersion must be non-negative");
        }
        let options = json!({
            "encodeRanges": true,
            "supportsHistoryOT": true,
            "age": age_ms
        });
        let args = match from_version {
            Some(version) => vec![json!(doc_id), json!(version), options],
            None => vec![json!(doc_id), options],
        };
        let id = self.emit("joinDoc", args).await?;
        let args = self.wait_for_ack(id, Duration::from_secs(30)).await?;
        if args.first().is_some_and(|value| !value.is_null()) {
            bail!("joinDoc failed: {}", args[0]);
        }
        Ok(JoinedDocument {
            snapshot: args.get(1).cloned().unwrap_or(Value::Null),
            version: args
                .get(2)
                .and_then(Value::as_i64)
                .ok_or_else(|| anyhow!("joinDoc returned no document version"))?,
            ops: args.get(3).cloned().unwrap_or_else(|| json!([])),
            ranges: args.get(4).cloned().unwrap_or_else(|| json!({})),
            ot_type: args
                .get(5)
                .and_then(Value::as_str)
                .unwrap_or("sharejs-text-ot")
                .to_owned(),
        })
    }

    /// Establish a new transport and rejoin the project. Documents must be
    /// rejoined explicitly, optionally with join_doc_from_version.
    pub async fn reconnect(&mut self) -> Result<()> {
        let base_url = self.base_url.clone();
        let cookie = self.cookie.clone();
        let project_id = self.project_id.clone();
        let mut replacement = Self::connect(&base_url, &cookie, &project_id).await?;
        replacement.join_project(&project_id).await?;
        let _old = std::mem::replace(self, replacement);
        Ok(())
    }

    pub async fn leave_doc(&mut self, doc_id: &str) -> Result<()> {
        let id = self.emit("leaveDoc", vec![json!(doc_id)]).await?;
        let args = self.wait_for_ack(id, Duration::from_secs(10)).await?;
        if args.first().is_some_and(|value| !value.is_null()) {
            bail!("leaveDoc failed: {}", args[0]);
        }
        Ok(())
    }

    pub async fn apply_update(
        &mut self,
        doc_id: &str,
        ops: &[Value],
        version: i64,
        meta: Option<Value>,
        options: &UpdateOptions,
    ) -> Result<UpdateConfirmation> {
        ensure!(version >= 0, "version must be non-negative");
        ensure!(
            !options.retry_after.is_zero(),
            "retry interval must be positive"
        );
        ensure!(
            !options.timeout.is_zero(),
            "update timeout must be positive"
        );
        if ops.is_empty() {
            return Ok(UpdateConfirmation {
                confirmed: true,
                version,
                attempts: 0,
                noop: true,
            });
        }
        let deadline = Instant::now() + options.timeout;
        let mut attempts = 0;
        while Instant::now() < deadline {
            attempts += 1;
            let mut update = json!({"doc": doc_id, "op": ops, "v": version});
            if let Some(meta) = &meta {
                update["meta"] = meta.clone();
            }
            if attempts > 1 {
                let public_id = self
                    .public_id
                    .as_deref()
                    .ok_or_else(|| anyhow!("cannot safely retry without joinProject publicId"))?;
                update["dupIfSource"] = json!([public_id]);
            }
            let ack_id = self
                .emit("applyOtUpdate", vec![json!(doc_id), update])
                .await?;
            let attempt_deadline = (Instant::now() + options.retry_after).min(deadline);
            loop {
                let remaining = attempt_deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                let packet = match timeout(remaining, self.read_packet()).await {
                    Ok(packet) => packet?,
                    Err(_) => break,
                };
                match packet {
                    Packet::Ack { id, args } if id == ack_id => {
                        if args.first().is_some_and(|value| !value.is_null()) {
                            bail!("applyOtUpdate failed: {}", args[0]);
                        }
                    }
                    Packet::Event { name, args } if name == "otUpdateApplied" => {
                        let update = args.first().cloned().unwrap_or(Value::Null);
                        let is_sender_confirmation = update.get("doc").and_then(Value::as_str)
                            == Some(doc_id)
                            && update.get("op").is_none()
                            && update
                                .get("v")
                                .and_then(Value::as_i64)
                                .is_some_and(|v| v >= version);
                        if is_sender_confirmation {
                            return Ok(UpdateConfirmation {
                                confirmed: true,
                                version: update["v"].as_i64().unwrap_or(version),
                                attempts,
                                noop: false,
                            });
                        }
                    }
                    Packet::Event { name, args } if name == "otUpdateError" => {
                        let message = args.get(1).cloned().unwrap_or(Value::Null);
                        if message.get("doc_id").and_then(Value::as_str) == Some(doc_id) {
                            bail!(
                                "otUpdateError for document {doc_id}: {}",
                                args.first().cloned().unwrap_or(Value::Null)
                            );
                        }
                    }
                    _ => {}
                }
            }
        }
        bail!("timed out waiting for otUpdateApplied for document {doc_id}")
    }

    pub async fn apply_tracked_update(
        &mut self,
        doc_id: &str,
        ops: &[Value],
        version: i64,
        options: &UpdateOptions,
    ) -> Result<UpdateConfirmation> {
        self.apply_update(
            doc_id,
            ops,
            version,
            Some(json!({"tc": generate_tc_seed()})),
            options,
        )
        .await
    }

    pub async fn next_event(&mut self) -> Result<RealtimeEvent> {
        loop {
            if let Packet::Event { name, args } = self.read_packet().await? {
                return Ok(RealtimeEvent { name, args });
            }
        }
    }

    pub async fn close(mut self) -> Result<()> {
        self.socket.close(None).await?;
        Ok(())
    }

    pub fn heartbeat_timeout(&self) -> Duration {
        self.heartbeat_timeout
    }

    pub fn public_id(&self) -> Option<&str> {
        self.public_id.as_deref()
    }
}

fn generate_tc_seed() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let random: [u8; 5] = rand::random();
    format!("{seconds:08x}{}", hex::encode(random))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_socket_io_09_wire_frames() {
        assert_eq!(parse_packet("1::").unwrap(), Packet::Connected);
        assert_eq!(parse_packet("2::").unwrap(), Packet::Heartbeat);
        assert_eq!(
            parse_packet(r#"5:::{"name":"otUpdateApplied","args":[{"doc":"d","v":2}]}"#).unwrap(),
            Packet::Event {
                name: "otUpdateApplied".into(),
                args: vec![json!({"doc":"d","v":2})]
            }
        );
        assert_eq!(
            parse_packet(r#"6:::7+[null,["line"],3]"#).unwrap(),
            Packet::Ack {
                id: 7,
                args: vec![Value::Null, json!(["line"]), json!(3)]
            }
        );
    }

    #[test]
    fn converts_self_hosted_http_to_websocket_scheme() {
        assert_eq!(
            websocket_base("https://www.overleaf.com").unwrap(),
            "wss://www.overleaf.com"
        );
        assert_eq!(
            websocket_base("http://localhost:3000/").unwrap(),
            "ws://localhost:3000"
        );
    }

    #[test]
    fn tracking_seed_matches_overleaf_shape() {
        let seed = generate_tc_seed();
        assert_eq!(seed.len(), 18);
        assert!(seed.chars().all(|ch| ch.is_ascii_hexdigit()));
    }
}
