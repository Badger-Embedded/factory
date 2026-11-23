//! The Salsa onboarding checks, against a fake `glab`.

mod support;

use std::path::Path;

use factory_runner::{Account, GitLab};
use factory_server::{GlabRepos, RepoChecker, RepoReport};

fn account() -> Account {
    Account {
        token: Some("tok-secret".into()),
        login: "onboarder".into(),
        name: "onboarder".into(),
        email: "1-onboarder@users.noreply.salsa.debian.org".into(),
    }
}

struct Salsa {
    level: u32,
    squash: &'static str,
    branch: bool,
    protected: bool,
    /// The protected branch rules, as the JSON array GitLab lists. Level 0
    /// is GitLab's "No one".
    rules: &'static str,
    /// The second page of rules.
    rules_page2: &'static str,
    /// Whether the pipeline listing answers HTTP 403, as with CI/CD disabled.
    pipelines_forbidden: bool,
    /// The ref a pipeline of the base branch's head commit ran on, if any.
    pipeline_ref: Option<&'static str>,
}

impl Salsa {
    fn fit() -> Salsa {
        Salsa {
            level: 30,
            squash: "default_on",
            branch: true,
            protected: true,
            rules: r#"[{"name":"main","merge_access_levels":[{"access_level":30}]}]"#,
            rules_page2: "[]",
            pipelines_forbidden: false,
            pipeline_ref: Some("main"),
        }
    }

    /// Installs a `glab` that logs the host and token of each call and answers
    /// the endpoints the checks read.
    fn repos(&self, dir: &Path) -> GlabRepos {
        let branch = if self.branch {
            format!(
                r#"echo '{{"protected":{},"commit":{{"id":"abc123"}}}}'"#,
                self.protected
            )
        } else {
            "echo 'glab: 404 Branch Not Found (HTTP 404)' >&2; exit 1".to_owned()
        };
        let pipelines = match self.pipeline_ref {
            Some(r) => format!(r#"[{{"id":5,"ref":"{r}"}}]"#),
            None => "[]".to_owned(),
        };
        let pipeline_ref = self.pipeline_ref.unwrap_or("none");
        let script = format!(
            r#"#!/bin/sh
echo "$GITLAB_HOST $GITLAB_TOKEN $*" >> "{log}"
case "$4" in
  projects/team%2Fsub%2Fpkg) echo '{{"squash_option":"{squash}","permissions":{{"project_access":{{"access_level":{level}}},"group_access":null}}}}' ;;
  projects/team%2Fsub%2Fpkg/repository/branches/main) {branch} ;;
  'projects/team%2Fsub%2Fpkg/protected_branches?per_page=100&page=1') echo '{rules}' ;;
  'projects/team%2Fsub%2Fpkg/protected_branches?per_page=100&page=2') echo '{rules_page2}' ;;
  'projects/team%2Fsub%2Fpkg/protected_branches?per_page=100&page=3') echo '[]' ;;
  projects/team%2Fsub%2Fpkg/pipelines*) {pipelines_answer} ;;
  *) echo 'glab: 404 Not Found (HTTP 404)' >&2; exit 1 ;;
esac
"#,
            log = dir.join("calls.log").display(),
            squash = self.squash,
            level = self.level,
            rules = self.rules,
            rules_page2 = self.rules_page2,
            pipelines_answer = if self.pipelines_forbidden {
                "echo 'glab: 403 Forbidden (HTTP 403)' >&2; exit 1".to_owned()
            } else {
                format!(
                    r#"if [ "$4" = "projects/team%2Fsub%2Fpkg/pipelines?ref={pipeline_ref}&sha=abc123&per_page=1" ]; then echo '{pipelines}'; else echo '[]'; fi"#
                )
            },
        );
        GlabRepos {
            gitlab: GitLab {
                glab: support::executable(dir, "glab", &script),
                ..GitLab::default()
            },
        }
    }
}

async fn check(salsa: &Salsa, repo: &str) -> (RepoReport, String) {
    let dir = tempfile::tempdir().unwrap();
    let repos = salsa.repos(dir.path());
    let report = repos.check(&account(), repo, "main").await.unwrap();
    let log = std::fs::read_to_string(dir.path().join("calls.log")).unwrap_or_default();
    (report, log)
}

#[tokio::test]
async fn a_developer_onboards_a_nested_project_with_no_warning() {
    let (report, log) = check(&Salsa::fit(), "team/sub/pkg").await;
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    assert!(!log.is_empty(), "glab was never called");
    for line in log.lines() {
        assert!(
            line.starts_with("salsa.debian.org tok-secret api "),
            "{line}"
        );
    }
}

#[tokio::test]
async fn a_reporter_cannot_push() {
    let (report, _) = check(
        &Salsa {
            level: 20,
            ..Salsa::fit()
        },
        "team/sub/pkg",
    )
    .await;
    assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
    assert!(report.errors[0].contains("onboarder has no push access"));
}

#[tokio::test]
async fn a_missing_base_branch_is_an_error() {
    let (report, _) = check(
        &Salsa {
            branch: false,
            ..Salsa::fit()
        },
        "team/sub/pkg",
    )
    .await;
    assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
    assert!(report.errors[0].contains("base branch \"main\" does not exist"));
}

#[tokio::test]
async fn squash_set_to_never_is_an_error() {
    let (report, _) = check(
        &Salsa {
            squash: "never",
            ..Salsa::fit()
        },
        "team/sub/pkg",
    )
    .await;
    assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
    assert!(report.errors[0].contains("squash"));
}

#[tokio::test]
async fn a_missing_or_invisible_project_is_an_error() {
    let (report, _) = check(&Salsa::fit(), "team/other").await;
    assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
    assert!(report.errors[0].contains("not visible to onboarder"));
}

#[tokio::test]
async fn a_malformed_path_is_refused_without_a_call() {
    for repo in ["pkg", "team//pkg", "team/pk g", "/team/pkg"] {
        let (report, log) = check(&Salsa::fit(), repo).await;
        assert_eq!(report.errors.len(), 1, "{repo}");
        assert!(report.errors[0].contains("project path"), "{repo}");
        assert!(log.is_empty(), "{repo}: {log}");
    }
}

#[tokio::test]
async fn a_maintainer_onboards_with_a_warning() {
    let (report, _) = check(
        &Salsa {
            level: 40,
            rules: r#"[{"name":"main","merge_access_levels":[{"access_level":40}]}]"#,
            ..Salsa::fit()
        },
        "team/sub/pkg",
    )
    .await;
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
    assert!(report.warnings[0].contains("Maintainer or Owner"));
}

#[tokio::test]
async fn an_unprotected_base_branch_warns() {
    let (report, _) = check(
        &Salsa {
            protected: false,
            ..Salsa::fit()
        },
        "team/sub/pkg",
    )
    .await;
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
    assert!(report.warnings[0].contains("is not protected"));
}

#[tokio::test]
async fn a_base_branch_head_without_a_pipeline_warns() {
    let (report, _) = check(
        &Salsa {
            pipeline_ref: None,
            ..Salsa::fit()
        },
        "team/sub/pkg",
    )
    .await;
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
    assert!(report.warnings[0].contains("no pipeline"));
}

#[tokio::test]
async fn protection_that_keeps_a_developer_from_merging_warns() {
    let (report, _) = check(
        &Salsa {
            rules: r#"[{"name":"main","merge_access_levels":[{"access_level":40}]}]"#,
            ..Salsa::fit()
        },
        "team/sub/pkg",
    )
    .await;
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
    assert!(report.warnings[0].contains("auto-merge will not work"));
}

#[tokio::test]
async fn a_failing_glab_is_an_error_without_the_token() {
    let dir = tempfile::tempdir().unwrap();
    let script = "#!/bin/sh\necho \"glab: boom tok-secret\" >&2\nexit 1\n";
    let repos = GlabRepos {
        gitlab: GitLab {
            glab: support::executable(dir.path(), "glab", script),
            ..GitLab::default()
        },
    };
    let err = repos
        .check(&account(), "team/sub/pkg", "main")
        .await
        .unwrap_err();
    assert!(err.contains("boom"), "{err}");
    assert!(!err.contains("tok-secret"), "{err}");
}

fn with_rules(rules: &'static str) -> Salsa {
    Salsa {
        rules,
        ..Salsa::fit()
    }
}

#[tokio::test]
async fn a_no_one_rule_does_not_let_a_developer_merge() {
    let salsa = with_rules(r#"[{"name":"main","merge_access_levels":[{"access_level":0}]}]"#);
    let (report, _) = check(&salsa, "team/sub/pkg").await;
    assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
    assert!(report.warnings[0].contains("auto-merge will not work"));
}

#[tokio::test]
async fn a_wildcard_only_restriction_warns() {
    let salsa = with_rules(r#"[{"name":"ma*","merge_access_levels":[{"access_level":40}]}]"#);
    let (report, _) = check(&salsa, "team/sub/pkg").await;
    assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
    assert!(report.warnings[0].contains("auto-merge will not work"));
}

#[tokio::test]
async fn a_more_permissive_wildcard_beats_a_strict_exact_rule() {
    let salsa = with_rules(
        r#"[{"name":"main","merge_access_levels":[{"access_level":40}]},{"name":"*","merge_access_levels":[{"access_level":30}]}]"#,
    );
    let (report, _) = check(&salsa, "team/sub/pkg").await;
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
}

#[tokio::test]
async fn a_rule_that_names_a_user_is_not_judged() {
    let salsa =
        with_rules(r#"[{"name":"main","merge_access_levels":[{"access_level":40,"user_id":9}]}]"#);
    let (report, _) = check(&salsa, "team/sub/pkg").await;
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
}

#[tokio::test]
async fn a_pipeline_on_another_ref_is_not_ci_of_the_base_branch() {
    let salsa = Salsa {
        pipeline_ref: Some("feature"),
        ..Salsa::fit()
    };
    let (report, _) = check(&salsa, "team/sub/pkg").await;
    assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
    assert!(report.warnings[0].contains("no pipeline"));
}

#[tokio::test]
async fn a_grant_on_the_second_page_of_rules_lets_a_developer_merge() {
    let salsa = Salsa {
        rules_page2: r#"[{"name":"*","merge_access_levels":[{"access_level":30}]}]"#,
        ..with_rules(r#"[{"name":"main","merge_access_levels":[{"access_level":40}]}]"#)
    };
    let (report, _) = check(&salsa, "team/sub/pkg").await;
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
}

#[tokio::test]
async fn disabled_ci_is_a_warning_and_not_an_error() {
    let salsa = Salsa {
        pipelines_forbidden: true,
        ..Salsa::fit()
    };
    let (report, _) = check(&salsa, "team/sub/pkg").await;
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
    assert!(report.warnings[0].contains("no pipeline"));
}
