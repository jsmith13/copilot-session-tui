//! `cst doctor` — report what CST found in the environment.
//!
//! Every dependency failure used to be reactive: you learned the Copilot CLI was
//! missing when a session refused to start, and that `gh` was missing when a GitHub
//! item refused to open. That is fine for the person who wrote CST and useless for
//! anyone else, so this states the situation up front.
//!
//! Only the Copilot CLI is required. Everything else names the single feature it
//! unlocks, so a user who keeps their code somewhere other than GitHub can read
//! "not installed" and correctly conclude that it does not matter to them.
//!
//! Probing is confined to [`gather`]; every line of wording is produced by a pure
//! function, so the report can be tested without any of these programs installed.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::github::{GhAuth, GhAuthHost};
use crate::hook_plugin::PluginStatus;
use crate::session::manager::CopilotProbe;

/// Copilot CLI release that introduced `--session-id`, which CST relies on to bind a
/// scratchpad and terminal to a session from the moment it starts.
const COPILOT_MINIMUM: &str = "1.0.51";
/// Release from which the authoritative lifecycle hooks work.
const COPILOT_RECOMMENDED: &str = "1.0.82";

/// `gh auth status` reaches the network. Long enough for a slow connection, short
/// enough that a VPN or captive portal cannot make the installer look hung.
const GH_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Importance {
    Required,
    Optional,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    Ok,
    /// Present but not usable as-is — `gh` installed but logged out, a settings file
    /// that does not parse.
    Degraded,
    Missing,
    /// The probe itself could not answer, which is not the same as a negative result.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub name: &'static str,
    pub importance: Importance,
    pub health: Health,
    pub detail: String,
    /// What the user loses while this is not `Ok`. Required checks leave this unset —
    /// losing CST entirely does not need naming.
    pub feature: Option<&'static str>,
    pub remedy: Option<String>,
}

impl Check {
    fn ok(name: &'static str, importance: Importance, detail: impl Into<String>) -> Self {
        Self {
            name,
            importance,
            health: Health::Ok,
            detail: detail.into(),
            feature: None,
            remedy: None,
        }
    }

    fn unhealthy(
        name: &'static str,
        importance: Importance,
        health: Health,
        detail: impl Into<String>,
        feature: Option<&'static str>,
        remedy: impl Into<String>,
    ) -> Self {
        Self {
            name,
            importance,
            health,
            detail: detail.into(),
            feature,
            remedy: Some(remedy.into()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub cst_version: &'static str,
    pub checks: Vec<Check>,
}

impl Report {
    /// Whether CST cannot do its job. Optional gaps deliberately do not count, so the
    /// installer can run this without a missing `gh` turning a good install red.
    pub fn has_required_failure(&self) -> bool {
        self.checks
            .iter()
            .any(|check| check.importance == Importance::Required && check.health != Health::Ok)
    }

    fn optional_gaps(&self) -> usize {
        self.checks
            .iter()
            .filter(|check| check.importance == Importance::Optional && check.health != Health::Ok)
            .count()
    }
}

/// Run every probe. The only impure function here.
pub fn gather(copilot_home: &Path) -> Report {
    let copilot = crate::session::manager::copilot_probe();
    let copilot_found = copilot.is_some();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let auth = crate::github::probe_auth(&cwd, GH_TIMEOUT);

    // Probing hooks without a Copilot CLI just produces a confusing "failed to start"
    // for a reason the report already states one line above.
    let hooks = copilot_found.then(|| crate::hook_plugin::status(copilot_home));

    let config_path = crate::config::config_path();
    let config = crate::config::load_existing_base_config(&config_path);

    Report {
        cst_version: env!("CARGO_PKG_VERSION"),
        checks: vec![
            copilot_check(copilot.as_ref()),
            gh_installed_check(&auth),
            gh_auth_check(&auth),
            git_check(crate::session::worktree::git_version()),
            tmux_check(crate::session::tmux::TmuxSupport::detect()),
            hooks_check(hooks),
            config_check(&config_path, config.as_ref().map(Option::is_some)),
        ],
    }
}

fn tmux_check(support: crate::session::tmux::TmuxSupport) -> Check {
    const NAME: &str = "tmux";
    const FEATURE: &str = "persistent sessions that survive CST exiting";
    match support {
        crate::session::tmux::TmuxSupport::Available => {
            Check::ok(NAME, Importance::Optional, "installed.".to_string())
        }
        crate::session::tmux::TmuxSupport::Unavailable(reason)
            if !crate::session::tmux::supported_platform() =>
        {
            // Not installable here, so no remedy to suggest; the report should not
            // read like something is wrong with the machine.
            Check::ok(
                NAME,
                Importance::Optional,
                format!("not applicable: {reason}."),
            )
        }
        crate::session::tmux::TmuxSupport::Unavailable(reason) => Check::unhealthy(
            NAME,
            Importance::Optional,
            Health::Missing,
            format!("{reason}."),
            Some(FEATURE),
            "Install tmux if you want sessions that keep running outside CST.".to_string(),
        ),
    }
}

fn copilot_check(probe: Option<&CopilotProbe>) -> Check {
    const NAME: &str = "Copilot CLI";
    let Some(probe) = probe else {
        return Check::unhealthy(
            NAME,
            Importance::Required,
            Health::Missing,
            "not found on PATH. CST cannot start or resume sessions without it.".to_string(),
            None,
            format!(
                "Install GitHub Copilot CLI {COPILOT_MINIMUM} or newer from \
                 https://github.com/github/copilot-cli",
            ),
        );
    };

    let where_it_is = match crate::updater::find_on_path(Path::new(&probe.program)) {
        Some(path) => format!("found at {}", path.display()),
        None => format!("found as {}", probe.program),
    };

    if !probe.version_ok {
        return Check::unhealthy(
            NAME,
            Importance::Required,
            Health::Degraded,
            format!("{where_it_is}, but `copilot --version` failed. This may not be the GitHub Copilot CLI."),
            None,
            "Check that the copilot on your PATH is GitHub Copilot CLI and not another program of the same name.".to_string(),
        );
    }

    let Some(version) = probe.version.as_deref() else {
        return Check::ok(
            NAME,
            Importance::Required,
            format!("{where_it_is}, but it did not report a version."),
        );
    };

    match version_number(version) {
        Some(number) if is_older_than(&number, COPILOT_MINIMUM) => Check::unhealthy(
            NAME,
            Importance::Required,
            Health::Degraded,
            format!("{where_it_is}, reporting {version}, which is older than {COPILOT_MINIMUM}."),
            None,
            format!(
                "Upgrade to {COPILOT_MINIMUM} or newer; CST names each session itself, \
                 which needs the --session-id option added in that release."
            ),
        ),
        Some(number) if is_older_than(&number, COPILOT_RECOMMENDED) => Check::unhealthy(
            NAME,
            Importance::Optional,
            Health::Degraded,
            format!("{where_it_is}, reporting {version}."),
            Some("authoritative session progress from the lifecycle hooks"),
            format!("Upgrade to {COPILOT_RECOMMENDED} or newer to enable the lifecycle hooks."),
        ),
        _ => Check::ok(
            NAME,
            Importance::Required,
            format!("{where_it_is}, reporting {version}."),
        ),
    }
}

const GH_FEATURE: &str = "GitHub issue, pull request and discussion inspection";

fn gh_installed_check(auth: &GhAuth) -> Check {
    const NAME: &str = "GitHub CLI";
    match auth {
        GhAuth::NotInstalled => Check::unhealthy(
            NAME,
            Importance::Optional,
            Health::Missing,
            "not installed.".to_string(),
            Some(GH_FEATURE),
            "Install https://cli.github.com/ if you want to open GitHub items inside CST."
                .to_string(),
        ),
        GhAuth::Unknown(detail) => Check::unhealthy(
            NAME,
            Importance::Optional,
            Health::Unknown,
            format!("could not be checked: {detail}"),
            Some(GH_FEATURE),
            "Run `gh auth status` yourself to see what it reports.".to_string(),
        ),
        _ => Check::ok(NAME, Importance::Optional, "installed.".to_string()),
    }
}

fn gh_auth_check(auth: &GhAuth) -> Check {
    const NAME: &str = "GitHub authentication";
    match auth {
        GhAuth::NotInstalled => Check::ok(
            NAME,
            Importance::Optional,
            "not checked, because the GitHub CLI is not installed.".to_string(),
        ),
        GhAuth::Unknown(_) => Check::ok(
            NAME,
            Importance::Optional,
            "not checked, because `gh auth status` did not answer.".to_string(),
        ),
        GhAuth::NoHosts => Check::unhealthy(
            NAME,
            Importance::Optional,
            Health::Degraded,
            "the GitHub CLI is installed but not logged in to any host.".to_string(),
            Some(GH_FEATURE),
            "Run `gh auth login`.".to_string(),
        ),
        GhAuth::LoggedIn(hosts) => {
            let summary = describe_hosts(hosts);
            if hosts.iter().all(|host| host.healthy) {
                let mut check = Check::ok(NAME, Importance::Optional, format!("{summary}."));
                // CST uses the host of each repository's remote, so being logged in
                // somewhere is not the same as being logged in where your code is.
                // Naming the hosts is what makes that state visible at all.
                check.remedy = Some(
                    "CST uses the host of each repository's remote; run \
                     `gh auth login --hostname HOST` for any host not listed."
                        .to_string(),
                );
                check
            } else {
                Check::unhealthy(
                    NAME,
                    Importance::Optional,
                    Health::Degraded,
                    format!("{summary}."),
                    Some(GH_FEATURE),
                    "Run `gh auth login --hostname HOST` for the hosts that failed.".to_string(),
                )
            }
        }
    }
}

fn describe_hosts(hosts: &[GhAuthHost]) -> String {
    hosts
        .iter()
        .map(|host| {
            let state = if host.healthy {
                "logged in"
            } else {
                "NOT working"
            };
            match &host.account {
                Some(account) => format!("{} as {account} ({state})", host.host),
                None => format!("{} ({state})", host.host),
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn git_check(version: Option<String>) -> Check {
    const NAME: &str = "Git";
    match version {
        Some(version) => {
            let where_it_is = match crate::updater::find_on_path(Path::new("git")) {
                Some(path) => format!("found at {}", path.display()),
                None => "found".to_string(),
            };
            Check::ok(
                NAME,
                Importance::Optional,
                format!("{where_it_is}, reporting {version}."),
            )
        }
        None => Check::unhealthy(
            NAME,
            Importance::Optional,
            Health::Missing,
            "not found on PATH.".to_string(),
            Some("isolated branch-backed worktree sessions"),
            "Install Git if you want CST to create a worktree per session.".to_string(),
        ),
    }
}

const HOOKS_FEATURE: &str = "authoritative session progress in the tab strip";

fn hooks_check(status: Option<anyhow::Result<PluginStatus>>) -> Check {
    const NAME: &str = "Copilot lifecycle hooks";
    match status {
        None => Check::ok(
            NAME,
            Importance::Optional,
            "not checked, because the Copilot CLI was not found.".to_string(),
        ),
        Some(Ok(PluginStatus::Installed)) => {
            Check::ok(NAME, Importance::Optional, "installed.".to_string())
        }
        Some(Ok(PluginStatus::NotInstalled)) => Check::unhealthy(
            NAME,
            Importance::Optional,
            Health::Missing,
            "not installed. CST falls back to reading terminal escape sequences.".to_string(),
            Some(HOOKS_FEATURE),
            "Run `cst hooks install`.".to_string(),
        ),
        Some(Err(error)) => Check::unhealthy(
            NAME,
            Importance::Optional,
            Health::Unknown,
            format!("could not be checked: {error:#}"),
            Some(HOOKS_FEATURE),
            "Run `cst hooks status` yourself to see what it reports.".to_string(),
        ),
    }
}

/// `parsed` is `Ok(true)` when the file exists and parses, `Ok(false)` when it is
/// simply absent, and `Err` when it exists but is broken.
fn config_check(path: &Path, parsed: Result<bool, &anyhow::Error>) -> Check {
    const NAME: &str = "Settings file";
    match parsed {
        // A fresh install has no settings file and must not look broken because of it.
        Ok(false) => Check::ok(
            NAME,
            Importance::Optional,
            format!("{} does not exist yet, so defaults apply.", path.display()),
        ),
        Ok(true) => Check::ok(
            NAME,
            Importance::Optional,
            format!("{} loads cleanly.", path.display()),
        ),
        Err(error) => Check::unhealthy(
            NAME,
            Importance::Optional,
            Health::Degraded,
            format!("{} could not be read: {error:#}", path.display()),
            Some("every setting in that file; CST starts with defaults instead"),
            "Fix or delete that file. CST currently ignores it silently at startup.".to_string(),
        ),
    }
}

/// Extract a dotted version from a banner such as `GitHub Copilot CLI 1.0.84-3`.
fn version_number(banner: &str) -> Option<String> {
    banner
        .split_whitespace()
        .find(|word| {
            let head = word.split(['-', '+']).next().unwrap_or(word);
            head.contains('.') && head.starts_with(|c: char| c.is_ascii_digit())
        })
        .map(|word| word.split(['-', '+']).next().unwrap_or(word).to_string())
}

/// Numeric comparison of dotted versions, tolerant of a missing or odd component.
///
/// Deliberately not semver: Copilot reports `1.0.84-3`, whose suffix is not a semver
/// pre-release, and treating it as one would make it sort *below* `1.0.84`.
fn is_older_than(version: &str, minimum: &str) -> bool {
    let parts = |text: &str| -> Vec<u64> {
        text.split('.')
            .map(|part| part.parse::<u64>().unwrap_or(0))
            .collect()
    };
    let (left, right) = (parts(version), parts(minimum));
    for index in 0..left.len().max(right.len()) {
        let a = left.get(index).copied().unwrap_or(0);
        let b = right.get(index).copied().unwrap_or(0);
        if a != b {
            return a < b;
        }
    }
    false
}

/// Render the report.
///
/// Plain ASCII, no colour and no check or cross glyphs: this runs at the end of both
/// installers, in whatever console the user happens to have, and a Windows host can
/// mangle the glyphs or paint a whole stderr stream red.
pub fn render(report: &Report, out: &mut impl Write) -> io::Result<()> {
    writeln!(
        out,
        "cst doctor — copilot-session-tui {}",
        report.cst_version
    )?;

    for importance in [Importance::Required, Importance::Optional] {
        let checks: Vec<&Check> = report
            .checks
            .iter()
            .filter(|check| check.importance == importance)
            .collect();
        if checks.is_empty() {
            continue;
        }
        writeln!(out)?;
        writeln!(
            out,
            "{}",
            match importance {
                Importance::Required => "Required",
                Importance::Optional => "Optional",
            }
        )?;
        for check in checks {
            let prefix = if check.health == Health::Missing {
                "MISSING - "
            } else {
                ""
            };
            writeln!(out, "  {}{}: {}", prefix, check.name, check.detail)?;
            if check.health != Health::Ok {
                if let Some(feature) = check.feature {
                    writeln!(out, "    Without it: {feature}.")?;
                }
            }
            if let Some(remedy) = &check.remedy {
                writeln!(out, "    {remedy}")?;
            }
        }
    }

    writeln!(out)?;
    if report.has_required_failure() {
        writeln!(
            out,
            "Something CST requires is missing, so sessions cannot start."
        )?;
    } else {
        let gaps = report.optional_gaps();
        if gaps == 0 {
            writeln!(out, "Everything is present.")?;
        } else {
            writeln!(
                out,
                "Everything required is present. {gaps} optional feature{} unavailable, \
                 which is fine if you do not use {}.",
                if gaps == 1 { " is" } else { "s are" },
                if gaps == 1 { "it" } else { "them" }
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(version: Option<&str>, version_ok: bool) -> CopilotProbe {
        CopilotProbe {
            program: "copilot".to_string(),
            version: version.map(str::to_string),
            version_ok,
        }
    }

    fn render_to_string(report: &Report) -> String {
        let mut buffer = Vec::new();
        render(report, &mut buffer).unwrap();
        String::from_utf8(buffer).unwrap()
    }

    fn report_of(checks: Vec<Check>) -> Report {
        Report {
            cst_version: "9.9.9",
            checks,
        }
    }

    #[test]
    fn a_missing_required_dependency_is_the_only_thing_that_fails_the_report() {
        let missing_required = report_of(vec![copilot_check(None)]);
        assert!(missing_required.has_required_failure());

        let missing_optional = report_of(vec![
            copilot_check(Some(&probe(Some("GitHub Copilot CLI 1.0.84"), true))),
            gh_installed_check(&GhAuth::NotInstalled),
            git_check(None),
        ]);
        assert!(
            !missing_optional.has_required_failure(),
            "a user on another forge must not be told their install is broken"
        );
        assert_eq!(missing_optional.optional_gaps(), 2);
    }

    #[test]
    fn every_unavailable_optional_check_names_the_feature_that_is_lost_not_just_the_binary() {
        let checks = vec![
            gh_installed_check(&GhAuth::NotInstalled),
            gh_auth_check(&GhAuth::NoHosts),
            git_check(None),
            hooks_check(Some(Ok(PluginStatus::NotInstalled))),
        ];
        for check in &checks {
            assert!(
                check.feature.is_some(),
                "{} says what is missing but not what it costs",
                check.name
            );
        }
        let rendered = render_to_string(&report_of(checks));
        assert!(rendered.contains("Without it: GitHub issue"), "{rendered}");
        assert!(
            rendered.contains("Without it: isolated branch"),
            "{rendered}"
        );
    }

    #[test]
    fn every_unhealthy_check_tells_the_user_what_to_do_about_it() {
        let checks = vec![
            copilot_check(None),
            gh_installed_check(&GhAuth::NotInstalled),
            gh_auth_check(&GhAuth::NoHosts),
            git_check(None),
            hooks_check(Some(Ok(PluginStatus::NotInstalled))),
        ];
        for check in &checks {
            assert!(
                check.remedy.is_some(),
                "{} leaves the user stuck",
                check.name
            );
        }
    }

    #[test]
    fn the_report_is_plain_ascii_so_every_console_can_print_it() {
        let report = report_of(vec![
            copilot_check(Some(&probe(Some("GitHub Copilot CLI 1.0.84"), true))),
            gh_auth_check(&GhAuth::LoggedIn(vec![GhAuthHost {
                host: "github.com".to_string(),
                account: Some("someone".to_string()),
                healthy: true,
            }])),
            git_check(None),
        ]);
        let rendered = render_to_string(&report);
        // The em dash in the title is the one deliberate exception; nothing inside a
        // check line may need a code page.
        for line in rendered.lines().skip(1) {
            assert!(line.is_ascii(), "not printable everywhere: {line}");
        }
    }

    #[test]
    fn the_report_separates_required_from_optional_so_a_gap_can_be_judged() {
        let rendered = render_to_string(&report_of(vec![
            copilot_check(Some(&probe(Some("GitHub Copilot CLI 1.0.84"), true))),
            git_check(None),
        ]));
        let required = rendered.find("Required").expect("required heading");
        let optional = rendered.find("Optional").expect("optional heading");
        assert!(required < optional, "got:\n{rendered}");
        assert!(rendered.contains("MISSING - Git"), "got:\n{rendered}");
    }

    #[test]
    fn a_copilot_older_than_the_session_id_release_fails_the_report() {
        let old = copilot_check(Some(&probe(Some("GitHub Copilot CLI 1.0.40"), true)));
        assert_eq!(old.importance, Importance::Required);
        assert_eq!(old.health, Health::Degraded);
        assert!(old.detail.contains("older than 1.0.51"), "{old:?}");
    }

    #[test]
    fn a_copilot_too_old_only_for_hooks_is_an_optional_gap_not_a_broken_install() {
        let middling = copilot_check(Some(&probe(Some("GitHub Copilot CLI 1.0.60"), true)));
        assert_eq!(middling.importance, Importance::Optional);
        assert!(!report_of(vec![middling]).has_required_failure());
    }

    #[test]
    fn a_copilot_that_cannot_report_its_version_is_still_a_working_installation() {
        let check = copilot_check(Some(&probe(None, true)));
        assert_eq!(check.health, Health::Ok);
    }

    #[test]
    fn a_copilot_that_fails_its_version_call_is_reported_as_suspect() {
        let check = copilot_check(Some(&probe(None, false)));
        assert_eq!(check.health, Health::Degraded);
        assert_eq!(check.importance, Importance::Required);
    }

    #[test]
    fn a_prerelease_suffix_does_not_make_a_version_look_older_than_it_is() {
        // Copilot reports 1.0.84-3, which is not a semver pre-release; treating it as
        // one would sort it below 1.0.84 and warn about a perfectly good install.
        assert_eq!(
            version_number("GitHub Copilot CLI 1.0.84-3"),
            Some("1.0.84".to_string())
        );
        assert!(!is_older_than("1.0.84", COPILOT_MINIMUM));
        assert!(is_older_than("1.0.9", "1.0.51"));
        assert!(!is_older_than("1.1", "1.0.51"));
    }

    #[test]
    fn being_logged_into_one_host_still_says_how_to_reach_another() {
        let check = gh_auth_check(&GhAuth::LoggedIn(vec![GhAuthHost {
            host: "github.com".to_string(),
            account: Some("someone".to_string()),
            healthy: true,
        }]));
        assert_eq!(check.health, Health::Ok);
        assert!(check.detail.contains("github.com as someone"), "{check:?}");
        assert!(
            check.remedy.as_deref().unwrap().contains("--hostname"),
            "the enterprise-host blind spot has to be visible even when all is well"
        );
    }

    #[test]
    fn a_host_whose_token_stopped_working_is_reported_as_degraded() {
        let check = gh_auth_check(&GhAuth::LoggedIn(vec![GhAuthHost {
            host: "github.example.com".to_string(),
            account: None,
            healthy: false,
        }]));
        assert_eq!(check.health, Health::Degraded);
        assert!(check.detail.contains("NOT working"), "{check:?}");
    }

    #[test]
    fn hooks_are_not_probed_at_all_when_the_copilot_cli_is_missing() {
        let check = hooks_check(None);
        assert_eq!(check.health, Health::Ok, "an unprobed check is not a gap");
        assert!(check.detail.contains("not checked"), "{check:?}");
    }

    #[test]
    fn a_settings_file_that_does_not_exist_is_healthy_because_defaults_apply() {
        let check = config_check(Path::new("/nowhere/config.json"), Ok(false));
        assert_eq!(check.health, Health::Ok);
        assert!(check.detail.contains("defaults apply"), "{check:?}");
    }

    #[test]
    fn a_settings_file_that_does_not_parse_is_reported_with_the_parser_error() {
        let error = anyhow::anyhow!("Invalid global settings in config.json");
        let check = config_check(Path::new("/somewhere/config.json"), Err(&error));
        assert_eq!(check.health, Health::Degraded);
        assert!(
            check.detail.contains("Invalid global settings"),
            "{check:?}"
        );
    }
}
