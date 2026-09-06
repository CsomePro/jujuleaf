use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::api::OverleafApi;
use crate::auth::Session;
use crate::socket::OverleafSocket;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocumentRef {
    pub id: String,
    pub path: String,
}

fn normalized_path(path: &str) -> String {
    format!("/{}", path.trim_start_matches('/'))
}

fn collect_folder(folder: &Value, prefix: &str, documents: &mut Vec<DocumentRef>) {
    for doc in folder
        .get("docs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let (Some(id), Some(name)) = (
            doc.get("_id").and_then(Value::as_str),
            doc.get("name").and_then(Value::as_str),
        ) {
            documents.push(DocumentRef {
                id: id.to_owned(),
                path: format!("{prefix}/{name}"),
            });
        }
    }
    for folder in folder
        .get("folders")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(name) = folder.get("name").and_then(Value::as_str) else {
            continue;
        };
        collect_folder(folder, &format!("{prefix}/{name}"), documents);
    }
}

pub fn collect_documents(project: &Value) -> Vec<DocumentRef> {
    let mut documents = Vec::new();
    if let Some(root) = project
        .get("rootFolder")
        .and_then(Value::as_array)
        .and_then(|folders| folders.first())
    {
        collect_folder(root, "", &mut documents);
    }
    documents
}

pub fn find_document(project: &Value, path: &str) -> Option<DocumentRef> {
    let path = normalized_path(path);
    collect_documents(project)
        .into_iter()
        .find(|document| document.path == path)
}

pub fn root_folder_id(project: &Value) -> Option<String> {
    project
        .get("rootFolder")
        .and_then(Value::as_array)
        .and_then(|folders| folders.first())
        .and_then(|root| root.get("_id"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

pub async fn connect_project(
    session: &Session,
    project_id: &str,
) -> Result<(OverleafSocket, Value)> {
    let socket = OverleafSocket::connect(&session.base_url, &session.cookie, project_id).await?;
    join_project_tree(socket, project_id).await
}

pub async fn connect_project_with_api(
    api: &mut OverleafApi,
    project_id: &str,
) -> Result<(OverleafSocket, Value)> {
    let socket = OverleafSocket::connect(api.base_url(), api.cookie(), project_id).await?;
    api.adopt_cookie(socket.cookie())?;
    join_project_tree(socket, project_id).await
}

async fn join_project_tree(
    mut socket: OverleafSocket,
    project_id: &str,
) -> Result<(OverleafSocket, Value)> {
    let response = socket.join_project(project_id).await?;
    let info = response
        .first()
        .cloned()
        .ok_or_else(|| anyhow!("joinProject returned no project information"))?;
    let project = info
        .get("project")
        .cloned()
        .ok_or_else(|| anyhow!("joinProject returned no project tree"))?;
    Ok((socket, project))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn walks_nested_overleaf_project_tree() {
        let project = json!({"rootFolder":[{
            "_id":"root",
            "docs":[{"_id":"d1","name":"main.tex"}],
            "folders":[{"name":"chapters","docs":[{"_id":"d2","name":"one.tex"}]}]
        }]});
        assert_eq!(
            collect_documents(&project),
            vec![
                DocumentRef {
                    id: "d1".into(),
                    path: "/main.tex".into()
                },
                DocumentRef {
                    id: "d2".into(),
                    path: "/chapters/one.tex".into()
                }
            ]
        );
        assert_eq!(
            find_document(&project, "chapters/one.tex").unwrap().id,
            "d2"
        );
        assert_eq!(root_folder_id(&project).as_deref(), Some("root"));
    }
}
