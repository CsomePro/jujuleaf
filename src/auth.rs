use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use directories::ProjectDirs;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::process::{Child, Command};
use tokio::time::{Instant, sleep};
use tokio_tungstenite::tungstenite::Message;

use crate::DEFAULT_BASE_URL;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub cookie: String,
    #[serde(default = "default_base_url")]
    pub base_url: String,
    #[serde(default)]
    pub created_at_ms: u64,
    #[serde(default)]
    pub updated_at_ms: u64,
}

fn default_base_url() -> String {
    DEFAULT_BASE_URL.to_owned()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

impl Session {
    pub fn new(cookie: impl Into<String>, base_url: impl Into<String>) -> Self {
        let mut cookie = cookie.into();
        if !cookie.contains('=') {
            cookie = format!("overleaf_session2={cookie}");
        }
        let timestamp = now_ms();
        Self {
            cookie,
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            created_at_ms: timestamp,
            updated_at_ms: timestamp,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SessionStore {
    path: PathBuf,
}

impl SessionStore {
    pub fn default_path() -> Result<PathBuf> {
        let dirs = ProjectDirs::from("", "", "jujuleaf")
            .ok_or_else(|| anyhow!("could not determine the user configuration directory"))?;
        Ok(dirs.config_dir().join("session.json"))
    }

    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn from_default_path() -> Result<Self> {
        Ok(Self::new(Self::default_path()?))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> Result<Option<Session>> {
        if !self.path.exists() {
            return self.load_legacy_session();
        }
        let bytes = std::fs::read(&self.path)
            .with_context(|| format!("failed to read {}", self.path.display()))?;
        let session = serde_json::from_slice(&bytes)
            .with_context(|| format!("invalid session file {}", self.path.display()))?;
        Ok(Some(session))
    }

    fn load_legacy_session(&self) -> Result<Option<Session>> {
        let Some(config_root) = self.path.parent().and_then(Path::parent) else {
            return Ok(None);
        };
        let legacy_path = config_root.join("overleaf-cli").join("session.json");
        if !legacy_path.exists() {
            return Ok(None);
        }
        let value: Value = serde_json::from_slice(&std::fs::read(&legacy_path)?)
            .context("invalid legacy overleaf-cli session")?;
        let Some(cookie) = value.get("cookie").and_then(Value::as_str) else {
            return Ok(None);
        };
        Ok(Some(Session::new(cookie, DEFAULT_BASE_URL)))
    }

    pub fn require(&self) -> Result<Session> {
        self.load()?
            .filter(|session| !session.cookie.is_empty())
            .ok_or_else(|| anyhow!("not authenticated; run: jujuleaf login"))
    }

    pub fn save(&self, session: &Session) -> Result<()> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| anyhow!("session path has no parent"))?;
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
        }
        let bytes = serde_json::to_vec_pretty(session)?;
        std::fs::write(&self.path, bytes)
            .with_context(|| format!("failed to write {}", self.path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }

    pub fn update_cookie(&self, cookie: &str) -> Result<()> {
        let mut session = self.require()?;
        if session.cookie != cookie {
            session.cookie = cookie.to_owned();
            session.updated_at_ms = now_ms();
            self.save(&session)?;
        }
        Ok(())
    }
}

pub fn find_chrome() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    #[cfg(target_os = "linux")]
    candidates.extend([
        "/usr/bin/google-chrome",
        "/usr/bin/google-chrome-stable",
        "/usr/bin/chromium-browser",
        "/usr/bin/chromium",
        "/snap/bin/chromium",
    ]);
    #[cfg(target_os = "macos")]
    candidates.extend([
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Google Chrome Canary.app/Contents/MacOS/Google Chrome Canary",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
    ]);
    #[cfg(target_os = "windows")]
    candidates.extend([
        r"C:\Program Files\Google\Chrome\Application\chrome.exe",
        r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
    ]);
    candidates
        .into_iter()
        .map(PathBuf::from)
        .find(|path| path.exists())
}

async fn wait_for_devtools_port(profile: &Path, child: &mut Child) -> Result<u16> {
    let path = profile.join("DevToolsActivePort");
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait()? {
            bail!("Chrome exited before login started ({status})");
        }
        if let Ok(text) = tokio::fs::read_to_string(&path).await
            && let Some(port) = text.lines().next()
            && let Ok(port) = port.parse()
        {
            return Ok(port);
        }
        sleep(Duration::from_millis(100)).await;
    }
    bail!("timed out waiting for Chrome DevTools")
}

async fn page_websocket(client: &reqwest::Client, port: u16) -> Result<String> {
    let targets: Vec<Value> = client
        .get(format!("http://127.0.0.1:{port}/json/list"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    targets
        .iter()
        .find(|target| target.get("type").and_then(Value::as_str) == Some("page"))
        .and_then(|target| target.get("webSocketDebuggerUrl"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| anyhow!("Chrome did not expose a login page"))
}

async fn cdp_call<S>(
    websocket: &mut tokio_tungstenite::WebSocketStream<S>,
    id: u64,
    method: &str,
    params: Value,
) -> Result<Value>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    websocket
        .send(Message::Text(
            json!({"id": id, "method": method, "params": params})
                .to_string()
                .into(),
        ))
        .await?;
    while let Some(message) = websocket.next().await {
        let message = message?;
        let Message::Text(text) = message else {
            continue;
        };
        let value: Value = serde_json::from_str(&text)?;
        if value.get("id").and_then(Value::as_u64) == Some(id) {
            if let Some(error) = value.get("error") {
                bail!("Chrome DevTools {method} failed: {error}");
            }
            return Ok(value.get("result").cloned().unwrap_or(Value::Null));
        }
    }
    bail!("Chrome DevTools connection closed during {method}")
}

fn cookie_header(cookies: &Value) -> Option<String> {
    let cookies = cookies.get("cookies")?.as_array()?;
    let wanted: Vec<_> = cookies
        .iter()
        .filter_map(|cookie| {
            let name = cookie.get("name")?.as_str()?;
            let value = cookie.get("value")?.as_str()?;
            ["overleaf_session2", "GCLB"]
                .contains(&name)
                .then(|| format!("{name}={value}"))
        })
        .collect();
    wanted
        .iter()
        .any(|pair| pair.starts_with("overleaf_session2="))
        .then(|| wanted.join("; "))
}

pub async fn interactive_login(base_url: &str) -> Result<String> {
    let chrome = find_chrome().ok_or_else(|| {
        anyhow!("Chrome/Chromium not found; use: jujuleaf login --cookie \"...\"")
    })?;
    let profile = tempfile::tempdir().context("failed to create temporary Chrome profile")?;
    let login_url = format!("{}/login", base_url.trim_end_matches('/'));
    let mut child = Command::new(chrome)
        .arg("--remote-debugging-port=0")
        .arg(format!("--user-data-dir={}", profile.path().display()))
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg(login_url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to start Chrome")?;

    let result = async {
        let port = wait_for_devtools_port(profile.path(), &mut child).await?;
        let client = reqwest::Client::new();
        let websocket_url = page_websocket(&client, port).await?;
        let (mut websocket, _) = tokio_tungstenite::connect_async(websocket_url).await?;
        cdp_call(&mut websocket, 1, "Network.enable", json!({})).await?;
        let deadline = Instant::now() + Duration::from_secs(5 * 60);
        let mut call_id = 2;
        while Instant::now() < deadline {
            let cookies =
                cdp_call(&mut websocket, call_id, "Network.getAllCookies", json!({})).await?;
            call_id += 1;
            if let Some(cookie) = cookie_header(&cookies) {
                cdp_call(
                    &mut websocket,
                    call_id,
                    "Page.navigate",
                    json!({"url": format!("{}/project", base_url.trim_end_matches('/'))}),
                )
                .await?;
                call_id += 1;
                sleep(Duration::from_secs(2)).await;
                let cookies =
                    cdp_call(&mut websocket, call_id, "Network.getAllCookies", json!({})).await?;
                return cookie_header(&cookies)
                    .or(Some(cookie))
                    .ok_or_else(|| anyhow!("login cookie disappeared"));
            }
            sleep(Duration::from_secs(1)).await;
        }
        bail!("login timed out after 5 minutes")
    }
    .await;

    child.kill().await.ok();
    child.wait().await.ok();
    result
}

pub fn merge_cookie(current: &str, set_cookie: &str) -> String {
    let Some((name, value)) = set_cookie
        .split(';')
        .next()
        .and_then(|pair| pair.trim().split_once('='))
    else {
        return current.to_owned();
    };
    if value.is_empty() {
        return current.to_owned();
    }
    let mut pairs: Vec<String> = current
        .split(';')
        .map(str::trim)
        .filter(|pair| !pair.is_empty())
        .filter(|pair| {
            pair.split_once('=')
                .is_none_or(|(existing, _)| existing != name)
        })
        .map(ToOwned::to_owned)
        .collect();
    pairs.insert(0, format!("{name}={value}"));
    pairs.join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_file_is_private_and_legacy_cookie_can_be_prefixed() {
        let temp = tempfile::tempdir().unwrap();
        let store = SessionStore::new(temp.path().join("session.json"));
        let session = Session::new("secret", DEFAULT_BASE_URL);
        store.save(&session).unwrap();
        assert_eq!(
            store.load().unwrap().unwrap().cookie,
            "overleaf_session2=secret"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(store.path())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn cookie_updates_preserve_other_infrastructure_cookies() {
        let merged = merge_cookie(
            "overleaf_session2=old; GCLB=route",
            "overleaf_session2=new; Path=/; HttpOnly",
        );
        assert_eq!(merged, "overleaf_session2=new; GCLB=route");
        let merged = merge_cookie(&merged, "GCLB=route2; Path=/");
        assert_eq!(merged, "GCLB=route2; overleaf_session2=new");
    }
}
