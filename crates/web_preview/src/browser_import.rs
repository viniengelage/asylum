use aes::cipher::{BlockDecryptMut, KeyIvInit, block_padding::Pkcs7};
use anyhow::{Context as _, Result, anyhow, bail};
use gpui::{AppContext as _, TaskExt as _};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Seconds between 1601-01-01, Chromium's time origin, and the Unix epoch.
const WINDOWS_TO_UNIX_EPOCH_SECONDS: i64 = 11_644_473_600;
/// From this cookie database version on, Chromium prefixes each decrypted value with the
/// SHA-256 of its host, so a value can't be moved to another site.
const HOST_DIGEST_COOKIE_VERSION: i64 = 24;
const HOST_DIGEST_LENGTH: usize = 32;
/// Firefox switched `moz_cookies.expiry` from seconds to milliseconds; no cookie expires
/// after year 5138, so anything bigger is milliseconds.
const FIREFOX_EXPIRY_MILLISECONDS_THRESHOLD: i64 = 100_000_000_000;
/// Seconds between the Unix epoch and 2001-01-01, the origin of Apple's absolute time.
const UNIX_TO_APPLE_EPOCH_SECONDS: f64 = 978_307_200.;
/// Where Safari keeps its cookies since it became sandboxed, and where it kept them before.
const SAFARI_COOKIE_FILES: [&str; 2] = [
    "Library/Containers/com.apple.Safari/Data/Library/Cookies/Cookies.binarycookies",
    "Library/Cookies/Cookies.binarycookies",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BrowserKind {
    Chrome,
    Arc,
    Brave,
    Edge,
    Chromium,
    Firefox,
    Safari,
}

impl BrowserKind {
    pub const ALL: [BrowserKind; 6] = [
        BrowserKind::Chrome,
        BrowserKind::Arc,
        BrowserKind::Brave,
        BrowserKind::Edge,
        BrowserKind::Chromium,
        BrowserKind::Firefox,
    ];

    pub fn name(self) -> &'static str {
        match self {
            BrowserKind::Chrome => "Google Chrome",
            BrowserKind::Arc => "Arc",
            BrowserKind::Brave => "Brave",
            BrowserKind::Edge => "Microsoft Edge",
            BrowserKind::Chromium => "Chromium",
            BrowserKind::Firefox => "Firefox",
            BrowserKind::Safari => "Safari",
        }
    }

    pub fn short_name(self) -> &'static str {
        match self {
            BrowserKind::Chrome => "Chrome",
            BrowserKind::Edge => "Edge",
            other => other.name(),
        }
    }

    pub fn monogram(self) -> &'static str {
        match self {
            BrowserKind::Chrome | BrowserKind::Chromium => "C",
            BrowserKind::Arc => "A",
            BrowserKind::Brave => "B",
            BrowserKind::Edge => "E",
            BrowserKind::Firefox => "F",
            BrowserKind::Safari => "S",
        }
    }

    /// Where the browser keeps its profiles, under `~/Library/Application Support`.
    fn support_subdirectory(self) -> &'static str {
        match self {
            BrowserKind::Chrome => "Google/Chrome",
            BrowserKind::Arc => "Arc/User Data",
            BrowserKind::Brave => "BraveSoftware/Brave-Browser",
            BrowserKind::Edge => "Microsoft Edge",
            BrowserKind::Chromium => "Chromium",
            BrowserKind::Firefox => "Firefox",
            BrowserKind::Safari => "Safari",
        }
    }

    /// The Keychain item holding the password Chromium-based browsers derive their cookie
    /// key from.
    fn keychain_item(self) -> Option<(&'static str, &'static str)> {
        match self {
            BrowserKind::Chrome => Some(("Chrome Safe Storage", "Chrome")),
            BrowserKind::Arc => Some(("Arc Safe Storage", "Arc")),
            BrowserKind::Brave => Some(("Brave Safe Storage", "Brave")),
            BrowserKind::Edge => Some(("Microsoft Edge Safe Storage", "Microsoft Edge")),
            BrowserKind::Chromium => Some(("Chromium Safe Storage", "Chromium")),
            BrowserKind::Firefox | BrowserKind::Safari => None,
        }
    }

    pub fn keychain_service(self) -> Option<&'static str> {
        self.keychain_item().map(|(service, _)| service)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SourceProfile {
    pub name: String,
    pub directory: PathBuf,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BrowserSource {
    pub kind: BrowserKind,
    pub profiles: Vec<SourceProfile>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Bookmark {
    pub title: String,
    pub url: String,
    #[serde(default)]
    pub folder: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SameSite {
    Unspecified,
    None,
    Lax,
    Strict,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ImportedCookie {
    pub name: String,
    pub value: String,
    /// The cookie's domain as stored: a leading dot marks a domain cookie, none a cookie
    /// for that host only.
    pub domain: String,
    pub path: String,
    pub secure: bool,
    pub http_only: bool,
    /// Microseconds since 1601-01-01, or `None` for a session cookie.
    pub expires: Option<i64>,
    pub same_site: SameSite,
}

impl ImportedCookie {
    pub fn host(&self) -> &str {
        self.domain.trim_start_matches('.')
    }

    pub fn is_host_only(&self) -> bool {
        !self.domain.starts_with('.')
    }

    /// The address a cookie store needs to accept the cookie for its site.
    pub fn url(&self) -> String {
        let scheme = if self.secure { "https" } else { "http" };
        format!("{scheme}://{}{}", self.host(), self.path)
    }
}

pub fn detect_sources() -> Vec<BrowserSource> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    let mut sources = detect_sources_in(&home.join("Library/Application Support"));
    // Safari ships with macOS, and its folder can't even be listed without Full Disk
    // Access, so it is always offered and the modal explains when it is out of reach.
    sources.push(BrowserSource {
        kind: BrowserKind::Safari,
        profiles: vec![SourceProfile {
            name: "Safari".into(),
            directory: home.join("Library/Safari"),
        }],
    });
    sources
}

/// Raised when macOS keeps a browser's files away from Asylum until the user grants it Full
/// Disk Access, which is always the case for Safari's.
#[derive(Debug)]
pub struct FullDiskAccessRequired;

impl std::fmt::Display for FullDiskAccessRequired {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("macOS needs Asylum to have Full Disk Access to read this browser")
    }
}

impl std::error::Error for FullDiskAccessRequired {}

fn read_protected(path: &Path) -> Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            Err(FullDiskAccessRequired.into())
        }
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

/// Whether Asylum may read Safari's data, which takes Full Disk Access.
pub fn safari_readable() -> bool {
    let Some(home) = dirs::home_dir() else {
        return false;
    };
    !matches!(
        std::fs::read_dir(home.join("Library/Safari")),
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied
    )
}

fn detect_sources_in(support: &Path) -> Vec<BrowserSource> {
    BrowserKind::ALL
        .into_iter()
        .filter_map(|kind| {
            let base = support.join(kind.support_subdirectory());
            let profiles = if kind == BrowserKind::Firefox {
                firefox_profiles(&base)
            } else {
                chromium_profiles(&base)
            };
            (!profiles.is_empty()).then_some(BrowserSource { kind, profiles })
        })
        .collect()
}

/// The profiles listed in `Local State`, keeping only those with something to import.
fn chromium_profiles(base: &Path) -> Vec<SourceProfile> {
    let has_data = |directory: &Path| {
        directory.join("Bookmarks").is_file() || chromium_cookie_database(directory).is_some()
    };
    let mut profiles = std::fs::read_to_string(base.join("Local State"))
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|state| {
            let cache = state
                .get("profile")?
                .get("info_cache")?
                .as_object()?
                .clone();
            Some(
                cache
                    .into_iter()
                    .map(|(directory, info)| SourceProfile {
                        name: info
                            .get("name")
                            .and_then(|name| name.as_str())
                            .unwrap_or(&directory)
                            .to_string(),
                        directory: base.join(&directory),
                    })
                    .collect::<Vec<_>>(),
            )
        })
        .unwrap_or_default();
    if profiles.is_empty() {
        profiles.push(SourceProfile {
            name: "Default".into(),
            directory: base.join("Default"),
        });
    }
    profiles.retain(|profile| has_data(&profile.directory));
    profiles.sort_by(|left, right| left.directory.cmp(&right.directory));
    profiles
}

fn firefox_profiles(base: &Path) -> Vec<SourceProfile> {
    let Ok(text) = std::fs::read_to_string(base.join("profiles.ini")) else {
        return Vec::new();
    };
    parse_firefox_profiles(&text, base)
        .into_iter()
        .filter(|profile| profile.directory.join("places.sqlite").is_file())
        .collect()
}

fn parse_firefox_profiles(text: &str, base: &Path) -> Vec<SourceProfile> {
    let mut sections: Vec<BTreeMap<String, String>> = Vec::new();
    for line in text.lines().map(str::trim) {
        if line.starts_with("[Profile") {
            sections.push(BTreeMap::new());
        } else if line.starts_with('[') {
            sections.push(BTreeMap::from([("ignore".to_string(), String::new())]));
        } else if let Some((key, value)) = line.split_once('=')
            && let Some(section) = sections.last_mut()
        {
            section.insert(key.trim().to_string(), value.trim().to_string());
        }
    }
    sections
        .into_iter()
        .filter(|section| !section.contains_key("ignore"))
        .filter_map(|section| {
            let path = section.get("Path")?;
            let directory = if section.get("IsRelative").is_none_or(|value| value == "1") {
                base.join(path)
            } else {
                PathBuf::from(path)
            };
            Some(SourceProfile {
                name: section.get("Name").cloned().unwrap_or_else(|| path.clone()),
                directory,
            })
        })
        .collect()
}

pub fn read_bookmarks(kind: BrowserKind, profile: &Path) -> Result<Vec<Bookmark>> {
    match kind {
        BrowserKind::Firefox => read_firefox_bookmarks(profile),
        BrowserKind::Arc => read_arc_pinned_tabs(profile),
        BrowserKind::Safari => match read_protected(&profile.join("Bookmarks.plist"))? {
            Some(bytes) => parse_safari_bookmarks(&bytes),
            None => Ok(Vec::new()),
        },
        _ => match std::fs::read_to_string(profile.join("Bookmarks")) {
            Ok(text) => parse_chromium_bookmarks(&text),
            // A profile that never saved a bookmark has no file.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(error) => Err(error).context("reading the Bookmarks file"),
        },
    }
}

/// Arc keeps what its users treat as bookmarks, the pinned tabs of each Space, in its own
/// sidebar file one level above the Chromium profiles.
fn read_arc_pinned_tabs(profile: &Path) -> Result<Vec<Bookmark>> {
    let sidebar = profile
        .parent()
        .and_then(Path::parent)
        .map(|arc| arc.join("StorableSidebar.json"))
        .ok_or_else(|| anyhow!("{} is not inside Arc's folder", profile.display()))?;
    let text = match std::fs::read_to_string(&sidebar) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).context("reading Arc's sidebar"),
    };
    let directory = profile
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    parse_arc_pinned_tabs(&text, &directory)
}

fn parse_arc_pinned_tabs(text: &str, profile_directory: &str) -> Result<Vec<Bookmark>> {
    use serde_json::Value;

    fn objects(value: Option<&Value>) -> impl Iterator<Item = &serde_json::Map<String, Value>> {
        value
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_object)
    }

    fn walk(
        id: &str,
        items: &BTreeMap<&str, &serde_json::Map<String, Value>>,
        folder: Option<&str>,
        bookmarks: &mut Vec<Bookmark>,
        depth: usize,
    ) {
        // Arc's sidebar is a tree; the bound guards against a malformed file with a cycle.
        if depth > 32 {
            return;
        }
        let Some(item) = items.get(id) else {
            return;
        };
        if let Some(tab) = item.get("data").and_then(|data| data.get("tab")) {
            if let Some(url) = tab.get("savedURL").and_then(Value::as_str)
                && url.starts_with("http")
            {
                let title = item
                    .get("title")
                    .and_then(Value::as_str)
                    .or_else(|| tab.get("savedTitle").and_then(Value::as_str))
                    .filter(|title| !title.is_empty())
                    .unwrap_or(url);
                bookmarks.push(Bookmark {
                    title: title.to_string(),
                    url: url.to_string(),
                    folder: folder.map(str::to_string),
                });
            }
            return;
        }
        let is_folder = item.get("data").and_then(|data| data.get("list")).is_some();
        let folder = if is_folder && folder.is_none() {
            item.get("title").and_then(Value::as_str)
        } else {
            folder
        };
        for child in item
            .get("childrenIds")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            walk(child, items, folder, bookmarks, depth + 1);
        }
    }

    let document: Value = serde_json::from_str(text)?;
    let containers = document
        .get("sidebar")
        .and_then(|sidebar| sidebar.get("containers"));
    let Some(container) = objects(containers).find(|container| container.contains_key("spaces"))
    else {
        return Ok(Vec::new());
    };
    let items: BTreeMap<&str, &serde_json::Map<String, Value>> = objects(container.get("items"))
        .filter_map(|item| Some((item.get("id")?.as_str()?, item)))
        .collect();

    let mut bookmarks = Vec::new();
    for space in objects(container.get("spaces")) {
        let profile = space.get("profile");
        let space_directory = match profile.and_then(|profile| profile.get("custom")) {
            Some(custom) => custom
                .get("_0")
                .and_then(|inner| inner.get("directoryBasename"))
                .and_then(Value::as_str)
                .unwrap_or_default(),
            None => "Default",
        };
        if space_directory != profile_directory {
            continue;
        }
        let ids: Vec<&str> = space
            .get("containerIDs")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        let pinned = ids
            .iter()
            .position(|id| *id == "pinned")
            .and_then(|index| ids.get(index + 1));
        if let Some(pinned) = pinned {
            let space_title = space.get("title").and_then(Value::as_str);
            let before = bookmarks.len();
            walk(pinned, &items, None, &mut bookmarks, 0);
            for bookmark in bookmarks.iter_mut().skip(before) {
                if bookmark.folder.is_none() {
                    bookmark.folder = space_title.map(str::to_string);
                }
            }
        }
    }
    Ok(bookmarks)
}

fn parse_chromium_bookmarks(text: &str) -> Result<Vec<Bookmark>> {
    fn walk(node: &serde_json::Value, folder: Option<&str>, bookmarks: &mut Vec<Bookmark>) {
        let name = node.get("name").and_then(|name| name.as_str());
        match node.get("type").and_then(|kind| kind.as_str()) {
            Some("url") => {
                if let Some(url) = node.get("url").and_then(|url| url.as_str())
                    && url.starts_with("http")
                {
                    bookmarks.push(Bookmark {
                        title: name
                            .filter(|name| !name.is_empty())
                            .unwrap_or(url)
                            .to_string(),
                        url: url.to_string(),
                        folder: folder.map(str::to_string),
                    });
                }
            }
            Some("folder") => {
                for child in node
                    .get("children")
                    .and_then(|children| children.as_array())
                    .into_iter()
                    .flatten()
                {
                    walk(child, folder.or(name), bookmarks);
                }
            }
            _ => {}
        }
    }

    let document: serde_json::Value = serde_json::from_str(text)?;
    let roots = document
        .get("roots")
        .ok_or_else(|| anyhow!("the Bookmarks file has no roots"))?;
    let mut bookmarks = Vec::new();
    for root in ["bookmark_bar", "other", "synced"] {
        if let Some(root) = roots.get(root) {
            for child in root
                .get("children")
                .and_then(|children| children.as_array())
                .into_iter()
                .flatten()
            {
                walk(child, None, &mut bookmarks);
            }
        }
    }
    Ok(bookmarks)
}

fn read_firefox_bookmarks(profile: &Path) -> Result<Vec<Bookmark>> {
    let copy = DatabaseCopy::new(&profile.join("places.sqlite"))?;
    let connection = sqlez::connection::Connection::open_file(&copy.path_string());
    let rows: Vec<(Option<String>, String, Option<String>)> = connection.select(
        "SELECT bookmark.title, place.url, parent.title \
         FROM moz_bookmarks AS bookmark \
         JOIN moz_places AS place ON bookmark.fk = place.id \
         LEFT JOIN moz_bookmarks AS parent ON bookmark.parent = parent.id \
         WHERE bookmark.type = 1 AND place.url LIKE 'http%' \
         ORDER BY bookmark.parent, bookmark.position",
    )?()?;
    Ok(rows
        .into_iter()
        .map(|(title, url, folder)| Bookmark {
            title: title
                .filter(|title| !title.is_empty())
                .unwrap_or_else(|| url.clone()),
            url,
            // Firefox's own roots ("toolbar", "menu", "unfiled") aren't folders the user made.
            folder: folder.filter(|folder| {
                !folder.is_empty() && !matches!(folder.as_str(), "toolbar" | "menu" | "unfiled")
            }),
        })
        .collect())
}

/// Reads the profile's cookies. For Chromium-based browsers this asks the Keychain for the
/// browser's storage password, which makes macOS ask the user.
pub async fn read_cookies(kind: BrowserKind, profile: &Path) -> Result<Vec<ImportedCookie>> {
    match kind.keychain_item() {
        None if kind == BrowserKind::Safari => read_safari_cookies(),
        None => read_firefox_cookies(profile),
        Some((service, account)) => {
            let password = keychain_password(service, account).await?;
            let database = chromium_cookie_database(profile)
                .ok_or_else(|| anyhow!("{} has no cookie database", profile.display()))?;
            read_chromium_cookies(&database, &chromium_cookie_key(&password))
        }
    }
}

/// Chromium moved the cookie database into `Network/` in version 96; profiles that have not
/// been opened since still keep it at the top.
fn chromium_cookie_database(profile: &Path) -> Option<PathBuf> {
    [profile.join("Network/Cookies"), profile.join("Cookies")]
        .into_iter()
        .find(|path| path.is_file())
}

#[derive(Debug)]
pub struct KeychainDenied;

impl std::fmt::Display for KeychainDenied {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("the Keychain did not give out the browser's storage password")
    }
}

impl std::error::Error for KeychainDenied {}

async fn keychain_password(service: &str, account: &str) -> Result<String> {
    let output = smol::process::Command::new("/usr/bin/security")
        .args(["find-generic-password", "-w", "-s", service, "-a", account])
        .output()
        .await
        .context("running the security tool")?;
    if !output.status.success() {
        return Err(KeychainDenied.into());
    }
    let password = String::from_utf8(output.stdout)?.trim_end().to_string();
    if password.is_empty() {
        return Err(KeychainDenied.into());
    }
    Ok(password)
}

fn chromium_cookie_key(password: &str) -> [u8; 16] {
    let mut key = [0u8; 16];
    pbkdf2::pbkdf2_hmac::<sha1::Sha1>(password.as_bytes(), b"saltysalt", 1003, &mut key);
    key
}

fn decrypt_chromium_value(
    encrypted: &[u8],
    key: &[u8; 16],
    host: &str,
    database_version: i64,
) -> Result<String> {
    let Some(ciphertext) = encrypted
        .strip_prefix(b"v10")
        .or_else(|| encrypted.strip_prefix(b"v11"))
    else {
        bail!("unknown cookie encryption");
    };
    let mut buffer = ciphertext.to_vec();
    let plaintext = cbc::Decryptor::<aes::Aes128>::new(key.into(), &[b' '; 16].into())
        .decrypt_padded_mut::<Pkcs7>(&mut buffer)
        .map_err(|_| anyhow!("the cookie did not decrypt"))?;
    let plaintext = if database_version >= HOST_DIGEST_COOKIE_VERSION {
        let (digest, value) = plaintext
            .split_at_checked(HOST_DIGEST_LENGTH)
            .ok_or_else(|| anyhow!("the decrypted cookie is too short"))?;
        if digest != Sha256::digest(host.as_bytes()).as_slice() {
            bail!("the cookie belongs to another host");
        }
        value
    } else {
        plaintext
    };
    Ok(String::from_utf8(plaintext.to_vec())?)
}

fn read_chromium_cookies(database: &Path, key: &[u8; 16]) -> Result<Vec<ImportedCookie>> {
    let copy = DatabaseCopy::new(database)?;
    let connection = sqlez::connection::Connection::open_file(&copy.path_string());
    let version = connection
        .select_row::<String>("SELECT value FROM meta WHERE key = 'version'")?()?
    .and_then(|version| version.parse::<i64>().ok())
    .unwrap_or(0);
    type Row = (
        String,
        String,
        String,
        Option<Vec<u8>>,
        String,
        i64,
        i64,
        i64,
        i64,
        i64,
    );
    let rows: Vec<Row> = connection.select(
        "SELECT host_key, name, value, encrypted_value, path, expires_utc, has_expires, \
         is_secure, is_httponly, samesite FROM cookies",
    )?()?;
    let mut cookies = Vec::with_capacity(rows.len());
    for (
        domain,
        name,
        value,
        encrypted,
        path,
        expires,
        has_expires,
        secure,
        http_only,
        same_site,
    ) in rows
    {
        let value = match encrypted.filter(|encrypted| !encrypted.is_empty()) {
            Some(encrypted) => match decrypt_chromium_value(&encrypted, key, &domain, version) {
                Ok(value) => value,
                Err(error) => {
                    log::debug!("browser import: skipping a cookie for {domain}: {error}");
                    continue;
                }
            },
            None => value,
        };
        cookies.push(ImportedCookie {
            name,
            value,
            domain,
            path,
            secure: secure != 0,
            http_only: http_only != 0,
            expires: (has_expires != 0 && expires > 0).then_some(expires),
            same_site: match same_site {
                0 => SameSite::None,
                1 => SameSite::Lax,
                2 => SameSite::Strict,
                _ => SameSite::Unspecified,
            },
        });
    }
    Ok(cookies)
}

fn read_firefox_cookies(profile: &Path) -> Result<Vec<ImportedCookie>> {
    let copy = DatabaseCopy::new(&profile.join("cookies.sqlite"))?;
    let connection = sqlez::connection::Connection::open_file(&copy.path_string());
    type Row = (String, String, String, String, i64, i64, i64, i64);
    let rows: Vec<Row> = connection.select(
        "SELECT host, name, value, path, expiry, isSecure, isHttpOnly, sameSite FROM moz_cookies",
    )?()?;
    Ok(rows
        .into_iter()
        .map(
            |(domain, name, value, path, expiry, secure, http_only, same_site)| ImportedCookie {
                name,
                value,
                domain,
                path,
                secure: secure != 0,
                http_only: http_only != 0,
                expires: (expiry > 0)
                    .then(|| unix_to_chromium_time(firefox_expiry_seconds(expiry))),
                same_site: match same_site {
                    0 => SameSite::None,
                    1 => SameSite::Lax,
                    2 => SameSite::Strict,
                    _ => SameSite::Unspecified,
                },
            },
        )
        .collect())
}

/// Safari's bookmarks: a tree of lists and leaves, with the Reading List and History kept
/// in it as well.
fn parse_safari_bookmarks(bytes: &[u8]) -> Result<Vec<Bookmark>> {
    fn walk(
        node: &plist::Dictionary,
        folder: Option<&str>,
        bookmarks: &mut Vec<Bookmark>,
        depth: usize,
    ) {
        if depth > 32 {
            return;
        }
        let text = |key: &str| node.get(key).and_then(plist::Value::as_string);
        match text("WebBookmarkType") {
            Some("WebBookmarkTypeLeaf") => {
                let Some(url) = text("URLString").filter(|url| url.starts_with("http")) else {
                    return;
                };
                let title = node
                    .get("URIDictionary")
                    .and_then(plist::Value::as_dictionary)
                    .and_then(|uri| uri.get("title"))
                    .and_then(plist::Value::as_string)
                    .filter(|title| !title.is_empty())
                    .unwrap_or(url);
                bookmarks.push(Bookmark {
                    title: title.to_string(),
                    url: url.to_string(),
                    folder: folder.map(str::to_string),
                });
            }
            Some("WebBookmarkTypeList") => {
                let title = text("Title");
                if title == Some("com.apple.ReadingList") {
                    return;
                }
                // The favorites bar and the menu are Safari's own roots, not folders the
                // user made.
                let folder = match title {
                    _ if depth == 0 => None,
                    Some("BookmarksBar" | "BookmarksMenu") | None => folder,
                    Some(title) => Some(title),
                };
                for child in node
                    .get("Children")
                    .and_then(plist::Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(plist::Value::as_dictionary)
                {
                    walk(child, folder, bookmarks, depth + 1);
                }
            }
            _ => {}
        }
    }

    let root = plist::Value::from_reader(std::io::Cursor::new(bytes))
        .context("reading Safari's Bookmarks.plist")?;
    let root = root
        .as_dictionary()
        .ok_or_else(|| anyhow!("Safari's Bookmarks.plist is not a dictionary"))?;
    let mut bookmarks = Vec::new();
    walk(root, None, &mut bookmarks, 0);
    Ok(bookmarks)
}

fn read_safari_cookies() -> Result<Vec<ImportedCookie>> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("no home directory"))?;
    for file in SAFARI_COOKIE_FILES {
        if let Some(bytes) = read_protected(&home.join(file))? {
            return parse_binary_cookies(&bytes);
        }
    }
    Ok(Vec::new())
}

/// `Cookies.binarycookies`: a big-endian header listing page sizes, then pages of
/// little-endian cookie records whose strings sit at offsets inside each record.
fn parse_binary_cookies(bytes: &[u8]) -> Result<Vec<ImportedCookie>> {
    fn slice(bytes: &[u8], start: usize, length: usize) -> Result<&[u8]> {
        bytes
            .get(start..start.checked_add(length).ok_or_else(|| anyhow!("offset overflow"))?)
            .ok_or_else(|| anyhow!("the cookie file is truncated"))
    }
    fn u32_be(bytes: &[u8], at: usize) -> Result<usize> {
        let field: [u8; 4] = slice(bytes, at, 4)?.try_into()?;
        Ok(u32::from_be_bytes(field) as usize)
    }
    fn u32_le(bytes: &[u8], at: usize) -> Result<usize> {
        let field: [u8; 4] = slice(bytes, at, 4)?.try_into()?;
        Ok(u32::from_le_bytes(field) as usize)
    }
    fn f64_le(bytes: &[u8], at: usize) -> Result<f64> {
        let field: [u8; 8] = slice(bytes, at, 8)?.try_into()?;
        Ok(f64::from_le_bytes(field))
    }
    fn string_at(record: &[u8], offset: usize) -> Result<String> {
        let tail = record
            .get(offset..)
            .ok_or_else(|| anyhow!("a cookie string is out of bounds"))?;
        let end = tail.iter().position(|byte| *byte == 0).unwrap_or(tail.len());
        Ok(String::from_utf8_lossy(&tail[..end]).into_owned())
    }

    if slice(bytes, 0, 4)? != b"cook" {
        bail!("not a Safari cookie file");
    }
    let page_count = u32_be(bytes, 4)?;
    let mut page_sizes = Vec::with_capacity(page_count.min(4096));
    for index in 0..page_count {
        page_sizes.push(u32_be(bytes, 8 + index * 4)?);
    }
    let mut page_start = 8 + page_count * 4;
    let mut cookies = Vec::new();
    for page_size in page_sizes {
        let page = slice(bytes, page_start, page_size)?;
        page_start += page_size;
        let cookie_count = u32_le(page, 4)?;
        for index in 0..cookie_count {
            let record_start = u32_le(page, 8 + index * 4)?;
            let record_size = u32_le(page, record_start)?;
            let record = slice(page, record_start, record_size)?;
            let flags = u32_le(record, 8)?;
            let expires = f64_le(record, 40)?;
            cookies.push(ImportedCookie {
                domain: string_at(record, u32_le(record, 16)?)?,
                name: string_at(record, u32_le(record, 20)?)?,
                path: string_at(record, u32_le(record, 24)?)?,
                value: string_at(record, u32_le(record, 28)?)?,
                secure: flags & 1 != 0,
                http_only: flags & 4 != 0,
                expires: (expires > 0.).then(|| {
                    unix_to_chromium_time((expires + UNIX_TO_APPLE_EPOCH_SECONDS) as i64)
                }),
                same_site: SameSite::Unspecified,
            });
        }
    }
    Ok(cookies)
}

fn firefox_expiry_seconds(expiry: i64) -> i64 {
    if expiry > FIREFOX_EXPIRY_MILLISECONDS_THRESHOLD {
        expiry / 1000
    } else {
        expiry
    }
}

fn unix_to_chromium_time(seconds: i64) -> i64 {
    seconds
        .saturating_add(WINDOWS_TO_UNIX_EPOCH_SECONDS)
        .saturating_mul(1_000_000)
}

/// A private copy of a browser's database. The browser may have it open and locked, and its
/// newest rows can still sit in the write-ahead log next to it.
struct DatabaseCopy {
    directory: tempfile::TempDir,
    path: PathBuf,
}

impl DatabaseCopy {
    fn new(source: &Path) -> Result<Self> {
        let directory = tempfile::tempdir()?;
        let file_name = source
            .file_name()
            .ok_or_else(|| anyhow!("{} has no file name", source.display()))?;
        let path = directory.path().join(file_name);
        std::fs::copy(source, &path).with_context(|| format!("copying {}", source.display()))?;
        for suffix in ["-wal", "-shm"] {
            let mut companion = source.as_os_str().to_owned();
            companion.push(suffix);
            let companion = PathBuf::from(companion);
            if companion.is_file() {
                let mut target = path.as_os_str().to_owned();
                target.push(suffix);
                std::fs::copy(&companion, PathBuf::from(target))?;
            }
        }
        Ok(Self { directory, path })
    }

    fn path_string(&self) -> String {
        debug_assert!(self.path.starts_with(self.directory.path()));
        self.path.to_string_lossy().into_owned()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ImportRecord {
    pub source: String,
    /// Unix seconds.
    pub imported_at: i64,
    pub bookmarks: usize,
    pub cookies: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CookieKey {
    pub url: String,
    pub name: String,
}

#[derive(Default, Serialize, Deserialize)]
struct LibraryData {
    #[serde(default)]
    bookmarks: Vec<Bookmark>,
    /// URLs of the bookmarks an import brought, so "clear imported data" leaves the ones
    /// starred here.
    #[serde(default)]
    imported_bookmarks: Vec<String>,
    #[serde(default)]
    imported_cookies: Vec<CookieKey>,
    #[serde(default)]
    last_import: Option<ImportRecord>,
    #[serde(default)]
    offer_dismissed: bool,
}

/// The browser's bookmarks and what was imported into this profile, kept next to the CEF
/// profile so each Asylum profile has its own.
pub struct BrowserLibrary {
    data: LibraryData,
    path: PathBuf,
}

impl gpui::Global for BrowserLibrary {}

impl BrowserLibrary {
    pub fn global(cx: &mut gpui::App) -> &mut Self {
        if !cx.has_global::<Self>() {
            cx.set_global(Self::load());
        }
        cx.global_mut::<Self>()
    }

    fn load() -> Self {
        let profile = paths::active_profile_id().unwrap_or(paths::DEFAULT_PROFILE_ID);
        let path = crate::cef_paths::support_dir()
            .join("Library")
            .join(format!("{profile}.json"));
        let data = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|error| {
                log::error!(
                    "browser library at {} is unreadable: {error}",
                    path.display()
                );
                LibraryData::default()
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => LibraryData::default(),
            Err(error) => {
                log::error!("reading the browser library at {}: {error}", path.display());
                LibraryData::default()
            }
        };
        Self { data, path }
    }

    pub fn bookmarks(&self) -> &[Bookmark] {
        &self.data.bookmarks
    }

    pub fn is_bookmarked(&self, url: &str) -> bool {
        self.data
            .bookmarks
            .iter()
            .any(|bookmark| bookmark.url == url)
    }

    pub fn last_import(&self) -> Option<&ImportRecord> {
        self.data.last_import.as_ref()
    }

    pub fn offers_import(&self) -> bool {
        !self.data.offer_dismissed && self.data.last_import.is_none()
    }

    pub fn has_imported_data(&self) -> bool {
        !self.data.imported_bookmarks.is_empty() || !self.data.imported_cookies.is_empty()
    }

    /// Changes the library and writes it to disk.
    pub fn update<R>(cx: &mut gpui::App, change: impl FnOnce(&mut Self) -> R) -> R {
        let result = change(Self::global(cx));
        cx.global::<Self>().save(cx);
        result
    }

    pub fn dismiss_offer(&mut self) {
        self.data.offer_dismissed = true;
    }

    pub fn toggle_bookmark(&mut self, title: &str, url: &str) {
        if self.is_bookmarked(url) {
            self.data.bookmarks.retain(|bookmark| bookmark.url != url);
        } else {
            self.data.bookmarks.insert(
                0,
                Bookmark {
                    title: if title.is_empty() { url } else { title }.to_string(),
                    url: url.to_string(),
                    folder: None,
                },
            );
        }
    }

    /// Adds what an import brought, skipping bookmarks already here, and returns how many
    /// bookmarks were new.
    pub fn record_import(
        &mut self,
        bookmarks: Vec<Bookmark>,
        cookies: Vec<CookieKey>,
        record: ImportRecord,
    ) -> usize {
        let mut added = 0;
        for bookmark in bookmarks {
            if self.is_bookmarked(&bookmark.url) {
                continue;
            }
            self.data.imported_bookmarks.push(bookmark.url.clone());
            self.data.bookmarks.push(bookmark);
            added += 1;
        }
        for cookie in cookies {
            if !self.data.imported_cookies.contains(&cookie) {
                self.data.imported_cookies.push(cookie);
            }
        }
        self.data.last_import = Some(record);
        added
    }

    /// Forgets everything imports brought and returns the cookies to delete from the store.
    pub fn clear_imported(&mut self) -> Vec<CookieKey> {
        let imported: std::collections::HashSet<String> =
            self.data.imported_bookmarks.drain(..).collect();
        self.data
            .bookmarks
            .retain(|bookmark| !imported.contains(&bookmark.url));
        self.data.last_import = None;
        std::mem::take(&mut self.data.imported_cookies)
    }

    fn save(&self, cx: &gpui::App) {
        let path = self.path.clone();
        let text = match serde_json::to_string_pretty(&self.data) {
            Ok(text) => text,
            Err(error) => {
                log::error!("serializing the browser library: {error}");
                return;
            }
        };
        cx.background_spawn(async move {
            if let Some(directory) = path.parent() {
                std::fs::create_dir_all(directory)?;
            }
            std::fs::write(&path, text)?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safari_bookmarks_skip_the_reading_list_and_its_own_roots() {
        let leaf = |title: &str, url: &str| {
            plist::Value::Dictionary(plist::Dictionary::from_iter([
                ("WebBookmarkType".to_string(), plist::Value::from("WebBookmarkTypeLeaf")),
                ("URLString".to_string(), plist::Value::from(url)),
                (
                    "URIDictionary".to_string(),
                    plist::Value::Dictionary(plist::Dictionary::from_iter([(
                        "title".to_string(),
                        plist::Value::from(title),
                    )])),
                ),
            ]))
        };
        let list = |title: &str, children: Vec<plist::Value>| {
            plist::Value::Dictionary(plist::Dictionary::from_iter([
                ("WebBookmarkType".to_string(), plist::Value::from("WebBookmarkTypeList")),
                ("Title".to_string(), plist::Value::from(title)),
                ("Children".to_string(), plist::Value::Array(children)),
            ]))
        };
        let root = list(
            "",
            vec![
                list(
                    "BookmarksBar",
                    vec![
                        leaf("GitHub", "https://github.com/"),
                        list("Trix", vec![leaf("", "https://trix.com.br/")]),
                    ],
                ),
                list("BookmarksMenu", vec![leaf("Apple", "https://apple.com/")]),
                list(
                    "com.apple.ReadingList",
                    vec![leaf("Later", "https://example.com/")],
                ),
            ],
        );
        let mut bytes = Vec::new();
        plist::to_writer_binary(&mut bytes, &root).expect("serializing the fixture");
        let bookmarks = parse_safari_bookmarks(&bytes).expect("parsing");
        let summary: Vec<(&str, &str, Option<&str>)> = bookmarks
            .iter()
            .map(|bookmark| {
                (
                    bookmark.title.as_str(),
                    bookmark.url.as_str(),
                    bookmark.folder.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            vec![
                ("GitHub", "https://github.com/", None),
                ("https://trix.com.br/", "https://trix.com.br/", Some("Trix")),
                ("Apple", "https://apple.com/", None),
            ]
        );
    }

    fn binary_cookie_record(domain: &str, name: &str, path: &str, value: &str, flags: u32) -> Vec<u8> {
        const HEADER: usize = 56;
        let mut strings = Vec::new();
        let mut offsets = Vec::new();
        for text in [domain, name, path, value] {
            offsets.push((HEADER + strings.len()) as u32);
            strings.extend_from_slice(text.as_bytes());
            strings.push(0);
        }
        let size = (HEADER + strings.len()) as u32;
        let mut record = Vec::new();
        record.extend_from_slice(&size.to_le_bytes());
        record.extend_from_slice(&0u32.to_le_bytes());
        record.extend_from_slice(&flags.to_le_bytes());
        record.extend_from_slice(&0u32.to_le_bytes());
        for offset in offsets {
            record.extend_from_slice(&offset.to_le_bytes());
        }
        record.extend_from_slice(&0u64.to_le_bytes());
        // 2030-01-01T00:00:00Z in Apple's absolute time.
        record.extend_from_slice(&915_148_800f64.to_le_bytes());
        record.extend_from_slice(&0f64.to_le_bytes());
        record.extend_from_slice(&strings);
        record
    }

    #[test]
    fn safari_binary_cookies_parse() {
        let records = [
            binary_cookie_record(".github.com", "user_session", "/", "abc", 5),
            binary_cookie_record("example.com", "theme", "/docs", "dark", 0),
        ];
        let header = 8 + records.len() * 4 + 4;
        let mut page = Vec::new();
        page.extend_from_slice(&0x0000_0100u32.to_be_bytes());
        page.extend_from_slice(&(records.len() as u32).to_le_bytes());
        let mut offset = header;
        for record in &records {
            page.extend_from_slice(&(offset as u32).to_le_bytes());
            offset += record.len();
        }
        page.extend_from_slice(&0u32.to_le_bytes());
        for record in &records {
            page.extend_from_slice(record);
        }
        let mut file = b"cook".to_vec();
        file.extend_from_slice(&1u32.to_be_bytes());
        file.extend_from_slice(&(page.len() as u32).to_be_bytes());
        file.extend_from_slice(&page);

        let cookies = parse_binary_cookies(&file).expect("parsing");
        assert_eq!(cookies.len(), 2);
        let session = &cookies[0];
        assert_eq!(
            (session.domain.as_str(), session.name.as_str(), session.value.as_str()),
            (".github.com", "user_session", "abc")
        );
        assert!(session.secure && session.http_only && !session.is_host_only());
        assert_eq!(session.expires, Some(unix_to_chromium_time(1_893_456_000)));
        let theme = &cookies[1];
        assert_eq!((theme.path.as_str(), theme.secure), ("/docs", false));
        assert!(theme.is_host_only());

        assert!(parse_binary_cookies(b"cook\0\0\0\x01").is_err());
        assert!(parse_binary_cookies(b"nope").is_err());
    }

    #[test]
    fn chromium_bookmarks_keep_their_top_folder() {
        let text = r#"{"roots": {
            "bookmark_bar": {"children": [
                {"type": "url", "name": "GitHub", "url": "https://github.com/"},
                {"type": "folder", "name": "Trix", "children": [
                    {"type": "folder", "name": "Infra", "children": [
                        {"type": "url", "name": "", "url": "https://console.aws.amazon.com/"}
                    ]},
                    {"type": "url", "name": "Settings", "url": "chrome://settings"}
                ]}
            ]},
            "other": {"children": [{"type": "url", "name": "Expo", "url": "https://expo.dev/"}]}
        }}"#;
        let bookmarks = parse_chromium_bookmarks(text).expect("valid bookmarks");
        assert_eq!(
            bookmarks,
            vec![
                Bookmark {
                    title: "GitHub".into(),
                    url: "https://github.com/".into(),
                    folder: None,
                },
                Bookmark {
                    title: "https://console.aws.amazon.com/".into(),
                    url: "https://console.aws.amazon.com/".into(),
                    folder: Some("Trix".into()),
                },
                Bookmark {
                    title: "Expo".into(),
                    url: "https://expo.dev/".into(),
                    folder: None,
                },
            ]
        );
    }

    #[test]
    fn arc_pinned_tabs_belong_to_their_profile() {
        let text = r#"{"sidebar": {"containers": [{"global": {}}, {
            "spaces": ["space-a", {"id": "space-a", "title": "Trabalho",
                "profile": {"custom": {"_0": {"directoryBasename": "Profile 1"}}},
                "containerIDs": ["pinned", "pinned-a", "unpinned", "unpinned-a"]},
                {"id": "space-b", "title": "Pessoal", "profile": {"default": true},
                "containerIDs": ["pinned", "pinned-b"]}],
            "items": [
                {"id": "pinned-a", "childrenIds": ["tab-1", "folder-1"], "data": {"itemContainer": {}}},
                {"id": "tab-1", "title": null, "data": {"tab": {"savedTitle": "GitHub", "savedURL": "https://github.com/"}}},
                {"id": "folder-1", "title": "Infra", "childrenIds": ["tab-2"], "data": {"list": {}}},
                {"id": "tab-2", "title": "AWS", "data": {"tab": {"savedTitle": "x", "savedURL": "https://console.aws.amazon.com/"}}},
                {"id": "pinned-b", "childrenIds": ["tab-3"], "data": {"itemContainer": {}}},
                {"id": "tab-3", "data": {"tab": {"savedTitle": "Nubank", "savedURL": "https://app.nubank.com.br/"}}}
            ]}]}}"#;
        let bookmarks = parse_arc_pinned_tabs(text, "Profile 1").expect("valid sidebar");
        assert_eq!(
            bookmarks,
            vec![
                Bookmark {
                    title: "GitHub".into(),
                    url: "https://github.com/".into(),
                    folder: Some("Trabalho".into()),
                },
                Bookmark {
                    title: "AWS".into(),
                    url: "https://console.aws.amazon.com/".into(),
                    folder: Some("Infra".into()),
                },
            ]
        );
        let default = parse_arc_pinned_tabs(text, "Default").expect("valid sidebar");
        assert_eq!(default.len(), 1);
    }

    #[test]
    fn firefox_profiles_resolve_relative_paths() {
        let text = "[Install4F96D1932A9F858E]\nDefault=Profiles/abc.default-release\n\n\
                    [Profile1]\nName=default\nIsRelative=1\nPath=Profiles/xyz.default\n\n\
                    [Profile0]\nName=default-release\nIsRelative=1\nPath=Profiles/abc.default-release\n\n\
                    [General]\nStartWithLastProfile=1\n";
        let base = Path::new("/support/Firefox");
        let profiles = parse_firefox_profiles(text, base);
        assert_eq!(
            profiles,
            vec![
                SourceProfile {
                    name: "default".into(),
                    directory: base.join("Profiles/xyz.default"),
                },
                SourceProfile {
                    name: "default-release".into(),
                    directory: base.join("Profiles/abc.default-release"),
                },
            ]
        );
    }

    fn encrypt(plaintext: &[u8], key: &[u8; 16]) -> Vec<u8> {
        use aes::cipher::BlockEncryptMut;
        let mut buffer = vec![0u8; plaintext.len() + 16];
        let ciphertext = cbc::Encryptor::<aes::Aes128>::new(key.into(), &[b' '; 16].into())
            .encrypt_padded_b2b_mut::<Pkcs7>(plaintext, &mut buffer)
            .expect("buffer is big enough")
            .to_vec();
        [b"v10".as_slice(), &ciphertext].concat()
    }

    #[test]
    fn chromium_values_decrypt_and_check_their_host() {
        let key = chromium_cookie_key("peanuts");
        let old = encrypt(b"session-token", &key);
        assert_eq!(
            decrypt_chromium_value(&old, &key, ".github.com", 23).expect("decrypts"),
            "session-token"
        );

        let digest = Sha256::digest(b".github.com");
        let new = encrypt(&[digest.as_slice(), b"session-token"].concat(), &key);
        assert_eq!(
            decrypt_chromium_value(&new, &key, ".github.com", 24).expect("decrypts"),
            "session-token"
        );
        assert!(decrypt_chromium_value(&new, &key, ".evil.com", 24).is_err());
        assert!(
            decrypt_chromium_value(&new, &chromium_cookie_key("other"), ".github.com", 24).is_err()
        );
    }

    #[test]
    fn firefox_expiry_becomes_chromium_time() {
        let unix = 1_800_000_000;
        let expected = (unix + WINDOWS_TO_UNIX_EPOCH_SECONDS) * 1_000_000;
        assert_eq!(
            unix_to_chromium_time(firefox_expiry_seconds(unix)),
            expected
        );
        assert_eq!(
            unix_to_chromium_time(firefox_expiry_seconds(unix * 1000)),
            expected
        );
    }

    #[test]
    fn cookies_map_to_the_address_their_store_accepts() {
        let cookie = ImportedCookie {
            name: "sid".into(),
            value: "1".into(),
            domain: ".vercel.com".into(),
            path: "/".into(),
            secure: true,
            http_only: true,
            expires: None,
            same_site: SameSite::Lax,
        };
        assert_eq!(cookie.url(), "https://vercel.com/");
        assert!(!cookie.is_host_only());
    }
}
