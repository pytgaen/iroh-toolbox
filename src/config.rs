use std::{path::PathBuf, time::Duration};

use anyhow::{Context, Result, bail, ensure};
use iroh::RelayUrl;
use serde::Deserialize;
use url::Url;

use crate::cli::Options;

#[derive(Clone)]
pub struct Settings {
    pub relay: Option<RelayUrl>,
    pub token: Option<String>,
    pub timeout: Duration,
    pub relay_only: bool,
    pub direct_only: bool,
    pub no_hints: bool,
}

#[derive(Default, Deserialize)]
struct FileConfig {
    #[serde(default)]
    relay: FileRelay,
    #[serde(default)]
    access: FileAccess,
}

#[derive(Default, Deserialize)]
struct FileRelay {
    url: Option<String>,
}

#[derive(Default, Deserialize)]
struct FileAccess {
    shared_token: Option<Token>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Token {
    One(String),
    Many(Vec<String>),
}

impl Settings {
    pub fn load(options: &Options) -> Result<Self> {
        Self::load_with(options, |key| std::env::var(key).ok())
    }

    fn load_with(options: &Options, env: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let path = options
            .config
            .clone()
            .or_else(|| env("IROH_TOOLBOX_CONFIG").map(PathBuf::from));
        let config: FileConfig = if let Some(path) = path {
            let source = std::fs::read_to_string(path).context("cannot read configuration file")?;
            // TOML parse errors can include the source line containing a secret.
            toml::from_str(&source).map_err(|_| anyhow::anyhow!("invalid TOML configuration"))?
        } else {
            FileConfig::default()
        };
        let relay = if options.direct_only {
            None
        } else {
            options
                .relay
                .clone()
                .or_else(|| env("IROH_TOOLBOX_RELAY"))
                .or_else(|| env("IROH_RELAY_URL"))
                .or(config.relay.url)
                .map(|v| relay_url(&v))
                .transpose()?
        };
        let token = env("IROH_TOOLBOX_TOKEN").or_else(|| env("IROH_RELAY_ACCESS_TOKEN"));
        let token = if options.direct_only {
            None
        } else {
            match (token, config.access.shared_token) {
                (Some(token), _) => Some(token),
                (None, Some(Token::One(token))) => Some(token),
                (None, Some(Token::Many(mut tokens))) if tokens.len() == 1 => tokens.pop(),
                (None, Some(Token::Many(_))) => bail!("select one token using IROH_TOOLBOX_TOKEN"),
                _ => None,
            }
        };
        if let Some(token) = &token {
            ensure!(
                !token.is_empty() && token.is_ascii(),
                "relay token must be a non-empty ASCII string"
            );
            ensure!(
                token.bytes().all(|b| b > 32 && b != 127),
                "relay token cannot contain whitespace or control characters"
            );
            let Some(relay) = &relay else {
                bail!("a relay URL is required when a token is configured");
            };
            // A bearer token over plain HTTP is readable on the network.
            ensure!(
                relay.scheme() == "https" || is_loopback(relay),
                "a relay token requires an https:// relay URL (http:// is accepted only for loopback)"
            );
        }
        let timeout = match options.timeout {
            Some(value) => value,
            None => {
                crate::cli::seconds(&env("IROH_TOOLBOX_TIMEOUT").unwrap_or_else(|| "15".into()))
                    .map_err(|_| {
                        anyhow::anyhow!("IROH_TOOLBOX_TIMEOUT must be finite, positive seconds")
                    })?
            }
        };
        Ok(Self {
            relay,
            token,
            timeout: Duration::from_secs_f64(timeout),
            relay_only: options.relay_only,
            direct_only: options.direct_only,
            no_hints: options.no_hints,
        })
    }

    pub fn redact(&self, text: impl AsRef<str>) -> String {
        match &self.token {
            Some(token) => text.as_ref().replace(token, "[REDACTED]"),
            None => text.as_ref().to_owned(),
        }
    }
}

fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

pub fn relay_url(value: &str) -> Result<RelayUrl> {
    let mut url = Url::parse(value).map_err(|_| anyhow::anyhow!("invalid relay URL"))?;
    ensure!(
        matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "relay URL must be http(s), without credentials, query or fragment"
    );
    url.set_path(&format!("{}/", url.path().trim_end_matches('/')));
    url.as_str().parse().context("invalid relay URL")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn precedence_and_secret_redaction() {
        let options = Options {
            relay: Some("https://cli.example".into()),
            ..Default::default()
        };
        let settings = Settings::load_with(&options, |key| match key {
            "IROH_TOOLBOX_RELAY" => Some("https://env.example".into()),
            "IROH_TOOLBOX_TOKEN" => Some("sample-secret".into()),
            "IROH_TOOLBOX_TIMEOUT" => Some("3".into()),
            _ => None,
        })
        .unwrap();
        assert_eq!(
            settings.relay.as_ref().unwrap().as_str(),
            "https://cli.example/"
        );
        assert_eq!(settings.timeout, Duration::from_secs(3));
        assert_eq!(settings.token.as_deref(), Some("sample-secret"));
        assert_eq!(settings.redact("Bearer sample-secret"), "Bearer [REDACTED]");
    }

    #[test]
    fn tokens_require_https_except_on_loopback() {
        let load = |relay: &str| {
            let options = Options {
                relay: Some(relay.into()),
                ..Default::default()
            };
            Settings::load_with(&options, |key| {
                (key == "IROH_TOOLBOX_TOKEN").then(|| "sample-secret".into())
            })
        };
        assert!(load("http://relay.example").is_err());
        for relay in [
            "https://relay.example",
            "http://127.0.0.1:3340",
            "http://[::1]:3340",
            "http://localhost:3340",
        ] {
            assert!(load(relay).is_ok(), "{relay}");
        }
    }

    #[test]
    fn rejects_credentials_in_relay_url() {
        for value in [
            "https://u:secret@example.com",
            "https://example.com/?token=secret",
            "https://example.com/#secret",
            "ftp://example.com",
        ] {
            assert!(relay_url(value).is_err());
        }
    }

    #[test]
    fn public_default_needs_no_custom_relay_and_never_receives_private_tokens() {
        let config = Settings::load_with(&Options::default(), |_| None).unwrap();
        assert!(config.relay.is_none() && config.token.is_none() && !config.direct_only);
        let config = Settings::load_with(
            &Options {
                relay_only: true,
                ..Default::default()
            },
            |_| None,
        )
        .unwrap();
        assert!(config.relay_only && config.relay.is_none());
        assert!(
            Settings::load_with(&Options::default(), |key| (key == "IROH_TOOLBOX_TOKEN")
                .then(|| "private-token".into()))
            .is_err()
        );
        let config = Settings::load_with(
            &Options {
                direct_only: true,
                ..Default::default()
            },
            |key| match key {
                "IROH_TOOLBOX_RELAY" => Some("https://private.example".into()),
                "IROH_TOOLBOX_TOKEN" => Some("private-token".into()),
                _ => None,
            },
        )
        .unwrap();
        assert!(config.direct_only && config.relay.is_none() && config.token.is_none());
    }

    #[test]
    fn rejects_invalid_token_and_timeout_without_disclosing_them() {
        for value in ["", "Bearer secret", "secret\n", "é"] {
            let options = Options {
                relay: Some("https://example.com".into()),
                ..Default::default()
            };
            let error = Settings::load_with(&options, |key| {
                (key == "IROH_TOOLBOX_TOKEN").then(|| value.into())
            })
            .err()
            .unwrap();
            assert!(!error.to_string().contains("secret"));
        }
        assert!(crate::cli::seconds("NaN").is_err());
        assert!(crate::cli::seconds("0").is_err());
    }
}
