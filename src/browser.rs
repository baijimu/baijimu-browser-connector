use crate::store::{ProfileMetadata, Store};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use headless_chrome::{browser::default_executable, protocol::cdp::Page, Browser, LaunchOptions};
use rand::{rngs::OsRng, RngCore};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    env,
    net::{IpAddr, ToSocketAddrs},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use url::{Host, Url};

pub struct BrowserSession {
    pub session_id: String,
    pub workspace_id: u64,
    pub profile_id: String,
    pub headless: bool,
    pub started_at: u64,
    browser: Browser,
    tab: Arc<headless_chrome::Tab>,
    snapshot_generation: u64,
}

pub type SharedSession = Arc<Mutex<BrowserSession>>;

pub fn create_session(
    store: &Store,
    profile: &ProfileMetadata,
    headless: bool,
    now: u64,
) -> Result<BrowserSession, String> {
    let data_dir = store.browser_data_dir(profile.workspace_id, &profile.profile_id);
    std::fs::create_dir_all(&data_dir).map_err(|error| error.to_string())?;
    let chrome_path = env::var_os("BAIJIMU_BROWSER_CHROME_PATH")
        .map(PathBuf::from)
        .map(Ok)
        .unwrap_or_else(default_executable)
        .map_err(|error| format!("Chrome or Edge was not found: {error}"))?;
    if !chrome_path.is_file() {
        return Err(format!(
            "configured browser executable is not a file: {}",
            chrome_path.display()
        ));
    }
    let options = LaunchOptions::default_builder()
        .headless(headless)
        .sandbox(true)
        .enable_gpu(!headless)
        .ignore_certificate_errors(false)
        .window_size(Some((1440, 1000)))
        .path(Some(chrome_path))
        .user_data_dir(Some(data_dir))
        .idle_browser_timeout(Duration::from_secs(120))
        .build()
        .map_err(|error| error.to_string())?;
    let browser =
        Browser::new(options).map_err(|error| format!("failed to launch browser: {error}"))?;
    let existing_tab = browser
        .get_tabs()
        .lock()
        .map_err(|_| "browser tab registry lock is poisoned".to_string())?
        .first()
        .cloned();
    let tab = match existing_tab {
        Some(tab) => tab,
        None => browser
            .new_tab()
            .map_err(|error| format!("browser did not create an initial tab: {error}"))?,
    };
    tab.set_default_timeout(Duration::from_secs(15));
    Ok(BrowserSession {
        session_id: random_id("session"),
        workspace_id: profile.workspace_id,
        profile_id: profile.profile_id.clone(),
        headless,
        started_at: now,
        browser,
        tab,
        snapshot_generation: 0,
    })
}

impl BrowserSession {
    pub fn summary(&self) -> Value {
        json!({
            "sessionId": self.session_id,
            "workspaceId": self.workspace_id,
            "profileId": self.profile_id,
            "headless": self.headless,
            "startedAt": self.started_at,
            "url": self.tab.get_url(),
            "title": self.tab.get_title().unwrap_or_default(),
            "tabCount": self.browser.get_tabs().lock().map(|tabs| tabs.len()).unwrap_or_default()
        })
    }

    pub fn navigate(&mut self, profile: &ProfileMetadata, raw_url: &str) -> Result<Value, String> {
        let url = authorize_url(profile, raw_url)?;
        self.tab
            .navigate_to(url.as_str())
            .map_err(|error| format!("navigation failed: {error}"))?
            .wait_until_navigated()
            .map_err(|error| format!("navigation did not complete: {error}"))?;
        self.snapshot_generation = self.snapshot_generation.saturating_add(1);
        Ok(self.page_state())
    }

    pub fn page_state(&self) -> Value {
        json!({
            "sessionId": self.session_id,
            "url": self.tab.get_url(),
            "title": self.tab.get_title().unwrap_or_default()
        })
    }

    pub fn snapshot(&mut self, max_elements: usize) -> Result<Value, String> {
        self.snapshot_generation = self.snapshot_generation.saturating_add(1);
        let prefix = format!("bjm-{}-{}", self.snapshot_generation, random_suffix());
        let prefix_json = serde_json::to_string(&prefix).map_err(|error| error.to_string())?;
        let script = format!(
            r#"JSON.stringify((() => {{
              const limit = {max_elements};
              const prefix = {prefix_json};
              const candidates = Array.from(document.querySelectorAll(
                'a,button,input,textarea,select,[role],[contenteditable="true"],video,h1,h2,h3,p,label'
              ));
              const visible = candidates.filter((element) => {{
                const style = getComputedStyle(element);
                const rect = element.getBoundingClientRect();
                return style.visibility !== 'hidden' && style.display !== 'none' && rect.width > 0 && rect.height > 0;
              }}).slice(0, limit);
              return {{
                url: location.href,
                title: document.title,
                elements: visible.map((element, index) => {{
                  const ref = `${{prefix}}-${{index + 1}}`;
                  element.setAttribute('data-baijimu-ref', ref);
                  const rect = element.getBoundingClientRect();
                  const text = (element.innerText || element.getAttribute('aria-label') || element.getAttribute('placeholder') || element.value || '').replace(/\s+/g, ' ').trim().slice(0, 500);
                  return {{
                    ref,
                    tag: element.tagName.toLowerCase(),
                    role: element.getAttribute('role'),
                    name: text,
                    type: element.getAttribute('type'),
                    disabled: Boolean(element.disabled) || element.getAttribute('aria-disabled') === 'true',
                    checked: typeof element.checked === 'boolean' ? element.checked : null,
                    x: Math.round(rect.x), y: Math.round(rect.y),
                    width: Math.round(rect.width), height: Math.round(rect.height)
                  }};
                }})
              }};
            }})())"#
        );
        let remote = self
            .tab
            .evaluate(&script, false)
            .map_err(|error| format!("snapshot failed: {error}"))?;
        let encoded = remote
            .value
            .and_then(|value| value.as_str().map(str::to_string))
            .ok_or_else(|| "snapshot returned no serialized value".to_string())?;
        serde_json::from_str(&encoded).map_err(|error| format!("invalid snapshot result: {error}"))
    }

    pub fn click(&mut self, element_ref: &str) -> Result<Value, String> {
        let selector = ref_selector(element_ref)?;
        self.tab
            .wait_until_visible(&selector)
            .map_err(|error| format!("element is not visible: {error}"))?
            .click()
            .map_err(|error| format!("click failed: {error}"))?;
        Ok(self.page_state())
    }

    pub fn type_text(
        &mut self,
        element_ref: &str,
        text: &str,
        clear: bool,
    ) -> Result<Value, String> {
        if text.chars().count() > 100_000 {
            return Err("text exceeds 100000 characters".to_string());
        }
        let selector = ref_selector(element_ref)?;
        let element = self
            .tab
            .wait_until_visible(&selector)
            .map_err(|error| format!("element is not visible: {error}"))?;
        if clear {
            element
                .call_js_fn(
                    "function(){ if ('value' in this) { this.value = ''; } else if (this.isContentEditable) { this.textContent = ''; } this.dispatchEvent(new Event('input', {bubbles:true})); }",
                    Vec::new(),
                    false,
                )
                .map_err(|error| format!("failed to clear element: {error}"))?;
        }
        element
            .type_into(text)
            .map_err(|error| format!("typing failed: {error}"))?;
        Ok(self.page_state())
    }

    pub fn press_key(&mut self, key: &str) -> Result<Value, String> {
        const KEYS: &[&str] = &[
            "Enter",
            "Tab",
            "Escape",
            "Backspace",
            "Delete",
            "ArrowUp",
            "ArrowDown",
            "ArrowLeft",
            "ArrowRight",
            "Home",
            "End",
            "PageUp",
            "PageDown",
            "Space",
        ];
        if !KEYS.contains(&key) {
            return Err(format!("unsupported key: {key}"));
        }
        self.tab
            .press_key(key)
            .map_err(|error| format!("key press failed: {error}"))?;
        Ok(self.page_state())
    }

    pub fn wait_for(&self, element_ref: &str, timeout_ms: u64) -> Result<Value, String> {
        let selector = ref_selector(element_ref)?;
        self.tab
            .wait_until_visible_with_custom_timeout(&selector, Duration::from_millis(timeout_ms))
            .map_err(|error| format!("element did not become visible: {error}"))?;
        Ok(json!({"visible": true, "ref": element_ref}))
    }

    pub fn upload_files(&self, element_ref: &str, files: &[PathBuf]) -> Result<Value, String> {
        let selector = ref_selector(element_ref)?;
        let strings = files
            .iter()
            .map(|path| {
                path.to_str()
                    .map(str::to_string)
                    .ok_or_else(|| format!("upload path is not valid UTF-8: {}", path.display()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let borrowed = strings.iter().map(String::as_str).collect::<Vec<_>>();
        self.tab
            .wait_for_element(&selector)
            .map_err(|error| format!("file input was not found: {error}"))?
            .set_input_files(&borrowed)
            .map_err(|error| format!("file selection failed: {error}"))?;
        Ok(json!({"selected": files.len(), "ref": element_ref}))
    }

    pub fn screenshot(&self, format: &str, quality: u32) -> Result<Value, String> {
        let (format, media_type, quality) = match format {
            "png" => (Page::CaptureScreenshotFormatOption::Png, "image/png", None),
            "jpeg" => (
                Page::CaptureScreenshotFormatOption::Jpeg,
                "image/jpeg",
                Some(quality.clamp(30, 95)),
            ),
            other => return Err(format!("unsupported screenshot format: {other}")),
        };
        let bytes = self
            .tab
            .capture_screenshot(format, quality, None, true)
            .map_err(|error| format!("screenshot failed: {error}"))?;
        if bytes.len() > 16 * 1024 * 1024 {
            return Err("screenshot exceeds 16 MiB response limit".to_string());
        }
        Ok(json!({
            "mediaType": media_type,
            "data": BASE64.encode(bytes),
            "url": self.tab.get_url()
        }))
    }
}

pub fn active_profile_ids(
    sessions: &HashMap<String, SharedSession>,
    workspace_id: u64,
) -> Vec<String> {
    sessions
        .values()
        .filter_map(|session| session.lock().ok())
        .filter(|session| session.workspace_id == workspace_id)
        .map(|session| session.profile_id.clone())
        .collect()
}

pub fn random_id(prefix: &str) -> String {
    format!("{prefix}-{}", random_suffix())
}

fn random_suffix() -> String {
    let mut bytes = [0_u8; 12];
    OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn ref_selector(element_ref: &str) -> Result<String, String> {
    let valid = (1..=100).contains(&element_ref.len())
        && element_ref
            .bytes()
            .all(|value| value.is_ascii_alphanumeric() || value == b'-');
    if !valid || !element_ref.starts_with("bjm-") {
        return Err("invalid element ref; take a fresh snapshot".to_string());
    }
    Ok(format!("[data-baijimu-ref=\"{element_ref}\"]"))
}

pub fn authorize_url(profile: &ProfileMetadata, raw_url: &str) -> Result<Url, String> {
    if raw_url.len() > 4096 {
        return Err("URL exceeds 4096 characters".to_string());
    }
    let url = Url::parse(raw_url).map_err(|error| format!("invalid URL: {error}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("only http and https URLs are supported".to_string());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("URL credentials are not allowed".to_string());
    }
    let host = url
        .host()
        .ok_or_else(|| "URL host is required".to_string())?;
    let host_text = host
        .to_string()
        .trim_matches(['[', ']'])
        .to_ascii_lowercase();
    if is_prohibited_host(&host) {
        return Err(
            "loopback, private, link-local, and unspecified destinations are blocked".to_string(),
        );
    }
    let allowed = profile.allowed_hosts.iter().any(|allowed| {
        allowed
            .strip_prefix("*.")
            .is_some_and(|suffix| host_text.ends_with(&format!(".{suffix}")))
            || allowed == &host_text
    });
    if !allowed {
        return Err(format!(
            "host is not authorized for profile {}: {host_text}",
            profile.profile_id
        ));
    }
    if let Host::Domain(domain) = host {
        let port = url.port_or_known_default().unwrap_or(443);
        let addresses = (domain, port)
            .to_socket_addrs()
            .map_err(|error| format!("URL host resolution failed: {error}"))?
            .collect::<Vec<_>>();
        if addresses.is_empty() || addresses.iter().any(|address| prohibited_ip(address.ip())) {
            return Err("URL host resolved to a prohibited destination".to_string());
        }
    }
    Ok(url)
}

fn is_prohibited_host(host: &Host<&str>) -> bool {
    match host {
        Host::Ipv4(value) => prohibited_ip(IpAddr::V4(*value)),
        Host::Ipv6(value) => prohibited_ip(IpAddr::V6(*value)),
        Host::Domain(value) => {
            value.eq_ignore_ascii_case("localhost")
                || value.ends_with(".localhost")
                || value.ends_with(".local")
        }
    }
}

fn prohibited_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(value) => {
            value.is_private()
                || value.is_loopback()
                || value.is_link_local()
                || value.is_unspecified()
                || value.is_broadcast()
                || value.is_documentation()
                || value.octets()[0] == 0
        }
        IpAddr::V6(value) => {
            value.is_loopback()
                || value.is_unspecified()
                || value.is_unique_local()
                || value.is_unicast_link_local()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(hosts: &[&str]) -> ProfileMetadata {
        ProfileMetadata {
            schema_version: 1,
            profile_id: "test".to_string(),
            display_name: "Test".to_string(),
            workspace_id: 1211,
            allowed_hosts: hosts.iter().map(|value| value.to_string()).collect(),
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn url_policy_blocks_credentials_local_networks_and_unlisted_hosts() {
        assert!(authorize_url(&profile(&["example.com"]), "file:///etc/passwd").is_err());
        assert!(authorize_url(&profile(&["127.0.0.1"]), "http://127.0.0.1/").is_err());
        assert!(
            authorize_url(&profile(&["example.com"]), "https://user:pass@example.com/").is_err()
        );
        assert!(authorize_url(&profile(&["example.com"]), "https://openai.com/").is_err());
    }

    #[test]
    fn selectors_only_accept_connector_generated_refs() {
        assert!(ref_selector("bjm-1-abcd-2").is_ok());
        assert!(ref_selector("x\"] body").is_err());
    }
}
