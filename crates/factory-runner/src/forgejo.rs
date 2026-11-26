use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::fmt::Write as _;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use factory_core::PrDescription;
use reqwest::{Method, Url};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::forge::{
    Access, Account, BoxFuture, Forge, ForgeAccount, LookupError, NOT_PERMITTED, PrQuery, REJECTED,
    SEARCH_PAGE,
};
use crate::github::{
    Ci, Comment, FailedCheck, HeadCommit, MAX_DIFF, MAX_LOG, OpenPr, PrDetails, PrInfo, PrState,
    PrStatus, PullRequest, RebaseCheckbox, RenovatePr, Rerun, tail, tick_rebase_checkbox,
};

const PAGE_SIZE: usize = 50;
/// The most pages a listing reads, so a runaway listing ends.
const MAX_PAGES: usize = 50;
/// How often `status` reads a pull request again that reports it cannot merge.
const CONFLICT_RECHECKS: u32 = 3;
/// How often `pr_info` and `pr_details` read a pull request again after its
/// head moved during the read.
const SNAPSHOT_TRIES: usize = 3;
/// The largest number of lines on each side for which `compare_patch` finds a
/// minimal diff. Larger files are shown as replaced.
const DIFF_CELLS: usize = 4_000_000;
const DIFF_CONTEXT: usize = 3;

/// The version a Forgejo server reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ForgejoVersion {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl ForgejoVersion {
    /// Parses the answer of `/api/v1/version`, such as
    /// `17.0.0-121-39320c0+gitea-1.22.0`. Forgejo appends the Gitea version it
    /// is compatible with, and Gitea itself does not, so an answer without
    /// `+gitea-` is not Forgejo's.
    ///
    /// # Errors
    ///
    /// Returns a message when the text is not a Forgejo version.
    pub fn parse(text: &str) -> Result<ForgejoVersion, String> {
        let (version, compat) = text
            .split_once("+gitea-")
            .ok_or_else(|| format!("{text:?} is not a Forgejo version"))?;
        if compat.is_empty() {
            return Err(format!("{text:?} is not a Forgejo version"));
        }
        let numbers = version.split(['-', '+']).next().unwrap_or(version);
        let mut parts = numbers.split('.').map(str::parse::<u64>);
        match (parts.next(), parts.next(), parts.next(), parts.next()) {
            (Some(Ok(major)), Some(Ok(minor)), Some(Ok(patch)), None) => Ok(ForgejoVersion {
                major,
                minor,
                patch,
            }),
            _ => Err(format!("{text:?} is not a Forgejo version")),
        }
    }

    /// Whether the server lists the jobs of a run and serves their logs.
    #[must_use]
    pub fn reads_job_logs(&self) -> bool {
        self.major >= 16
    }

    /// Whether the server reruns a job.
    #[must_use]
    pub fn reruns_jobs(&self) -> bool {
        self.major >= 17
    }
}

/// An HTTPS client for Forgejo. It follows no redirect, so a token never
/// leaves the server it was meant for, and reads no proxy or credential from
/// the environment. `roots` are trusted in addition to the system roots.
///
/// # Errors
///
/// Returns a message when the client cannot be built.
pub fn client(roots: &[reqwest::Certificate]) -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .https_only(true)
        .user_agent("factory")
        .timeout(Duration::from_secs(60));
    for root in roots {
        builder = builder.add_root_certificate(root.clone());
    }
    builder
        .build()
        .map_err(|err| format!("building the Forgejo client: {err}"))
}

/// Reads the version of the Forgejo server at `base_url`.
///
/// # Errors
///
/// Returns a message when the server does not answer, or answers with
/// something that is not a Forgejo version.
pub async fn version(base_url: &str, client: &reqwest::Client) -> Result<ForgejoVersion, String> {
    reported_version(base_url, client)
        .await
        .map(|(_, version)| version)
}

/// Like [`version`], and also returns the text the server reported.
///
/// # Errors
///
/// As [`version`].
pub async fn reported_version(
    base_url: &str,
    client: &reqwest::Client,
) -> Result<(String, ForgejoVersion), String> {
    #[derive(Deserialize)]
    struct Answer {
        version: String,
    }
    let url = format!("{}/api/v1/version", base_url.trim_end_matches('/'));
    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|err| format!("GET {url} failed: {err}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("GET {url} answered HTTP {}", status.as_u16()));
    }
    let text = response
        .text()
        .await
        .map_err(|err| format!("GET {url} failed: {err}"))?;
    let answer: Answer = serde_json::from_str(&text)
        .map_err(|_| format!("GET {url} did not answer with a version"))?;
    let parsed = ForgejoVersion::parse(&answer.version)?;
    Ok((answer.version, parsed))
}

/// Talks to a Forgejo server over its HTTP API as the account each call
/// names. The token goes only in the `Authorization` header.
#[derive(Debug, Clone)]
pub struct Forgejo {
    base_url: String,
    client: reqwest::Client,
    /// How many times `rerun_failed` reads the pull request while it waits
    /// for the server to stop reporting the old failure.
    pub settle_polls: u32,
    pub settle_interval: Duration,
}

/// A failed API call. `status` is `None` when the request did not complete.
struct ApiError {
    status: Option<u16>,
    text: String,
}

impl From<ApiError> for String {
    fn from(err: ApiError) -> String {
        err.text
    }
}

impl From<String> for ApiError {
    fn from(text: String) -> ApiError {
        ApiError { status: None, text }
    }
}

struct Reply {
    status: u16,
    response: reqwest::Response,
}

#[derive(Default, Deserialize)]
struct UserJson {
    #[serde(default)]
    login: String,
    #[serde(default)]
    id: u64,
}

#[derive(Default, Deserialize)]
struct RefJson {
    #[serde(default)]
    sha: String,
    #[serde(default, rename = "ref")]
    name: String,
    #[serde(default)]
    label: String,
    #[serde(default)]
    repo_id: u64,
    repo: Option<RefRepoJson>,
}

#[derive(Deserialize)]
struct RefRepoJson {
    clone_url: Option<String>,
}

#[derive(Deserialize)]
struct PullJson {
    number: u64,
    #[serde(default)]
    title: String,
    body: Option<String>,
    #[serde(default)]
    state: String,
    #[serde(default)]
    merged: bool,
    mergeable: Option<bool>,
    #[serde(default)]
    draft: bool,
    merged_at: Option<String>,
    #[serde(default)]
    html_url: String,
    user: Option<UserJson>,
    #[serde(default)]
    head: RefJson,
    #[serde(default)]
    base: RefJson,
}

impl PullJson {
    /// A head repository that is not the base repository, or that is gone.
    fn is_fork(&self) -> bool {
        self.head.repo_id == 0 || self.head.repo_id != self.base.repo_id
    }

    fn author(&self) -> String {
        self.user
            .as_ref()
            .map_or_else(|| "ghost".to_owned(), |user| user.login.clone())
    }
}

#[derive(Deserialize)]
struct StatusJson {
    #[serde(default)]
    context: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    target_url: String,
}

#[derive(Deserialize)]
struct RunJson {
    id: u64,
    #[serde(default)]
    index_in_repo: u64,
}

#[derive(Deserialize)]
struct JobJson {
    id: u64,
    #[serde(default)]
    status: String,
    #[serde(default)]
    html_url: String,
}

#[derive(Deserialize)]
struct CommentJson {
    #[serde(default)]
    body: String,
    #[serde(default)]
    created_at: String,
    user: Option<UserJson>,
}

#[derive(Deserialize)]
struct ReviewJson {
    id: u64,
    #[serde(default)]
    body: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    submitted_at: String,
    #[serde(default)]
    comments_count: u64,
    user: Option<UserJson>,
}

#[derive(Deserialize)]
struct ReviewCommentJson {
    #[serde(default)]
    body: String,
    #[serde(default)]
    created_at: String,
    #[serde(default)]
    path: String,
    #[serde(default)]
    position: u64,
    #[serde(default)]
    original_position: u64,
    user: Option<UserJson>,
}

/// Percent-encodes `text` for a URL path. A `/` stays.
fn encode_path(text: &str) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'/') {
            out.push(char::from(byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

/// `owner/name` as the path of its repository.
fn repo_path(repo: &str) -> Result<String, String> {
    match repo.split('/').collect::<Vec<_>>().as_slice() {
        [owner, name] if !owner.is_empty() && !name.is_empty() => Ok(format!(
            "/repos/{}/{}",
            encode_path(owner),
            encode_path(name)
        )),
        _ => Err(format!("{repo} is not owner/name")),
    }
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let doy = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// Forgejo's RFC 3339 time, such as `2026-11-23T20:53:06+01:00`, as
/// `2026-11-23 19:53:06` in UTC, which compares as text with the scheduler's
/// watermarks. Text that is not a time comes back unchanged.
fn utc(text: &str) -> String {
    parse_utc(text).unwrap_or_else(|| text.to_owned())
}

fn parse_utc(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    if bytes.len() < 20 || !matches!(bytes[10], b'T' | b' ') {
        return None;
    }
    let number = |range: std::ops::Range<usize>| -> Option<i64> {
        let part = text.get(range)?;
        part.bytes()
            .all(|b| b.is_ascii_digit())
            .then(|| part.parse().ok())?
    };
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    let (hour, minute, second) = (number(11..13)?, number(14..16)?, number(17..19)?);
    let mut rest = &text[19..];
    if let Some(fraction) = rest.strip_prefix('.') {
        let digits = fraction.bytes().take_while(u8::is_ascii_digit).count();
        rest = &fraction[digits..];
    }
    let offset = match rest {
        "Z" | "z" => 0,
        _ if rest.len() >= 5 && matches!(rest.as_bytes()[0], b'+' | b'-') => {
            let hours = rest.get(1..3)?.parse::<i64>().ok()?;
            let minutes = rest.get(rest.len() - 2..)?.parse::<i64>().ok()?;
            let sign = if rest.starts_with('-') { -1 } else { 1 };
            sign * (hours * 3600 + minutes * 60)
        }
        _ => return None,
    };
    let seconds =
        days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second - offset;
    let (year, month, day) = civil_from_days(seconds.div_euclid(86_400));
    let rest = seconds.rem_euclid(86_400);
    Some(format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    ))
}

#[derive(Clone, Copy, PartialEq)]
enum Op {
    Same,
    Del,
    Ins,
}

/// The edit script from the lines `a` to the lines `b`: a minimal one, unless
/// the middle is larger than `DIFF_CELLS`, which it replaces whole.
fn line_ops(a: &[&str], b: &[&str]) -> Vec<Op> {
    let prefix = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let suffix = a[prefix..]
        .iter()
        .rev()
        .zip(b[prefix..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let (mid_a, mid_b) = (&a[prefix..a.len() - suffix], &b[prefix..b.len() - suffix]);
    let mut ops = vec![Op::Same; prefix];
    if mid_a.len().saturating_mul(mid_b.len()) <= DIFF_CELLS {
        let width = mid_b.len() + 1;
        let mut table = vec![0u32; (mid_a.len() + 1) * width];
        for i in (0..mid_a.len()).rev() {
            for j in (0..mid_b.len()).rev() {
                table[i * width + j] = if mid_a[i] == mid_b[j] {
                    table[(i + 1) * width + j + 1] + 1
                } else {
                    table[(i + 1) * width + j].max(table[i * width + j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < mid_a.len() || j < mid_b.len() {
            if i < mid_a.len() && j < mid_b.len() && mid_a[i] == mid_b[j] {
                ops.push(Op::Same);
                i += 1;
                j += 1;
            } else if j == mid_b.len()
                || (i < mid_a.len() && table[(i + 1) * width + j] >= table[i * width + j + 1])
            {
                ops.push(Op::Del);
                i += 1;
            } else {
                ops.push(Op::Ins);
                j += 1;
            }
        }
    } else {
        ops.extend(std::iter::repeat_n(Op::Del, mid_a.len()));
        ops.extend(std::iter::repeat_n(Op::Ins, mid_b.len()));
    }
    ops.extend(std::iter::repeat_n(Op::Same, suffix));
    ops
}

/// A unified diff of two texts with `DIFF_CONTEXT` lines of context and no
/// file header, like the patch of a GitHub compare. `None` when no line
/// differs.
fn unified_diff(old: &str, new: &str) -> Option<String> {
    let a: Vec<&str> = old.split_inclusive('\n').collect();
    let b: Vec<&str> = new.split_inclusive('\n').collect();
    let ops = line_ops(&a, &b);
    let mut at = Vec::with_capacity(ops.len());
    let (mut i, mut j) = (0, 0);
    for op in &ops {
        at.push((i, j));
        match op {
            Op::Same => {
                i += 1;
                j += 1;
            }
            Op::Del => i += 1,
            Op::Ins => j += 1,
        }
    }
    let changes: Vec<usize> = (0..ops.len()).filter(|&k| ops[k] != Op::Same).collect();
    let mut out = String::new();
    let mut group = 0;
    while group < changes.len() {
        let mut last = group;
        while last + 1 < changes.len() && changes[last + 1] - changes[last] <= 2 * DIFF_CONTEXT + 1
        {
            last += 1;
        }
        let start = changes[group].saturating_sub(DIFF_CONTEXT);
        let end = (changes[last] + DIFF_CONTEXT + 1).min(ops.len());
        let old_count = ops[start..end].iter().filter(|&&o| o != Op::Ins).count();
        let new_count = ops[start..end].iter().filter(|&&o| o != Op::Del).count();
        let (old_from, new_from) = at[start];
        let old_start = if old_count == 0 {
            old_from
        } else {
            old_from + 1
        };
        let new_start = if new_count == 0 {
            new_from
        } else {
            new_from + 1
        };
        let _ = writeln!(
            out,
            "@@ -{old_start},{old_count} +{new_start},{new_count} @@"
        );
        for k in start..end {
            let (i, j) = at[k];
            let (mark, line) = match ops[k] {
                Op::Same => (' ', a[i]),
                Op::Del => ('-', a[i]),
                Op::Ins => ('+', b[j]),
            };
            out.push(mark);
            out.push_str(line);
            if !line.ends_with('\n') {
                out.push_str("\n\\ No newline at end of file\n");
            }
        }
        group = last + 1;
    }
    (!out.is_empty()).then(|| out.strip_suffix('\n').unwrap_or(&out).to_owned())
}

/// The job a Forgejo Actions URL names: the run's index in the repository and
/// the job's index in the run. The URL must be on the instance `base` and
/// name `repo`, so a mount path called `actions` or a repository owned by
/// `actions` is not mistaken for the Actions route that follows them.
fn actions_job_ref(base: &Url, repo: &str, url: &str) -> Option<(u64, u64)> {
    let url = base.join(url).ok()?;
    if url.origin() != base.origin() {
        return None;
    }
    let mut segments = url.path_segments()?;
    for expected in base.path_segments()?.filter(|s| !s.is_empty()) {
        if segments.next()? != expected {
            return None;
        }
    }
    for expected in repo.split('/') {
        if !segments.next()?.eq_ignore_ascii_case(expected) {
            return None;
        }
    }
    if segments.next()? != "actions" || segments.next()? != "runs" {
        return None;
    }
    let run = segments.next()?.parse().ok()?;
    if segments.next()? != "jobs" {
        return None;
    }
    Some((run, segments.next()?.parse().ok()?))
}

fn moved(number: u64) -> String {
    format!("the head of pull request {number} kept moving while it was read")
}

/// `url` without a trailing `/attempt/N`.
fn without_attempt(url: &str) -> &str {
    match url.rsplit_once("/attempt/") {
        Some((head, number)) if number.bytes().all(|b| b.is_ascii_digit()) => head,
        _ => url,
    }
}

impl PullJson {
    /// Whether the pull request comes from `branch` of the repository itself.
    /// `head.ref` turns into `refs/pull/N/head` once the branch is deleted, so
    /// the label names the branch, and the repository id tells a fork's
    /// branch of the same name apart.
    fn is_from(&self, branch: &str) -> bool {
        self.head.label == branch && self.head.repo_id == self.base.repo_id
    }
}

impl Forgejo {
    /// A Forgejo at `base_url`, such as `https://codeberg.org`, trusting
    /// `roots` in addition to the system roots.
    ///
    /// # Errors
    ///
    /// Returns a message when the HTTP client cannot be built.
    pub fn new(base_url: &str, roots: &[reqwest::Certificate]) -> Result<Forgejo, String> {
        Ok(Forgejo {
            base_url: base_url.trim_end_matches('/').to_owned(),
            client: client(roots)?,
            settle_polls: 10,
            settle_interval: Duration::from_secs(3),
        })
    }

    /// The version of this server.
    ///
    /// # Errors
    ///
    /// See [`version`].
    pub async fn version(&self) -> Result<ForgejoVersion, String> {
        version(&self.base_url, &self.client).await
    }

    /// A JSON read of `path` under the repository `repo`, such as
    /// `branches/main`, as `account`. It is `None` when the server answers
    /// 404, and any other failure is an error.
    ///
    /// # Errors
    ///
    /// Returns a message when `repo` is not `owner/name`, the request fails,
    /// or the answer is not JSON.
    pub async fn read_repo_json(
        &self,
        account: &Account,
        repo: &str,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<Option<Value>, String> {
        let full = format!("{}/{}", repo_path(repo)?, encode_path(path));
        let full = full.trim_end_matches('/');
        match self
            .json_value(account, Method::GET, full, query, None)
            .await
        {
            Ok(value) => Ok(Some(value)),
            Err(err) if err.status == Some(404) => Ok(None),
            Err(err) => Err(err.text),
        }
    }

    fn host(&self) -> String {
        Url::parse(&self.base_url)
            .ok()
            .and_then(|url| url.host_str().map(str::to_owned))
            .unwrap_or_default()
    }

    /// The base URL as a directory, so a relative link keeps the mount path.
    fn base(&self) -> Result<Url, String> {
        Url::parse(&format!("{}/", self.base_url))
            .map_err(|err| format!("the Forgejo URL is invalid: {err}"))
    }

    fn error(token: Option<&str>, what: &str, status: Option<u16>, detail: &str) -> ApiError {
        let mut detail = detail.trim().to_owned();
        if let Some(token) = token.filter(|token| !token.is_empty()) {
            detail = detail.replace(token, "<redacted>");
        }
        let detail: String = detail.chars().take(300).collect();
        let marker = match status {
            Some(401) => REJECTED,
            Some(403) if what.split(' ').next() != Some("GET") => NOT_PERMITTED,
            _ => "",
        };
        let code = status.map_or(String::new(), |code| format!("HTTP {code}: "));
        ApiError {
            status,
            text: format!("{marker}{what} failed: {code}{detail}"),
        }
    }

    /// Sends the request. A reply with any status comes back as `Ok`.
    async fn send(
        &self,
        account: &Account,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        body: Option<&Value>,
    ) -> Result<Reply, ApiError> {
        let token = account.token.as_deref();
        let what = format!("{method} {path}");
        let mut request = self
            .client
            .request(method, format!("{}/api/v1{path}", self.base_url))
            .query(query)
            .header("Accept", "application/json");
        if let Some(token) = token {
            let mut value = reqwest::header::HeaderValue::from_str(&format!("token {token}"))
                .map_err(|_| Self::error(None, &what, None, "the token is not a valid header"))?;
            value.set_sensitive(true);
            request = request.header(reqwest::header::AUTHORIZATION, value);
        }
        if let Some(body) = body {
            request = request
                .header("Content-Type", "application/json")
                .body(body.to_string());
        }
        let response = request
            .send()
            .await
            .map_err(|err| Self::error(token, &what, None, &err.to_string()))?;
        Ok(Reply {
            status: response.status().as_u16(),
            response,
        })
    }

    /// As `send`, with any status but a success as an error.
    async fn call(
        &self,
        account: &Account,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        body: Option<&Value>,
    ) -> Result<Reply, ApiError> {
        let what = format!("{method} {path}");
        let reply = self.send(account, method, path, query, body).await?;
        if (200..300).contains(&reply.status) {
            return Ok(reply);
        }
        let status = reply.status;
        let text = reply.response.text().await.unwrap_or_default();
        let detail = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|value| value["message"].as_str().map(str::to_owned))
            .unwrap_or(text);
        Err(Self::error(
            account.token.as_deref(),
            &what,
            Some(status),
            &detail,
        ))
    }

    async fn json_value(
        &self,
        account: &Account,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        body: Option<&Value>,
    ) -> Result<Value, ApiError> {
        let what = format!("{method} {path}");
        let reply = self.call(account, method, path, query, body).await?;
        let text =
            reply.response.text().await.map_err(|err| {
                Self::error(account.token.as_deref(), &what, None, &err.to_string())
            })?;
        serde_json::from_str(&text)
            .map_err(|_| Self::error(None, &what, None, "the answer is not JSON"))
    }

    async fn get<T: DeserializeOwned>(
        &self,
        account: &Account,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<T, ApiError> {
        let value = self
            .json_value(account, Method::GET, path, query, None)
            .await?;
        serde_json::from_value(value).map_err(|err| {
            Self::error(
                account.token.as_deref(),
                &format!("GET {path}"),
                None,
                &format!("unexpected answer: {err}"),
            )
        })
    }

    /// Every item of a listing, read page by page. Null items are skipped.
    /// `key` names the field that holds the list when the answer is an
    /// object. A null list is empty. Any other shape, or more pages than
    /// `MAX_PAGES`, is an error, so a listing is never silently partial.
    async fn list<T: DeserializeOwned>(
        &self,
        account: &Account,
        path: &str,
        query: &[(&str, &str)],
        key: Option<&str>,
    ) -> Result<Vec<T>, ApiError> {
        let what = format!("GET {path}");
        let token = account.token.as_deref();
        let mut items = Vec::new();
        for page in 1..=MAX_PAGES {
            let mut full = query.to_vec();
            let page_text = page.to_string();
            let limit = PAGE_SIZE.to_string();
            full.push(("page", &page_text));
            full.push(("limit", &limit));
            let value = self
                .json_value(account, Method::GET, path, &full, None)
                .await?;
            let list = match key {
                Some(key) => value.get(key),
                None => Some(&value),
            };
            let page_items = match list {
                Some(Value::Array(page_items)) => page_items.clone(),
                Some(Value::Null) => Vec::new(),
                _ => {
                    let place = key.map_or(String::new(), |key| format!(" in {key:?}"));
                    return Err(Self::error(
                        None,
                        &what,
                        None,
                        &format!("the answer holds no list{place}"),
                    ));
                }
            };
            if page_items.is_empty() {
                return Ok(items);
            }
            for item in page_items.into_iter().filter(|item| !item.is_null()) {
                items.push(serde_json::from_value(item).map_err(|err| {
                    Self::error(token, &what, None, &format!("unexpected answer: {err}"))
                })?);
            }
        }
        Err(Self::error(
            None,
            &what,
            None,
            &format!("the listing has more than {MAX_PAGES} pages"),
        ))
    }

    /// The body of a successful GET as text, cut as the caller needs. It
    /// stops reading once more than `keep` bytes arrived, keeping the last
    /// `keep` bytes when `last` is set, and failing otherwise.
    async fn text(
        &self,
        account: &Account,
        path: &str,
        query: &[(&str, &str)],
        keep: usize,
        last: bool,
    ) -> Result<String, ApiError> {
        let what = format!("GET {path}");
        let token = account.token.as_deref();
        let mut reply = self.call(account, Method::GET, path, query, None).await?;
        let mut bytes: Vec<u8> = Vec::new();
        while let Some(chunk) = reply
            .response
            .chunk()
            .await
            .map_err(|err| Self::error(token, &what, None, &err.to_string()))?
        {
            bytes.extend_from_slice(&chunk);
            if bytes.len() > keep {
                if !last {
                    return Err(Self::error(None, &what, None, "the answer is too large"));
                }
                let cut = bytes.len() - keep;
                bytes.drain(..cut);
            }
        }
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    async fn pull(&self, account: &Account, repo: &str, number: u64) -> Result<PullJson, ApiError> {
        self.get(
            account,
            &format!("{}/pulls/{number}", repo_path(repo)?),
            &[],
        )
        .await
    }

    async fn pull_files(
        &self,
        account: &Account,
        repo: &str,
        number: u64,
    ) -> Result<Vec<String>, ApiError> {
        #[derive(Deserialize)]
        struct File {
            filename: String,
        }
        let files: Vec<File> = self
            .list(
                account,
                &format!("{}/pulls/{number}/files", repo_path(repo)?),
                &[],
                None,
            )
            .await?;
        Ok(files.into_iter().map(|file| file.filename).collect())
    }

    async fn open_pulls(
        &self,
        account: &Account,
        repo: &str,
        query: &[(&str, &str)],
    ) -> Result<Vec<PullJson>, ApiError> {
        self.list(account, &format!("{}/pulls", repo_path(repo)?), query, None)
            .await
    }

    /// The checks of `sha`: the combined commit status, one status per
    /// context. A `warning` neither fails nor passes.
    async fn ci(&self, account: &Account, repo: &str, sha: &str) -> Result<Ci, ApiError> {
        if sha.is_empty() {
            return Ok(Ci::NoChecks);
        }
        let path = format!("{}/commits/{}/status", repo_path(repo)?, encode_path(sha));
        let statuses: Vec<StatusJson> = match self.list(account, &path, &[], Some("statuses")).await
        {
            Ok(statuses) => statuses,
            Err(err) if err.status == Some(404) => return Ok(Ci::NoChecks),
            Err(err) => return Err(err),
        };
        let base = self.base().ok();
        let mut failed = Vec::new();
        let (mut pending, mut passed) = (false, false);
        for status in &statuses {
            match status.status.as_str() {
                "failure" | "error" => {
                    let url = (!status.target_url.is_empty()).then(|| {
                        base.as_ref()
                            .and_then(|base| base.join(&status.target_url).ok())
                            .map_or_else(|| status.target_url.clone(), String::from)
                    });
                    failed.push(FailedCheck {
                        name: status.context.clone(),
                        url,
                    });
                }
                "pending" => pending = true,
                "success" => passed = true,
                _ => {}
            }
        }
        Ok(if !failed.is_empty() {
            Ci::Failed(failed)
        } else if pending {
            Ci::Pending
        } else if passed {
            Ci::Passed
        } else {
            Ci::NoChecks
        })
    }

    async fn comments(
        &self,
        account: &Account,
        repo: &str,
        number: u64,
        ignore: &[String],
    ) -> Result<Vec<Comment>, ApiError> {
        let ignored = |user: &Option<UserJson>| {
            user.as_ref().is_some_and(|user| {
                ignore
                    .iter()
                    .any(|login| login.eq_ignore_ascii_case(&user.login))
            })
        };
        let author = |user: &Option<UserJson>| {
            user.as_ref()
                .map_or_else(String::new, |user| user.login.clone())
        };
        let repo = repo_path(repo)?;
        let mut comments = Vec::new();
        let issue: Vec<CommentJson> = self
            .list(
                account,
                &format!("{repo}/issues/{number}/comments"),
                &[],
                None,
            )
            .await?;
        for comment in issue {
            if !comment.body.trim().is_empty() && !ignored(&comment.user) {
                comments.push(Comment {
                    author: author(&comment.user),
                    body: comment.body,
                    at: utc(&comment.created_at),
                    location: None,
                });
            }
        }
        let reviews: Vec<ReviewJson> = self
            .list(
                account,
                &format!("{repo}/pulls/{number}/reviews"),
                &[],
                None,
            )
            .await?;
        for review in reviews {
            if review.state == "PENDING" || ignored(&review.user) {
                continue;
            }
            if !review.body.trim().is_empty() {
                comments.push(Comment {
                    author: author(&review.user),
                    body: review.body.clone(),
                    at: utc(&review.submitted_at),
                    location: None,
                });
            }
            if review.comments_count == 0 {
                continue;
            }
            let inline: Vec<ReviewCommentJson> = self
                .get(
                    account,
                    &format!("{repo}/pulls/{number}/reviews/{}/comments", review.id),
                    &[],
                )
                .await?;
            for comment in inline {
                if comment.body.trim().is_empty() || ignored(&comment.user) {
                    continue;
                }
                let line = if comment.position > 0 {
                    comment.position
                } else {
                    comment.original_position
                };
                let location = match (comment.path.is_empty(), line) {
                    (true, _) => None,
                    (false, 0) => Some(comment.path.clone()),
                    (false, line) => Some(format!("{}:{line}", comment.path)),
                };
                comments.push(Comment {
                    author: author(&comment.user),
                    body: comment.body,
                    at: utc(&comment.created_at),
                    location,
                });
            }
        }
        comments.sort_by(|a, b| a.at.cmp(&b.at));
        Ok(comments)
    }

    async fn status(
        &self,
        account: &Account,
        repo: &str,
        number: u64,
        ignore: &[String],
    ) -> Result<PrStatus, ApiError> {
        let mut pull = self.pull(account, repo, number).await?;
        // `mergeable` is false for a conflict, but also while the server
        // works the state out, for a draft and after a failed calculation,
        // and the API cannot tell them apart. The pull request is never
        // reported as conflicting. While mergeability is unresolved it is
        // reported as pending instead, so it is not taken for ready.
        let mut rechecks = 0;
        while pull.state == "open" && pull.mergeable == Some(false) && rechecks < CONFLICT_RECHECKS
        {
            tokio::time::sleep(self.settle_interval).await;
            pull = self.pull(account, repo, number).await?;
            rechecks += 1;
        }
        let state = if pull.merged {
            PrState::Merged
        } else if pull.state == "open" {
            PrState::Open
        } else {
            PrState::Closed
        };
        let mut ci = self.ci(account, repo, &pull.head.sha).await?;
        let unresolved = pull.state == "open" && (pull.mergeable != Some(true) || pull.draft);
        if unresolved && matches!(ci, Ci::Passed | Ci::NoChecks) {
            ci = Ci::Pending;
        }
        let comments = self.comments(account, repo, number, ignore).await?;
        Ok(PrStatus {
            state,
            conflicting: false,
            ci,
            comments,
            head: pull.head.sha,
        })
    }

    async fn rerun_failed(
        &self,
        account: &Account,
        repo: &str,
        number: u64,
    ) -> Result<Rerun, String> {
        let head = self.pull(account, repo, number).await?.head.sha;
        let repo_path = repo_path(repo)?;
        let runs: Vec<RunJson> = match self
            .list(
                account,
                &format!("{repo_path}/actions/runs"),
                &[("head_sha", &head)],
                Some("workflow_runs"),
            )
            .await
        {
            Ok(runs) => runs,
            Err(err) if matches!(err.status, Some(404 | 405)) => {
                return Ok(self.no_rerun(&err));
            }
            Err(err) => return Err(err.text),
        };
        let mut started = Vec::new();
        for run in &runs {
            // A job rerun requeues the jobs that depend on it, so the jobs
            // are read again before each rerun. A completed job can be
            // rerun while its run is still active.
            let mut tried: Vec<u64> = Vec::new();
            loop {
                let jobs: Vec<JobJson> = match self
                    .get(
                        account,
                        &format!("{repo_path}/actions/runs/{}/jobs", run.id),
                        &[],
                    )
                    .await
                {
                    Ok(jobs) => jobs,
                    Err(err) if matches!(err.status, Some(404 | 405)) => {
                        return Ok(self.no_rerun(&err));
                    }
                    Err(err) => return Err(err.text),
                };
                let Some(job) = jobs
                    .iter()
                    .find(|job| job.status == "failure" && !tried.contains(&job.id))
                else {
                    break;
                };
                tried.push(job.id);
                match self
                    .call(
                        account,
                        Method::POST,
                        &format!("{repo_path}/actions/jobs/{}/rerun", job.id),
                        &[],
                        None,
                    )
                    .await
                {
                    Ok(_) => {
                        if !started.contains(&run.id) {
                            started.push(run.id);
                        }
                    }
                    Err(err) if matches!(err.status, Some(404 | 405)) && started.is_empty() => {
                        return Ok(self.no_rerun(&err));
                    }
                    Err(err) => return Err(err.text),
                }
            }
        }
        if started.is_empty() {
            return Ok(Rerun::NothingToRerun);
        }
        for _ in 0..self.settle_polls {
            if !matches!(
                self.status(account, repo, number, &[]).await?.ci,
                Ci::Failed(_)
            ) {
                break;
            }
            tokio::time::sleep(self.settle_interval).await;
        }
        Ok(Rerun::Started(started))
    }

    fn no_rerun(&self, err: &ApiError) -> Rerun {
        tracing::info!(
            "{} cannot rerun failed Actions jobs, which needs Forgejo 17: {}",
            self.base_url,
            err.text
        );
        Rerun::NothingToRerun
    }

    async fn merge(
        &self,
        account: &Account,
        repo: &str,
        number: u64,
        head: Option<&str>,
    ) -> Result<(), String> {
        let mut body = json!({ "Do": "squash", "delete_branch_after_merge": true });
        if let Some(head) = head {
            body["head_commit_id"] = json!(head);
        }
        self.call(
            account,
            Method::POST,
            &format!("{}/pulls/{number}/merge", repo_path(repo)?),
            &[],
            Some(&body),
        )
        .await?;
        Ok(())
    }

    async fn head_commit(
        &self,
        account: &Account,
        repo: &str,
        number: u64,
    ) -> Result<HeadCommit, String> {
        #[derive(Deserialize)]
        struct Entry {
            #[serde(rename = "type")]
            kind: String,
            #[serde(default)]
            body: String,
            #[serde(default)]
            created_at: String,
        }
        #[derive(Deserialize)]
        struct Push {
            #[serde(default)]
            commit_ids: Vec<String>,
        }
        #[derive(Deserialize)]
        struct Commit {
            created: Option<String>,
        }
        let head = self.pull(account, repo, number).await?.head.sha;
        let repo = repo_path(repo)?;
        let timeline: Vec<Entry> = self
            .list(
                account,
                &format!("{repo}/issues/{number}/timeline"),
                &[],
                None,
            )
            .await?;
        let pushed_at = timeline
            .iter()
            .filter(|entry| entry.kind == "pull_push")
            .filter(|entry| {
                serde_json::from_str::<Push>(&entry.body)
                    .is_ok_and(|push| push.commit_ids.contains(&head))
            })
            .map(|entry| utc(&entry.created_at))
            .next_back();
        let committed_at = match self
            .get::<Commit>(
                account,
                &format!("{repo}/git/commits/{}", encode_path(&head)),
                &[],
            )
            .await
        {
            Ok(commit) => commit.created.as_deref().map(utc),
            Err(err) if err.status == Some(404) => None,
            Err(err) => return Err(err.text),
        };
        Ok(HeadCommit {
            sha: head,
            pushed_at,
            committed_at,
        })
    }

    async fn file_at(
        &self,
        account: &Account,
        repo: &str,
        path: &str,
        git_ref: &str,
    ) -> Result<Option<String>, String> {
        let path = format!("{}/raw/{}", repo_path(repo)?, encode_path(path));
        match self
            .text(account, &path, &[("ref", git_ref)], MAX_DIFF * 5, false)
            .await
        {
            Ok(text) => Ok(Some(text)),
            Err(err) if err.status == Some(404) => Ok(None),
            Err(err) => Err(err.text),
        }
    }

    async fn failure_report(
        &self,
        account: &Account,
        repo: &str,
        checks: &[FailedCheck],
    ) -> Result<String, String> {
        let base = self.base()?;
        let repo_path = repo_path(repo)?;
        let mut jobs_by_run: HashMap<u64, Vec<JobJson>> = HashMap::new();
        let mut report = String::new();
        for check in checks {
            let _ = writeln!(report, "Check {:?} failed.", check.name);
            let Some(url) = check.url.as_deref() else {
                report.push_str("It is not a Forgejo Actions job, so it has no log here.\n\n");
                continue;
            };
            let Some((run_index, _)) = actions_job_ref(&base, repo, url) else {
                let _ = writeln!(
                    report,
                    "It is not a Forgejo Actions job, so it has no log here.\nDetails: {url}\n"
                );
                continue;
            };
            let log = async {
                if let Entry::Vacant(slot) = jobs_by_run.entry(run_index) {
                    let runs: Vec<RunJson> = self
                        .list(
                            account,
                            &format!("{repo_path}/actions/runs"),
                            &[("run_number", &run_index.to_string())],
                            Some("workflow_runs"),
                        )
                        .await?;
                    let jobs = match runs.iter().find(|run| run.index_in_repo == run_index) {
                        Some(run) => {
                            self.get(
                                account,
                                &format!("{repo_path}/actions/runs/{}/jobs", run.id),
                                &[],
                            )
                            .await?
                        }
                        None => Vec::new(),
                    };
                    slot.insert(jobs);
                }
                let wanted = base.join(without_attempt(url)).ok();
                let job = jobs_by_run[&run_index].iter().find(|job| {
                    wanted.is_some() && base.join(without_attempt(&job.html_url)).ok() == wanted
                });
                let Some(job) = job else {
                    return Err(Self::error(
                        None,
                        "matching the check",
                        None,
                        "no job has its URL",
                    ));
                };
                self.text(
                    account,
                    &format!("{repo_path}/actions/jobs/{}/logs", job.id),
                    &[],
                    MAX_LOG * 2,
                    true,
                )
                .await
            }
            .await;
            match log {
                Ok(log) => report.push_str(tail(&log, MAX_LOG)),
                Err(err) if err.status == Some(401) => return Err(err.text),
                Err(err) => {
                    let _ = writeln!(
                        report,
                        "Could not fetch its log: {}\nDetails: {url}",
                        err.text
                    );
                }
            }
            report.push('\n');
        }
        Ok(report)
    }
}

impl Forge for Forgejo {
    fn clone_url(&self, repo: &str) -> String {
        format!("{}/{repo}.git", self.base_url)
    }

    fn lookup_account<'a>(
        &'a self,
        token: &'a str,
    ) -> BoxFuture<'a, Result<ForgeAccount, LookupError>> {
        Box::pin(async move {
            let probe = Account {
                token: Some(token.to_owned()),
                login: String::new(),
                name: String::new(),
                email: String::new(),
            };
            let user: UserJson = self.get(&probe, "/user", &[]).await.map_err(|err| {
                if matches!(err.status, Some(401 | 403)) {
                    LookupError::Rejected
                } else {
                    LookupError::Failed(err.text.replace(token, "<redacted>"))
                }
            })?;
            if user.login.is_empty() {
                return Err(LookupError::Failed(
                    "GET /user answered without a login".to_owned(),
                ));
            }
            Ok(ForgeAccount {
                login: user.login,
                id: user.id,
            })
        })
    }

    fn access<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
    ) -> BoxFuture<'a, Result<Access, String>> {
        Box::pin(async move {
            #[derive(Default, Deserialize)]
            struct Permissions {
                #[serde(default)]
                admin: bool,
                #[serde(default)]
                push: bool,
            }
            #[derive(Deserialize)]
            struct Repository {
                #[serde(default)]
                permissions: Permissions,
            }
            match self
                .get::<Repository>(account, &repo_path(repo)?, &[])
                .await
            {
                Ok(repository) if repository.permissions.admin => Ok(Access::Admin),
                Ok(repository) if repository.permissions.push => Ok(Access::Write),
                Ok(_) => Ok(Access::Read),
                Err(err) if matches!(err.status, Some(403 | 404)) => Ok(Access::NotVisible),
                Err(err) => Err(err.text),
            }
        })
    }

    fn may_merge<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        number: u64,
        access: Access,
    ) -> BoxFuture<'a, Result<bool, String>> {
        Box::pin(async move {
            #[derive(Deserialize)]
            struct Branch {
                #[serde(default)]
                user_can_merge: bool,
            }
            if !matches!(access, Access::Write | Access::Admin) {
                return Ok(false);
            }
            let base = self.pull(account, repo, number).await?.base.name;
            let branch: Branch = self
                .get(
                    account,
                    &format!("{}/branches/{}", repo_path(repo)?, encode_path(&base)),
                    &[],
                )
                .await?;
            Ok(branch.user_can_merge)
        })
    }

    fn account(&self, token: String, found: &ForgeAccount) -> Account {
        Account {
            token: Some(token),
            login: found.login.clone(),
            name: found.login.clone(),
            email: format!("{}@noreply.{}", found.login, self.host()),
        }
    }

    fn git_auth_header(&self, account: &Account) -> Option<String> {
        let token = account.token.as_ref()?;
        let user = if account.login.is_empty() {
            "factory"
        } else {
            &account.login
        };
        let basic = STANDARD.encode(format!("{user}:{token}"));
        Some(format!("Authorization: Basic {basic}"))
    }

    fn compare_range(&self, body: &str, upstream: &str) -> Option<(String, String)> {
        let marker = format!(
            "{}/{}/compare/",
            self.base_url.to_ascii_lowercase(),
            upstream.to_ascii_lowercase()
        );
        let start = body.to_ascii_lowercase().find(&marker)? + marker.len();
        let link: String = body[start..]
            .chars()
            .take_while(|c| !c.is_whitespace() && !matches!(c, ')' | ']' | '>' | '"' | '\''))
            .collect();
        let (from, to) = link.split_once("...").or_else(|| link.split_once(".."))?;
        (!from.is_empty() && !to.is_empty()).then(|| (from.to_owned(), to.to_owned()))
    }

    fn merged_pr<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        branch: &'a str,
    ) -> BoxFuture<'a, Result<Option<u64>, String>> {
        Box::pin(async move {
            let closed = self
                .open_pulls(
                    account,
                    repo,
                    &[("state", "closed"), ("sort", "recentclose")],
                )
                .await?;
            Ok(closed
                .iter()
                .find(|pull| pull.merged && pull.is_from(branch))
                .map(|pull| pull.number))
        })
    }

    fn open_pr<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        base: &'a str,
        branch: &'a str,
        description: &'a PrDescription,
    ) -> BoxFuture<'a, Result<u64, String>> {
        Box::pin(async move {
            let path = format!("{}/pulls", repo_path(repo)?);
            let body = json!({
                "head": branch,
                "base": base,
                "title": description.title,
                "body": description.body,
            });
            match self
                .call(account, Method::POST, &path, &[], Some(&body))
                .await
            {
                Ok(reply) => {
                    let answer = reply.response.bytes().await.unwrap_or_default();
                    let pull: PullJson = serde_json::from_slice(&answer)
                        .map_err(|_| "POST pulls answered without a pull request".to_owned())?;
                    return Ok(pull.number);
                }
                Err(err) if err.status == Some(409) => {}
                Err(err) => return Err(err.text),
            }
            let open = self
                .open_pulls(account, repo, &[("state", "open"), ("base", base)])
                .await?;
            let existing = open
                .iter()
                .find(|pull| pull.is_from(branch) && pull.base.name == base)
                .ok_or_else(|| format!("{branch} has a pull request that is not open"))?;
            self.call(
                account,
                Method::PATCH,
                &format!("{path}/{}", existing.number),
                &[],
                Some(&json!({ "title": description.title, "body": description.body })),
            )
            .await?;
            Ok(existing.number)
        })
    }

    fn rerun_failed<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        number: u64,
    ) -> BoxFuture<'a, Result<Rerun, String>> {
        Box::pin(Forgejo::rerun_failed(self, account, repo, number))
    }

    fn merge<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        number: u64,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(Forgejo::merge(self, account, repo, number, None))
    }

    fn renovate_prs<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        base: &'a str,
        author: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Vec<RenovatePr>, String>> {
        Box::pin(async move {
            let author = author.ok_or_else(|| {
                format!(
                    "{} has no fixed Renovate account, so the project needs one",
                    self.host()
                )
            })?;
            let open = self
                .open_pulls(
                    account,
                    repo,
                    &[("state", "open"), ("base", base), ("poster", author)],
                )
                .await?;
            Ok(open
                .into_iter()
                .filter(|pull| pull.base.name == base)
                .filter(|pull| {
                    pull.user
                        .as_ref()
                        .is_some_and(|user| user.login.eq_ignore_ascii_case(author))
                })
                .map(|pull| RenovatePr {
                    number: pull.number,
                    title: pull.title,
                    url: pull.html_url,
                    head: pull.head.sha,
                })
                .collect())
        })
    }

    /// A listing is the pulls endpoint, one page, and null entries in it are
    /// skipped. Its title search is the issues endpoint with `type=pulls`,
    /// because the pulls endpoint has no query. That search also matches the
    /// body and has no title-only mode, so the rows are filtered to those
    /// whose title holds the text, and each is read again as a pull request
    /// for its branches. The page is cut before the filter, so when the forge
    /// fills the page and the filter leaves fewer than 21 open rows, the search
    /// is an error and not a short result.
    fn search_open_prs<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        query: &'a str,
    ) -> BoxFuture<'a, Result<Vec<OpenPr>, String>> {
        Box::pin(async move {
            let limit = SEARCH_PAGE.to_string();
            let path = repo_path(repo)?;
            let pulls: Vec<Option<PullJson>> = match PrQuery::parse(query) {
                PrQuery::Number(number) => match self.pull(account, repo, number).await {
                    Ok(pull) => vec![Some(pull)],
                    Err(err) if err.status == Some(404) => Vec::new(),
                    Err(err) => return Err(err.into()),
                },
                PrQuery::All => {
                    self.get(
                        account,
                        &format!("{path}/pulls"),
                        &[
                            ("state", "open"),
                            ("sort", "recentupdate"),
                            ("limit", &limit),
                        ],
                    )
                    .await?
                }
                PrQuery::Title(text) => {
                    #[derive(Deserialize)]
                    struct Found {
                        number: u64,
                        #[serde(default)]
                        title: String,
                    }
                    let found: Vec<Option<Found>> = self
                        .get(
                            account,
                            &format!("{path}/issues"),
                            &[
                                ("type", "pulls"),
                                ("state", "open"),
                                ("q", text),
                                ("sort", "recentupdate"),
                                ("limit", &limit),
                            ],
                        )
                        .await?;
                    let mut pulls = Vec::new();
                    let saturated = found.len() >= SEARCH_PAGE;
                    let needle = text.to_lowercase();
                    for row in found
                        .into_iter()
                        .flatten()
                        .filter(|row| row.title.to_lowercase().contains(&needle))
                    {
                        match self.pull(account, repo, row.number).await {
                            Ok(pull) if pull.state == "open" && !pull.merged => {
                                pulls.push(Some(pull));
                            }
                            Ok(_) => {}
                            Err(err) if err.status == Some(404) => {}
                            Err(err) => return Err(err.into()),
                        }
                    }
                    if saturated && pulls.len() < SEARCH_PAGE {
                        return Err(format!(
                            "{} cannot search pull request titles, and the first {SEARCH_PAGE} matches of {text:?} include other fields, so the title matches may be incomplete. Search for more of the title, or a number",
                            self.host()
                        ));
                    }
                    pulls
                }
            };
            Ok(pulls
                .into_iter()
                .flatten()
                .filter(|pull| pull.state == "open" && !pull.merged)
                .map(|pull| OpenPr {
                    fork: pull.is_fork(),
                    author: pull.author(),
                    number: pull.number,
                    title: pull.title,
                    head_branch: pull.head.name,
                    base_branch: pull.base.name,
                    draft: pull.draft,
                    url: pull.html_url,
                })
                .collect())
        })
    }

    fn pull_request<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        number: u64,
    ) -> BoxFuture<'a, Result<Option<PullRequest>, String>> {
        Box::pin(async move {
            let pull = match self.pull(account, repo, number).await {
                Ok(pull) => pull,
                Err(err) if err.status == Some(404) => return Ok(None),
                Err(err) => return Err(err.into()),
            };
            let state = if pull.merged {
                PrState::Merged
            } else if pull.state == "open" {
                PrState::Open
            } else {
                PrState::Closed
            };
            Ok(Some(PullRequest {
                number: pull.number,
                state,
                draft: pull.draft,
                fork: pull.is_fork(),
                author: pull.author(),
                head_repo_clone_url: pull.head.repo.and_then(|repo| repo.clone_url),
                head_branch: pull.head.name,
                head_sha: pull.head.sha,
                base_branch: pull.base.name,
                title: pull.title,
                body: pull.body.unwrap_or_default(),
                url: pull.html_url,
            }))
        })
    }

    fn pr_info<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        number: u64,
    ) -> BoxFuture<'a, Result<PrInfo, String>> {
        Box::pin(async move {
            for _ in 0..SNAPSHOT_TRIES {
                let pull = self.pull(account, repo, number).await?;
                let files = self.pull_files(account, repo, number).await?;
                if self.pull(account, repo, number).await?.head.sha == pull.head.sha {
                    return Ok(PrInfo {
                        title: pull.title,
                        body: pull.body.unwrap_or_default(),
                        head: pull.head.sha,
                        files,
                    });
                }
            }
            Err(moved(number))
        })
    }

    fn head_commit<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        number: u64,
    ) -> BoxFuture<'a, Result<HeadCommit, String>> {
        Box::pin(Forgejo::head_commit(self, account, repo, number))
    }

    fn merged_at<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        number: u64,
    ) -> BoxFuture<'a, Result<Option<String>, String>> {
        Box::pin(async move {
            let pull = self.pull(account, repo, number).await?;
            Ok(pull.merged_at.as_deref().map(utc))
        })
    }

    fn pr_details<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        number: u64,
    ) -> BoxFuture<'a, Result<PrDetails, String>> {
        Box::pin(async move {
            for _ in 0..SNAPSHOT_TRIES {
                let pull = self.pull(account, repo, number).await?;
                let files = self.pull_files(account, repo, number).await?;
                let diff = self
                    .text(
                        account,
                        &format!("{}/pulls/{number}.diff", repo_path(repo)?),
                        &[],
                        MAX_DIFF,
                        false,
                    )
                    .await
                    .map_err(|err| {
                        if err.text.ends_with("the answer is too large") {
                            format!(
                                "the diff of pull request {number} is larger than {MAX_DIFF} bytes"
                            )
                        } else {
                            err.text
                        }
                    })?;
                if self.pull(account, repo, number).await?.head.sha == pull.head.sha {
                    return Ok(PrDetails {
                        title: pull.title,
                        body: pull.body.unwrap_or_default(),
                        head: pull.head.sha,
                        files,
                        diff,
                    });
                }
            }
            Err(moved(number))
        })
    }

    fn request_renovate_rebase<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        number: u64,
        head: &'a str,
    ) -> BoxFuture<'a, Result<RebaseCheckbox, String>> {
        Box::pin(async move {
            let pull = self.pull(account, repo, number).await?;
            if pull.head.sha != head {
                return Ok(RebaseCheckbox::HeadMoved);
            }
            let body = pull.body.unwrap_or_default();
            let Some(ticked) = tick_rebase_checkbox(&body) else {
                return Ok(RebaseCheckbox::Missing);
            };
            if ticked != body {
                self.call(
                    account,
                    Method::PATCH,
                    &format!("{}/pulls/{number}", repo_path(repo)?),
                    &[],
                    Some(&json!({ "body": ticked })),
                )
                .await?;
            }
            Ok(RebaseCheckbox::Ticked)
        })
    }

    fn comment<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        number: u64,
        body: &'a str,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.call(
                account,
                Method::POST,
                &format!("{}/issues/{number}/comments", repo_path(repo)?),
                &[],
                Some(&json!({ "body": body })),
            )
            .await?;
            Ok(())
        })
    }

    fn approve<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        number: u64,
        head: &'a str,
        body: &'a str,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.call(
                account,
                Method::POST,
                &format!("{}/pulls/{number}/reviews", repo_path(repo)?),
                &[],
                Some(&json!({ "event": "APPROVED", "commit_id": head, "body": body })),
            )
            .await?;
            Ok(())
        })
    }

    fn merge_head<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        number: u64,
        head: &'a str,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(Forgejo::merge(self, account, repo, number, Some(head)))
    }

    fn file_at<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        path: &'a str,
        git_ref: &'a str,
    ) -> BoxFuture<'a, Result<Option<String>, String>> {
        Box::pin(Forgejo::file_at(self, account, repo, path, git_ref))
    }

    fn compare_patch<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        from: &'a str,
        to: &'a str,
        path: &'a str,
    ) -> BoxFuture<'a, Result<Option<String>, String>> {
        Box::pin(async move {
            let old = Forgejo::file_at(self, account, repo, path, from).await?;
            let new = Forgejo::file_at(self, account, repo, path, to).await?;
            if old == new {
                return Ok(None);
            }
            Ok(unified_diff(
                old.as_deref().unwrap_or_default(),
                new.as_deref().unwrap_or_default(),
            ))
        })
    }

    fn status<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        number: u64,
        ignore: &'a [String],
    ) -> BoxFuture<'a, Result<PrStatus, String>> {
        Box::pin(async move { Ok(Forgejo::status(self, account, repo, number, ignore).await?) })
    }

    fn failure_report<'a>(
        &'a self,
        account: &'a Account,
        repo: &'a str,
        checks: &'a [FailedCheck],
    ) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(Forgejo::failure_report(self, account, repo, checks))
    }
}
