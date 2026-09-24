use anyhow::{Context as _, Result};
use futures::AsyncReadExt as _;
use http_client::{AsyncBody, HttpClient, Request};
use serde::{Deserialize, de::DeserializeOwned};
use std::sync::Arc;

const API_URL: &str = "https://api.clickup.com/api/v2";

/// The failures the UI reacts to differently. Anything else surfaces as a plain error.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    /// The token was regenerated, revoked or mistyped.
    #[error("O ClickUp recusou o token")]
    Unauthorized,
    #[error("Limite de requisições do ClickUp atingido; tente de novo em instantes")]
    RateLimited,
    #[error("O ClickUp respondeu {status}: {body}")]
    Status { status: u16, body: String },
}

#[derive(Clone, Debug, Deserialize)]
pub struct User {
    pub id: u64,
    pub username: Option<String>,
    pub email: String,
}

impl User {
    /// ClickUp leaves `username` empty for accounts that never set one.
    pub fn display_name(&self) -> &str {
        self.username
            .as_deref()
            .filter(|name| !name.trim().is_empty())
            .unwrap_or(&self.email)
    }
}

/// What the API calls a "team" is a workspace everywhere in ClickUp's own UI.
#[derive(Clone, Debug, Deserialize)]
pub struct Workspace {
    pub id: String,
    pub name: String,
}

#[derive(Deserialize)]
struct UserResponse {
    user: User,
}

#[derive(Deserialize)]
struct WorkspacesResponse {
    teams: Vec<Workspace>,
}

/// A task as the list shows it. ClickUp sends far more; only what the panel reads is kept.
#[derive(Clone, Debug, Deserialize)]
pub struct Task {
    pub id: String,
    pub custom_id: Option<String>,
    pub name: String,
    pub url: String,
    pub status: TaskStatus,
    pub priority: Option<Priority>,
    /// Milliseconds since the epoch. ClickUp sends timestamps as strings.
    #[serde(default, deserialize_with = "deserialize_millis")]
    pub due_date: Option<i64>,
    #[serde(default, deserialize_with = "deserialize_millis")]
    pub date_closed: Option<i64>,
    pub list: ListReference,
    #[serde(default)]
    pub assignees: Vec<Assignee>,
}

impl Task {
    /// The ID people type and put in branch names: the custom one when the space has them.
    pub fn display_id(&self) -> &str {
        self.custom_id.as_deref().unwrap_or(&self.id)
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct TaskStatus {
    pub status: String,
    pub color: String,
    /// `open` (not started), `custom` (in progress), `done` or `closed`.
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, deserialize_with = "deserialize_order_index")]
    pub orderindex: i64,
}

impl TaskStatus {
    pub fn is_closed(&self) -> bool {
        matches!(self.kind.as_str(), "closed" | "done")
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct Priority {
    /// `urgent`, `high`, `normal` or `low`.
    pub priority: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ListReference {
    pub id: String,
    pub name: String,
}

#[derive(Deserialize)]
struct TasksResponse {
    tasks: Vec<Task>,
}

/// ClickUp's fixed page size for task queries; a shorter page is the last one.
const TASK_PAGE_SIZE: usize = 100;
/// A safety net against paging forever if the API ever stops returning short pages.
const MAX_TASK_PAGES: usize = 20;

/// The tasks assigned to `user_id` in a workspace. With `closed_since` set, the closed ones
/// updated since then are included, which keeps the closed tab from paging through years of
/// history.
pub async fn get_assigned_tasks(
    client: &Arc<dyn HttpClient>,
    token: &str,
    workspace_id: &str,
    user_id: u64,
    closed_since: Option<i64>,
) -> Result<Vec<Task>> {
    let mut tasks = Vec::new();
    for page in 0..MAX_TASK_PAGES {
        let mut path = format!(
            "/team/{workspace_id}/task?assignees[]={user_id}&subtasks=true&order_by=due_date&page={page}"
        );
        if let Some(closed_since) = closed_since {
            path.push_str(&format!(
                "&include_closed=true&date_updated_gt={closed_since}"
            ));
        }
        let response: TasksResponse = get(client, token, &path).await?;
        let page_length = response.tasks.len();
        tasks.extend(response.tasks);
        if page_length < TASK_PAGE_SIZE {
            break;
        }
    }
    // The filter above is the one that keeps the query small, but ClickUp has answered it
    // with every task of the workspace before, so the tasks are checked here as well.
    tasks.retain(|task| {
        task.assignees
            .iter()
            .any(|assignee| assignee.id == Some(user_id))
    });
    Ok(tasks)
}

fn deserialize_millis<'de, D>(deserializer: D) -> std::result::Result<Option<i64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(match value {
        Some(serde_json::Value::String(text)) => text.parse().ok(),
        Some(serde_json::Value::Number(number)) => number.as_i64(),
        _ => None,
    })
}

fn deserialize_user_id<'de, D>(deserializer: D) -> std::result::Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(match value {
        Some(serde_json::Value::String(text)) => text.parse().ok(),
        Some(serde_json::Value::Number(number)) => number.as_u64(),
        _ => None,
    })
}

/// Status positions arrive as numbers in some responses and as strings in others.
fn deserialize_order_index<'de, D>(deserializer: D) -> std::result::Result<i64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(match value {
        Some(serde_json::Value::String(text)) => text.parse().unwrap_or_default(),
        Some(serde_json::Value::Number(number)) => number.as_i64().unwrap_or_default(),
        _ => 0,
    })
}

/// A task with what the detail view shows beyond the list row.
#[derive(Clone, Debug, Deserialize)]
pub struct TaskDetail {
    #[serde(flatten)]
    pub task: Task,
    /// The description as plain text. ClickUp sends an empty string when there is none.
    #[serde(default)]
    pub text_content: Option<String>,
    /// Milliseconds.
    #[serde(default, deserialize_with = "deserialize_millis")]
    pub time_estimate: Option<i64>,
    /// Milliseconds.
    #[serde(default, deserialize_with = "deserialize_millis")]
    pub time_spent: Option<i64>,
    pub folder: Option<FolderReference>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Assignee {
    /// ClickUp sends user ids as numbers in some responses and as strings in others.
    #[serde(default, deserialize_with = "deserialize_user_id")]
    pub id: Option<u64>,
    pub username: Option<String>,
    pub email: Option<String>,
}

impl Assignee {
    pub fn display_name(&self) -> &str {
        self.username
            .as_deref()
            .filter(|name| !name.trim().is_empty())
            .or(self.email.as_deref())
            .unwrap_or("?")
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct FolderReference {
    pub name: String,
    /// Lists created straight in a space live in a hidden folder the UI never names.
    #[serde(default)]
    pub hidden: bool,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Comment {
    pub id: String,
    /// The comment as plain text, which leaves images and attachments out.
    #[serde(default)]
    pub comment_text: String,
    /// The comment as ClickUp stores it: runs of text, images and attachments. Kept as raw
    /// JSON because where an image sits in it depends on the editor that wrote the comment.
    #[serde(default)]
    pub comment: Vec<serde_json::Value>,
    pub user: Assignee,
    #[serde(default, deserialize_with = "deserialize_millis")]
    pub date: Option<i64>,
}

impl Comment {
    /// The images and files attached in the comment, in the order they appear: any object in it
    /// with a URL on ClickUp's attachment host, plus attachment links in the plain text.
    pub fn attachments(&self) -> Vec<CommentAttachment> {
        let mut attachments = Vec::new();
        for block in &self.comment {
            collect_attachments(block, &mut attachments);
        }
        for word in self.comment_text.split_whitespace() {
            if is_attachment_url(word) && !attachments.iter().any(|found| found.url == word) {
                attachments.push(CommentAttachment::from_url(word.to_string()));
            }
        }
        attachments
    }
}

fn is_attachment_url(url: &str) -> bool {
    url.strip_prefix("https://")
        .and_then(|rest| rest.split('/').next())
        .is_some_and(|host| host.ends_with(".clickup-attachments.com"))
}

fn collect_attachments(value: &serde_json::Value, attachments: &mut Vec<CommentAttachment>) {
    match value {
        serde_json::Value::Object(object) => {
            let url = object.get("url").and_then(|url| url.as_str());
            if let Some(url) = url.filter(|url| is_attachment_url(url)) {
                if !attachments.iter().any(|found| found.url == url) {
                    let text = |key: &str| {
                        object
                            .get(key)
                            .and_then(|value| value.as_str())
                            .map(str::to_string)
                    };
                    let mut attachment = CommentAttachment::from_url(url.to_string());
                    attachment.name = text("title").or_else(|| text("name"));
                    // ClickUp puts a MIME type in `extension` in some responses and a file
                    // extension in others, and `type` is either "image" or the extension.
                    let hints = [text("extension"), text("type"), text("mimetype")];
                    if hints.iter().flatten().any(|hint| is_image_hint(hint)) {
                        attachment.is_image = true;
                    }
                    attachments.push(attachment);
                }
                return;
            }
            for value in object.values() {
                collect_attachments(value, attachments);
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                collect_attachments(value, attachments);
            }
        }
        // Some editors store the attachment as JSON inside a string attribute.
        serde_json::Value::String(text)
            if text.starts_with('{') && text.contains("clickup-attachments") =>
        {
            if let Ok(nested) = serde_json::from_str::<serde_json::Value>(text) {
                collect_attachments(&nested, attachments);
            }
        }
        _ => {}
    }
}

fn is_image_hint(hint: &str) -> bool {
    let hint = hint.to_ascii_lowercase();
    hint == "image"
        || hint.starts_with("image/")
        || matches!(
            hint.trim_start_matches('.'),
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "svg" | "heic"
        )
}

#[derive(Clone, Debug, PartialEq)]
pub struct CommentAttachment {
    pub url: String,
    pub name: Option<String>,
    pub is_image: bool,
}

impl CommentAttachment {
    fn from_url(url: String) -> Self {
        let file_name = url
            .split(['?', '#'])
            .next()
            .and_then(|path| path.rsplit('/').next())
            .filter(|name| !name.is_empty())
            .map(|name| {
                urlencoding::decode(name).map_or(name.to_string(), |name| name.into_owned())
            });
        let is_image = file_name
            .as_deref()
            .and_then(|name| name.rsplit_once('.'))
            .is_some_and(|(_, extension)| is_image_hint(extension));
        Self {
            url,
            name: file_name,
            is_image,
        }
    }

    pub fn is_image(&self) -> bool {
        self.is_image
    }

    /// The file extension to cache the attachment under.
    pub fn extension(&self) -> Option<&str> {
        let path = self.url.split(['?', '#']).next()?;
        let (_, extension) = path.rsplit('/').next()?.rsplit_once('.')?;
        extension
            .chars()
            .all(|character| character.is_ascii_alphanumeric())
            .then_some(extension)
    }

    pub fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or("anexo")
    }
}

#[derive(Deserialize)]
struct CommentsResponse {
    comments: Vec<Comment>,
}

#[derive(Deserialize)]
struct ListResponse {
    statuses: Vec<TaskStatus>,
}

/// The timer running for the user, if any.
#[derive(Clone, Debug, Deserialize)]
pub struct RunningTimer {
    pub task: Option<TimerTask>,
    /// Milliseconds since the epoch.
    #[serde(deserialize_with = "deserialize_millis")]
    pub start: Option<i64>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct TimerTask {
    pub id: String,
}

#[derive(Deserialize)]
struct RunningTimerResponse {
    data: Option<RunningTimer>,
}

pub async fn get_task(
    client: &Arc<dyn HttpClient>,
    token: &str,
    task_id: &str,
) -> Result<TaskDetail> {
    get(client, token, &format!("/task/{task_id}")).await
}

/// The statuses a task in `list_id` can move to, in board order.
pub async fn get_list_statuses(
    client: &Arc<dyn HttpClient>,
    token: &str,
    list_id: &str,
) -> Result<Vec<TaskStatus>> {
    let response: ListResponse = get(client, token, &format!("/list/{list_id}")).await?;
    let mut statuses = response.statuses;
    statuses.sort_by_key(|status| status.orderindex);
    Ok(statuses)
}

pub async fn set_task_status(
    client: &Arc<dyn HttpClient>,
    token: &str,
    task_id: &str,
    status: &str,
) -> Result<()> {
    send(
        client,
        token,
        http_client::Method::PUT,
        &format!("/task/{task_id}"),
        serde_json::json!({ "status": status }),
    )
    .await
    .map(|_: serde_json::Value| ())
}

/// The comments on a task, oldest first.
pub async fn get_comments(
    client: &Arc<dyn HttpClient>,
    token: &str,
    task_id: &str,
) -> Result<Vec<Comment>> {
    let response: CommentsResponse =
        get(client, token, &format!("/task/{task_id}/comment")).await?;
    let mut comments = response.comments;
    comments.sort_by_key(|comment| comment.date);
    Ok(comments)
}

pub async fn post_comment(
    client: &Arc<dyn HttpClient>,
    token: &str,
    task_id: &str,
    text: &str,
) -> Result<()> {
    send(
        client,
        token,
        http_client::Method::POST,
        &format!("/task/{task_id}/comment"),
        serde_json::json!({ "comment_text": text, "notify_all": false }),
    )
    .await
    .map(|_: serde_json::Value| ())
}

pub async fn get_running_timer(
    client: &Arc<dyn HttpClient>,
    token: &str,
    workspace_id: &str,
) -> Result<Option<RunningTimer>> {
    let response: RunningTimerResponse = get(
        client,
        token,
        &format!("/team/{workspace_id}/time_entries/current"),
    )
    .await?;
    Ok(response.data)
}

pub async fn start_timer(
    client: &Arc<dyn HttpClient>,
    token: &str,
    workspace_id: &str,
    task_id: &str,
) -> Result<()> {
    send(
        client,
        token,
        http_client::Method::POST,
        &format!("/team/{workspace_id}/time_entries/start"),
        serde_json::json!({ "tid": task_id }),
    )
    .await
    .map(|_: serde_json::Value| ())
}

pub async fn stop_timer(
    client: &Arc<dyn HttpClient>,
    token: &str,
    workspace_id: &str,
) -> Result<()> {
    send(
        client,
        token,
        http_client::Method::POST,
        &format!("/team/{workspace_id}/time_entries/stop"),
        serde_json::json!({}),
    )
    .await
    .map(|_: serde_json::Value| ())
}

/// Downloads a file attached to a comment. The token only goes to ClickUp's own attachment
/// hosts, never to a link someone pasted into the comment.
pub async fn download_attachment(
    client: &Arc<dyn HttpClient>,
    token: &str,
    url: &str,
) -> Result<Vec<u8>> {
    let host = url
        .strip_prefix("https://")
        .and_then(|rest| rest.split('/').next())
        .unwrap_or_default();
    let is_clickup = host == "clickup.com"
        || host.ends_with(".clickup.com")
        || host.ends_with(".clickup-attachments.com");
    let mut request = Request::get(url);
    if is_clickup {
        request = request.header("Authorization", token);
    }
    let mut response = client
        .send(request.body(AsyncBody::default())?)
        .await
        .context("falha ao baixar o anexo do ClickUp")?;
    let status = response.status();
    let mut bytes = Vec::new();
    response.body_mut().read_to_end(&mut bytes).await?;
    anyhow::ensure!(
        status.is_success(),
        "o ClickUp respondeu {status} ao baixar o anexo"
    );
    Ok(bytes)
}

pub async fn get_user(client: &Arc<dyn HttpClient>, token: &str) -> Result<User> {
    let response: UserResponse = get(client, token, "/user").await?;
    Ok(response.user)
}

pub async fn get_workspaces(client: &Arc<dyn HttpClient>, token: &str) -> Result<Vec<Workspace>> {
    let response: WorkspacesResponse = get(client, token, "/team").await?;
    Ok(response.teams)
}

async fn get<T: DeserializeOwned>(
    client: &Arc<dyn HttpClient>,
    token: &str,
    path: &str,
) -> Result<T> {
    // Personal tokens go in the header as they are; only OAuth tokens take a `Bearer` prefix.
    let request = Request::get(format!("{API_URL}{path}"))
        .header("Authorization", token)
        .header("Accept", "application/json")
        .body(AsyncBody::default())?;
    execute(client, path, request).await
}

async fn send<T: DeserializeOwned>(
    client: &Arc<dyn HttpClient>,
    token: &str,
    method: http_client::Method,
    path: &str,
    body: serde_json::Value,
) -> Result<T> {
    let request = Request::builder()
        .method(method)
        .uri(format!("{API_URL}{path}"))
        .header("Authorization", token)
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
        .body(AsyncBody::from(serde_json::to_string(&body)?))?;
    execute(client, path, request).await
}

async fn execute<T: DeserializeOwned>(
    client: &Arc<dyn HttpClient>,
    path: &str,
    request: Request<AsyncBody>,
) -> Result<T> {
    // Errors name the endpoint; the query only makes them longer.
    let path = path.split('?').next().unwrap_or(path);
    // Errors name the endpoint; the query only makes them longer.
    let path = path.split('?').next().unwrap_or(path);
    let mut response = client
        .send(request)
        .await
        .with_context(|| format!("falha ao chamar o ClickUp em {path}"))?;

    let mut body = String::new();
    response.body_mut().read_to_string(&mut body).await?;

    let status = response.status();
    if status.as_u16() == 401 {
        return Err(ApiError::Unauthorized.into());
    }
    if status.as_u16() == 429 {
        return Err(ApiError::RateLimited.into());
    }
    if !status.is_success() {
        return Err(ApiError::Status {
            status: status.as_u16(),
            body,
        }
        .into());
    }

    serde_json::from_str(&body).with_context(|| format!("resposta inesperada do ClickUp em {path}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_task_with_string_timestamps() {
        let task: Task = serde_json::from_str(
            r##"{
                "id": "86a1b2c3d",
                "custom_id": null,
                "name": "Login com biometria",
                "url": "https://app.clickup.com/t/86a1b2c3d",
                "status": {"status": "in progress", "color": "#4194f6", "type": "custom", "orderindex": 1},
                "priority": {"id": "2", "priority": "high", "color": "#ffcc00"},
                "due_date": "1790200000000",
                "date_closed": null,
                "list": {"id": "901", "name": "Sprint 34"}
            }"##,
        )
        .unwrap();
        assert_eq!(task.due_date, Some(1_790_200_000_000));
        assert_eq!(task.date_closed, None);
        assert_eq!(task.status.orderindex, 1);
        assert!(!task.status.is_closed());
        assert_eq!(task.display_id(), "86a1b2c3d");
        assert_eq!(
            task.priority.map(|priority| priority.priority).as_deref(),
            Some("high")
        );
    }

    #[test]
    fn parses_a_task_detail() {
        let detail: TaskDetail = serde_json::from_str(
            r##"{
                "id": "86a1b2", "custom_id": "CU-86a1b2", "name": "Login com biometria",
                "url": "https://app.clickup.com/t/86a1b2",
                "status": {"status": "em andamento", "color": "#4194f6", "type": "custom", "orderindex": 1},
                "priority": {"priority": "high"},
                "due_date": null,
                "list": {"id": "901", "name": "Sprint 34"},
                "folder": {"id": "7", "name": "App Mobile", "hidden": false},
                "text_content": "Permitir login com Face ID.",
                "assignees": [{"id": 1, "username": "Vinicios", "email": "v@example.com", "color": "#7b68ee"}],
                "time_estimate": 14400000,
                "time_spent": "10560000"
            }"##,
        )
        .unwrap();
        assert_eq!(detail.task.display_id(), "CU-86a1b2");
        assert_eq!(detail.time_estimate, Some(14_400_000));
        assert_eq!(detail.time_spent, Some(10_560_000));
        assert_eq!(detail.task.assignees[0].display_name(), "Vinicios");
        assert_eq!(
            detail.folder.map(|folder| folder.name).as_deref(),
            Some("App Mobile")
        );
    }

    #[test]
    fn reads_the_assignees_of_listed_tasks() {
        let response: TasksResponse = serde_json::from_str(
            r##"{"tasks": [
                {"id": "1", "name": "minha", "url": "u", "custom_id": null,
                 "status": {"status": "open", "color": "#ccc", "type": "open", "orderindex": 0},
                 "list": {"id": "9", "name": "Sprint"},
                 "assignees": [{"id": "7", "username": "Vinicios"}, {"id": 8, "username": "Ana"}, {"id": null}]},
                {"id": "2", "name": "da Ana", "url": "u", "custom_id": null,
                 "status": {"status": "open", "color": "#ccc", "type": "open", "orderindex": 0},
                 "list": {"id": "9", "name": "Sprint"},
                 "assignees": [{"id": 8, "username": "Ana"}]}
            ]}"##,
        )
        .unwrap();
        let mine: Vec<&str> = response
            .tasks
            .iter()
            .filter(|task| task.assignees.iter().any(|assignee| assignee.id == Some(7)))
            .map(|task| task.name.as_str())
            .collect();
        assert_eq!(mine, ["minha"]);
    }

    #[test]
    fn parses_comments_and_the_running_timer() {
        let comments: CommentsResponse = serde_json::from_str(
            r##"{"comments": [{
                "id": "9", "comment_text": "Lembra do caso X ", "date": "1790100000000",
                "user": {"id": 2, "username": "Mariana"},
                "comment": [
                    {"text": "Lembra do caso X "},
                    {"type": "image", "text": "tela.png", "image": {
                        "id": "a", "name": "tela.png", "title": "tela.png", "type": "image",
                        "extension": "png", "url": "https://t1.p.clickup-attachments.com/tela.png",
                        "thumbnail_large": "https://t1.p.clickup-attachments.com/tela_large.png"}},
                    {"type": "attachment", "attachment": {
                        "name": "log.txt", "extension": "txt",
                        "url": "https://t1.p.clickup-attachments.com/log.txt"}},
                    {"text": "\n", "attributes": {"block-id": "x"}}
                ]
            }]}"##,
        )
        .unwrap();
        let comment = &comments.comments[0];
        assert_eq!(comment.user.display_name(), "Mariana");
        assert_eq!(comment.date, Some(1_790_100_000_000));
        let attachments = comment.attachments();
        assert_eq!(attachments.len(), 2);
        assert!(attachments[0].is_image());
        assert_eq!(
            attachments[0].url,
            "https://t1.p.clickup-attachments.com/tela.png"
        );
        assert_eq!(attachments[0].display_name(), "tela.png");
        assert!(!attachments[1].is_image());
        assert_eq!(attachments[1].display_name(), "log.txt");

        let timer: RunningTimerResponse = serde_json::from_str(
            r##"{"data": {"id": "t1", "task": {"id": "86a1b2"}, "start": "1790200000000", "duration": "-12000"}}"##,
        )
        .unwrap();
        let timer = timer.data.unwrap();
        assert_eq!(timer.task.map(|task| task.id).as_deref(), Some("86a1b2"));
        assert_eq!(timer.start, Some(1_790_200_000_000));

        let idle: RunningTimerResponse = serde_json::from_str(r#"{"data": null}"#).unwrap();
        assert!(idle.data.is_none());
    }

    #[test]
    fn finds_images_wherever_the_editor_put_them() {
        let comment: Comment = serde_json::from_str(
            r##"{
                "id": "1", "comment_text": "veja https://t1.p.clickup-attachments.com/t1/b/foto%20final.jpg",
                "user": {"id": 1},
                "comment": [
                    {"type": "image", "text": "print.png", "image": {
                        "name": "print.png", "type": "png", "extension": "image/png",
                        "url": "https://t1.p.clickup-attachments.com/t1/a/print.png"}},
                    {"text": "\n", "attributes": {"data-attachment":
                        "{\"title\":\"diagrama\",\"mimetype\":\"image/webp\",\"url\":\"https://t1.p.clickup-attachments.com/t1/c/diagrama\"}"}},
                    {"text": "link externo https://example.com/x.png"}
                ]
            }"##,
        )
        .unwrap();
        let attachments = comment.attachments();
        let names: Vec<(&str, bool)> = attachments
            .iter()
            .map(|attachment| (attachment.display_name(), attachment.is_image()))
            .collect();
        assert_eq!(
            names,
            [
                ("print.png", true),
                ("diagrama", true),
                ("foto final.jpg", true)
            ]
        );
        assert_eq!(attachments[0].extension(), Some("png"));
        assert_eq!(attachments[1].extension(), None);
    }

    #[gpui::test]
    async fn sends_the_token_only_to_clickup_hosts() {
        let client: Arc<dyn HttpClient> =
            http_client::FakeHttpClient::create(|request| async move {
                let authorized = request.headers().contains_key("Authorization");
                Ok(http_client::Response::builder()
                    .status(200)
                    .body(if authorized { "com token" } else { "sem token" }.into())?)
            });
        let read = |url: &'static str| {
            let client = client.clone();
            async move {
                String::from_utf8(download_attachment(&client, "pk_1", url).await.unwrap()).unwrap()
            }
        };
        assert_eq!(
            read("https://t1.p.clickup-attachments.com/t1/a/tela.png").await,
            "com token"
        );
        assert_eq!(read("https://app.clickup.com/x.png").await, "com token");
        assert_eq!(
            read("https://example.com/clickup.com/x.png").await,
            "sem token"
        );
        assert_eq!(read("https://evilclickup.com/x.png").await, "sem token");
    }

    #[test]
    fn tolerates_missing_optional_fields() {
        let task: Task = serde_json::from_str(
            r##"{
                "id": "1", "custom_id": "APP-12", "name": "x", "url": "u",
                "status": {"status": "complete", "color": "#6bc950", "type": "closed", "orderindex": "3"},
                "priority": null,
                "list": {"id": "2", "name": "Bugs"}
            }"##,
        )
        .unwrap();
        assert_eq!(task.due_date, None);
        assert_eq!(task.status.orderindex, 3);
        assert!(task.status.is_closed());
        assert_eq!(task.display_id(), "APP-12");
    }
}
