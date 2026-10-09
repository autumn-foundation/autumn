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

    let mut txt = String::new();
    if policy.content_signals.is_some() {
        txt.push_str(CONTENT_SIGNAL_COMMENT);
    }
    txt.push_str("User-agent: *\n");
    // A crawler class shares the `*` group when it is allowed, or when the
    // whole site is closed (dev/test): one group then holds every rule.
    for (bots, access) in &classes {
        if *access == CrawlerAccess::Allow || !is_prod {
            push_user_agents(&mut txt, bots);
        }
    }
    txt.push_str(if is_prod {
        "Allow: /\n"
    } else {
        "Disallow: /\n"
    });
    if let Some(signals) = policy.content_signals {
        txt.push_str("Content-Signal: ");
        txt.push_str(&content_signal_value(signals));
        txt.push('\n');
    }
    for rule in additional_rules {
        txt.push_str(rule);
        txt.push('\n');
    }

    if is_prod {
        for (bots, access) in &classes {
            if *access == CrawlerAccess::Disallow && !bots.is_empty() {
                txt.push('\n');
                push_user_agents(&mut txt, bots);
                txt.push_str("Disallow: /\n");
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
