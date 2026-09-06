use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use regex::Regex;
use reqwest::header::{ACCEPT, CONTENT_TYPE, COOKIE, HeaderMap, HeaderValue, LOCATION, SET_COOKIE};
use reqwest::{Method, Response, StatusCode};
use serde_json::{Value, json};

use crate::auth::{Session, SessionStore, merge_supported_cookie};

pub struct OverleafApi {
    client: reqwest::Client,
    base_url: String,
    cookie: String,
    csrf: Option<String>,
    session_store: Option<SessionStore>,
}

impl OverleafApi {
    pub fn new(session: &Session, session_store: Option<SessionStore>) -> Result<Self> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            client,
            base_url: session.base_url.trim_end_matches('/').to_owned(),
            cookie: session.cookie.clone(),
            csrf: None,
            session_store,
        })
    }

    pub fn cookie(&self) -> &str {
        &self.cookie
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn csrf(&self) -> Option<&str> {
        self.csrf.as_deref()
    }

    pub(crate) fn adopt_cookie(&mut self, cookie: &str) -> Result<()> {
        if self.cookie != cookie {
            self.cookie = cookie.to_owned();
            if let Some(store) = &self.session_store {
                store.update_cookie(&self.cookie)?;
            }
        }
        Ok(())
    }

    fn url(&self, path: &str) -> String {
        if path.starts_with("http://") || path.starts_with("https://") {
            path.to_owned()
        } else {
            format!("{}{}", self.base_url, path)
        }
    }

    fn headers(&self, mutation: bool) -> Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        headers.insert(COOKIE, HeaderValue::from_str(&self.cookie)?);
        if mutation && let Some(csrf) = &self.csrf {
            headers.insert("x-csrf-token", HeaderValue::from_str(csrf)?);
        }
        Ok(headers)
    }

    async fn finish_response(&mut self, response: Response) -> Result<Response> {
        if response.status() == StatusCode::FOUND
            && response
                .headers()
                .get(LOCATION)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|location| location.contains("/login"))
        {
            bail!("session expired; run: jujuleaf login");
        }

        let mut updated = self.cookie.clone();
        for value in response.headers().get_all(SET_COOKIE) {
            let value = value.to_str().unwrap_or_default();
            updated = merge_supported_cookie(&updated, value);
        }
        self.adopt_cookie(&updated)?;
        Ok(response)
    }

    pub async fn request(
        &mut self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Response> {
        let mutation = method != Method::GET && method != Method::HEAD;
        if mutation && self.csrf.is_none() {
            self.fetch_csrf().await?;
        }
        let mut request = self
            .client
            .request(method, self.url(path))
            .headers(self.headers(mutation)?);
        if let Some(body) = body {
            request = request.header(CONTENT_TYPE, "application/json").json(&body);
        }
        let response = request.send().await?;
        self.finish_response(response).await
    }

    async fn json_response(response: Response, action: &str) -> Result<Value> {
        let status = response.status();
        if !status.is_success() {
            let detail = response.text().await.unwrap_or_default();
            bail!("{action} failed ({status}): {}", detail.trim());
        }
        let bytes = response.bytes().await?;
        if bytes.is_empty() {
            Ok(json!({"success": true}))
        } else {
            serde_json::from_slice(&bytes)
                .with_context(|| format!("{action} returned invalid JSON"))
        }
    }

    pub async fn get_json(&mut self, path: &str, action: &str) -> Result<Value> {
        let response = self.request(Method::GET, path, None).await?;
        Self::json_response(response, action).await
    }

    pub async fn post_json(&mut self, path: &str, body: Value, action: &str) -> Result<Value> {
        let response = self.request(Method::POST, path, Some(body)).await?;
        Self::json_response(response, action).await
    }

    pub async fn delete_json(&mut self, path: &str, action: &str) -> Result<Value> {
        let response = self.request(Method::DELETE, path, None).await?;
        Self::json_response(response, action).await
    }

    pub async fn fetch_csrf(&mut self) -> Result<String> {
        let response = self
            .client
            .get(self.url("/project"))
            .headers(self.headers(false)?)
            .send()
            .await?;
        let response = self.finish_response(response).await?;
        if !response.status().is_success() {
            bail!(
                "failed to load Overleaf project page ({})",
                response.status()
            );
        }
        let html = response.text().await?;
        let regex = Regex::new(r#"ol-csrfToken[^"]*"\s+content="([^"]*)""#)?;
        let csrf = regex
            .captures(&html)
            .and_then(|captures| captures.get(1))
            .map(|capture| capture.as_str().to_owned())
            .ok_or_else(|| anyhow!("could not find CSRF token; session may be expired"))?;
        self.csrf = Some(csrf.clone());
        Ok(csrf)
    }

    pub async fn current_user_id(&mut self, project_id: &str) -> Result<String> {
        self.fetch_csrf().await?;
        let response = self
            .request(Method::GET, &format!("/project/{project_id}"), None)
            .await?;
        let html = response.error_for_status()?.text().await?;
        let regex = Regex::new(r#"ol-user_id[^"]*"\s+content="([^"]*)""#)?;
        regex
            .captures(&html)
            .and_then(|captures| captures.get(1))
            .map(|capture| capture.as_str().to_owned())
            .ok_or_else(|| anyhow!("could not determine the current Overleaf user ID"))
    }

    pub async fn list_projects(&mut self) -> Result<Value> {
        self.get_json("/user/projects", "list projects").await
    }

    pub async fn create_project(&mut self, name: &str) -> Result<Value> {
        self.post_json(
            "/project/new",
            json!({"projectName": name, "template": "none"}),
            "create project",
        )
        .await
    }

    pub async fn rename_project(&mut self, project_id: &str, name: &str) -> Result<Value> {
        self.post_json(
            &format!("/project/{project_id}/rename"),
            json!({"newProjectName": name}),
            "rename project",
        )
        .await
    }

    pub async fn entities(&mut self, project_id: &str) -> Result<Value> {
        self.get_json(&format!("/project/{project_id}/entities"), "get entities")
            .await
    }

    pub async fn compile(&mut self, project_id: &str, draft: bool) -> Result<Value> {
        self.post_json(
            &format!("/project/{project_id}/compile"),
            json!({"check": "silent", "draft": draft}),
            "compile project",
        )
        .await
    }

    pub async fn download_pdf(&mut self, project_id: &str) -> Result<Vec<u8>> {
        let result = self.compile(project_id, false).await?;
        let status = result
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        if status != "success" {
            bail!("compilation failed: {status}");
        }
        let url = result
            .get("outputFiles")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .find(|file| file.get("path").and_then(Value::as_str) == Some("output.pdf"))
            .and_then(|file| file.get("url"))
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("no PDF in compile output"))?
            .to_owned();
        self.download_url(&url, "download PDF").await
    }

    pub async fn download_zip(&mut self, project_id: &str) -> Result<Vec<u8>> {
        self.download_url(
            &format!("/project/{project_id}/download/zip"),
            "download project zip",
        )
        .await
    }

    pub async fn download_url(&mut self, url: &str, action: &str) -> Result<Vec<u8>> {
        let response = self.request(Method::GET, url, None).await?;
        let status = response.status();
        if !status.is_success() {
            bail!("{action} failed ({status})");
        }
        Ok(response.bytes().await?.to_vec())
    }

    pub async fn threads(&mut self, project_id: &str) -> Result<Value> {
        self.get_json(&format!("/project/{project_id}/threads"), "get threads")
            .await
    }

    pub async fn updates(&mut self, project_id: &str, min_count: usize) -> Result<Value> {
        self.get_json(
            &format!("/project/{project_id}/updates?min_count={min_count}"),
            "get updates",
        )
        .await
    }

    pub async fn word_count(&mut self, project_id: &str) -> Result<Value> {
        self.get_json(
            &format!("/project/{project_id}/wordcount"),
            "get word count",
        )
        .await
    }

    pub async fn create_doc(
        &mut self,
        project_id: &str,
        name: &str,
        parent_folder_id: Option<&str>,
    ) -> Result<Value> {
        let mut body = json!({"name": name});
        if let Some(parent) = parent_folder_id {
            body["parent_folder_id"] = json!(parent);
        }
        self.post_json(
            &format!("/project/{project_id}/doc"),
            body,
            "create document",
        )
        .await
    }

    pub async fn delete_doc(&mut self, project_id: &str, doc_id: &str) -> Result<Value> {
        self.delete_json(
            &format!("/project/{project_id}/doc/{doc_id}"),
            "delete document",
        )
        .await
    }

    pub async fn create_folder(
        &mut self,
        project_id: &str,
        name: &str,
        parent_folder_id: Option<&str>,
    ) -> Result<Value> {
        let mut body = json!({"name": name});
        if let Some(parent) = parent_folder_id {
            body["parent_folder_id"] = json!(parent);
        }
        self.post_json(
            &format!("/project/{project_id}/folder"),
            body,
            "create folder",
        )
        .await
    }

    pub async fn delete_folder(&mut self, project_id: &str, folder_id: &str) -> Result<Value> {
        self.delete_json(
            &format!("/project/{project_id}/folder/{folder_id}"),
            "delete folder",
        )
        .await
    }

    pub async fn rename_entity(
        &mut self,
        project_id: &str,
        entity_type: &str,
        entity_id: &str,
        name: &str,
    ) -> Result<Value> {
        self.post_json(
            &format!("/project/{project_id}/{entity_type}/{entity_id}/rename"),
            json!({"name": name}),
            "rename entity",
        )
        .await
    }

    pub async fn move_entity(
        &mut self,
        project_id: &str,
        entity_type: &str,
        entity_id: &str,
        folder_id: &str,
    ) -> Result<Value> {
        self.post_json(
            &format!("/project/{project_id}/{entity_type}/{entity_id}/move"),
            json!({"folder_id": folder_id}),
            "move entity",
        )
        .await
    }

    pub async fn upload(
        &mut self,
        project_id: &str,
        folder_id: &str,
        local_path: &Path,
        remote_name: &str,
    ) -> Result<Value> {
        if self.csrf.is_none() {
            self.fetch_csrf().await?;
        }
        let bytes = tokio::fs::read(local_path)
            .await
            .with_context(|| format!("failed to read {}", local_path.display()))?;
        let file = reqwest::multipart::Part::bytes(bytes)
            .file_name(remote_name.to_owned())
            .mime_str("application/octet-stream")?;
        let form = reqwest::multipart::Form::new()
            .text("relativePath", "null")
            .text("relativePath", "null")
            .text("name", remote_name.to_owned())
            .text("type", "application/octet-stream")
            .part("qqfile", file);
        let response = self
            .client
            .post(self.url(&format!(
                "/project/{project_id}/upload?folder_id={folder_id}"
            )))
            .headers(self.headers(true)?)
            .multipart(form)
            .send()
            .await?;
        let response = self.finish_response(response).await?;
        Self::json_response(response, "upload file").await
    }

    pub async fn diff(
        &mut self,
        project_id: &str,
        pathname: &str,
        from: i64,
        to: i64,
    ) -> Result<Value> {
        self.get_json(
            &format!(
                "/project/{project_id}/diff?pathname={}&from={from}&to={to}",
                urlencoding::encode(pathname)
            ),
            "get diff",
        )
        .await
    }

    pub async fn send_message(
        &mut self,
        project_id: &str,
        thread_id: &str,
        content: &str,
    ) -> Result<Value> {
        self.post_json(
            &format!("/project/{project_id}/thread/{thread_id}/messages"),
            json!({"content": content}),
            "send thread message",
        )
        .await
    }

    pub async fn edit_message(
        &mut self,
        project_id: &str,
        thread_id: &str,
        message_id: &str,
        content: &str,
    ) -> Result<Value> {
        self.post_json(
            &format!("/project/{project_id}/thread/{thread_id}/messages/{message_id}/edit"),
            json!({"content": content}),
            "edit thread message",
        )
        .await
    }

    pub async fn delete_message(
        &mut self,
        project_id: &str,
        thread_id: &str,
        message_id: &str,
    ) -> Result<Value> {
        self.delete_json(
            &format!("/project/{project_id}/thread/{thread_id}/messages/{message_id}"),
            "delete thread message",
        )
        .await
    }

    pub async fn set_thread_resolved(
        &mut self,
        project_id: &str,
        doc_id: &str,
        thread_id: &str,
        resolved: bool,
    ) -> Result<Value> {
        let action = if resolved { "resolve" } else { "reopen" };
        self.post_json(
            &format!("/project/{project_id}/doc/{doc_id}/thread/{thread_id}/{action}"),
            json!({}),
            action,
        )
        .await
    }

    pub async fn delete_thread(
        &mut self,
        project_id: &str,
        doc_id: &str,
        thread_id: &str,
    ) -> Result<Value> {
        self.delete_json(
            &format!("/project/{project_id}/doc/{doc_id}/thread/{thread_id}"),
            "delete thread",
        )
        .await
    }

    pub async fn set_track_changes(
        &mut self,
        project_id: &str,
        user_id: &str,
        enabled: bool,
    ) -> Result<Value> {
        self.post_json(
            &format!("/project/{project_id}/track_changes"),
            json!({"on_for": {user_id: enabled}}),
            "set track changes",
        )
        .await
    }

    pub async fn accept_changes(
        &mut self,
        project_id: &str,
        doc_id: &str,
        change_ids: &[String],
    ) -> Result<Value> {
        self.post_json(
            &format!("/project/{project_id}/doc/{doc_id}/changes/accept"),
            json!({"change_ids": change_ids}),
            "accept changes",
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::LoginPreset;

    #[test]
    fn url_supports_overleaf_and_absolute_compile_outputs() {
        let session = Session::new("token", "https://example.test/");
        let api = OverleafApi::new(&session, None).unwrap();
        assert_eq!(api.url("/project"), "https://example.test/project");
        assert_eq!(
            api.url("https://cdn.example.test/output.pdf"),
            "https://cdn.example.test/output.pdf"
        );
    }

    #[test]
    fn adopted_socket_cookie_updates_memory_and_the_selected_profile() {
        let temp = tempfile::tempdir().unwrap();
        let store = SessionStore::new(temp.path().join("cstcloud.json"));
        let session = Session::new_with_preset(
            "overleaf.sid=old; latex-session=route-old",
            "https://latex.cstcloud.cn",
            LoginPreset::Cstcloud,
        );
        store.save(&session).unwrap();
        let mut api = OverleafApi::new(&session, Some(store.clone())).unwrap();

        api.adopt_cookie("overleaf.sid=new; latex-session=route-new")
            .unwrap();

        assert_eq!(api.cookie(), "overleaf.sid=new; latex-session=route-new");
        let persisted = store.require().unwrap();
        assert_eq!(persisted.cookie, api.cookie());
        assert_eq!(persisted.login_preset, LoginPreset::Cstcloud);
    }
}
