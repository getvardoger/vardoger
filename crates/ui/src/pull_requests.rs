//! The Pull requests page, after Shepherd's pull-requests extension (itself
//! t3code's view): every project repository's pull (GitHub) and merge
//! (GitLab) requests on one list, grouped Authored / Reviewing / Others, two
//! lines a row. A search with `label:`, `author:`, `repo:` and `is:`, a Sort
//! menu, a Filters menu whose rows open a submenu beside it, and Refresh.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use commutator_proto::change_request_assessment::CiState;
use commutator_proto::{
    ChangeRequestFilter, ChangeRequestListItem, ChangeRequestMergeability, ChangeRequestPage,
    ChangeRequestReviewDecision, ChangeRequestState, ChangeRequestStateFilter,
};
use commutator_rpc::{RpcError, capability_errors, methods};
use gpui::{
    AnyElement, Context, Entity, IntoElement, ListState, Render, SharedString, Subscription, Task,
    Window, div, list, prelude::*, px,
};
use serde::{Deserialize, Serialize};

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::icons::{self, icon};
use crate::popover;
use crate::settings::widgets;
use crate::state::AppState;
use crate::theme::Theme;

const PAGE_MAX_WIDTH: f32 = 1100.0;
const PAGE_PAD_X: f32 = 28.0;
/// A stored listing younger than this is shown as is; an older one is shown
/// and read again behind it.
const FRESH: Duration = Duration::from_secs(60);

// ---- the picks -----------------------------------------------------------

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Sort {
    #[default]
    Updated,
    Stale,
    Newest,
    Largest,
    Smallest,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Involvement {
    #[default]
    All,
    Reviewing,
    Authored,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DraftPick {
    #[default]
    All,
    Only,
    Hide,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ReviewPick {
    #[default]
    All,
    Approved,
    ChangesRequested,
    ReviewRequired,
    None,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ChecksPick {
    #[default]
    All,
    Passing,
    Failing,
    Pending,
}

/// The page's picks, the viewer's own, kept across launches.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PullRequestPicks {
    pub sort: Sort,
    pub state: ChangeRequestStateFilter,
    pub involvement: Involvement,
    /// A login; empty is anyone.
    pub author: String,
    /// Any of these; empty is any.
    pub labels: Vec<String>,
    pub draft: DraftPick,
    pub review: ReviewPick,
    pub checks: ChecksPick,
    /// A repository (`owner/repo`, lowercase); empty is every project.
    pub project: String,
}

impl PullRequestPicks {
    /// How many filters differ from their defaults, for the Filters badge.
    fn active(&self) -> usize {
        [
            self.state != ChangeRequestStateFilter::Open,
            self.involvement != Involvement::All,
            !self.author.is_empty(),
            !self.labels.is_empty(),
            self.draft != DraftPick::All,
            self.review != ReviewPick::All,
            self.checks != ChecksPick::All,
            !self.project.is_empty(),
        ]
        .into_iter()
        .filter(|on| *on)
        .count()
    }

    /// Every pick but the search, the state (the listing's own) and the
    /// involvement (which picks the groups).
    fn passes(&self, row: &Row) -> bool {
        let item = &row.item;
        (match self.draft {
            DraftPick::All => true,
            DraftPick::Only => item.is_draft,
            DraftPick::Hide => !item.is_draft,
        }) && (self.review == ReviewPick::All || review_of(item) == self.review)
            && (self.checks == ChecksPick::All || checks_of(item) == Some(self.checks))
            && (self.author.is_empty() || item.author.login == self.author)
            && (self.labels.is_empty() || item.labels.iter().any(|l| self.labels.contains(&l.name)))
            && (self.project.is_empty() || item.repository.eq_ignore_ascii_case(&self.project))
    }
}

fn review_of(item: &ChangeRequestListItem) -> ReviewPick {
    match item.review_decision {
        ChangeRequestReviewDecision::Approved => ReviewPick::Approved,
        ChangeRequestReviewDecision::ChangesRequested => ReviewPick::ChangesRequested,
        ChangeRequestReviewDecision::ReviewRequired => ReviewPick::ReviewRequired,
        ChangeRequestReviewDecision::Unknown => ReviewPick::None,
    }
}

fn checks_of(item: &ChangeRequestListItem) -> Option<ChecksPick> {
    match item.ci.state {
        CiState::Passed => Some(ChecksPick::Passing),
        CiState::Failed => Some(ChecksPick::Failing),
        CiState::Pending => Some(ChecksPick::Pending),
        _ => None,
    }
}

// ---- search ----------------------------------------------------------------

/// `label:x author:x repo:x is:draft|approved|mine|open|merged|closed`; the
/// rest is matched against the number, title, repository, branch and author.
fn matches(row: &Row, query: &str) -> bool {
    let item = &row.item;
    let mut text = Vec::new();
    for word in query.split_whitespace() {
        let word = word.to_lowercase();
        let ok = match word.split_once(':') {
            Some(("label", v)) if !v.is_empty() => item
                .labels
                .iter()
                .any(|l| l.name.to_lowercase().contains(v)),
            Some(("author", "me")) => row.authored(),
            Some(("author", v)) if !v.is_empty() => {
                format!("{} {}", item.author.login, item.author.name)
                    .to_lowercase()
                    .contains(v)
            }
            Some(("repo", v)) if !v.is_empty() => item.repository.to_lowercase().contains(v),
            Some(("is", v)) if !v.is_empty() => match v {
                "draft" => item.is_draft,
                "approved" => item.review_decision == ChangeRequestReviewDecision::Approved,
                "mine" => row.authored(),
                "open" => item.state == ChangeRequestState::Open,
                "merged" => item.state == ChangeRequestState::Merged,
                "closed" => item.state == ChangeRequestState::Closed,
                _ => false,
            },
            _ => {
                text.push(word);
                true
            }
        };
        if !ok {
            return false;
        }
    }
    let text = text.join(" ");
    text.is_empty()
        || format!(
            "#{} {} {} {} {}",
            item.number, item.title, item.repository, item.head_ref_name, item.author.login
        )
        .to_lowercase()
        .contains(&text)
}

fn sort_rows(rows: &mut [&Row], sort: Sort) {
    let size = |r: &Row| r.item.additions + r.item.deletions;
    match sort {
        Sort::Updated => rows.sort_by(|a, b| b.item.updated_at.cmp(&a.item.updated_at)),
        Sort::Stale => rows.sort_by(|a, b| a.item.updated_at.cmp(&b.item.updated_at)),
        Sort::Newest => rows.sort_by(|a, b| b.item.created_at.cmp(&a.item.created_at)),
        Sort::Largest => rows.sort_by(|a, b| size(b).cmp(&size(a))),
        Sort::Smallest => rows.sort_by(|a, b| size(a).cmp(&size(b))),
    }
}

// ---- the listing -----------------------------------------------------------

/// One pull request and the device that read it (where it opens).
#[derive(Debug, Clone, PartialEq)]
struct Row {
    item: ChangeRequestListItem,
    device: String,
}

impl Row {
    fn authored(&self) -> bool {
        self.item.viewer_did_author == Some(true)
    }

    fn group(&self) -> Group {
        if self.authored() {
            Group::Authored
        } else if self.item.viewer_review_requested == Some(true) {
            Group::Reviewing
        } else {
            Group::Others
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Group {
    Authored,
    Reviewing,
    Others,
}

impl Group {
    const ALL: [Group; 3] = [Group::Authored, Group::Reviewing, Group::Others];

    fn label(self) -> &'static str {
        match self {
            Group::Authored => "Authored",
            Group::Reviewing => "Reviewing",
            Group::Others => "Others",
        }
    }

    fn glyph(self) -> &'static str {
        match self {
            Group::Authored => icons::LUCIDE_PEN_LINE,
            Group::Reviewing => icons::EYE,
            Group::Others => icons::LUCIDE_USERS,
        }
    }
}

/// A repository asked for its pull requests: a GitHub one by name, any other
/// (GitLab) by one of its checkouts, which its device resolves.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Source {
    device: String,
    /// `None` for this device; otherwise the relay forwards to it.
    target: Option<String>,
    /// The repository by name, or a checkout's folder.
    repository: Option<String>,
    cwd: Option<String>,
    label: String,
}

impl Source {
    fn key(&self) -> String {
        format!("{}\n{:?}\n{:?}", self.device, self.repository, self.cwd)
    }
}

/// Every git project on an online device, one per repository.
fn sources(state: &AppState) -> Vec<Source> {
    let now = Utc::now();
    let local = state.local_device_id.clone().or_else(|| {
        state
            .engine()
            .map(|engine| engine.engine_info().device_id.clone())
    });
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for space in &state.spaces {
        if !space.git_detected || !state.device_online(&space.device_id, now) {
            continue;
        }
        let key = match (&space.github_repo, &space.repository_id) {
            (Some(repo), _) => repo.to_ascii_lowercase(),
            (None, Some(group)) => format!("group:{group}"),
            (None, None) => format!("path:{}:{}", space.device_id, space.path),
        };
        if !seen.insert(key) {
            continue;
        }
        let target = (Some(&space.device_id) != local.as_ref()).then(|| space.device_id.clone());
        out.push(Source {
            device: space.device_id.clone(),
            target,
            repository: space.github_repo.clone(),
            cwd: space.github_repo.is_none().then(|| space.path.clone()),
            label: space.github_repo.clone().unwrap_or_else(|| {
                space
                    .path
                    .rsplit('/')
                    .next()
                    .unwrap_or(&space.path)
                    .to_owned()
            }),
        });
    }
    out
}

#[derive(Debug, Clone, PartialEq)]
struct Failure {
    repository: String,
    message: String,
}

/// Why a repository couldn't be read, in words.
fn failure_text(error: &RpcError) -> String {
    use capability_errors as codes;
    match error {
        RpcError::Capability(c) if c == codes::PULL_REQUESTS_CLI_UNAVAILABLE => {
            "Install the GitHub CLI (gh) or GitLab CLI (glab) on its device".into()
        }
        RpcError::Capability(c) if c == codes::PULL_REQUESTS_AUTHENTICATION => {
            "Sign in with gh auth login (or glab auth login) on its device".into()
        }
        RpcError::Capability(c) if c == codes::PULL_REQUESTS_RATE_LIMITED => {
            "Rate limited; try again in a few minutes".into()
        }
        RpcError::Capability(c) if c == codes::PULL_REQUESTS_TIMEOUT => "Timed out".into(),
        RpcError::UnknownMethod(_) => "Update Commutator on its device".into(),
        _ => "Couldn’t be read".into(),
    }
}

/// A checkout on neither GitHub nor GitLab (or an engine predating `cwd`):
/// nothing to list, not a failure.
fn silent(source: &Source, error: &RpcError) -> bool {
    let text = error.to_string();
    source.cwd.is_some()
        && (text.contains(capability_errors::PULL_REQUESTS_UNSUPPORTED)
            || text.contains("repository must be"))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Read {
    /// The engines' stored copies, whatever their age.
    Stored,
    /// The engines' in-memory answers (five minutes) or the provider.
    Fresh,
    /// The provider, always.
    Refresh,
}

// ---- menus -----------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FilterKey {
    State,
    Involvement,
    Author,
    Labels,
    Draft,
    Review,
    Checks,
    Project,
}

type Choice<T> = (T, &'static str, &'static str);

const STATES: [Choice<ChangeRequestStateFilter>; 4] = [
    (
        ChangeRequestStateFilter::Open,
        "Open",
        icons::LUCIDE_GIT_PULL_REQUEST_ARROW,
    ),
    (
        ChangeRequestStateFilter::Merged,
        "Merged",
        icons::LUCIDE_GIT_MERGE,
    ),
    (
        ChangeRequestStateFilter::Closed,
        "Closed",
        icons::LUCIDE_GIT_PULL_REQUEST_CLOSED,
    ),
    (ChangeRequestStateFilter::All, "All", icons::LUCIDE_LAYERS),
];
const INVOLVEMENTS: [Choice<Involvement>; 3] = [
    (Involvement::All, "All", icons::LUCIDE_LAYERS),
    (Involvement::Reviewing, "Reviewing", icons::EYE),
    (Involvement::Authored, "Authored", icons::LUCIDE_PEN_LINE),
];
const DRAFTS: [Choice<DraftPick>; 3] = [
    (DraftPick::All, "All", icons::LUCIDE_LAYERS),
    (
        DraftPick::Only,
        "Drafts only",
        icons::LUCIDE_GIT_PULL_REQUEST_DRAFT,
    ),
    (DraftPick::Hide, "Hide drafts", icons::LUCIDE_EYE_OFF),
];
const REVIEWS: [Choice<ReviewPick>; 5] = [
    (ReviewPick::All, "All", icons::LUCIDE_LAYERS),
    (ReviewPick::Approved, "Approved", icons::LUCIDE_CIRCLE_CHECK),
    (
        ReviewPick::ChangesRequested,
        "Changes requested",
        icons::LUCIDE_CIRCLE_X,
    ),
    (
        ReviewPick::ReviewRequired,
        "Review required",
        icons::LUCIDE_CIRCLE_DASHED,
    ),
    (ReviewPick::None, "No reviews", icons::LUCIDE_CIRCLE_SLASH),
];
const CHECKS: [Choice<ChecksPick>; 4] = [
    (ChecksPick::All, "All", icons::LUCIDE_LAYERS),
    (ChecksPick::Passing, "Passing", icons::LUCIDE_CIRCLE_CHECK),
    (ChecksPick::Failing, "Failing", icons::LUCIDE_CIRCLE_X),
    (ChecksPick::Pending, "Pending", icons::LUCIDE_CIRCLE_DOT),
];
const SORTS: [Choice<Sort>; 5] = [
    (Sort::Updated, "Recently updated", icons::CLOCK_CIRCLE),
    (
        Sort::Stale,
        "Least recently updated",
        icons::LUCIDE_CALENDAR_ARROW_UP,
    ),
    (Sort::Newest, "Newest", icons::LUCIDE_CALENDAR_ARROW_DOWN),
    (Sort::Largest, "Largest", icons::LUCIDE_MAXIMIZE_2),
    (Sort::Smallest, "Smallest", icons::LUCIDE_MINIMIZE_2),
];

fn choice<T: PartialEq + Copy>(choices: &[Choice<T>], value: T) -> Choice<T> {
    *choices.iter().find(|c| c.0 == value).unwrap_or(&choices[0])
}

// ---- the page --------------------------------------------------------------

/// One line of the list: a group's header or a pull request.
#[derive(Clone)]
enum Line {
    Header(Group, usize, bool),
    Item(usize),
}

pub struct PullRequestsPage {
    state: Entity<AppState>,
    picks: PullRequestPicks,
    rows: Vec<Row>,
    failures: Vec<Failure>,
    /// The sources the listing was read from, to read again when they change.
    read_from: Vec<String>,
    loaded_at: Option<Instant>,
    loading: bool,
    /// Some answer has arrived since the page opened.
    loaded: bool,
    generation: u64,
    task: Option<Task<()>>,
    visible: bool,
    search: Entity<ComposerInput>,
    query: String,
    /// The rows shown, in order, and the lines the list draws.
    shown: Vec<usize>,
    lines: Vec<Line>,
    list: ListState,
    selected_url: Option<String>,
    sort_menu: popover::Popup<()>,
    filter_menu: popover::Popup<()>,
    /// The filter pulled onto the bar whose dropdown is open.
    quick_menu: popover::Popup<FilterKey>,
    /// The bar's width at the last layout, which decides how many filters
    /// sit on it.
    bar_width: Option<f32>,
    /// The filter whose submenu shows beside the Filters card.
    opened: Option<FilterKey>,
    /// The Author and Labels submenus' search.
    sub_search: Entity<ComposerInput>,
    sub_query: String,
    _subscriptions: Vec<Subscription>,
}

impl PullRequestsPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| {
            ComposerInput::with_context(
                "Search pull requests, or label:bug author:me repo:app is:draft",
                "PaletteSearch",
                cx,
            )
            .with_text_metrics(13.0, 18.0)
            .with_accessibility_role(gpui::Role::SearchInput)
            .with_single_line()
            .with_tab_stop()
        });
        let sub_search = cx.new(|cx| {
            ComposerInput::with_context("Search", "PaletteSearch", cx)
                .with_text_metrics(12.0, 16.0)
                .with_single_line()
        });
        let subscriptions = vec![
            cx.subscribe(&search, |page: &mut Self, input, event, cx| {
                if matches!(event, ComposerInputEvent::Edited) {
                    page.query = input.read(cx).text().to_string();
                    page.rebuild();
                    cx.notify();
                }
            }),
            cx.subscribe(&sub_search, |page: &mut Self, input, event, cx| {
                if matches!(event, ComposerInputEvent::Edited) {
                    page.sub_query = input.read(cx).text().to_lowercase();
                    cx.notify();
                }
            }),
            // A device coming online, or a project added, has more to list.
            cx.observe(&state, |page, state, cx| {
                if page.visible && page.read_from != keys(&sources(state.read(cx))) {
                    page.load(Read::Stored, cx);
                }
            }),
        ];
        let picks = crate::settings::current(cx).pull_request_picks;
        Self {
            state,
            picks,
            rows: Vec::new(),
            failures: Vec::new(),
            read_from: Vec::new(),
            loaded_at: None,
            loading: false,
            loaded: false,
            generation: 0,
            task: None,
            visible: false,
            search,
            query: String::new(),
            shown: Vec::new(),
            lines: Vec::new(),
            list: ListState::new(0, gpui::ListAlignment::Top, px(600.0)),
            selected_url: None,
            sort_menu: popover::Popup::default(),
            filter_menu: popover::Popup::default(),
            quick_menu: popover::Popup::default(),
            bar_width: None,
            opened: None,
            sub_search,
            sub_query: String::new(),
            _subscriptions: subscriptions,
        }
    }

    /// The pull request as the list last read it, and the device it came from.
    pub(crate) fn preview(&self, url: &str) -> Option<(ChangeRequestListItem, String)> {
        self.rows
            .iter()
            .find(|row| row.item.url == url)
            .map(|row| (row.item.clone(), row.device.clone()))
    }

    pub(crate) fn select_url(&mut self, url: Option<String>, cx: &mut Context<Self>) {
        self.selected_url = url;
        cx.notify();
    }

    pub fn on_visible(&mut self, cx: &mut Context<Self>) {
        self.visible = true;
        match self.loaded_at {
            None => self.load(Read::Stored, cx),
            Some(at) if at.elapsed() > FRESH => self.load(Read::Fresh, cx),
            Some(_) => {}
        }
    }

    pub fn on_hidden(&mut self) {
        self.visible = false;
        self.sort_menu = popover::Popup::default();
        self.filter_menu = popover::Popup::default();
        self.opened = None;
    }

    fn set_picks(&mut self, change: impl FnOnce(&mut PullRequestPicks), cx: &mut Context<Self>) {
        let state = self.picks.state;
        change(&mut self.picks);
        let picks = self.picks.clone();
        crate::settings::update(crate::settings::SavePolicy::Debounced, cx, |s| {
            s.pull_request_picks = picks;
        });
        if self.picks.state != state {
            self.rows.clear();
            self.load(Read::Stored, cx);
        }
        self.rebuild();
        cx.notify();
    }

    /// Ask every source for its pull requests in the picked state: all of
    /// them, and the viewer's own and review requests (which a busy
    /// repository's newest fifty may not reach).
    fn load(&mut self, read: Read, cx: &mut Context<Self>) {
        let state = self.state.read(cx);
        let Some(engine) = state.engine().cloned() else {
            return;
        };
        let sources = sources(state);
        self.read_from = keys(&sources);
        self.generation += 1;
        let generation = self.generation;
        self.loading = true;
        let picked = self.picks.state;
        let calls: Vec<_> = sources
            .iter()
            .flat_map(|source| {
                [
                    ChangeRequestFilter::All,
                    ChangeRequestFilter::Authored,
                    ChangeRequestFilter::Reviewing,
                ]
                .map(|filter| (source.clone(), filter))
            })
            .map(|(source, filter)| {
                let mut params = serde_json::json!({
                    "filter": filter,
                    "state": picked,
                    "refresh": read == Read::Refresh,
                    "cached": read == Read::Stored,
                });
                if let Some(target) = &source.target {
                    params["targetDeviceId"] = target.as_str().into();
                }
                match (&source.repository, &source.cwd) {
                    (Some(repo), _) => params["repository"] = repo.as_str().into(),
                    (None, Some(cwd)) => params["cwd"] = cwd.as_str().into(),
                    _ => {}
                }
                let engine = engine.clone();
                async move {
                    let page = engine
                        .client()
                        .call(methods::LIST_CHANGE_REQUEST_PAGE, params)
                        .await
                        .and_then(|value| {
                            serde_json::from_value::<ChangeRequestPage>(value)
                                .map_err(|e| RpcError::Failed(e.to_string()))
                        });
                    (source, page)
                }
            })
            .collect();
        self.task = Some(cx.spawn(async move |page, cx| {
            let answers = futures::future::join_all(calls).await;
            let stale = answers.iter().any(|(_, page)| {
                page.as_ref().is_ok_and(|p| {
                    p.fetched_at.is_none_or(|at| {
                        Utc::now()
                            .signed_duration_since(at)
                            .to_std()
                            .unwrap_or_default()
                            > FRESH
                    })
                })
            });
            let (rows, failures) = merge(answers);
            let _ = page.update(cx, |page, cx| {
                if page.generation != generation {
                    return;
                }
                page.rows = rows;
                page.failures = failures;
                page.loading = false;
                page.loaded = true;
                page.loaded_at = Some(Instant::now());
                page.rebuild();
                if read == Read::Stored && stale {
                    page.load(Read::Fresh, cx);
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// The shown rows and the list's lines, from the rows, picks and search.
    fn rebuild(&mut self) {
        let picks = &self.picks;
        let mut shown: Vec<&Row> = self
            .rows
            .iter()
            .filter(|row| picks.passes(row) && matches(row, &self.query))
            .collect();
        sort_rows(&mut shown, picks.sort);
        let wanted: &[Group] = match picks.involvement {
            Involvement::All => &Group::ALL,
            Involvement::Authored => &[Group::Authored],
            Involvement::Reviewing => &[Group::Reviewing],
        };
        let index: HashMap<&str, usize> = self
            .rows
            .iter()
            .enumerate()
            .map(|(i, r)| (r.item.url.as_str(), i))
            .collect();
        let mut order = Vec::new();
        let mut lines = Vec::new();
        for group in wanted {
            let members: Vec<usize> = shown
                .iter()
                .filter(|row| row.group() == *group)
                .map(|row| index[row.item.url.as_str()])
                .collect();
            if members.is_empty() {
                continue;
            }
            lines.push(Line::Header(*group, members.len(), lines.is_empty()));
            for i in members {
                lines.push(Line::Item(order.len()));
                order.push(i);
            }
        }
        self.shown = order;
        self.lines = lines;
        self.list.reset(self.lines.len());
    }

    fn open(&self, row: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.rows.get(row) else {
            return;
        };
        window.dispatch_action(
            Box::new(crate::pull_request_detail::OpenPullRequest(
                row.item.url.clone(),
                Some(row.device.clone()),
            )),
            cx,
        );
    }
}

#[cfg(feature = "pull-request-fixture")]
impl PullRequestsPage {
    /// Open `sort`, `filters`, or `filters:<n>` with its n-th row's submenu.
    pub fn fixture_open(&mut self, menu: &str, cx: &mut Context<Self>) {
        self.close_menus(cx);
        self.sort_menu = popover::Popup::default();
        self.filter_menu = popover::Popup::default();
        match menu.split_once(':') {
            _ if menu == "sort" => self.sort_menu.open(()),
            Some(("filters", n)) => {
                self.filter_menu.open(());
                self.opened = [
                    FilterKey::State,
                    FilterKey::Involvement,
                    FilterKey::Author,
                    FilterKey::Labels,
                    FilterKey::Draft,
                    FilterKey::Review,
                    FilterKey::Checks,
                    FilterKey::Project,
                ]
                .get(n.parse::<usize>().unwrap_or(0))
                .copied();
            }
            _ if menu == "filters" => self.filter_menu.open(()),
            _ => {}
        }
        cx.notify();
    }

    pub fn fixture_search(&mut self, query: &str, cx: &mut Context<Self>) {
        self.search
            .update(cx, |input, cx| input.set_text(query, cx));
        self.query = query.to_owned();
        self.rebuild();
        cx.notify();
    }
}

fn keys(sources: &[Source]) -> Vec<String> {
    sources.iter().map(Source::key).collect()
}

/// One row per pull request from every answer; the viewer's relationships
/// from whichever answer knew them.
fn merge(answers: Vec<(Source, Result<ChangeRequestPage, RpcError>)>) -> (Vec<Row>, Vec<Failure>) {
    let mut rows: Vec<Row> = Vec::new();
    let mut at: HashMap<String, usize> = HashMap::new();
    let mut failures: Vec<Failure> = Vec::new();
    for (source, page) in answers {
        match page {
            Ok(page) => {
                for item in page.items {
                    match at.get(&item.url) {
                        Some(&i) => {
                            let known = &mut rows[i].item;
                            known.viewer_did_author =
                                or_true(known.viewer_did_author, item.viewer_did_author);
                            known.viewer_review_requested = or_true(
                                known.viewer_review_requested,
                                item.viewer_review_requested,
                            );
                        }
                        None => {
                            at.insert(item.url.clone(), rows.len());
                            rows.push(Row {
                                item,
                                device: source.device.clone(),
                            });
                        }
                    }
                }
            }
            Err(error) if silent(&source, &error) => {}
            Err(error) => {
                if !failures.iter().any(|f| f.repository == source.label) {
                    failures.push(Failure {
                        repository: source.label.clone(),
                        message: failure_text(&error),
                    });
                }
            }
        }
    }
    (rows, failures)
}

fn or_true(a: Option<bool>, b: Option<bool>) -> Option<bool> {
    match (a, b) {
        (Some(true), _) | (_, Some(true)) => Some(true),
        (Some(false), _) | (_, Some(false)) => Some(false),
        _ => None,
    }
}

// ---- drawing ---------------------------------------------------------------

/// `1234567` as `1,234,567`.
pub(crate) fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// "5m ago", "3h ago", "2d ago", "1y ago".
pub(crate) fn ago(at: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let s = now.signed_duration_since(at).num_seconds().max(0);
    match s {
        s if s < 3_600 => format!("{}m ago", (s / 60).max(1)),
        s if s < 86_400 => format!("{}h ago", s / 3_600),
        s if s < 86_400 * 365 => format!("{}d ago", s / 86_400),
        s => format!("{}y ago", s / 86_400 / 365),
    }
}

/// GitHub's merged violet (t3code's violet-600 / violet-300), not the accent.
pub(crate) fn merged_color(theme: &Theme) -> gpui::Hsla {
    if theme.appearance.is_dark() {
        gpui::rgba(0xc4b5fde6).into()
    } else {
        gpui::rgb(0x7c3aed).into()
    }
}

/// A pull request's state glyph and its color: open, draft, merged, closed.
pub(crate) fn state_glyph(
    state: ChangeRequestState,
    draft: bool,
    theme: &Theme,
) -> (&'static str, gpui::Hsla, &'static str) {
    match state {
        ChangeRequestState::Merged => (icons::LUCIDE_GIT_MERGE, merged_color(theme), "Merged"),
        ChangeRequestState::Closed => (
            icons::LUCIDE_GIT_PULL_REQUEST_CLOSED,
            theme.danger,
            "Closed",
        ),
        ChangeRequestState::Open if draft => (
            icons::LUCIDE_GIT_PULL_REQUEST_DRAFT,
            theme.text_faint,
            "Draft",
        ),
        ChangeRequestState::Open => (icons::LUCIDE_GIT_PULL_REQUEST_ARROW, theme.success, "Open"),
    }
}

/// The state glyph; a conflict lays a red triangle over its corner.
pub(crate) fn glyph(
    state: ChangeRequestState,
    draft: bool,
    conflict: bool,
    size: f32,
    theme: &Theme,
) -> AnyElement {
    let (path, color, _) = state_glyph(state, draft, theme);
    let mark = size * 0.75;
    div()
        .relative()
        .flex_none()
        .size(px(size))
        .child(icon(path).size(px(size)).text_color(color))
        .when(conflict && state == ChangeRequestState::Open, |el| {
            el.child(
                div()
                    .absolute()
                    .right(px(-mark * 0.4))
                    .bottom(px(-mark * 0.4))
                    .size(px(mark))
                    .child(
                        icon(icons::LUCIDE_TRIANGLE_SOLID)
                            .absolute()
                            .size(px(mark))
                            .text_color(theme.bg),
                    )
                    .child(
                        icon(icons::LUCIDE_TRIANGLE_ALERT)
                            .absolute()
                            .size(px(mark))
                            .text_color(theme.danger),
                    ),
            )
        })
        .into_any_element()
}

/// A person's avatar, or their initial while it loads or when it can't.
pub(crate) fn avatar(url: Option<String>, login: &str, size: f32, theme: &Theme) -> AnyElement {
    let initial: SharedString = login
        .chars()
        .next()
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or("?".into())
        .into();
    let (wash, ink) = (theme.element_hover, theme.text_muted);
    let letter = move || {
        div()
            .size(px(size))
            .flex_none()
            .rounded_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(wash)
            .text_color(ink)
            .text_size(px((size * 0.6).round()))
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .child(initial.clone())
            .into_any_element()
    };
    match url.filter(|url| url.starts_with("https://")) {
        None => letter(),
        Some(url) => gpui::img(SharedString::from(url))
            .size(px(size))
            .flex_none()
            .rounded_full()
            .object_fit(gpui::ObjectFit::Cover)
            .with_loading(letter.clone())
            .with_fallback(letter)
            .into_any_element(),
    }
}

/// A label chip in its provider colour, as t3code's: a faint wash of the
/// colour with the name in it, lifted (or darkened) to read on the theme.
/// A label without a colour falls back to the neutral chip.
pub(crate) fn chip(label: &commutator_proto::ChangeRequestLabel, theme: &Theme) -> gpui::Div {
    let (bg, fg) = match label_hsla(&label.color) {
        Some(c) => {
            let l = if theme.appearance.is_dark() { c.l.max(0.72) } else { c.l.min(0.38) };
            (c.alpha(0.16), gpui::Hsla { l, a: 1.0, ..c })
        }
        None => (theme.element_hover, theme.text_muted),
    };
    div()
        .flex_none()
        .max_w(px(180.0))
        .h(px(18.0))
        .px(px(6.0))
        .flex()
        .items_center()
        .rounded(px(4.0))
        .bg(bg)
        .text_color(fg)
        .text_size(px(11.0))
        .font_weight(gpui::FontWeight::MEDIUM)
        .truncate()
        .child(SharedString::from(label.name.clone()))
}

/// GitHub sends `d73a4a`, GitLab `#428BCA`.
fn label_hsla(color: &str) -> Option<gpui::Hsla> {
    let hex = color.trim().trim_start_matches('#');
    if hex.len() != 6 {
        return None;
    }
    u32::from_str_radix(hex, 16).ok().map(|v| gpui::rgb(v).into())
}

pub(crate) fn provider_logo(provider: &str, theme: &Theme) -> AnyElement {
    if provider == "gitlab" {
        icon(icons::GITLAB_MARK)
            .size(px(12.0))
            .flex_none()
            .text_color(gpui::rgb(0xe24329))
            .into_any_element()
    } else {
        icon(icons::GITHUB_MARK)
            .size(px(12.0))
            .flex_none()
            .text_color(theme.text)
            .into_any_element()
    }
}

const SEARCH_MIN: f32 = 200.0;

/// The filters that fit on the bar, in its order. The rest stay in Filters:
/// State goes first, Project last.
fn quick_keys(picks: &PullRequestPicks, width: f32) -> Vec<FilterKey> {
    // Search, Sort, Filters (with its count), Refresh and their gaps.
    let fixed = SEARCH_MIN + 80.0 + 110.0 + 30.0 + 3.0 * 8.0;
    // ponytail: estimated from the label's length, not shaped text.
    let chip = |label: &str| 8.0 + 54.0 + label.chars().count().min(22) as f32 * 7.2;
    let state = choice(&STATES, picks.state).1.to_owned();
    let project = if picks.project.is_empty() { "All projects".into() } else { picks.project.clone() };
    let author = if picks.author.is_empty() { "Anyone".into() } else { picks.author.clone() };
    let mut room = width - fixed;
    let mut fits = |label: &str| {
        let w = chip(label);
        (room >= w).then(|| room -= w).is_some()
    };
    let (project, author, state) = (fits(&project), fits(&author), fits(&state));
    [(FilterKey::State, state), (FilterKey::Project, project), (FilterKey::Author, author)]
        .into_iter()
        .filter_map(|(key, on)| on.then_some(key))
        .collect()
}

/// A bar button: an icon and maybe a label, as Shepherd's `secondary`.
fn bar_button(
    id: &'static str,
    glyph: &'static str,
    label: Option<&'static str>,
    open: bool,
    theme: &Theme,
) -> gpui::Stateful<gpui::Div> {
    let hover = theme.element_hover;
    div()
        .id(id)
        .h(px(30.0))
        .flex_none()
        .px(px(if label.is_some() { 10.0 } else { 0.0 }))
        .when(label.is_none(), |el| el.w(px(30.0)).justify_center())
        .flex()
        .items_center()
        .gap(px(6.0))
        .rounded(px(8.0))
        .border_1()
        .border_color(theme.border)
        .text_size(px(12.5))
        .text_color(theme.text)
        .cursor_pointer()
        .when(open, |el| el.bg(hover))
        .hover(move |s| s.bg(hover))
        .child(icon(glyph).size(px(14.0)).text_color(theme.text_muted))
        .children(label)
}

fn lead(glyph: &'static str, theme: &Theme) -> Option<AnyElement> {
    Some(
        icon(glyph)
            .size(px(14.0))
            .flex_none()
            .text_color(theme.text_muted)
            .into_any_element(),
    )
}

/// A menu row: its icon, its name, a value or a check, maybe a chevron.
fn menu_item(
    id: impl Into<SharedString>,
    lead: Option<AnyElement>,
    name: impl Into<SharedString>,
    value: Option<SharedString>,
    on: bool,
    chevron: bool,
    theme: &Theme,
) -> gpui::Stateful<gpui::Div> {
    let id: SharedString = id.into();
    let hover = theme.element_hover;
    div()
        .id(gpui::ElementId::Name(id))
        .h(px(28.0))
        .px(px(8.0))
        .flex()
        .items_center()
        .gap(px(8.0))
        .rounded(px(6.0))
        .text_size(px(13.0))
        .text_color(theme.text)
        .cursor_pointer()
        .hover(move |s| s.bg(hover))
        .children(lead)
        .child(div().flex_1().min_w_0().truncate().child(name.into()))
        .children(value.map(|v| {
            div()
                .flex_none()
                .max_w(px(140.0))
                .truncate()
                .text_color(theme.text_muted)
                .child(v)
        }))
        .when(on, |el| {
            el.child(
                icon(icons::CHECK)
                    .size(px(13.0))
                    .flex_none()
                    .text_color(theme.text_muted),
            )
        })
        .when(chevron, |el| {
            el.child(
                icon(icons::LUCIDE_CHEVRON_RIGHT)
                    .size(px(13.0))
                    .flex_none()
                    .text_color(theme.text_muted),
            )
        })
}

impl PullRequestsPage {
    fn close_menus(&mut self, cx: &mut Context<Self>) {
        if self.sort_menu.begin_close() {
            popover::reap_popup(cx, |page: &mut Self| &mut page.sort_menu);
        }
        if self.filter_menu.begin_close() {
            popover::reap_popup(cx, |page: &mut Self| &mut page.filter_menu);
        }
        if self.quick_menu.begin_close() {
            popover::reap_popup(cx, |page: &mut Self| &mut page.quick_menu);
        }
        self.opened = None;
        cx.notify();
    }

    fn render_sort(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let mut trigger = bar_button(
            "pr-sort",
            icons::LUCIDE_ARROW_DOWN_UP,
            Some("Sort"),
            self.sort_menu.is_open(),
            theme,
        )
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(|page, _, _, _| page.sort_menu.note_trigger_press()),
        )
        .on_click(cx.listener(|page, _, _, cx| {
            cx.stop_propagation();
            if page.sort_menu.take_press_was_open() {
                page.close_menus(cx);
            } else {
                page.close_menus(cx);
                page.sort_menu.open(());
            }
            cx.notify();
        }));
        if self.sort_menu.get().is_some() {
            let menu = popover::popover_card(theme)
                .w(px(240.0))
                .flex()
                .flex_col()
                .on_mouse_down_out(cx.listener(|page, _, _, cx| page.close_menus(cx)))
                .children(SORTS.iter().map(|&(sort, label, glyph)| {
                    menu_item(
                        format!("pr-sort-{sort:?}"),
                        lead(glyph, theme),
                        label,
                        None,
                        self.picks.sort == sort,
                        false,
                        theme,
                    )
                    .on_click(cx.listener(move |page, _, _, cx| {
                        cx.stop_propagation();
                        page.close_menus(cx);
                        page.set_picks(|p| p.sort = sort, cx);
                    }))
                }));
            trigger = trigger.child(popover::anchored_menu_below_end(
                "pr-sort-menu",
                menu.into_any_element(),
                self.sort_menu.closing_since(),
            ));
        }
        trigger.into_any_element()
    }

    /// A filter changed often enough to sit on the bar: its value, and the
    /// Filters card's own submenu as a dropdown.
    fn render_quick(&mut self, key: FilterKey, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let p = &self.picks;
        let (value, glyph, set) = match key {
            FilterKey::State => {
                let (_, label, glyph) = choice(&STATES, p.state);
                (label.to_owned(), glyph, p.state != ChangeRequestStateFilter::Open)
            }
            FilterKey::Project if p.project.is_empty() => ("All projects".to_owned(), icons::LUCIDE_FOLDER_GIT_2, false),
            FilterKey::Project => (p.project.clone(), icons::LUCIDE_FOLDER_GIT_2, true),
            FilterKey::Author if p.author.is_empty() => ("Anyone".to_owned(), icons::LUCIDE_USER_ROUND, false),
            FilterKey::Author => (p.author.clone(), icons::LUCIDE_USER_ROUND, true),
            _ => return div().into_any_element(),
        };
        let open = self.quick_menu.get() == Some(&key) && self.quick_menu.is_open();
        let hover = theme.element_hover;
        let mut trigger = div()
            .id(SharedString::from(format!("pr-quick-{key:?}")))
            .debug_selector(move || format!("pr-quick-{key:?}"))
            .h(px(30.0))
            .flex_none()
            .px(px(10.0))
            .flex()
            .items_center()
            .gap(px(6.0))
            .rounded(px(8.0))
            .border_1()
            .border_color(if set { theme.accent.opacity(0.5) } else { theme.border })
            .text_size(px(12.5))
            .text_color(theme.text)
            .cursor_pointer()
            .when(open, |el| el.bg(hover))
            .hover(move |s| s.bg(hover))
            .child(icon(glyph).size(px(14.0)).text_color(theme.text_muted))
            .child(div().max_w(px(160.0)).truncate().child(value))
            .child(icon(icons::ALT_ARROW_DOWN).size(px(12.0)).text_color(theme.text_muted))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |page, _, _, _| page.quick_menu.note_trigger_press_matching(|k| *k == key)),
            )
            .on_click(cx.listener(move |page, _, window, cx| {
                cx.stop_propagation();
                let was_open = page.quick_menu.take_press_was_open();
                page.close_menus(cx);
                if !was_open {
                    page.quick_menu.open(key);
                    page.open_sub(key, window, cx);
                    // The Filters card's submenu stays shut.
                    page.opened = None;
                }
                cx.notify();
            }));
        if self.quick_menu.get() == Some(&key) {
            let sub = div()
                .on_mouse_down_out(cx.listener(|page, _, _, cx| page.close_menus(cx)))
                .child(self.render_sub(key, theme, cx));
            trigger = trigger.child(popover::anchored_menu_below(
                format!("pr-quick-menu-{key:?}"),
                sub.into_any_element(),
                self.quick_menu.closing_since(),
            ));
        }
        trigger.into_any_element()
    }

    fn render_filters(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let active = self.picks.active();
        let mut trigger = bar_button(
            "pr-filters",
            icons::LUCIDE_LIST_FILTER,
            Some("Filters"),
            self.filter_menu.is_open(),
            theme,
        )
        .when(active > 0, |el| {
            el.child(
                div()
                    .min_w(px(16.0))
                    .h(px(16.0))
                    .px(px(4.0))
                    .rounded(px(8.0))
                    .bg(theme.accent)
                    .text_color(theme.on_accent)
                    .text_size(px(10.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(SharedString::from(active.to_string())),
            )
        })
        .on_mouse_down(
            gpui::MouseButton::Left,
            cx.listener(|page, _, _, _| page.filter_menu.note_trigger_press()),
        )
        .on_click(cx.listener(|page, _, _, cx| {
            cx.stop_propagation();
            if page.filter_menu.take_press_was_open() {
                page.close_menus(cx);
            } else {
                page.close_menus(cx);
                page.filter_menu.open(());
            }
            cx.notify();
        }));
        if self.filter_menu.get().is_some() {
            let p = &self.picks;
            let project = if p.project.is_empty() {
                "All projects".to_owned()
            } else {
                p.project.clone()
            };
            let rows: [Option<(FilterKey, &'static str, String, &'static str)>; 11] = [
                Some((
                    FilterKey::State,
                    "State",
                    choice(&STATES, p.state).1.into(),
                    choice(&STATES, p.state).2,
                )),
                Some((
                    FilterKey::Involvement,
                    "Involvement",
                    choice(&INVOLVEMENTS, p.involvement).1.into(),
                    choice(&INVOLVEMENTS, p.involvement).2,
                )),
                None,
                Some((
                    FilterKey::Author,
                    "Author",
                    if p.author.is_empty() {
                        "Anyone".into()
                    } else {
                        p.author.clone()
                    },
                    icons::LUCIDE_USER_ROUND,
                )),
                Some((
                    FilterKey::Labels,
                    "Labels",
                    if p.labels.is_empty() {
                        "Any".into()
                    } else {
                        p.labels.join(", ")
                    },
                    icons::TAG,
                )),
                Some((
                    FilterKey::Draft,
                    "Draft",
                    choice(&DRAFTS, p.draft).1.into(),
                    choice(&DRAFTS, p.draft).2,
                )),
                Some((
                    FilterKey::Review,
                    "Review",
                    choice(&REVIEWS, p.review).1.into(),
                    choice(&REVIEWS, p.review).2,
                )),
                Some((
                    FilterKey::Checks,
                    "Checks",
                    choice(&CHECKS, p.checks).1.into(),
                    choice(&CHECKS, p.checks).2,
                )),
                None,
                Some((
                    FilterKey::Project,
                    "Project",
                    project,
                    if p.project.is_empty() {
                        icons::LUCIDE_LAYERS
                    } else {
                        icons::LUCIDE_FOLDER_GIT_2
                    },
                )),
                None,
            ];
            let mut card = popover::popover_card(theme)
                .w(px(260.0))
                .flex()
                .flex_col()
                .on_mouse_down_out(cx.listener(|page, _, _, cx| page.close_menus(cx)));
            for row in rows.into_iter().take(10) {
                let Some((key, name, value, glyph)) = row else {
                    card = card.child(popover::menu_separator());
                    continue;
                };
                let open = self.opened == Some(key);
                let mut item = menu_item(
                    format!("pr-filter-{key:?}"),
                    lead(glyph, theme),
                    name,
                    Some(value.into()),
                    false,
                    true,
                    theme,
                )
                .relative()
                .when(open, |el| el.bg(theme.element_hover))
                .on_hover(cx.listener(move |page, hovered: &bool, window, cx| {
                    if *hovered && page.opened != Some(key) {
                        page.open_sub(key, window, cx);
                    }
                }))
                .on_click(cx.listener(move |page, _, window, cx| {
                    cx.stop_propagation();
                    page.open_sub(key, window, cx);
                }));
                if open {
                    let sub = self.render_sub(key, theme, cx);
                    item = item.child(popover::nested_menu(
                        format!("pr-filter-sub-{key:?}"),
                        sub,
                        true,
                    ));
                }
                card = card.child(item);
            }
            if active > 0 {
                card = card.child(popover::menu_separator()).child(
                    menu_item(
                        "pr-filter-reset",
                        lead(icons::RESTART, theme),
                        "Clear filters",
                        None,
                        false,
                        false,
                        theme,
                    )
                    .on_click(cx.listener(|page, _, _, cx| {
                        cx.stop_propagation();
                        page.close_menus(cx);
                        page.set_picks(
                            |p| {
                                *p = PullRequestPicks {
                                    sort: p.sort,
                                    ..Default::default()
                                }
                            },
                            cx,
                        );
                    })),
                );
            }
            trigger = trigger.child(popover::anchored_menu_below_end(
                "pr-filter-menu",
                card.into_any_element(),
                self.filter_menu.closing_since(),
            ));
        }
        trigger.into_any_element()
    }

    fn open_sub(&mut self, key: FilterKey, window: &mut Window, cx: &mut Context<Self>) {
        self.opened = Some(key);
        self.sub_query.clear();
        self.sub_search
            .update(cx, |input, cx| input.set_text("", cx));
        if matches!(key, FilterKey::Author | FilterKey::Labels) {
            let focus = gpui::Focusable::focus_handle(self.sub_search.read(cx), cx);
            window.focus(&focus, cx);
        }
        cx.notify();
    }

    /// A filter's choices; Author and Labels list who and what the rows
    /// have, the most frequent first, with their own search.
    fn render_sub(&mut self, key: FilterKey, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        macro_rules! choices {
            ($list:expr, $field:ident) => {
                $list
                    .iter()
                    .map(|&(value, label, glyph)| {
                        menu_item(
                            format!("pr-sub-{key:?}-{value:?}"),
                            lead(glyph, theme),
                            label,
                            None,
                            self.picks.$field == value,
                            false,
                            theme,
                        )
                        .on_click(cx.listener(move |page, _, _, cx| {
                            cx.stop_propagation();
                            page.close_menus(cx);
                            page.set_picks(|p| p.$field = value, cx);
                        }))
                        .into_any_element()
                    })
                    .collect::<Vec<_>>()
            };
        }
        let card = popover::popover_card(theme)
            .w(px(280.0))
            .max_h(px(480.0))
            .flex()
            .flex_col()
            // Its clicks are the Filters card's own: don't let them close it.
            .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation());
        let items: Vec<AnyElement> = match key {
            FilterKey::State => choices!(STATES, state),
            FilterKey::Involvement => choices!(INVOLVEMENTS, involvement),
            FilterKey::Draft => choices!(DRAFTS, draft),
            FilterKey::Review => choices!(REVIEWS, review),
            FilterKey::Checks => choices!(CHECKS, checks),
            FilterKey::Project => {
                let mut repos: Vec<String> = Vec::new();
                for row in &self.rows {
                    let repo = row.item.repository.to_ascii_lowercase();
                    if !repos.contains(&repo) {
                        repos.push(repo);
                    }
                }
                repos.sort();
                std::iter::once(String::new())
                    .chain(repos)
                    .map(|repo| {
                        let label = if repo.is_empty() {
                            "All projects".to_owned()
                        } else {
                            repo.clone()
                        };
                        let glyph = if repo.is_empty() {
                            icons::LUCIDE_LAYERS
                        } else {
                            icons::LUCIDE_FOLDER_GIT_2
                        };
                        let on = self.picks.project == repo;
                        menu_item(
                            format!("pr-sub-project-{repo}"),
                            lead(glyph, theme),
                            label,
                            None,
                            on,
                            false,
                            theme,
                        )
                        .on_click(cx.listener(move |page, _, _, cx| {
                            cx.stop_propagation();
                            page.close_menus(cx);
                            let repo = repo.clone();
                            page.set_picks(|p| p.project = repo, cx);
                        }))
                        .into_any_element()
                    })
                    .collect()
            }
            FilterKey::Author | FilterKey::Labels => {
                let authors = key == FilterKey::Author;
                let mut counts: HashMap<String, usize> = HashMap::new();
                let mut avatars: HashMap<String, Option<String>> = HashMap::new();
                for row in &self.rows {
                    if authors {
                        *counts.entry(row.item.author.login.clone()).or_default() += 1;
                        avatars.insert(
                            row.item.author.login.clone(),
                            row.item.author.avatar(&row.item.provider),
                        );
                    } else {
                        for label in &row.item.labels {
                            *counts.entry(label.name.clone()).or_default() += 1;
                        }
                    }
                }
                counts.remove("");
                let mut names: Vec<String> = counts.keys().cloned().collect();
                names.sort_by(|a, b| counts[b].cmp(&counts[a]).then_with(|| a.cmp(b)));
                let q = self.sub_query.clone();
                let any = if authors {
                    self.picks.author.is_empty()
                } else {
                    self.picks.labels.is_empty()
                };
                let mut items = vec![
                    div()
                        .px(px(6.0))
                        .pt(px(4.0))
                        .pb(px(6.0))
                        .mb(px(4.0))
                        .border_b_1()
                        .border_color(theme.border)
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .child(
                            icon(icons::MAGNIFER)
                                .size(px(13.0))
                                .text_color(theme.text_muted),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .h(px(16.0))
                                .overflow_hidden()
                                .child(self.sub_search.clone()),
                        )
                        .into_any_element(),
                    menu_item(
                        format!("pr-sub-{key:?}-any"),
                        lead(icons::LUCIDE_LAYERS, theme),
                        if authors { "Anyone" } else { "Any" },
                        None,
                        any,
                        false,
                        theme,
                    )
                    .on_click(cx.listener(move |page, _, _, cx| {
                        cx.stop_propagation();
                        if authors {
                            page.close_menus(cx);
                            page.set_picks(|p| p.author.clear(), cx);
                        } else {
                            page.set_picks(|p| p.labels.clear(), cx);
                        }
                    }))
                    .into_any_element(),
                ];
                for name in names.into_iter().filter(|n| n.to_lowercase().contains(&q)) {
                    let count: SharedString = format!("{}", counts[&name]).into();
                    let on = if authors {
                        self.picks.author == name
                    } else {
                        self.picks.labels.contains(&name)
                    };
                    let face = authors
                        .then(|| avatar(avatars.get(&name).cloned().flatten(), &name, 18.0, theme));
                    let item = menu_item(
                        format!("pr-sub-{key:?}-{name}"),
                        face,
                        name.clone(),
                        Some(count),
                        on,
                        false,
                        theme,
                    );
                    items.push(
                        item.on_click(cx.listener(move |page, _, _, cx| {
                            cx.stop_propagation();
                            let name = name.clone();
                            if authors {
                                page.close_menus(cx);
                                page.set_picks(|p| p.author = name, cx);
                            } else {
                                page.set_picks(
                                    |p| match p.labels.iter().position(|l| *l == name) {
                                        Some(i) => {
                                            p.labels.remove(i);
                                        }
                                        None => p.labels.push(name),
                                    },
                                    cx,
                                );
                            }
                        }))
                        .into_any_element(),
                    );
                }
                items
            }
        };
        card.child(
            div()
                .id("pr-sub-scroll")
                .flex()
                .flex_col()
                .overflow_y_scroll()
                .children(items),
        )
        .into_any_element()
    }

    fn render_line(&mut self, index: usize, _: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = Theme::of(cx).clone();
        let column = || {
            div()
                .w_full()
                .max_w(px(PAGE_MAX_WIDTH))
                .mx_auto()
                .px(px(PAGE_PAD_X - 12.0))
        };
        // A list item is laid out as a root, where `mx_auto` does nothing:
        // the column centres inside a full-width wrapper instead.
        let line = match self.lines.get(index).cloned() {
            None => div().into_any_element(),
            Some(Line::Header(group, count, first)) => column()
                .pt(px(if first { 4.0 } else { 18.0 }))
                .pb(px(4.0))
                .child(
                    div()
                        .px(px(12.0))
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .text_size(px(12.5))
                        .text_color(theme.text_muted)
                        .child(
                            icon(group.glyph())
                                .size(px(14.0))
                                .text_color(theme.text_muted),
                        )
                        .child(
                            div()
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .child(group.label()),
                        )
                        .child(
                            div()
                                .text_color(theme.text_faint)
                                .child(SharedString::from(count.to_string())),
                        )
                        .child(div().flex_1().h(px(1.0)).bg(theme.border)),
                )
                .into_any_element(),
            Some(Line::Item(n)) => {
                let i = self.shown[n];
                column()
                    .child(self.render_row(i, &theme, cx))
                    .into_any_element()
            }
        };
        div().w_full().child(line).into_any_element()
    }

    fn render_row(&self, i: usize, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let row = &self.rows[i];
        let item = &row.item;
        let now = Utc::now();
        let selected = self.selected_url.as_deref() == Some(item.url.as_str());
        let mono = theme.font_mono.clone();
        let hover = theme.element_hover;
        let ci = match item.ci.state {
            CiState::Passed => Some((icons::LUCIDE_CIRCLE_CHECK, theme.success, "Checks pass")),
            CiState::Failed => Some((icons::LUCIDE_CIRCLE_X, theme.danger, "Checks fail")),
            CiState::Pending => Some((icons::LUCIDE_CIRCLE_DOT, theme.warning, "Checks run")),
            _ => None,
        };
        let review = match item.review_decision {
            ChangeRequestReviewDecision::Approved => {
                Some((icons::LUCIDE_USER_CHECK, theme.success, "Approved"))
            }
            ChangeRequestReviewDecision::ChangesRequested => Some((
                icons::LUCIDE_USER_ROUND_X,
                theme.danger,
                "Changes requested",
            )),
            _ => None,
        };
        let mark = |(path, color, tip): (&'static str, gpui::Hsla, &'static str),
                    id: &'static str| {
            div()
                .id(id)
                .flex_none()
                .child(icon(path).size(px(14.0)).text_color(color))
                .tooltip(widgets::text_tooltip(tip))
        };
        let labels = item.labels.len();
        let line1 = div()
            .flex()
            .items_center()
            .gap(px(6.0))
            .min_w_0()
            .child(
                div()
                    .flex_none()
                    .font_family(mono.clone())
                    .text_size(px(12.0))
                    .text_color(theme.text_muted)
                    .child(SharedString::from(format!("#{}", item.number))),
            )
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_color(theme.text)
                    .child(SharedString::from(item.title.clone())),
            )
            .children(ci.map(|c| mark(c, "pr-row-ci")))
            .children(review.map(|r| mark(r, "pr-row-review")));
        let line2 = div()
            .mt(px(2.0))
            .flex()
            .items_center()
            .gap(px(6.0))
            .min_w_0()
            .overflow_hidden()
            .text_size(px(12.0))
            .text_color(theme.text_muted)
            .child(provider_logo(&item.provider, theme))
            .child(avatar(
                item.author.avatar(&item.provider),
                &item.author.login,
                16.0,
                theme,
            ))
            .child(
                div()
                    .flex_none()
                    .child(SharedString::from(item.author.login.clone())),
            )
            .child(
                div()
                    .flex_none()
                    .text_color(theme.text_faint)
                    .child(SharedString::from(item.repository.clone())),
            )
            .children(item.labels.iter().take(3).map(|l| chip(l, theme)))
            .when(labels > 3, |el| {
                el.child(
                    div()
                        .flex_none()
                        .child(SharedString::from(format!("+{}", labels - 3))),
                )
            });
        let side = div()
            .flex_none()
            .flex()
            .flex_col()
            .items_end()
            .text_size(px(12.0))
            .line_height(px(19.0))
            .child(
                div()
                    .flex()
                    .gap(px(4.0))
                    .font_family(mono)
                    .child(
                        div()
                            .text_color(theme.success)
                            .child(SharedString::from(format!(
                                "+{}",
                                thousands(item.additions)
                            ))),
                    )
                    .child(
                        div()
                            .text_color(theme.danger)
                            .child(SharedString::from(format!(
                                "−{}",
                                thousands(item.deletions)
                            ))),
                    ),
            )
            .child(
                div()
                    .text_color(theme.text_muted)
                    .child(SharedString::from(ago(item.updated_at, now))),
            );
        div()
            .id(gpui::ElementId::Name(format!("pr-row-{}", item.url).into()))
            .flex()
            .items_start()
            .gap(px(10.0))
            .px(px(12.0))
            .py(px(9.0))
            .rounded(px(8.0))
            .cursor_pointer()
            .text_size(px(13.0))
            .when(selected, |el| el.bg(crate::theme::card_selected_bg()))
            .hover(move |s| s.bg(hover))
            .tooltip(widgets::text_tooltip(format!(
                "{} · {}",
                item.repository, item.head_ref_name
            )))
            .on_click(cx.listener(move |page, _, window, cx| page.open(i, window, cx)))
            .child(div().mt(px(1.0)).child(glyph(
                item.state,
                item.is_draft,
                item.mergeability == ChangeRequestMergeability::Conflicting,
                16.0,
                theme,
            )))
            .child(div().flex_1().min_w_0().child(line1).child(line2))
            .child(side)
            .into_any_element()
    }

    fn render_message(&self, theme: &Theme) -> Option<AnyElement> {
        let text: SharedString = if !self.loaded {
            return None;
        } else if self.read_from.is_empty() {
            "No repositories yet: add a project in a git repository on GitHub or GitLab.".into()
        } else if self.rows.is_empty() {
            match self.picks.state {
                ChangeRequestStateFilter::Open => "No open pull requests.".into(),
                ChangeRequestStateFilter::Merged => "No merged pull requests.".into(),
                ChangeRequestStateFilter::Closed => "No closed pull requests.".into(),
                ChangeRequestStateFilter::All => "No pull requests.".into(),
            }
        } else if self.lines.is_empty() {
            "Nothing matches.".into()
        } else {
            return None;
        };
        Some(
            div()
                .py(px(48.0))
                .flex()
                .justify_center()
                .text_size(px(13.0))
                .text_color(theme.text_muted)
                .child(text)
                .into_any_element(),
        )
    }
}

impl Render for PullRequestsPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Drawn means shown: a page made on the route itself never heard
        // `on_visible`.
        self.visible = true;
        if self.loaded_at.is_none() && self.task.is_none() {
            self.load(Read::Stored, cx);
        }
        let theme = Theme::of(cx).clone();
        let count = self.loaded.then_some(self.shown.len());
        let refresh = div()
            .id("pr-refresh")
            .size(px(30.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(8.0))
            .border_1()
            .border_color(theme.border)
            .cursor_pointer()
            .hover({
                let hover = theme.element_hover;
                move |s| s.bg(hover)
            })
            .tooltip(widgets::text_tooltip("Refresh"))
            .on_click(cx.listener(|page, _, _, cx| page.load(Read::Refresh, cx)))
            .child(if self.loading {
                crate::loaders::ring_spinner("pr-refresh-spinner", 13.0, theme.text_muted)
                    .into_any_element()
            } else {
                icon(icons::REFRESH)
                    .size(px(14.0))
                    .text_color(theme.text_muted)
                    .into_any_element()
            });
        let search = crate::surface_chrome::input()
            .h(px(30.0))
            .min_w(px(SEARCH_MIN))
            .rounded(px(8.0))
            .px(px(10.0))
            .child(
                icon(icons::MAGNIFER)
                    .size(px(14.0))
                    .text_color(theme.text_muted),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h(px(18.0))
                    .overflow_hidden()
                    .child(self.search.clone()),
            );
        let measure = {
            let page = cx.weak_entity();
            gpui::canvas(
                move |bounds, window, cx| {
                    let width = f32::from(bounds.size.width);
                    window.defer(cx, move |_, cx| {
                        let _ = page.update(cx, |page, cx| {
                            if page.bar_width.is_none_or(|old| (old - width).abs() > 0.5) {
                                page.bar_width = Some(width);
                                cx.notify();
                            }
                        });
                    });
                },
                |_, _, _, _| {},
            )
            .absolute()
            .inset_0()
        };
        let fitting = quick_keys(&self.picks, self.bar_width.unwrap_or(0.0));
        let quick: Vec<AnyElement> =
            fitting.into_iter().map(|key| self.render_quick(key, &theme, cx)).collect();
        let failures = (!self.failures.is_empty()).then(|| {
            let n = self.failures.len();
            div()
                .mt(px(8.0))
                .flex()
                .flex_col()
                .gap(px(2.0))
                .text_size(px(12.0))
                .text_color(theme.warning)
                .child(SharedString::from(format!(
                    "{n} {} couldn’t be read",
                    if n == 1 { "repository" } else { "repositories" }
                )))
                .children(self.failures.iter().map(|f| {
                    div()
                        .text_color(theme.text_muted)
                        .child(SharedString::from(format!(
                            "{} · {}",
                            f.repository, f.message
                        )))
                }))
        });
        let header = div()
            .debug_selector(|| "pull-requests-column".into())
            .w_full()
            .max_w(px(PAGE_MAX_WIDTH))
            .mx_auto()
            .px(px(PAGE_PAD_X))
            .pt(px(Theme::TITLEBAR_HEIGHT + 16.0))
            .pb(px(8.0))
            .flex_none()
            .child(widgets::page_header(&theme, "Pull requests", count))
            .child(
                div()
                    .mt(px(14.0))
                    .relative()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(measure)
                    .child(search)
                    .children(quick)
                    .child(self.render_sort(&theme, cx))
                    .child(self.render_filters(&theme, cx))
                    .child(refresh),
            )
            .children(failures);
        let body = match self.render_message(&theme) {
            Some(message) => message,
            None if !self.loaded => div()
                .py(px(64.0))
                .flex()
                .justify_center()
                .child(crate::loaders::ring_spinner(
                    "pr-loading",
                    18.0,
                    theme.text_muted,
                ))
                .into_any_element(),
            None => list(self.list.clone(), cx.processor(Self::render_line))
                .size_full()
                .into_any_element(),
        };
        let _ = window;
        div()
            .id("pull-requests-page")
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.bg)
            .child(header)
            .child(div().flex_1().min_h_0().pb(px(16.0)).child(body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use commutator_proto::{ChangeRequestActor, ChangeRequestLabel};

    #[test]
    fn label_colours_parse_from_either_provider() {
        assert!(label_hsla("d73a4a").is_some());
        assert!(label_hsla("#428BCA").is_some());
        assert!(label_hsla("").is_none());
        assert!(label_hsla("blue").is_none());
    }

    fn row(number: u64, authored: Option<bool>, requested: Option<bool>) -> Row {
        Row {
            device: "d".into(),
            item: ChangeRequestListItem {
                provider: "github".into(),
                author: ChangeRequestActor {
                    login: "ann".into(),
                    ..Default::default()
                },
                repository: "acme/app".into(),
                head_ref_oid: String::new(),
                ci: Default::default(),
                viewer_did_author: authored,
                viewer_review_requested: requested,
                number,
                title: format!("Fix thing {number}"),
                url: format!("https://github.com/acme/app/pull/{number}"),
                state: ChangeRequestState::Open,
                is_draft: number == 2,
                review_decision: ChangeRequestReviewDecision::Unknown,
                additions: number * 10,
                deletions: 0,
                mergeability: ChangeRequestMergeability::Unknown,
                created_at: DateTime::UNIX_EPOCH,
                updated_at: DateTime::UNIX_EPOCH,
                labels: vec![ChangeRequestLabel {
                    name: "bug".into(),
                    color: String::new(),
                }],
                head_ref_name: "fix".into(),
            },
        }
    }

    #[test]
    fn search_reads_qualifiers_and_words() {
        let mine = row(1, Some(true), None);
        let draft = row(2, Some(false), None);
        assert!(matches(&mine, "author:me label:bu"));
        assert!(!matches(&draft, "author:me"));
        assert!(matches(&draft, "is:draft repo:acme thing"));
        assert!(!matches(&mine, "is:draft"));
        assert!(matches(&mine, "#1"));
        assert!(!matches(&mine, "label:feature"));
    }

    #[test]
    fn answers_merge_by_url_and_keep_what_any_knew() {
        let source = Source {
            device: "d".into(),
            target: None,
            repository: Some("acme/app".into()),
            cwd: None,
            label: "acme/app".into(),
        };
        let page = |rows: Vec<Row>| ChangeRequestPage {
            items: rows.into_iter().map(|r| r.item).collect(),
            ..Default::default()
        };
        let (rows, failures) = merge(vec![
            (
                source.clone(),
                Ok(page(vec![
                    row(1, None, None),
                    row(2, Some(false), Some(false)),
                ])),
            ),
            (source.clone(), Ok(page(vec![row(1, Some(true), None)]))),
            (source.clone(), Ok(page(vec![row(2, None, Some(true))]))),
            (
                source,
                Err(RpcError::Capability(
                    capability_errors::PULL_REQUESTS_AUTHENTICATION.into(),
                )),
            ),
        ]);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].group(), Group::Authored);
        assert_eq!(rows[1].group(), Group::Reviewing);
        assert_eq!(failures.len(), 1);
    }

    #[test]
    fn the_bar_keeps_what_fits_project_first() {
        let picks = PullRequestPicks::default();
        assert!(quick_keys(&picks, 400.0).is_empty());
        assert_eq!(quick_keys(&picks, 620.0), [FilterKey::Project]);
        assert_eq!(quick_keys(&picks, 2000.0), [FilterKey::State, FilterKey::Project, FilterKey::Author]);
    }

    #[test]
    fn picks_filter_and_count() {
        let mut picks = PullRequestPicks::default();
        assert_eq!(picks.active(), 0);
        picks.draft = DraftPick::Hide;
        picks.labels = vec!["bug".into()];
        assert_eq!(picks.active(), 2);
        assert!(picks.passes(&row(1, None, None)));
        assert!(!picks.passes(&row(2, None, None)));
        assert_eq!(thousands(1234567), "1,234,567");
    }
}
