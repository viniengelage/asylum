//! GitHub Projects (v2) through the GraphQL API, the only one that has them. Uses the account
//! connected in the Repo dock.

use anyhow::{Context as _, Result};
use chrono::NaiveDate;
use http_client::HttpClient;
use repo_hosting::github::{self, Credentials};
use serde::Deserialize;
use std::{collections::HashMap, sync::Arc};

/// A project the repository's people can pick in the dock.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectSummary {
    pub id: String,
    pub title: String,
    pub number: u64,
    pub url: String,
}

/// What a repository needs for linking branches, and the projects its people can see.
#[derive(Clone, Debug)]
pub struct Repository {
    pub id: String,
    pub default_branch: Option<String>,
    pub default_branch_oid: Option<String>,
    /// The projects linked to the repository first, then its owner's, then the viewer's.
    pub projects: Vec<ProjectSummary>,
    pub viewer_id: String,
    pub viewer_login: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FieldOption {
    pub id: String,
    pub name: String,
    /// GitHub's color name, such as `GREEN` or `PURPLE`.
    pub color: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SingleSelectField {
    pub id: String,
    pub name: String,
    pub options: Vec<FieldOption>,
}

impl SingleSelectField {
    pub fn option(&self, id: &str) -> Option<&FieldOption> {
        self.options.iter().find(|option| option.id == id)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Iteration {
    pub id: String,
    pub title: String,
    pub start_date: Option<NaiveDate>,
    pub duration_days: u32,
}

impl Iteration {
    pub fn end_date(&self) -> Option<NaiveDate> {
        self.start_date.map(|start| {
            start + chrono::Duration::days(i64::from(self.duration_days.saturating_sub(1)))
        })
    }

    pub fn contains(&self, date: NaiveDate) -> bool {
        match (self.start_date, self.end_date()) {
            (Some(start), Some(end)) => start <= date && date <= end,
            _ => false,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct IterationField {
    pub id: String,
    pub name: String,
    /// The active and upcoming ones, as GitHub orders them.
    pub iterations: Vec<Iteration>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct NumberField {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum FieldValue {
    Option(String),
    Iteration(String),
    Number(f64),
    Text(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemKind {
    Issue,
    PullRequest,
    Draft,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Label {
    pub name: String,
    /// Hex without the `#`.
    pub color: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LinkedPullRequest {
    pub number: u64,
    pub url: String,
    /// `OPEN`, `CLOSED` or `MERGED`.
    pub state: String,
}

/// A card of the project: an issue, a pull request or a draft.
#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    /// The project item's id, which field updates take.
    pub id: String,
    pub kind: ItemKind,
    /// The issue's or pull request's own id, which comments and assignees take.
    pub content_id: String,
    pub number: Option<u64>,
    pub title: String,
    pub url: Option<String>,
    pub closed: bool,
    pub body: String,
    /// `owner/name`.
    pub repository: Option<String>,
    pub author: Option<String>,
    pub assignees: Vec<String>,
    pub labels: Vec<Label>,
    pub linked_branches: Vec<String>,
    pub pull_requests: Vec<LinkedPullRequest>,
    /// By field id.
    pub values: HashMap<String, FieldValue>,
}

impl Item {
    /// `asylum#198`, or the draft's title when it has no number.
    pub fn reference(&self) -> String {
        match (self.number, &self.repository) {
            (Some(number), Some(repository)) => {
                let name = repository.rsplit('/').next().unwrap_or(repository);
                format!("{name}#{number}")
            }
            (Some(number), None) => format!("#{number}"),
            _ => "rascunho".to_string(),
        }
    }

    pub fn option_id(&self, field_id: &str) -> Option<&str> {
        match self.values.get(field_id)? {
            FieldValue::Option(id) => Some(id),
            _ => None,
        }
    }

    pub fn iteration_id(&self, field_id: &str) -> Option<&str> {
        match self.values.get(field_id)? {
            FieldValue::Iteration(id) => Some(id),
            _ => None,
        }
    }

    pub fn number_value(&self, field_id: &str) -> Option<f64> {
        match self.values.get(field_id)? {
            FieldValue::Number(number) => Some(*number),
            _ => None,
        }
    }

    pub fn is_assigned_to(&self, login: &str) -> bool {
        self.assignees
            .iter()
            .any(|assignee| assignee.eq_ignore_ascii_case(login))
    }

    /// The `- [ ]` and `- [x]` lines of the body, which GitHub shows as a checklist.
    pub fn subtasks(&self) -> Vec<Subtask> {
        subtasks(&self.body)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Subtask {
    pub text: String,
    pub done: bool,
    /// Which line of the body it is, to tick it.
    pub line: usize,
}

pub fn subtasks(body: &str) -> Vec<Subtask> {
    body.lines()
        .enumerate()
        .filter_map(|(line, text)| {
            let trimmed = text.trim_start();
            let rest = trimmed
                .strip_prefix("- ")
                .or_else(|| trimmed.strip_prefix("* "))?;
            let (done, text) = if let Some(text) = rest.strip_prefix("[ ] ") {
                (false, text)
            } else if let Some(text) = rest
                .strip_prefix("[x] ")
                .or_else(|| rest.strip_prefix("[X] "))
            {
                (true, text)
            } else {
                return None;
            };
            Some(Subtask {
                text: text.trim().to_string(),
                done,
                line,
            })
        })
        .collect()
}

/// The body with one checklist line ticked or unticked.
pub fn toggle_subtask(body: &str, line: usize, done: bool) -> String {
    body.lines()
        .enumerate()
        .map(|(index, text)| {
            if index != line {
                return text.to_string();
            }
            if done {
                text.replacen("[ ]", "[x]", 1)
            } else {
                text.replacen("[x]", "[ ]", 1).replacen("[X]", "[ ]", 1)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RateLimit {
    pub remaining: u64,
    pub limit: u64,
}

/// A project with the fields the dock and the board use, and every open item.
#[derive(Clone, Debug)]
pub struct Project {
    pub summary: ProjectSummary,
    pub status: Option<SingleSelectField>,
    pub priority: Option<SingleSelectField>,
    pub iteration: Option<IterationField>,
    pub estimate: Option<NumberField>,
    pub items: Vec<Item>,
    pub rate_limit: RateLimit,
}

impl Project {
    pub fn item(&self, id: &str) -> Option<&Item> {
        self.items.iter().find(|item| item.id == id)
    }

    pub fn status_of(&self, item: &Item) -> Option<&FieldOption> {
        let field = self.status.as_ref()?;
        field.option(item.option_id(&field.id)?)
    }

    pub fn priority_of(&self, item: &Item) -> Option<&FieldOption> {
        let field = self.priority.as_ref()?;
        field.option(item.option_id(&field.id)?)
    }

    pub fn iteration_of(&self, item: &Item) -> Option<&Iteration> {
        let field = self.iteration.as_ref()?;
        let id = item.iteration_id(&field.id)?;
        field.iterations.iter().find(|iteration| iteration.id == id)
    }

    pub fn estimate_of(&self, item: &Item) -> Option<f64> {
        item.number_value(&self.estimate.as_ref()?.id)
    }

    /// The iteration today falls in.
    pub fn current_iteration(&self, today: NaiveDate) -> Option<&Iteration> {
        self.iteration
            .as_ref()?
            .iterations
            .iter()
            .find(|iteration| iteration.contains(today))
    }

    /// The status options the board uses as columns, in the project's order.
    pub fn columns(&self) -> &[FieldOption] {
        self.status
            .as_ref()
            .map_or(&[], |field| field.options.as_slice())
    }

    /// The option that means "being worked on": named so, or else the second column.
    pub fn in_progress_option(&self) -> Option<&FieldOption> {
        let options = self.columns();
        options
            .iter()
            .find(|option| {
                let name = option.name.to_lowercase();
                name.contains("progress") || name.contains("andamento") || name == "doing"
            })
            .or_else(|| options.get(2).or(options.get(1)))
    }

    /// The option that means "waiting for review".
    pub fn review_option(&self) -> Option<&FieldOption> {
        self.columns().iter().find(|option| {
            let name = option.name.to_lowercase();
            name.contains("review") || name.contains("revisão") || name.contains("revisao")
        })
    }

    /// The option that means "finished", which GitHub's own workflow moves closed items to.
    pub fn done_option(&self) -> Option<&FieldOption> {
        let options = self.columns();
        options
            .iter()
            .find(|option| {
                let name = option.name.to_lowercase();
                name == "done" || name.contains("feito") || name.contains("conclu")
            })
            .or_else(|| options.last())
    }
}

const PROJECT_FIELDS: &str = "id title number url closed";

pub async fn get_repository(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    owner: &str,
    name: &str,
) -> Result<Repository> {
    #[derive(Deserialize)]
    struct Node {
        id: String,
        title: String,
        number: u64,
        url: String,
        #[serde(default)]
        closed: bool,
    }
    #[derive(Deserialize)]
    struct Nodes {
        #[serde(default)]
        nodes: Vec<Option<Node>>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Owner {
        #[serde(default)]
        projects_v2: Option<Nodes>,
    }
    #[derive(Deserialize)]
    struct Target {
        #[serde(default)]
        oid: Option<String>,
    }
    #[derive(Deserialize)]
    struct BranchRef {
        name: String,
        #[serde(default)]
        target: Option<Target>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct RepositoryNode {
        id: String,
        #[serde(default)]
        default_branch_ref: Option<BranchRef>,
        #[serde(default)]
        projects_v2: Option<Nodes>,
        #[serde(default)]
        owner: Option<Owner>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Viewer {
        id: String,
        login: String,
        #[serde(default)]
        projects_v2: Option<Nodes>,
    }
    #[derive(Deserialize)]
    struct Data {
        repository: Option<RepositoryNode>,
        viewer: Viewer,
    }

    let query = format!(
        "query($owner: String!, $name: String!) {{
          repository(owner: $owner, name: $name) {{
            id
            defaultBranchRef {{ name target {{ oid }} }}
            projectsV2(first: 20) {{ nodes {{ {PROJECT_FIELDS} }} }}
            owner {{ ... on ProjectV2Owner {{ projectsV2(first: 20) {{ nodes {{ {PROJECT_FIELDS} }} }} }} }}
          }}
          viewer {{ id login projectsV2(first: 20) {{ nodes {{ {PROJECT_FIELDS} }} }} }}
        }}"
    );
    let data: Data = github::graphql(
        client,
        credentials,
        &query,
        serde_json::json!({ "owner": owner, "name": name }),
    )
    .await?;
    let repository = data
        .repository
        .with_context(|| format!("o GitHub não mostra {owner}/{name} para esta conta"))?;

    let mut projects: Vec<ProjectSummary> = Vec::new();
    let groups = [
        repository.projects_v2,
        repository.owner.and_then(|owner| owner.projects_v2),
        data.viewer.projects_v2,
    ];
    for node in groups
        .into_iter()
        .flatten()
        .flat_map(|nodes| nodes.nodes)
        .flatten()
    {
        if node.closed || projects.iter().any(|project| project.id == node.id) {
            continue;
        }
        projects.push(ProjectSummary {
            id: node.id,
            title: node.title,
            number: node.number,
            url: node.url,
        });
    }
    let branch = repository.default_branch_ref;
    Ok(Repository {
        id: repository.id,
        default_branch_oid: branch
            .as_ref()
            .and_then(|branch| branch.target.as_ref())
            .and_then(|target| target.oid.clone()),
        default_branch: branch.map(|branch| branch.name),
        projects,
        viewer_id: data.viewer.id,
        viewer_login: data.viewer.login,
    })
}

const ITEM_CONTENT: &str = "
  __typename
  ... on DraftIssue { id title body }
  ... on Issue {
    id number title url state body
    author { login }
    repository { nameWithOwner }
    assignees(first: 6) { nodes { login } }
    labels(first: 6) { nodes { name color } }
    linkedBranches(first: 3) { nodes { ref { name } } }
    closedByPullRequestsReferences(first: 3, includeClosedPrs: true) { nodes { number url state } }
  }
  ... on PullRequest {
    id number title url state body headRefName
    author { login }
    repository { nameWithOwner }
    assignees(first: 6) { nodes { login } }
    labels(first: 6) { nodes { name color } }
  }
";

const FIELD_VALUES: &str = "
  ... on ProjectV2ItemFieldSingleSelectValue { optionId field { ... on ProjectV2FieldCommon { id } } }
  ... on ProjectV2ItemFieldIterationValue { iterationId field { ... on ProjectV2FieldCommon { id } } }
  ... on ProjectV2ItemFieldNumberValue { number field { ... on ProjectV2FieldCommon { id } } }
  ... on ProjectV2ItemFieldTextValue { text field { ... on ProjectV2FieldCommon { id } } }
";

#[derive(Deserialize)]
struct Login {
    login: String,
}

#[derive(Deserialize)]
struct LoginNodes {
    #[serde(default)]
    nodes: Vec<Option<Login>>,
}

#[derive(Deserialize)]
struct LabelNode {
    name: String,
    #[serde(default)]
    color: String,
}

#[derive(Deserialize)]
struct LabelNodes {
    #[serde(default)]
    nodes: Vec<Option<LabelNode>>,
}

#[derive(Deserialize)]
struct RefName {
    name: String,
}

#[derive(Deserialize)]
struct LinkedBranch {
    #[serde(rename = "ref")]
    reference: Option<RefName>,
}

#[derive(Deserialize)]
struct LinkedBranches {
    #[serde(default)]
    nodes: Vec<Option<LinkedBranch>>,
}

#[derive(Deserialize)]
struct PullRequestReference {
    number: u64,
    url: String,
    state: String,
}

#[derive(Deserialize)]
struct PullRequestReferences {
    #[serde(default)]
    nodes: Vec<Option<PullRequestReference>>,
}

#[derive(Deserialize)]
struct RepositoryName {
    #[serde(rename = "nameWithOwner")]
    name_with_owner: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ContentNode {
    #[serde(rename = "__typename")]
    typename: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    number: Option<u64>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    author: Option<Login>,
    #[serde(default)]
    repository: Option<RepositoryName>,
    #[serde(default)]
    assignees: Option<LoginNodes>,
    #[serde(default)]
    labels: Option<LabelNodes>,
    #[serde(default)]
    linked_branches: Option<LinkedBranches>,
    #[serde(default)]
    closed_by_pull_requests_references: Option<PullRequestReferences>,
    #[serde(default)]
    head_ref_name: Option<String>,
}

#[derive(Deserialize)]
struct FieldReference {
    #[serde(default)]
    id: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FieldValueNode {
    #[serde(default)]
    option_id: Option<String>,
    #[serde(default)]
    iteration_id: Option<String>,
    #[serde(default)]
    number: Option<f64>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    field: Option<FieldReference>,
}

#[derive(Deserialize)]
struct FieldValueNodes {
    #[serde(default)]
    nodes: Vec<Option<FieldValueNode>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ItemNode {
    id: String,
    #[serde(default)]
    is_archived: bool,
    #[serde(default)]
    content: Option<ContentNode>,
    #[serde(default)]
    field_values: Option<FieldValueNodes>,
}

impl ItemNode {
    fn into_item(self) -> Option<Item> {
        if self.is_archived {
            return None;
        }
        let content = self.content?;
        let kind = match content.typename.as_str() {
            "Issue" => ItemKind::Issue,
            "PullRequest" => ItemKind::PullRequest,
            "DraftIssue" => ItemKind::Draft,
            _ => return None,
        };
        let mut values = HashMap::new();
        for value in self
            .field_values
            .map(|values| values.nodes)
            .unwrap_or_default()
            .into_iter()
            .flatten()
        {
            let Some(field_id) = value.field.and_then(|field| field.id) else {
                continue;
            };
            let value = if let Some(option_id) = value.option_id {
                FieldValue::Option(option_id)
            } else if let Some(iteration_id) = value.iteration_id {
                FieldValue::Iteration(iteration_id)
            } else if let Some(number) = value.number {
                FieldValue::Number(number)
            } else if let Some(text) = value.text {
                FieldValue::Text(text)
            } else {
                continue;
            };
            values.insert(field_id, value);
        }
        let mut linked_branches: Vec<String> = content
            .linked_branches
            .map(|branches| branches.nodes)
            .unwrap_or_default()
            .into_iter()
            .flatten()
            .filter_map(|branch| branch.reference.map(|reference| reference.name))
            .collect();
        linked_branches.extend(content.head_ref_name);
        Some(Item {
            id: self.id,
            kind,
            content_id: content.id.unwrap_or_default(),
            number: content.number,
            title: content.title.unwrap_or_default(),
            url: content.url,
            closed: matches!(content.state.as_deref(), Some("CLOSED" | "MERGED")),
            body: content.body.unwrap_or_default(),
            repository: content
                .repository
                .map(|repository| repository.name_with_owner),
            author: content.author.map(|author| author.login),
            assignees: content
                .assignees
                .map(|assignees| assignees.nodes)
                .unwrap_or_default()
                .into_iter()
                .flatten()
                .map(|assignee| assignee.login)
                .collect(),
            labels: content
                .labels
                .map(|labels| labels.nodes)
                .unwrap_or_default()
                .into_iter()
                .flatten()
                .map(|label| Label {
                    name: label.name,
                    color: label.color,
                })
                .collect(),
            linked_branches,
            pull_requests: content
                .closed_by_pull_requests_references
                .map(|references| references.nodes)
                .unwrap_or_default()
                .into_iter()
                .flatten()
                .map(|reference| LinkedPullRequest {
                    number: reference.number,
                    url: reference.url,
                    state: reference.state,
                })
                .collect(),
            values,
        })
    }
}

/// Enough for the open work of a team's project; archived items stay out.
const MAX_ITEM_PAGES: usize = 5;

pub async fn get_project(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    summary: &ProjectSummary,
) -> Result<Project> {
    #[derive(Deserialize)]
    struct OptionNode {
        id: String,
        name: String,
        #[serde(default)]
        color: String,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct IterationNode {
        id: String,
        title: String,
        #[serde(default)]
        start_date: Option<NaiveDate>,
        #[serde(default)]
        duration: u32,
    }
    #[derive(Deserialize)]
    struct IterationConfiguration {
        #[serde(default)]
        iterations: Vec<IterationNode>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct FieldNode {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        data_type: Option<String>,
        #[serde(default)]
        options: Option<Vec<OptionNode>>,
        #[serde(default)]
        configuration: Option<IterationConfiguration>,
    }
    #[derive(Deserialize)]
    struct FieldNodes {
        #[serde(default)]
        nodes: Vec<Option<FieldNode>>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct PageInfo {
        has_next_page: bool,
        #[serde(default)]
        end_cursor: Option<String>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct ItemPage {
        page_info: PageInfo,
        #[serde(default)]
        nodes: Vec<Option<ItemNode>>,
    }
    #[derive(Deserialize)]
    struct ProjectNode {
        #[serde(default)]
        fields: Option<FieldNodes>,
        items: ItemPage,
    }
    #[derive(Deserialize)]
    struct RateLimitNode {
        remaining: u64,
        limit: u64,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Data {
        node: Option<ProjectNode>,
        #[serde(default)]
        rate_limit: Option<RateLimitNode>,
    }

    let query = format!(
        "query($id: ID!, $after: String) {{
          node(id: $id) {{
            ... on ProjectV2 {{
              fields(first: 40) {{ nodes {{
                ... on ProjectV2Field {{ id name dataType }}
                ... on ProjectV2SingleSelectField {{ id name dataType options {{ id name color }} }}
                ... on ProjectV2IterationField {{ id name dataType configuration {{ iterations {{ id title startDate duration }} }} }}
              }} }}
              items(first: 100, after: $after) {{
                pageInfo {{ hasNextPage endCursor }}
                nodes {{
                  id isArchived
                  fieldValues(first: 20) {{ nodes {{ {FIELD_VALUES} }} }}
                  content {{ {ITEM_CONTENT} }}
                }}
              }}
            }}
          }}
          rateLimit {{ remaining limit }}
        }}"
    );

    let mut fields: Vec<FieldNode> = Vec::new();
    let mut items = Vec::new();
    let mut rate_limit = RateLimit::default();
    let mut after: Option<String> = None;
    for page in 0..MAX_ITEM_PAGES {
        let data: Data = github::graphql(
            client,
            credentials,
            &query,
            serde_json::json!({ "id": summary.id, "after": after }),
        )
        .await?;
        let node = data
            .node
            .with_context(|| format!("o projeto {} não existe mais", summary.title))?;
        if page == 0 {
            fields = node
                .fields
                .map(|fields| fields.nodes)
                .unwrap_or_default()
                .into_iter()
                .flatten()
                .collect();
        }
        if let Some(limit) = data.rate_limit {
            rate_limit = RateLimit {
                remaining: limit.remaining,
                limit: limit.limit,
            };
        }
        items.extend(
            node.items
                .nodes
                .into_iter()
                .flatten()
                .filter_map(ItemNode::into_item),
        );
        if !node.items.page_info.has_next_page {
            break;
        }
        after = node.items.page_info.end_cursor;
    }

    let named = |field: &FieldNode, names: &[&str]| {
        field.name.as_deref().is_some_and(|name| {
            names
                .iter()
                .any(|candidate| name.eq_ignore_ascii_case(candidate))
        })
    };
    let single_select = |field: &FieldNode| SingleSelectField {
        id: field.id.clone().unwrap_or_default(),
        name: field.name.clone().unwrap_or_default(),
        options: field
            .options
            .iter()
            .flatten()
            .map(|option| FieldOption {
                id: option.id.clone(),
                name: option.name.clone(),
                color: option.color.clone(),
            })
            .collect(),
    };
    let data_type = |field: &FieldNode| field.data_type.clone().unwrap_or_default();

    let status = fields
        .iter()
        .find(|field| data_type(field) == "SINGLE_SELECT" && named(field, &["Status"]))
        .map(single_select);
    let priority = fields
        .iter()
        .find(|field| {
            data_type(field) == "SINGLE_SELECT" && named(field, &["Priority", "Prioridade"])
        })
        .map(single_select);
    let iteration = fields
        .iter()
        .find(|field| data_type(field) == "ITERATION")
        .map(|field| IterationField {
            id: field.id.clone().unwrap_or_default(),
            name: field.name.clone().unwrap_or_default(),
            iterations: field
                .configuration
                .iter()
                .flat_map(|configuration| &configuration.iterations)
                .map(|iteration| Iteration {
                    id: iteration.id.clone(),
                    title: iteration.title.clone(),
                    start_date: iteration.start_date,
                    duration_days: iteration.duration,
                })
                .collect(),
        });
    let estimate = fields
        .iter()
        .find(|field| {
            data_type(field) == "NUMBER"
                && named(
                    field,
                    &["Estimate", "Estimativa", "Points", "Pontos", "Size"],
                )
        })
        .map(|field| NumberField {
            id: field.id.clone().unwrap_or_default(),
            name: field.name.clone().unwrap_or_default(),
        });

    Ok(Project {
        summary: summary.clone(),
        status,
        priority,
        iteration,
        estimate,
        items,
        rate_limit,
    })
}

/// A new value for one of an item's fields.
#[derive(Clone, Debug, PartialEq)]
pub enum NewValue {
    Option(String),
    Iteration(String),
    Number(f64),
    Clear,
}

pub async fn set_field(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    project_id: &str,
    item_id: &str,
    field_id: &str,
    value: NewValue,
) -> Result<()> {
    let variables = serde_json::json!({
        "project": project_id,
        "item": item_id,
        "field": field_id,
    });
    let (query, variables) = match value {
        NewValue::Clear => (
            "mutation($project: ID!, $item: ID!, $field: ID!) { clearProjectV2ItemFieldValue(input: {projectId: $project, itemId: $item, fieldId: $field}) { clientMutationId } }",
            variables,
        ),
        NewValue::Option(option) => (
            "mutation($project: ID!, $item: ID!, $field: ID!, $option: String!) { updateProjectV2ItemFieldValue(input: {projectId: $project, itemId: $item, fieldId: $field, value: {singleSelectOptionId: $option}}) { clientMutationId } }",
            with(variables, "option", serde_json::json!(option)),
        ),
        NewValue::Iteration(iteration) => (
            "mutation($project: ID!, $item: ID!, $field: ID!, $iteration: String!) { updateProjectV2ItemFieldValue(input: {projectId: $project, itemId: $item, fieldId: $field, value: {iterationId: $iteration}}) { clientMutationId } }",
            with(variables, "iteration", serde_json::json!(iteration)),
        ),
        NewValue::Number(number) => (
            "mutation($project: ID!, $item: ID!, $field: ID!, $number: Float!) { updateProjectV2ItemFieldValue(input: {projectId: $project, itemId: $item, fieldId: $field, value: {number: $number}}) { clientMutationId } }",
            with(variables, "number", serde_json::json!(number)),
        ),
    };
    github::graphql::<serde_json::Value>(client, credentials, query, variables).await?;
    Ok(())
}

fn with(
    mut variables: serde_json::Value,
    key: &str,
    value: serde_json::Value,
) -> serde_json::Value {
    if let Some(map) = variables.as_object_mut() {
        map.insert(key.to_string(), value);
    }
    variables
}

pub async fn add_assignee(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    content_id: &str,
    user_id: &str,
) -> Result<()> {
    github::graphql::<serde_json::Value>(
        client,
        credentials,
        "mutation($id: ID!, $user: ID!) { addAssigneesToAssignable(input: {assignableId: $id, assigneeIds: [$user]}) { clientMutationId } }",
        serde_json::json!({ "id": content_id, "user": user_id }),
    )
    .await?;
    Ok(())
}

/// Creates `name` on the remote from `oid` and links it to the issue, as "Create a branch" on
/// the issue's page does.
pub async fn create_linked_branch(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    issue_id: &str,
    repository_id: &str,
    oid: &str,
    name: &str,
) -> Result<()> {
    github::graphql::<serde_json::Value>(
        client,
        credentials,
        "mutation($issue: ID!, $repository: ID!, $oid: GitObjectID!, $name: String!) { createLinkedBranch(input: {issueId: $issue, repositoryId: $repository, oid: $oid, name: $name}) { linkedBranch { id } } }",
        serde_json::json!({
            "issue": issue_id,
            "repository": repository_id,
            "oid": oid,
            "name": name,
        }),
    )
    .await?;
    Ok(())
}

pub async fn add_comment(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    content_id: &str,
    body: &str,
) -> Result<()> {
    github::graphql::<serde_json::Value>(
        client,
        credentials,
        "mutation($id: ID!, $body: String!) { addComment(input: {subjectId: $id, body: $body}) { clientMutationId } }",
        serde_json::json!({ "id": content_id, "body": body }),
    )
    .await?;
    Ok(())
}

pub async fn update_body(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    item: &Item,
    body: &str,
) -> Result<()> {
    let query = match item.kind {
        ItemKind::Issue => {
            "mutation($id: ID!, $body: String!) { updateIssue(input: {id: $id, body: $body}) { clientMutationId } }"
        }
        ItemKind::PullRequest => {
            "mutation($id: ID!, $body: String!) { updatePullRequest(input: {pullRequestId: $id, body: $body}) { clientMutationId } }"
        }
        ItemKind::Draft => {
            "mutation($id: ID!, $body: String!) { updateProjectV2DraftIssue(input: {draftIssueId: $id, body: $body}) { clientMutationId } }"
        }
    };
    github::graphql::<serde_json::Value>(
        client,
        credentials,
        query,
        serde_json::json!({ "id": item.content_id, "body": body }),
    )
    .await?;
    Ok(())
}

/// Adds a draft to the project, which can become an issue later on GitHub.
pub async fn add_draft(
    client: &Arc<dyn HttpClient>,
    credentials: &Credentials,
    project_id: &str,
    title: &str,
) -> Result<String> {
    #[derive(Deserialize)]
    struct ProjectItem {
        id: String,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Payload {
        project_item: ProjectItem,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Data {
        add_project_v2_draft_issue: Payload,
    }
    let data: Data = github::graphql(
        client,
        credentials,
        "mutation($project: ID!, $title: String!) { addProjectV2DraftIssue(input: {projectId: $project, title: $title}) { projectItem { id } } }",
        serde_json::json!({ "project": project_id, "title": title }),
    )
    .await?;
    Ok(data.add_project_v2_draft_issue.project_item.id)
}

/// The branch name "Começar" suggests: `feat/204-rerun-actions`, or `fix/` for bugs.
pub fn branch_name(item: &Item) -> String {
    let prefix = if item
        .labels
        .iter()
        .any(|label| label.name.eq_ignore_ascii_case("bug"))
    {
        "fix"
    } else {
        "feat"
    };
    let slug: String = item
        .title
        .to_lowercase()
        .chars()
        .map(|character| {
            let character = unaccent(character);
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '-'
            }
        })
        .collect();
    let words: Vec<&str> = slug.split('-').filter(|word| !word.is_empty()).collect();
    let slug = words.into_iter().take(4).collect::<Vec<_>>().join("-");
    match item.number {
        Some(number) => format!("{prefix}/{number}-{slug}"),
        None => format!("{prefix}/{slug}"),
    }
}

fn unaccent(character: char) -> char {
    match character {
        'á' | 'à' | 'â' | 'ã' | 'ä' => 'a',
        'é' | 'è' | 'ê' | 'ë' => 'e',
        'í' | 'ì' | 'î' | 'ï' => 'i',
        'ó' | 'ò' | 'ô' | 'õ' | 'ö' => 'o',
        'ú' | 'ù' | 'û' | 'ü' => 'u',
        'ç' => 'c',
        other => other,
    }
}

/// Which item the checked-out branch is for: the branch GitHub linked to it, or a number in its
/// name (`feat/204-…`, `204-…`).
pub fn item_for_branch<'a>(items: &'a [Item], branch: &str) -> Option<&'a Item> {
    if let Some(item) = items
        .iter()
        .find(|item| item.linked_branches.iter().any(|linked| linked == branch))
    {
        return Some(item);
    }
    let last = branch.rsplit('/').next().unwrap_or(branch);
    let digits: String = last
        .chars()
        .take_while(|character| character.is_ascii_digit())
        .collect();
    let number: u64 = digits.parse().ok()?;
    items
        .iter()
        .find(|item| item.number == Some(number) && item.kind == ItemKind::Issue)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(number: u64, title: &str) -> Item {
        Item {
            id: format!("item-{number}"),
            kind: ItemKind::Issue,
            content_id: format!("issue-{number}"),
            number: Some(number),
            title: title.to_string(),
            url: None,
            closed: false,
            body: String::new(),
            repository: Some("viniengelage/asylum".to_string()),
            author: None,
            assignees: Vec::new(),
            labels: Vec::new(),
            linked_branches: Vec::new(),
            pull_requests: Vec::new(),
            values: HashMap::default(),
        }
    }

    #[test]
    fn names_the_branch_after_the_item() {
        let mut rerun = item(204, "Re-rodar job do Actions pelo dock");
        assert_eq!(branch_name(&rerun), "feat/204-re-rodar-job-do");
        rerun.labels.push(Label {
            name: "bug".to_string(),
            color: "d73a4a".to_string(),
        });
        assert_eq!(branch_name(&rerun), "fix/204-re-rodar-job-do");
        assert_eq!(
            branch_name(&item(7, "Sincronização ação")),
            "feat/7-sincronizacao-acao"
        );
    }

    #[test]
    fn finds_the_item_of_the_branch() {
        let mut linked = item(198, "Sincronizar");
        linked.linked_branches.push("projects-sync".to_string());
        let items = vec![item(204, "Re-rodar"), linked];
        assert_eq!(
            item_for_branch(&items, "projects-sync").and_then(|item| item.number),
            Some(198)
        );
        assert_eq!(
            item_for_branch(&items, "feat/204-rerun-actions").and_then(|item| item.number),
            Some(204)
        );
        assert_eq!(item_for_branch(&items, "main"), None);
    }

    #[test]
    fn reads_and_ticks_the_checklist() {
        let body =
            "Contexto\n\n- [ ] Botão Re-rodar\n- [x] Só os jobs que falharam\n* [ ] Atualizar";
        let tasks = subtasks(body);
        assert_eq!(tasks.len(), 3);
        assert!(!tasks[0].done && tasks[1].done);
        let ticked = toggle_subtask(body, tasks[0].line, true);
        assert!(subtasks(&ticked)[0].done);
        assert_eq!(ticked.lines().count(), body.lines().count());
    }
}
