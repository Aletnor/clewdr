use std::{
    collections::HashSet,
    fmt::{Debug, Display},
    net::{IpAddr, SocketAddr},
};

use axum::http::{Uri, uri::Scheme};
use clap::Parser;
use colored::Colorize;
use figment::{
    Figment,
    providers::{Env, Format, Toml},
};
use http::uri::Authority;
use passwords::PasswordGenerator;
use serde::{Deserialize, Serialize};
use tokio::spawn;
use tracing::error;
use url::Url;
use wreq::Proxy;

use super::{CONFIG_PATH, ENDPOINT_URL};
use crate::{
    Args,
    config::{
        CC_CLIENT_ID, CookieStatus, UselessCookie, default_check_update, default_ip,
        default_max_retries, default_port, default_skip_cool_down, default_use_real_roles,
    },
    error::ClewdrError,
};

/// Generates a random password for authentication
/// Creates a secure 64-character password with mixed character types
///
/// # Returns
/// A random password string
fn generate_password() -> String {
    let pg = PasswordGenerator {
        length: 64,
        numbers: true,
        lowercase_letters: true,
        uppercase_letters: true,
        symbols: false,
        spaces: false,
        exclude_similar_characters: true,
        strict: true,
    };

    println!("{}", "Generating random password......".green());
    pg.generate_one().unwrap()
}

/// A struct representing the configuration of the application
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ClewdrConfig {
    // key configurations
    #[serde(default)]
    pub cookie_array: HashSet<CookieStatus>,
    #[serde(default)]
    pub wasted_cookie: HashSet<UselessCookie>,

    // Server settings, cannot hot reload
    #[serde(default = "default_ip")]
    ip: IpAddr,
    #[serde(default = "default_port")]
    port: u16,

    // App settings, can hot reload, but meaningless
    #[serde(default = "default_check_update")]
    pub check_update: bool,
    #[serde(default)]
    pub auto_update: bool,
    #[serde(default)]
    pub no_fs: bool,
    #[serde(default)]
    pub log_to_file: bool,

    // Network settings, can hot reload
    #[serde(default)]
    password: String,
    #[serde(default)]
    admin_password: String,
    #[serde(default)]
    pub proxy: Option<String>,
    #[serde(default)]
    pub rproxy: Option<Url>,

    // Api settings, can hot reload
    #[serde(default = "default_max_retries")]
    pub max_retries: usize,
    #[serde(default)]
    pub preserve_chats: bool,
    #[serde(default)]
    pub web_search: bool,
    #[serde(default)]
    pub enable_web_count_tokens: bool,
    #[serde(default)]
    pub sanitize_messages: bool,

    // Cookie settings, can hot reload
    #[serde(default)]
    pub skip_first_warning: bool,
    #[serde(default)]
    pub skip_second_warning: bool,
    #[serde(default)]
    pub skip_restricted: bool,
    #[serde(default)]
    pub skip_non_pro: bool,
    #[serde(default = "default_skip_cool_down")]
    pub skip_rate_limit: bool,
    #[serde(default)]
    pub skip_normal_pro: bool,

    // Prompt configurations, can hot reload
    #[serde(default = "default_use_real_roles")]
    pub use_real_roles: bool,
    #[serde(default)]
    pub custom_h: Option<String>,
    #[serde(default)]
    pub custom_a: Option<String>,
    #[serde(default)]
    pub custom_prompt: String,

    // Claude Code settings, can hot reload
    #[serde(default)]
    pub claude_code_client_id: Option<String>,
    #[serde(default)]
    pub custom_system: Option<String>,

    // Prompt cache settings (Claude Code channel), can hot reload.
    // See middleware/claude/prompt_cache.rs.
    #[serde(default = "default_cache_on")]
    pub auto_cache: bool,
    #[serde(default = "default_cache_on")]
    pub magic_cache: bool,
    #[serde(default)]
    pub cache_ttl: Option<String>,
    // -1: stamp the last user message. n >= 0: SillyTavern's cachingAtDepth,
    // stamp the messages n and n + 2 role switches from the end.
    #[serde(default = "default_cache_depth")]
    pub cache_at_depth: i64,

    // Skip field, can hot reload
    #[serde(skip)]
    pub wreq_proxy: Option<Proxy>,
}

fn default_cache_on() -> bool {
    true
}

fn default_cache_depth() -> i64 {
    -1
}

impl Default for ClewdrConfig {
    fn default() -> Self {
        Self {
            max_retries: default_max_retries(),
            check_update: default_check_update(),
            auto_update: false,
            cookie_array: HashSet::new(),
            wasted_cookie: HashSet::new(),
            password: String::new(),
            admin_password: String::new(),
            proxy: None,
            ip: default_ip(),
            port: default_port(),
            rproxy: None,
            use_real_roles: default_use_real_roles(),
            custom_prompt: String::new(),
            custom_h: None,
            custom_a: None,
            wreq_proxy: None,
            preserve_chats: false,
            web_search: false,
            enable_web_count_tokens: false,
            sanitize_messages: false,
            skip_first_warning: false,
            skip_second_warning: false,
            skip_restricted: false,
            skip_non_pro: false,
            skip_rate_limit: default_skip_cool_down(),
            skip_normal_pro: false,
            claude_code_client_id: None,
            custom_system: None,
            auto_cache: default_cache_on(),
            magic_cache: default_cache_on(),
            cache_ttl: None,
            cache_at_depth: default_cache_depth(),
            no_fs: false,
            log_to_file: false,
        }
    }
}

impl Display for ClewdrConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // What a person needs right after starting it: where the panel is and
        // the two things to paste into the chat client. Only the Claude Code
        // endpoint is shown, since that is the one logged-in accounts serve.
        // The address is one a browser on this machine can open, even when the
        // server was bound to every interface.
        let host = if self.ip.is_unspecified() {
            IpAddr::from([127, 0, 0, 1])
        } else {
            self.ip
        };
        let authority: Authority = SocketAddr::new(host, self.port)
            .to_string()
            .parse()
            .map_err(|_| std::fmt::Error)?;
        let panel = Uri::builder()
            .scheme(Scheme::HTTP)
            .authority(authority)
            .path_and_query("")
            .build()
            .map_err(|_| std::fmt::Error)?
            .to_string();
        write!(
            f,
            "管理面板（用浏览器打开）：{}\n\
            管理员密钥（登面板用）：{}\n\
            酒馆里填的反代地址：{}\n\
            酒馆里填的密码（API 密钥）：{}\n",
            panel.green().underline(),
            self.admin_password.yellow(),
            (panel.clone() + "code/v1").green().underline(),
            self.password.yellow(),
        )?;
        if let Some(ref proxy) = self.proxy {
            writeln!(f, "代理：{}", proxy.to_string().blue())?;
        }
        if let Some(ref rproxy) = self.rproxy {
            writeln!(f, "上游地址覆盖：{}", rproxy.to_string().blue())?;
        }
        writeln!(
            f,
            "第一次用：打开管理面板 → 账户 → 右上角加号 → 授权登录。\n\
            反代地址和密码在面板「设置」页最上面也能复制。这个窗口要一直开着。"
        )?;
        Ok(())
    }
}

impl ClewdrConfig {
    pub fn user_auth(&self, key: &str) -> bool {
        key == self.password
    }

    pub fn admin_auth(&self, key: &str) -> bool {
        key == self.admin_password
    }

    /// A blank key arriving from the panel means "leave it as it is". Without
    /// this `validate` would roll a fresh random one, and whoever cleared the
    /// field by accident is locked out of a key they have never seen.
    pub fn keep_keys_if_blank(&mut self, current: &ClewdrConfig) {
        if self.password.trim().is_empty() {
            self.password = current.password.clone();
        }
        if self.admin_password.trim().is_empty() {
            self.admin_password = current.admin_password.clone();
        }
    }

    pub fn cc_client_id(&self) -> String {
        self.claude_code_client_id
            .as_deref()
            .unwrap_or(CC_CLIENT_ID)
            .to_string()
    }

    /// Loads configuration from files and environment variables
    /// Combines settings from config.toml, clewdr.toml, and environment variables
    /// Also loads cookies from a file if specified
    ///
    /// # Returns
    /// * Config instance
    pub fn new() -> Self {
        // Load config from TOML then override with environment variables.
        // Use double underscore "__" to map nested keys.
        let mut config: ClewdrConfig = Figment::from(Toml::file(CONFIG_PATH.as_path()))
            .admerge(Env::prefixed("CLEWDR_").split("__"))
            .extract_lossy()
            .inspect_err(|e| {
                error!("Failed to load config: {}", e);
            })
            .unwrap_or_default();
        if let Some(ref f) = Args::try_parse().ok().and_then(|a| a.file) {
            // load cookies from file
            if f.exists() {
                if let Ok(cookies) = std::fs::read_to_string(f) {
                    let cookies = cookies
                        .lines()
                        .filter_map(|line| CookieStatus::new(line, None).ok());
                    config.cookie_array.extend(cookies);
                } else {
                    error!("Failed to read cookie file: {}", f.display());
                }
            } else {
                error!("Cookie file not found: {}", f.display());
            }
        }
        let config = config.validate();
        if !config.no_fs {
            let config_clone = config.to_owned();
            spawn(async move {
                config_clone.save().await.unwrap_or_else(|e| {
                    error!("Failed to save config: {}", e);
                });
            });
        }
        config
    }

    /// Gets the API endpoint for the Claude service
    /// Returns the reverse proxy URL if configured, otherwise the default endpoint
    ///
    /// # Returns
    /// The URL for the API endpoint
    pub fn endpoint(&self) -> Url {
        if let Some(ref proxy) = self.rproxy {
            return proxy.to_owned();
        }
        ENDPOINT_URL.to_owned()
    }

    /// address of proxy
    pub fn address(&self) -> SocketAddr {
        SocketAddr::new(self.ip, self.port)
    }

    /// Save the configuration to a file
    pub async fn save(&self) -> Result<(), ClewdrError> {
        if self.no_fs {
            return Ok(());
        }
        if let Some(parent) = CONFIG_PATH.parent()
            && !parent.exists()
        {
            tokio::fs::create_dir_all(parent).await?;
        }
        let path = CONFIG_PATH.as_path();
        let data = toml::ser::to_string_pretty(self)?;
        tokio::fs::write(path, data).await?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o600);
            tokio::fs::set_permissions(path, perms).await?;
        }
        Ok(())
    }

    /// Validate the configuration
    pub fn validate(mut self) -> Self {
        if self.password.trim().is_empty() {
            self.password = generate_password();
        }
        if self.admin_password.trim().is_empty() {
            self.admin_password = generate_password();
        }
        self.cookie_array = self.cookie_array.into_iter().map(|x| x.reset()).collect();
        self.wreq_proxy = self.proxy.to_owned().and_then(|p| {
            Proxy::all(p)
                .inspect_err(|e| {
                    self.proxy = None;
                    error!("Failed to parse proxy: {}", e);
                })
                .ok()
        });
        // No proxy filled in: follow the one the machine itself is set to use.
        // A VPN client in "system proxy" mode only announces itself there, and
        // without this a fresh install talks to upstream directly and is refused.
        if self.wreq_proxy.is_none()
            && let Some(found) = system_proxy()
            && let Ok(proxy) = Proxy::all(found.as_str())
        {
            println!("代理（跟随系统设置）：{}", found.blue());
            self.wreq_proxy = Some(proxy);
        }
        self
    }
}

/// The proxy this machine is configured to use: the usual environment
/// variables first, then on Windows the system proxy setting.
fn system_proxy() -> Option<String> {
    for name in [
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
        "HTTP_PROXY",
        "http_proxy",
    ] {
        if let Ok(value) = std::env::var(name)
            && let Some(proxy) = proxy_url(&value)
        {
            return Some(proxy);
        }
    }
    #[cfg(windows)]
    {
        let read = |value: &str| -> Option<String> {
            let out = std::process::Command::new("reg")
                .args([
                    "query",
                    r"HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings",
                    "/v",
                    value,
                ])
                .output()
                .ok()?;
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .find(|line| line.trim_start().starts_with(value))
                .and_then(|line| line.split_whitespace().last())
                .map(str::to_owned)
        };
        if read("ProxyEnable").as_deref() == Some("0x1") {
            return read("ProxyServer").and_then(|server| proxy_url(&server));
        }
    }
    None
}

/// Turns a proxy setting into a URL. Takes a plain `host:port`, a full URL,
/// or the Windows per-protocol form `http=host:port;https=host:port`.
fn proxy_url(setting: &str) -> Option<String> {
    let setting = setting.trim();
    if setting.is_empty() {
        return None;
    }
    if !setting.contains('=') {
        return Some(if setting.contains("://") {
            setting.to_owned()
        } else {
            format!("http://{setting}")
        });
    }
    let entries: Vec<(&str, &str)> = setting
        .split(';')
        .filter_map(|entry| entry.trim().split_once('='))
        .collect();
    let pick = |scheme: &str| {
        entries
            .iter()
            .find(|(name, host)| name.eq_ignore_ascii_case(scheme) && !host.is_empty())
            .map(|(_, host)| *host)
    };
    if let Some(host) = pick("https").or_else(|| pick("http")) {
        return Some(if host.contains("://") {
            host.to_owned()
        } else {
            format!("http://{host}")
        });
    }
    pick("socks").map(|host| {
        if host.contains("://") {
            host.to_owned()
        } else {
            format!("socks5://{host}")
        }
    })
}

#[cfg(test)]
mod tests {
    use super::proxy_url;

    #[test]
    fn a_bare_host_and_port_becomes_an_http_proxy() {
        assert_eq!(proxy_url("127.0.0.1:7890").as_deref(), Some("http://127.0.0.1:7890"));
        assert_eq!(proxy_url(" 127.0.0.1:7890 ").as_deref(), Some("http://127.0.0.1:7890"));
    }

    #[test]
    fn a_full_url_is_kept_as_it_is() {
        assert_eq!(proxy_url("socks5://127.0.0.1:1080").as_deref(), Some("socks5://127.0.0.1:1080"));
        assert_eq!(proxy_url("http://127.0.0.1:7897").as_deref(), Some("http://127.0.0.1:7897"));
    }

    #[test]
    fn the_per_protocol_form_prefers_https_then_http_then_socks() {
        assert_eq!(
            proxy_url("http=127.0.0.1:7890;https=127.0.0.1:7891;socks=127.0.0.1:7892").as_deref(),
            Some("http://127.0.0.1:7891")
        );
        assert_eq!(proxy_url("http=127.0.0.1:7890").as_deref(), Some("http://127.0.0.1:7890"));
        assert_eq!(proxy_url("socks=127.0.0.1:7892").as_deref(), Some("socks5://127.0.0.1:7892"));
    }

    #[test]
    fn nothing_usable_means_no_proxy() {
        assert_eq!(proxy_url(""), None);
        assert_eq!(proxy_url("ftp=127.0.0.1:21"), None);
    }
}
