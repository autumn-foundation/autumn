//! `robots.txt` rules for AI agents: content signals and AI crawler groups.

use super::{AeoConfig, ContentSignalsConfig, CrawlerAccess};

/// AI search crawlers. They fetch the pages that AI answers cite.
pub const AI_SEARCH_CRAWLERS: &[&str] = &["OAI-SearchBot", "Claude-SearchBot", "PerplexityBot"];

/// Fetchers that run when a user asks an AI to read a page.
pub const AI_USER_FETCHERS: &[&str] = &["ChatGPT-User", "Claude-User", "Perplexity-User"];

/// AI training crawlers. Blocking them does not remove a site from AI search.
pub const AI_TRAINING_CRAWLERS: &[&str] = &[
    "GPTBot",
    "ClaudeBot",
    "Google-Extended",
    "Applebot-Extended",
    "CCBot",
    "Bytespider",
    "Amazonbot",
    "meta-externalagent",
];

/// The AI part of a `robots.txt` file.
///
/// [`BotPolicy::default`] adds nothing, so the output equals plain
/// [`crate::seo::robots_txt`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct BotPolicy {
    /// The `Content-Signal` values, or `None` for no line.
    pub content_signals: Option<ContentSignalsConfig>,
    /// Access for each AI crawler class, or `None` for no explicit groups.
    pub crawlers: Option<super::AiCrawlersConfig>,
    /// Absolute URL of the ARD manifest, written as `Agentmap:`.
    pub agentmap: Option<String>,
}

impl BotPolicy {
    /// The policy that `[aeo]` asks for. Empty when AEO is off.
    #[must_use]
    pub fn from_config(config: &AeoConfig) -> Self {
        if !config.enabled {
            return Self::default();
        }
        Self {
            content_signals: config
                .content_signals
                .enabled
                .then_some(config.content_signals),
            crawlers: Some(config.ai_crawlers),
            agentmap: None,
        }
    }

    /// Set the `Agentmap:` URL.
    #[must_use]
    pub fn agentmap(mut self, url: Option<String>) -> Self {
        self.agentmap = url;
        self
    }
}

/// Render `robots.txt` with an AI [`BotPolicy`].
///
/// Group order: the `*` group (with allowed AI crawlers), then one group for
/// each disallowed AI crawler class. Additional rules follow the `*` group
/// rules, as in [`crate::seo::robots_txt`].
#[must_use]
pub fn robots_txt_with_policy(
    profile: &str,
    sitemap_url: Option<&str>,
    additional_rules: &[String],
    policy: &BotPolicy,
) -> String {
    const NONE: &[&str] = &[];
    let is_prod = matches!(profile, "prod" | "production");
    let classes: [(&[&str], CrawlerAccess); 3] =
        policy
            .crawlers
            .map_or([(NONE, CrawlerAccess::Allow); 3], |c| {
                [
                    (AI_SEARCH_CRAWLERS, c.search),
                    (AI_USER_FETCHERS, c.user_fetch),
                    (AI_TRAINING_CRAWLERS, c.training),
                ]
            });

    // A bot that `additional_rules` names already has the operator's own
    // group. Listing it here too would merge the two groups (RFC 9309
    // §2.2.1), and `Allow: /` could then cancel the operator's `Disallow`.
    let named = named_user_agents(additional_rules);
    let keep = |bot: &&str| !named.iter().any(|n| n.eq_ignore_ascii_case(bot));
    let classes = classes.map(|(bots, access)| {
        (
            bots.iter().copied().filter(keep).collect::<Vec<&str>>(),
            access,
        )
    });
    let signal = policy
        .content_signals
        .map(|s| format!("Content-Signal: {}\n", content_signal_value(s)));

    let mut txt = String::new();
    if signal.is_some() {
        txt.push_str(CONTENT_SIGNAL_COMMENT);
    }
    // An operator `*` group binds only crawlers that no group names: a
    // crawler named here would obey this group and skip the operator's
    // (RFC 9309 §2.2.1). Such a crawler is then left to the `*` groups,
    // which merge.
    let operator_wildcard = named.contains(&"*");
    txt.push_str("User-agent: *\n");
    // A crawler class shares the `*` group when it is allowed, or when the
    // whole site is closed (dev/test): one group then holds every rule.
    for (bots, access) in &classes {
        if !operator_wildcard && (*access == CrawlerAccess::Allow || !is_prod) {
            push_user_agents(&mut txt, bots);
        }
    }
    txt.push_str(if is_prod {
        "Allow: /\n"
    } else {
        "Disallow: /\n"
    });
    // The `*` groups merge, so an operator signal for `*` replaces this one
    // rather than contradicting it.
    if let Some(signal) = signal
        .as_deref()
        .filter(|_| wildcard_signal(additional_rules).is_none())
    {
        txt.push_str(signal);
    }
    push_additional_rules(&mut txt, additional_rules, signal.as_deref());

    if is_prod {
        for (bots, access) in &classes {
            if *access == CrawlerAccess::Disallow && !bots.is_empty() {
                txt.push('\n');
                push_user_agents(&mut txt, bots);
                txt.push_str("Disallow: /\n");
                // A bot obeys only its own group, so the group repeats the
                // signals.
                if let Some(signal) = &signal {
                    txt.push_str(signal);
                }
            }
        }
    }

    if let Some(url) = sitemap_url {
        txt.push_str("\nSitemap: ");
        txt.push_str(url);
        txt.push('\n');
    }
    if let Some(url) = &policy.agentmap {
        if sitemap_url.is_none() {
            txt.push('\n');
        }
        txt.push_str("Agentmap: ");
        txt.push_str(url);
        txt.push('\n');
    }
    txt
}

/// Append the operator's `additional_rules`. Lines before their first
/// `User-agent` extend the `*` group, which already carries the signal. A
/// group the operator opens is the only one its crawler obeys, so it gets
/// `signal` too, unless it states its own `Content-Signal` or names only `*`
/// (it then merges with the generated `*` group, which has one).
fn push_additional_rules(txt: &mut String, rules: &[String], signal: Option<&str>) {
    let starts = |line: &str, field: &str| {
        line.trim_start()
            .get(..field.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(field))
    };
    // The operator group being written, and whether it has its signal.
    let mut open: Option<bool> = None;
    // The open group names only `*`, so it merges with the generated one.
    let mut wildcard_only = false;
    let mut after_agent = false;
    let close = |txt: &mut String, open: Option<bool>, wildcard_only: bool| {
        if let (Some(false), Some(signal), false) = (open, signal, wildcard_only) {
            txt.push_str(signal);
        }
    };
    for line in rules.iter().flat_map(|rule| rule.lines()) {
        // A blank or comment line is no rule (RFC 9309 §2.2): it neither
        // ends a run of `User-agent` lines nor starts a group.
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            txt.push_str(line);
            txt.push('\n');
            continue;
        }
        let agent = starts(line, "user-agent:");
        if agent && !after_agent {
            close(txt, open, wildcard_only);
            open = Some(false);
            wildcard_only = true;
        }
        if agent {
            wildcard_only &= agent_name(line) == Some("*");
        }
        if starts(line, "content-signal:") {
            open = open.map(|_| true);
        }
        after_agent = agent;
        txt.push_str(line);
        txt.push('\n');
    }
    close(txt, open, wildcard_only);
}

/// The explanation block that contentsignals.org recommends, verbatim. It
/// states the reservation of rights under EU Directive 2019/790, Art. 4.
const CONTENT_SIGNAL_COMMENT: &str = "\
# As a condition of accessing this website, you agree to
# abide by the following content signals:

# (a)  If a content-signal = yes, you may collect content
# for the corresponding use.
# (b)  If a content-signal = no, you may not collect content
# for the corresponding use.
# (c)  If the website operator does not include a content
# signal for a corresponding use, the website operator
# neither grants nor restricts permission via content signal
# with respect to the corresponding use.

# The content signals and their meanings are:

# search: building a search index and providing search
# results (e.g., returning hyperlinks and short excerpts
# from your website's contents).  Search does not include
# providing AI-generated search summaries.
# ai-input: inputting content into one or more AI models
# (e.g., retrieval augmented generation, grounding, or other
# real-time taking of content for generative AI search
# answers).
# ai-train: training or fine-tuning AI models.

# ANY RESTRICTIONS EXPRESSED VIA CONTENT SIGNALS ARE EXPRESS
# RESERVATIONS OF RIGHTS UNDER ARTICLE 4 OF THE EUROPEAN
# UNION DIRECTIVE 2019/790 ON COPYRIGHT AND RELATED RIGHTS
# IN THE DIGITAL SINGLE MARKET.

";

/// The `User-agent` names in `rules`, without a trailing `# comment`.
/// The value of a `User-agent:` line, without a trailing comment.
fn agent_name(line: &str) -> Option<&str> {
    let (key, value) = line.split_once(':')?;
    let name = value.split('#').next().unwrap_or(value).trim();
    (key.trim().eq_ignore_ascii_case("user-agent") && !name.is_empty()).then_some(name)
}

/// The `Content-Signal` value that `additional_rules` states for `*`: before
/// its first `User-agent`, or in a group that names `*`. It replaces the
/// generated signal, in `robots.txt` and in the Markdown `Content-Signal`
/// header alike.
#[must_use]
pub(crate) fn wildcard_signal(rules: &[String]) -> Option<&str> {
    // Lines before the first group extend the `*` group.
    let mut wildcard = true;
    let mut after_agent = false;
    for line in rules.iter().flat_map(|rule| rule.lines()) {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some(name) = agent_name(trimmed) {
            if !after_agent {
                wildcard = false;
            }
            wildcard |= name == "*";
            after_agent = true;
            continue;
        }
        after_agent = false;
        let Some((key, value)) = trimmed.split_once(':') else {
            continue;
        };
        if wildcard && key.trim().eq_ignore_ascii_case("content-signal") {
            let value = value.split('#').next().unwrap_or(value).trim();
            if !value.is_empty() {
                return Some(value);
            }
        }
    }
    None
}

fn named_user_agents(rules: &[String]) -> Vec<&str> {
    // An entry may hold several lines, as the writer emits them.
    rules
        .iter()
        .flat_map(|rule| rule.lines())
        .filter_map(agent_name)
        .collect()
}

fn push_user_agents(txt: &mut String, bots: &[&str]) {
    for bot in bots {
        txt.push_str("User-agent: ");
        txt.push_str(bot);
        txt.push('\n');
    }
}

/// `search=yes, ai-input=yes, ai-train=no`.
#[must_use]
pub fn content_signal_value(signals: ContentSignalsConfig) -> String {
    let yn = |b: bool| if b { "yes" } else { "no" };
    format!(
        "search={}, ai-input={}, ai-train={}",
        yn(signals.search),
        yn(signals.ai_input),
        yn(signals.ai_train)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aeo::AiCrawlersConfig;

    fn prod_default() -> String {
        robots_txt_with_policy(
            "prod",
            Some("https://example.com/sitemap.xml"),
            &["Disallow: /admin".to_owned()],
            &BotPolicy::from_config(&AeoConfig::default()),
        )
    }

    #[test]
    fn empty_policy_matches_plain_robots_txt() {
        for profile in ["dev", "prod"] {
            let rules = ["Disallow: /admin".to_owned()];
            assert_eq!(
                robots_txt_with_policy(
                    profile,
                    Some("https://e.com/s.xml"),
                    &rules,
                    &BotPolicy::default()
                ),
                crate::seo::robots_txt(profile, Some("https://e.com/s.xml"), &rules),
            );
        }
    }

    #[test]
    fn default_policy_writes_content_signal_in_wildcard_group() {
        let txt = prod_default();
        let signal = txt
            .lines()
            .position(|l| l == "Content-Signal: search=yes, ai-input=yes, ai-train=no")
            .unwrap_or_else(|| panic!("no Content-Signal line:\n{txt}"));
        let wildcard = txt.lines().position(|l| l == "User-agent: *").unwrap();
        assert!(wildcard < signal, "signal must follow the * group:\n{txt}");
    }

    #[test]
    fn default_policy_lists_every_ai_crawler_in_the_wildcard_group() {
        let txt = prod_default();
        let lines: Vec<&str> = txt.lines().collect();
        let first_rule = lines.iter().position(|l| l.starts_with("Allow:")).unwrap();
        for bot in AI_SEARCH_CRAWLERS
            .iter()
            .chain(AI_USER_FETCHERS)
            .chain(AI_TRAINING_CRAWLERS)
        {
            let at = lines
                .iter()
                .position(|l| *l == format!("User-agent: {bot}"))
                .unwrap_or_else(|| panic!("{bot} missing:\n{txt}"));
            assert!(at < first_rule, "{bot} must share the * rules:\n{txt}");
        }
        assert_eq!(
            txt.matches("Disallow: /admin").count(),
            1,
            "shared group keeps one copy of each rule:\n{txt}"
        );
    }

    #[test]
    fn disallowed_training_crawlers_get_their_own_group() {
        let config = AeoConfig {
            ai_crawlers: AiCrawlersConfig {
                training: CrawlerAccess::Disallow,
                ..AiCrawlersConfig::default()
            },
            ..AeoConfig::default()
        };
        let txt = robots_txt_with_policy("prod", None, &[], &BotPolicy::from_config(&config));
        let groups: Vec<&str> = txt.split("\n\n").collect();
        let training = groups
            .iter()
            .find(|g| g.contains("User-agent: GPTBot"))
            .unwrap_or_else(|| panic!("no GPTBot group:\n{txt}"));
        assert!(training.contains("Disallow: /"), "{training}");
        assert!(!training.contains("User-agent: *"), "{training}");
        assert!(
            !training.contains("User-agent: OAI-SearchBot"),
            "{training}"
        );
    }

    #[test]
    fn operator_bot_groups_keep_their_rules() {
        let rules = ["User-agent: GPTBot".to_owned(), "Disallow: /".to_owned()];
        let txt = robots_txt_with_policy(
            "prod",
            None,
            &rules,
            &BotPolicy::from_config(&AeoConfig::default()),
        );
        assert_eq!(txt.matches("User-agent: GPTBot").count(), 1, "{txt}");
        assert!(txt.contains("User-agent: ClaudeBot"), "{txt}");
    }

    /// Codex review (P2): a crawler named in the generated group would skip
    /// an operator `*` group, so with one it is named only to be blocked.
    #[test]
    fn an_operator_wildcard_group_binds_the_allowed_crawlers() {
        let rules = ["User-agent: *".to_owned(), "Disallow: /private".to_owned()];
        let policy = |training| {
            BotPolicy::from_config(&AeoConfig {
                ai_crawlers: AiCrawlersConfig {
                    training,
                    ..AiCrawlersConfig::default()
                },
                ..AeoConfig::default()
            })
        };
        let txt = robots_txt_with_policy("prod", None, &rules, &policy(CrawlerAccess::Allow));
        for bot in AI_SEARCH_CRAWLERS
            .iter()
            .chain(AI_USER_FETCHERS)
            .chain(AI_TRAINING_CRAWLERS)
        {
            assert!(!txt.contains(&format!("User-agent: {bot}\n")), "{txt}");
        }
        assert!(txt.contains("User-agent: *\nDisallow: /private"), "{txt}");

        // A blocked class keeps its own group.
        let txt = robots_txt_with_policy("prod", None, &rules, &policy(CrawlerAccess::Disallow));
        assert!(!txt.contains("User-agent: OAI-SearchBot"), "{txt}");
        let training = txt
            .split("\n\n")
            .find(|g| g.contains("User-agent: GPTBot"))
            .unwrap_or_else(|| panic!("no GPTBot group:\n{txt}"));
        assert!(training.contains("Disallow: /\n"), "{training}");
        assert!(!training.contains("User-agent: *"), "{training}");
    }

    /// Codex review (P2): the `*` groups merge, so an operator signal for
    /// `*` replaces the generated one instead of contradicting it.
    #[test]
    fn an_operator_wildcard_signal_replaces_the_generated_one() {
        let policy = BotPolicy::from_config(&AeoConfig::default());
        let generated = format!(
            "Content-Signal: {}",
            content_signal_value(AeoConfig::default().content_signals)
        );
        let own = "Content-Signal: search=yes, ai-input=yes, ai-train=yes";
        let cases: [&[&str]; 2] = [
            // Before the first group, the rule extends `*`.
            &[own, "Disallow: /admin"],
            // A group of its own that names `*`.
            &["User-agent: Bingbot", "User-agent: *", own, "Disallow: /x"],
        ];
        for rules in cases {
            let rules: Vec<String> = rules.iter().map(|r| (*r).to_owned()).collect();
            let txt = robots_txt_with_policy("prod", None, &rules, &policy);
            assert_eq!(txt.matches(own).count(), 1, "{txt}");
            assert!(!txt.contains(&generated), "{txt}");
        }

        assert_eq!(
            wildcard_signal(&[own.to_owned()]),
            Some("search=yes, ai-input=yes, ai-train=yes")
        );

        // An operator `*` group with no signal shares the generated one,
        // once.
        let rules = ["User-agent: *".to_owned(), "Disallow: /x".to_owned()];
        let txt = robots_txt_with_policy("prod", None, &rules, &policy);
        assert_eq!(txt.matches(&generated).count(), 1, "{txt}");

        // A group naming `*` and another crawler still carries the signal:
        // that crawler obeys only this group.
        let rules = [
            "User-agent: Bingbot".to_owned(),
            "User-agent: *".to_owned(),
            "Disallow: /x".to_owned(),
        ];
        let txt = robots_txt_with_policy("prod", None, &rules, &policy);
        let mixed = txt
            .split("User-agent: Bingbot")
            .nth(1)
            .unwrap_or_else(|| panic!("no Bingbot group:\n{txt}"));
        assert!(mixed.contains(&generated), "{txt}");

        // A signal in another crawler's group leaves `*` alone.
        let rules = ["User-agent: Bingbot".to_owned(), own.to_owned()];
        let txt = robots_txt_with_policy("prod", None, &rules, &policy);
        assert_eq!(txt.matches(&generated).count(), 1, "{txt}");
        assert_eq!(txt.matches(own).count(), 1, "{txt}");
    }

    /// Codex review (P2): a crawler with the operator's own group obeys
    /// only that group, so it carries the signal too.
    #[test]
    fn operator_groups_carry_the_signal() {
        let rules = [
            "Disallow: /admin".to_owned(),
            "User-agent: GPTBot".to_owned(),
            "Disallow: /".to_owned(),
            "User-agent: Bingbot".to_owned(),
            "Content-Signal: search=yes".to_owned(),
            "Disallow: /private".to_owned(),
        ];
        let txt = robots_txt_with_policy(
            "prod",
            None,
            &rules,
            &BotPolicy::from_config(&AeoConfig::default()),
        );
        let gpt = txt
            .split("User-agent: ")
            .find(|g| g.starts_with("GPTBot\n"))
            .unwrap_or_else(|| panic!("{txt}"));
        assert!(gpt.contains("Content-Signal: "), "{txt}");
        let bing = txt
            .split("User-agent: ")
            .find(|g| g.starts_with("Bingbot\n"))
            .unwrap_or_else(|| panic!("{txt}"));
        assert_eq!(bing.matches("Content-Signal:").count(), 1, "its own: {txt}");
        // `Disallow: /admin` joins the `*` group, which has the signal once.
        let star = txt.split("User-agent: GPTBot").next().unwrap();
        assert_eq!(star.matches("Content-Signal:").count(), 1, "{txt}");
    }

    /// Codex review (P2): a comment between `User-agent` lines keeps them in
    /// one group, so the signal goes after the group's rules.
    #[test]
    fn a_comment_keeps_user_agents_in_one_group() {
        let rules = [
            "User-agent: GPTBot".to_owned(),
            "# AI crawlers".to_owned(),
            "User-agent: ClaudeBot".to_owned(),
            "Disallow: /private".to_owned(),
        ];
        let txt = robots_txt_with_policy(
            "prod",
            None,
            &rules,
            &BotPolicy::from_config(&AeoConfig::default()),
        );
        let start = txt.find("User-agent: GPTBot").unwrap();
        let group = &txt[start..];
        let disallow = group.find("Disallow: /private").unwrap();
        assert!(
            !group[..disallow].contains("Content-Signal:"),
            "the signal splits the group:\n{txt}"
        );
        assert!(group[disallow..].contains("Content-Signal:"), "{txt}");
    }

    /// Codex review (P2): one entry may hold a whole group.
    #[test]
    fn a_multi_line_rule_still_owns_its_bot() {
        let rules = ["User-agent: GPTBot\nDisallow: /".to_owned()];
        let txt = robots_txt_with_policy(
            "prod",
            None,
            &rules,
            &BotPolicy::from_config(&AeoConfig::default()),
        );
        assert_eq!(txt.matches("User-agent: GPTBot").count(), 1, "{txt}");
    }

    #[test]
    fn an_operator_group_with_a_comment_still_owns_its_bot() {
        let rules = [
            "User-agent: GPTBot # block".to_owned(),
            "Disallow: /".to_owned(),
        ];
        let txt = robots_txt_with_policy(
            "prod",
            None,
            &rules,
            &BotPolicy::from_config(&AeoConfig::default()),
        );
        assert!(!txt.contains("User-agent: GPTBot\n"), "{txt}");
        assert!(txt.contains("User-agent: ClaudeBot"), "{txt}");
    }

    #[test]
    fn disallowed_groups_repeat_the_signal() {
        let config = AeoConfig {
            ai_crawlers: AiCrawlersConfig {
                training: CrawlerAccess::Disallow,
                ..AiCrawlersConfig::default()
            },
            ..AeoConfig::default()
        };
        let txt = robots_txt_with_policy("prod", None, &[], &BotPolicy::from_config(&config));
        let group = txt
            .split("\n\n")
            .find(|g| g.contains("User-agent: GPTBot"))
            .unwrap();
        assert!(group.contains("Content-Signal: "), "{group}");
    }

    #[test]
    fn dev_profile_disallows_every_crawler_and_keeps_the_signal() {
        let txt = robots_txt_with_policy(
            "dev",
            None,
            &[],
            &BotPolicy::from_config(&AeoConfig::default()),
        );
        assert!(txt.contains("Disallow: /"), "{txt}");
        assert!(!txt.contains("Allow: /\n"), "{txt}");
        assert!(txt.contains("Content-Signal:"), "{txt}");
    }

    #[test]
    fn signals_follow_config_values() {
        let config = AeoConfig {
            content_signals: ContentSignalsConfig {
                search: true,
                ai_input: false,
                ai_train: true,
                ..ContentSignalsConfig::default()
            },
            ..AeoConfig::default()
        };
        let txt = robots_txt_with_policy("prod", None, &[], &BotPolicy::from_config(&config));
        assert!(
            txt.contains("Content-Signal: search=yes, ai-input=no, ai-train=yes"),
            "{txt}"
        );
    }

    #[test]
    fn disabled_signals_and_disabled_aeo_write_nothing_extra() {
        let mut config = AeoConfig::default();
        config.content_signals.enabled = false;
        let txt = robots_txt_with_policy("prod", None, &[], &BotPolicy::from_config(&config));
        assert!(!txt.contains("Content-Signal"), "{txt}");

        config.enabled = false;
        assert_eq!(BotPolicy::from_config(&config), BotPolicy::default());
    }

    #[test]
    fn agentmap_follows_sitemap() {
        let policy = BotPolicy::from_config(&AeoConfig::default()).agentmap(Some(
            "https://example.com/.well-known/ai-catalog.json".to_owned(),
        ));
        let txt = robots_txt_with_policy(
            "prod",
            Some("https://example.com/sitemap.xml"),
            &[],
            &policy,
        );
        assert!(
            txt.ends_with(
                "Sitemap: https://example.com/sitemap.xml\n\
                 Agentmap: https://example.com/.well-known/ai-catalog.json\n"
            ),
            "{txt}"
        );
    }

    #[test]
    fn config_parses_from_toml() {
        let config: AeoConfig = toml::from_str(
            r#"
            enabled = true
            [content_signals]
            ai_train = true
            [ai_crawlers]
            training = "disallow"
            "#,
        )
        .unwrap();
        assert!(config.content_signals.ai_train);
        assert!(config.content_signals.search);
        assert_eq!(config.ai_crawlers.training, CrawlerAccess::Disallow);
        assert_eq!(config.ai_crawlers.search, CrawlerAccess::Allow);
    }
}
