use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail, ensure};
use clap::ValueEnum;
use directories::ProjectDirs;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::process::{Child, Command};
use tokio::time::{Instant, sleep};
use tokio_tungstenite::tungstenite::Message;

use crate::DEFAULT_BASE_URL;

pub const DEFAULT_PROFILE: &str = "default";
pub const CSTCLOUD_BASE_URL: &str = "https://latex.cstcloud.cn";

const AUTO_AUTH_COOKIES: &[&str] = &["overleaf_session2", "overleaf.sid", "sharelatex.sid"];
const AUTO_ROUTE_COOKIES: &[&str] = &["GCLB", "latex-session"];
const OVERLEAF_AUTH_COOKIES: &[&str] = &["overleaf_session2"];
const OVERLEAF_ROUTE_COOKIES: &[&str] = &["GCLB"];
const CSTCLOUD_AUTH_COOKIES: &[&str] = &["overleaf.sid"];
const CSTCLOUD_ROUTE_COOKIES: &[&str] = &["latex-session"];

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum LoginPreset {
    /// Detect official, Community Edition, and legacy ShareLaTeX cookies.
    #[default]
    Auto,
    /// Official overleaf.com using overleaf_session2 and GCLB.
    Overleaf,
    /// CSTCloud SSO using overleaf.sid and latex-session.
    Cstcloud,
}

impl LoginPreset {
    pub fn default_base_url(self) -> &'static str {
        match self {
            Self::Auto | Self::Overleaf => DEFAULT_BASE_URL,
            Self::Cstcloud => CSTCLOUD_BASE_URL,
        }
    }

    pub fn login_path(self) -> &'static str {
        match self {
            Self::Cstcloud => "/oidc/login",
            Self::Auto | Self::Overleaf => "/login",
        }
    }

    pub fn success_path(self) -> &'static str {
        "/project"
    }

    pub fn auth_cookie_names(self) -> &'static [&'static str] {
        match self {
            Self::Auto => AUTO_AUTH_COOKIES,
            Self::Overleaf => OVERLEAF_AUTH_COOKIES,
            Self::Cstcloud => CSTCLOUD_AUTH_COOKIES,
        }
    }

    pub fn route_cookie_names(self) -> &'static [&'static str] {
        match self {
            Self::Auto => AUTO_ROUTE_COOKIES,
            Self::Overleaf => OVERLEAF_ROUTE_COOKIES,
            Self::Cstcloud => CSTCLOUD_ROUTE_COOKIES,
        }
    }

    pub fn retains_cookie(self, name: &str) -> bool {
        self.auth_cookie_names().contains(&name) || self.route_cookie_names().contains(&name)
    }

    fn default_cookie_name(self, base_url: &str) -> &'static str {
        match self {
            Self::Overleaf => "overleaf_session2",
            Self::Cstcloud => "overleaf.sid",
            Self::Auto if base_url.trim_end_matches('/') == DEFAULT_BASE_URL => "overleaf_session2",
            Self::Auto => "overleaf.sid",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub cookie: String,
    #[serde(default = "default_base_url")]
    pub base_url: String,
    #[serde(default)]
    pub login_preset: LoginPreset,
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
        Self::new_with_preset(cookie, base_url, LoginPreset::Auto)
    }

    pub fn new_with_preset(
        cookie: impl Into<String>,
        base_url: impl Into<String>,
        login_preset: LoginPreset,
    ) -> Self {
        let base_url = base_url.into().trim_end_matches('/').to_owned();
        let mut cookie = cookie.into();
        if !cookie.contains('=') {
            cookie = format!("{}={cookie}", login_preset.default_cookie_name(&base_url));
        }
        let timestamp = now_ms();
        Self {
            cookie,
            base_url,
            login_preset,
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
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> Result<Option<Session>> {
        if !self.path.exists() {
            return Ok(None);
        }
        let bytes = std::fs::read(&self.path)
            .with_context(|| format!("failed to read {}", self.path.display()))?;
        let mut session: Session = serde_json::from_slice(&bytes)
            .with_context(|| format!("invalid session file {}", self.path.display()))?;
        session.base_url = session.base_url.trim_end_matches('/').to_owned();
        Ok(Some(session))
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

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileSummary {
    pub name: String,
    pub base_url: String,
    pub login_preset: LoginPreset,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub active: bool,
}

#[derive(Debug, Clone)]
pub struct ProfileStore {
    root: PathBuf,
}

impl ProfileStore {
    pub fn default_root() -> Result<PathBuf> {
        let dirs = ProjectDirs::from("", "", "jujuleaf")
            .ok_or_else(|| anyhow!("could not determine the user configuration directory"))?;
        Ok(dirs.config_dir().to_owned())
    }

    pub fn from_default_path() -> Result<Self> {
        Self::new(Self::default_root()?)
    }

    pub fn new(root: impl Into<PathBuf>) -> Result<Self> {
        let store = Self { root: root.into() };
        store.ensure_migrated()?;
        Ok(store)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn profiles_dir(&self) -> PathBuf {
        self.root.join("profiles")
    }

    fn active_path(&self) -> PathBuf {
        self.root.join("active-profile")
    }

    fn migration_marker(&self) -> PathBuf {
        self.root.join(".profiles-v1-migrated")
    }

    fn create_private_dir(path: &Path) -> Result<()> {
        std::fs::create_dir_all(path)
            .with_context(|| format!("failed to create {}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }

    fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
        let parent = path
            .parent()
            .ok_or_else(|| anyhow!("configuration path has no parent"))?;
        Self::create_private_dir(parent)?;
        std::fs::write(path, bytes)
            .with_context(|| format!("failed to write {}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }

    fn ensure_migrated(&self) -> Result<()> {
        Self::create_private_dir(&self.root)?;
        Self::create_private_dir(&self.profiles_dir())?;
        let marker = self.migration_marker();
        if marker.exists() {
            return Ok(());
        }

        let default_store = self.session_store_unchecked(DEFAULT_PROFILE);
        if !default_store.path().exists() {
            let mut candidates = vec![self.root.join("session.json")];
            if let Some(config_root) = self.root.parent() {
                candidates.push(config_root.join("overleaf-cli").join("session.json"));
            }
            for candidate in candidates {
                if let Some(session) = SessionStore::new(candidate).load()? {
                    default_store.save(&session)?;
                    break;
                }
            }
        }
        Self::write_private(&marker, b"1\n")
    }

    fn session_store_unchecked(&self, name: &str) -> SessionStore {
        SessionStore::new(self.profiles_dir().join(format!("{name}.json")))
    }

    pub fn session_store(&self, name: &str) -> Result<SessionStore> {
        validate_profile_name(name)?;
        Ok(self.session_store_unchecked(name))
    }

    pub fn active_name(&self) -> Result<String> {
        let path = self.active_path();
        if !path.exists() {
            return Ok(DEFAULT_PROFILE.to_owned());
        }
        let name = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let name = name.trim();
        validate_profile_name(name)?;
        Ok(name.to_owned())
    }

    pub fn resolve(&self, explicit: Option<&str>) -> Result<String> {
        match explicit {
            Some(name) => {
                validate_profile_name(name)?;
                Ok(name.to_owned())
            }
            None => self.active_name(),
        }
    }

    pub fn set_active(&self, name: &str) -> Result<()> {
        let session = self.session_store(name)?.load()?;
        ensure!(
            session.is_some_and(|session| !session.cookie.is_empty()),
            "profile '{name}' does not exist; create it with: jujuleaf login --profile {name}"
        );
        Self::write_private(&self.active_path(), format!("{name}\n").as_bytes())
    }

    pub fn list(&self) -> Result<Vec<ProfileSummary>> {
        let active = self.active_name()?;
        let mut profiles = BTreeMap::new();
        for entry in std::fs::read_dir(self.profiles_dir())? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let Some(name) = path.file_stem().and_then(|value| value.to_str()) else {
                continue;
            };
            if validate_profile_name(name).is_err() {
                continue;
            }
            let Some(session) = SessionStore::new(&path).load()? else {
                continue;
            };
            profiles.insert(
                name.to_owned(),
                ProfileSummary {
                    name: name.to_owned(),
                    base_url: session.base_url,
                    login_preset: session.login_preset,
                    created_at_ms: session.created_at_ms,
                    updated_at_ms: session.updated_at_ms,
                    active: name == active,
                },
            );
        }
        Ok(profiles.into_values().collect())
    }

    pub fn get(&self, name: &str) -> Result<ProfileSummary> {
        validate_profile_name(name)?;
        self.list()?
            .into_iter()
            .find(|profile| profile.name == name)
            .ok_or_else(|| anyhow!("profile '{name}' does not exist"))
    }

    pub fn delete(&self, name: &str) -> Result<()> {
        let store = self.session_store(name)?;
        ensure!(store.path().exists(), "profile '{name}' does not exist");
        std::fs::remove_file(store.path())
            .with_context(|| format!("failed to delete profile '{name}'"))?;
        if self.active_name()? == name {
            let remaining: Vec<_> = self
                .list()?
                .into_iter()
                .map(|profile| profile.name)
                .collect();
            let replacement = remaining
                .iter()
                .find(|candidate| candidate.as_str() == DEFAULT_PROFILE)
                .cloned()
                .or_else(|| remaining.into_iter().next());
            match replacement {
                Some(replacement) => {
                    Self::write_private(
                        &self.active_path(),
                        format!("{replacement}\n").as_bytes(),
                    )?;
                }
                None if self.active_path().exists() => {
                    std::fs::remove_file(self.active_path())?;
                }
                None => {}
            }
        }
        Ok(())
    }
}

pub fn validate_profile_name(name: &str) -> Result<()> {
    ensure!(!name.is_empty(), "profile name cannot be empty");
    ensure!(name.len() <= 64, "profile name cannot exceed 64 characters");
    ensure!(
        name.as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')),
        "profile name must start with an ASCII letter or digit and contain only letters, digits, '-' or '_'"
    );
    Ok(())
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

fn cookie_domain_matches(base_host: &str, cookie_domain: &str) -> bool {
    let cookie_domain = cookie_domain.trim_start_matches('.');
    base_host == cookie_domain
        || base_host
            .strip_suffix(cookie_domain)
            .is_some_and(|prefix| prefix.ends_with('.'))
}

fn cookie_header(cookies: &Value, base_url: &str, preset: LoginPreset) -> Option<String> {
    let base_host = reqwest::Url::parse(base_url).ok()?.host_str()?.to_owned();
    let cookies = cookies.get("cookies")?.as_array()?;
    let mut wanted = BTreeMap::new();
    for cookie in cookies {
        let Some(name) = cookie.get("name").and_then(Value::as_str) else {
            continue;
        };
        let Some(value) = cookie.get("value").and_then(Value::as_str) else {
            continue;
        };
        let Some(domain) = cookie.get("domain").and_then(Value::as_str) else {
            continue;
        };
        if preset.retains_cookie(name) && cookie_domain_matches(&base_host, domain) {
            wanted.insert(name.to_owned(), value.to_owned());
        }
    }
    preset
        .auth_cookie_names()
        .iter()
        .any(|name| wanted.contains_key(*name))
        .then(|| {
            wanted
                .into_iter()
                .map(|(name, value)| format!("{name}={value}"))
                .collect::<Vec<_>>()
                .join("; ")
        })
}

fn is_authenticated_page(state: &Value, base_url: &str, success_path: &str) -> bool {
    let Some(value) = state.pointer("/result/value") else {
        return false;
    };
    let Some(href) = value.get("href").and_then(Value::as_str) else {
        return false;
    };
    let (Ok(current), Ok(base)) = (reqwest::Url::parse(href), reqwest::Url::parse(base_url)) else {
        return false;
    };
    let current_path = current.path().trim_end_matches('/');
    let success_path = success_path.trim_end_matches('/');
    let is_success_path = current_path == success_path
        || current_path
            .strip_prefix(success_path)
            .is_some_and(|suffix| suffix.starts_with('/'));
    current.scheme() == base.scheme()
        && current.host_str() == base.host_str()
        && current.port_or_known_default() == base.port_or_known_default()
        && is_success_path
        && value.get("hasUserId").and_then(Value::as_bool) == Some(true)
        && value.get("hasCsrfToken").and_then(Value::as_bool) == Some(true)
}

pub async fn interactive_login(base_url: &str, preset: LoginPreset) -> Result<String> {
    let chrome = find_chrome().ok_or_else(|| {
        anyhow!("Chrome/Chromium not found; use: jujuleaf login --cookie \"...\"")
    })?;
    let profile = tempfile::tempdir().context("failed to create temporary Chrome profile")?;
    let login_url = format!("{}{}", base_url.trim_end_matches('/'), preset.login_path());
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
            let state = cdp_call(
                &mut websocket,
                call_id,
                "Runtime.evaluate",
                json!({
                    "expression": "({href: window.location.href, hasUserId: Boolean(document.querySelector('meta[name=\"ol-user_id\"]')?.content), hasCsrfToken: Boolean(document.querySelector('meta[name=\"ol-csrfToken\"]')?.content)})",
                    "returnByValue": true
                }),
            )
            .await?;
            call_id += 1;
            if is_authenticated_page(&state, base_url, preset.success_path()) {
                let cookies =
                    cdp_call(&mut websocket, call_id, "Network.getAllCookies", json!({})).await?;
                call_id += 1;
                if let Some(cookie) = cookie_header(&cookies, base_url, preset) {
                    return Ok(cookie);
                }
            }
            sleep(Duration::from_secs(1)).await;
        }
        bail!(
            "login timed out after 5 minutes; expected an authenticated {} page and one of: {}",
            preset.success_path(),
            preset.auth_cookie_names().join(", ")
        )
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

pub fn merge_supported_cookie(current: &str, set_cookie: &str) -> String {
    let Some(name) = set_cookie
        .split(';')
        .next()
        .and_then(|pair| pair.trim().split_once('='))
        .map(|(name, _)| name)
    else {
        return current.to_owned();
    };
    if AUTO_AUTH_COOKIES.contains(&name) || AUTO_ROUTE_COOKIES.contains(&name) {
        merge_cookie(current, set_cookie)
    } else {
        current.to_owned()
    }
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

        let merged = merge_supported_cookie(
            "overleaf.sid=old; latex-session=route",
            "overleaf.sid=new; Path=/; HttpOnly",
        );
        assert_eq!(merged, "overleaf.sid=new; latex-session=route");
        let merged = merge_supported_cookie(&merged, "latex-session=route2; Path=/");
        assert_eq!(merged, "latex-session=route2; overleaf.sid=new");
        assert_eq!(
            merge_supported_cookie(&merged, "analytics=ignored; Path=/"),
            merged
        );
    }

    #[test]
    fn cstcloud_preset_uses_the_current_endpoint_and_session_cookie() {
        assert_eq!(LoginPreset::Cstcloud.default_base_url(), CSTCLOUD_BASE_URL);
        assert_eq!(LoginPreset::Cstcloud.login_path(), "/oidc/login");
        let session =
            Session::new_with_preset("signed-session", CSTCLOUD_BASE_URL, LoginPreset::Cstcloud);
        assert_eq!(session.cookie, "overleaf.sid=signed-session");
        assert_eq!(session.login_preset, LoginPreset::Cstcloud);
    }

    #[test]
    fn browser_capture_requires_authenticated_same_origin_page() {
        let cookies = json!({
            "cookies": [
                {"name": "overleaf.sid", "value": "idp", "domain": "aai.cstcloud.net"},
                {"name": "overleaf.sid", "value": "auth", "domain": "latex.cstcloud.cn"},
                {"name": "latex-session", "value": "route", "domain": ".cstcloud.cn"},
                {"name": "unrelated", "value": "private", "domain": "latex.cstcloud.cn"}
            ]
        });
        let header = cookie_header(&cookies, CSTCLOUD_BASE_URL, LoginPreset::Cstcloud).unwrap();
        assert!(header.contains("overleaf.sid=auth"));
        assert!(header.contains("latex-session=route"));
        assert!(!header.contains("idp"));
        assert!(!header.contains("private"));

        let anonymous = json!({
            "result": {"value": {
                "href": "https://latex.cstcloud.cn/login",
                "hasUserId": false,
                "hasCsrfToken": true
            }}
        });
        assert!(!is_authenticated_page(
            &anonymous,
            CSTCLOUD_BASE_URL,
            "/project"
        ));

        let authenticated = json!({
            "result": {"value": {
                "href": "https://latex.cstcloud.cn/project",
                "hasUserId": true,
                "hasCsrfToken": true
            }}
        });
        assert!(is_authenticated_page(
            &authenticated,
            CSTCLOUD_BASE_URL,
            "/project"
        ));
        assert!(!is_authenticated_page(
            &authenticated,
            "https://another.example",
            "/project"
        ));
        let misleading_path = json!({
            "result": {"value": {
                "href": "https://latex.cstcloud.cn/project-impersonation",
                "hasUserId": true,
                "hasCsrfToken": true
            }}
        });
        assert!(!is_authenticated_page(
            &misleading_path,
            CSTCLOUD_BASE_URL,
            "/project"
        ));
    }

    #[test]
    fn profiles_migrate_the_single_session_and_switch_safely() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("jujuleaf");
        let old_store = SessionStore::new(root.join("session.json"));
        old_store
            .save(&Session::new("old-secret", "https://official.test/"))
            .unwrap();

        let profiles = ProfileStore::new(&root).unwrap();
        let default = profiles.get(DEFAULT_PROFILE).unwrap();
        assert_eq!(default.base_url, "https://official.test");
        assert!(default.active);

        profiles
            .session_store("company")
            .unwrap()
            .save(&Session::new(
                "company-secret",
                "https://latex.company.test",
            ))
            .unwrap();
        profiles.set_active("company").unwrap();
        assert_eq!(profiles.active_name().unwrap(), "company");
        assert!(profiles.get("company").unwrap().active);

        profiles.delete("company").unwrap();
        assert_eq!(profiles.active_name().unwrap(), DEFAULT_PROFILE);
        assert!(profiles.get(DEFAULT_PROFILE).unwrap().active);
    }

    #[test]
    fn profile_names_cannot_escape_the_profile_directory() {
        for invalid in ["", ".", "../secret", "with/slash", "two words"] {
            assert!(
                validate_profile_name(invalid).is_err(),
                "accepted {invalid:?}"
            );
        }
        for valid in ["default", "company-prod", "lab_2", "2026"] {
            validate_profile_name(valid).unwrap();
        }
    }
}
