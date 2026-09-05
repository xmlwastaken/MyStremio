//! Discord Rich Presence for MyStremio.
//!
//! This is a port of the Rich Presence implementation from
//! `Loukious/stremio-shell-ng` onto the MyStremio shell.
//!
//! Compared to the previous MyStremio implementation this adds:
//!   * real poster artwork pulled from Cinemeta / Anime-Kitsu (via images.weserv.nl)
//!   * episode names, series `SxxExx` state text and release years
//!   * IMDb / Kitsu and "Open in Stremio" buttons
//!   * a small play/pause badge overlay
//!   * accurate elapsed + remaining timestamps taken natively from mpv
//!   * a `/detail/` (browsing a title) presence, not just player + menu
//!   * watch-party / lobby `Party` + "Join Watch Party" support
//!   * an `RPCconfig.ini` next to the executable for power-user tuning
//!
//! Architecture note: upstream reads player state from its own native mpv
//! statics and the webview URL. MyStremio keeps the same idea -- playback
//! values are tapped straight out of the mpv event loop
//! (see `stremio_player/player.rs`) while the current route is pushed in from
//! the webui bridge (`assets/custom_discord_presence.js`). The presence itself
//! is driven by a single background thread, exactly like upstream, instead of
//! being rebuilt from scraped DOM strings on every poll.

use discord_rich_presence::{
    activity::{Activity, ActivityType, Assets, Button, Party, Timestamps},
    DiscordIpc, DiscordIpcClient,
};
use ini::Ini;
use libmpv2::events::PropertyData;
use serde_json::Value;
use std::collections::HashMap;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Discord application the presence is published under.
/// Overridable per-payload (`appId`) and via `RPCconfig.ini`.
const DEFAULT_APP_ID: &str = "997798118185771059";

pub const ICON_URL: &str =
    "https://raw.githubusercontent.com/Stremio/stremio-web/refs/heads/development/assets/images/icon.png";
const PAUSED_ICON_URL: &str = "https://i.imgur.com/eCUJpm9.png";

const CINEMETA_ENDPOINT: &str = "https://v3-cinemeta.strem.io/meta";
const KITSU_ENDPOINT: &str = "https://anime-kitsu.strem.fun/meta";

// ── Shared shell state ─────────────────────────────────────────────────────

/// Current webui route, e.g. `#/player/<encoded>` or `#/detail/series/tt0903747`.
static CURRENT_URL: Mutex<String> = Mutex::new(String::new());
/// mpv `time-pos`, seconds.
static CURRENT_TIME: Mutex<f64> = Mutex::new(0.0);
/// mpv `duration`, seconds.
static TOTAL_DURATION: Mutex<f64> = Mutex::new(0.0);
/// mpv `pause`.
static IS_PAUSED: Mutex<bool> = Mutex::new(false);

/// Master switch, owned by the webui setting (`stremio-custom-discord-rp-enabled`).
static ENABLED: AtomicBool = AtomicBool::new(false);
/// Webui toggle `stremio-custom-discord-rp-show-paused`.
static UI_SHOW_PAUSED: AtomicBool = AtomicBool::new(true);
/// Webui toggle `stremio-custom-discord-rp-show-menu`.
static UI_SHOW_MENU: AtomicBool = AtomicBool::new(true);

/// Application id requested by the webui payload (empty = use config/default).
static REQUESTED_APP_ID: Mutex<String> = Mutex::new(String::new());

// ── Watch party / lobby state ──────────────────────────────────────────────

/// Unique party id for the current lobby (empty = no active lobby).
pub static LOBBY_PARTY_ID: Mutex<String> = Mutex::new(String::new());
/// Opaque join secret advertised through Discord.
pub static LOBBY_JOIN_SECRET: Mutex<String> = Mutex::new(String::new());
/// Members currently in the lobby, including the host.
pub static LOBBY_MEMBER_COUNT: Mutex<i32> = Mutex::new(0);
/// Maximum lobby size.
pub static LOBBY_MAX_SIZE: Mutex<i32> = Mutex::new(8);

struct LobbyPresence {
    party_id: String,
    join_secret: String,
    member_count: i32,
    max_size: i32,
}

impl LobbyPresence {
    fn others_text(&self) -> Option<String> {
        let others = self.member_count.saturating_sub(1);
        if others <= 0 {
            None
        } else if others == 1 {
            Some("Watching with 1 other".to_string())
        } else {
            Some(format!("Watching with {} others", others))
        }
    }
}

fn lobby_presence(config: &Config) -> Option<LobbyPresence> {
    let party_id = LOBBY_PARTY_ID.lock().map(|p| p.clone()).unwrap_or_default();
    if party_id.is_empty() {
        return None;
    }

    let join_secret = LOBBY_JOIN_SECRET
        .lock()
        .map(|s| s.clone())
        .unwrap_or_default();
    let member_count = LOBBY_MEMBER_COUNT.lock().map(|c| *c).unwrap_or(1).max(1);
    let max_size = LOBBY_MAX_SIZE
        .lock()
        .map(|m| *m)
        .unwrap_or(config.lobby_max_size)
        .max(member_count);

    Some(LobbyPresence {
        party_id,
        join_secret,
        member_count,
        max_size,
    })
}

/// Publish the active watch-party lobby so it shows up as a joinable Discord party.
pub fn set_lobby(party_id: &str, join_secret: &str, member_count: i32, max_size: i32) {
    if let Ok(mut guard) = LOBBY_PARTY_ID.lock() {
        *guard = party_id.to_string();
    }
    if let Ok(mut guard) = LOBBY_JOIN_SECRET.lock() {
        *guard = join_secret.to_string();
    }
    if let Ok(mut guard) = LOBBY_MEMBER_COUNT.lock() {
        *guard = member_count;
    }
    if let Ok(mut guard) = LOBBY_MAX_SIZE.lock() {
        *guard = max_size.max(1);
    }
    signal(Signal::Refresh);
}

/// Tear down the advertised watch party.
pub fn clear_lobby() {
    set_lobby("", "", 0, 8);
}

// ── Config ─────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct Config {
    app_id: String,
    show_buttons: bool,
    link_target: String,
    disable_in_menu: bool,
    disable_when_paused: bool,
    refresh_interval: u64,
    show_small_image: bool,
    swap_name_and_title: bool,
    lobby_max_size: i32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            app_id: DEFAULT_APP_ID.to_string(),
            show_buttons: true,
            link_target: "app".to_string(),
            disable_in_menu: false,
            disable_when_paused: false,
            refresh_interval: 5,
            show_small_image: true,
            swap_name_and_title: false,
            lobby_max_size: 8,
        }
    }
}

fn config_path() -> Option<std::path::PathBuf> {
    let exe_path = std::env::current_exe().ok()?;
    Some(exe_path.parent()?.join("RPCconfig.ini"))
}

fn write_default_config(path: &std::path::Path) {
    let defaults = Config::default();
    let mut ini = Ini::new();
    ini.with_section(Some("Discord"))
        .set("app_id", defaults.app_id.as_str());
    ini.with_section(Some("Buttons"))
        .set("show_buttons", "true")
        // "app" -> stremio:///detail/..., "web" -> https://web.stremio.com/...
        .set("link_target", "app");
    ini.with_section(Some("Activity"))
        .set("disable_in_menu", "false")
        .set("disable_when_paused", "false")
        .set("refresh_interval", "5")
        .set("show_small_image", "true")
        .set("swap_name_and_title", "false");
    ini.with_section(Some("Lobby")).set("lobby_max_size", "8");

    if let Err(error) = ini.write_to_file(path) {
        eprintln!("[DiscordRPC] could not write default config: {error}");
    }
}

fn bool_at(ini: &Ini, section: &str, key: &str, fallback: bool) -> bool {
    ini.section(Some(section))
        .and_then(|sec| sec.get(key))
        .map(|value| value.eq_ignore_ascii_case("true"))
        .unwrap_or(fallback)
}

/// Load `RPCconfig.ini` from next to the executable, creating it on first run.
/// Never panics: a broken/unreadable file simply falls back to defaults.
fn load_or_create_config() -> Config {
    let defaults = Config::default();
    let Some(path) = config_path() else {
        return defaults;
    };

    if !path.exists() {
        write_default_config(&path);
    }

    let Ok(ini) = Ini::load_from_file(&path) else {
        eprintln!(
            "[DiscordRPC] could not parse {}, using defaults",
            path.display()
        );
        return defaults;
    };

    let app_id = ini
        .section(Some("Discord"))
        .and_then(|sec| sec.get("app_id"))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_APP_ID)
        .to_string();

    let link_target = ini
        .section(Some("Buttons"))
        .and_then(|sec| sec.get("link_target"))
        .map(|value| value.trim().to_lowercase())
        .unwrap_or_else(|| "app".to_string());

    let refresh_interval = ini
        .section(Some("Activity"))
        .and_then(|sec| sec.get("refresh_interval"))
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(5)
        .clamp(1, 60);

    let lobby_max_size = ini
        .section(Some("Lobby"))
        .and_then(|sec| sec.get("lobby_max_size"))
        .and_then(|value| value.trim().parse::<i32>().ok())
        .unwrap_or(8)
        .clamp(2, 32);

    Config {
        app_id,
        show_buttons: bool_at(&ini, "Buttons", "show_buttons", true),
        link_target,
        disable_in_menu: bool_at(&ini, "Activity", "disable_in_menu", false),
        disable_when_paused: bool_at(&ini, "Activity", "disable_when_paused", false),
        refresh_interval,
        show_small_image: bool_at(&ini, "Activity", "show_small_image", true),
        swap_name_and_title: bool_at(&ini, "Activity", "swap_name_and_title", false),
        lobby_max_size,
    }
}

/// Overlay the in-app MyStremio settings toggles on top of the ini file.
/// The webui switches win when they are more restrictive, so the Settings page
/// keeps working exactly as before.
fn effective_config(base: &Config) -> Config {
    let mut config = base.clone();

    let requested = REQUESTED_APP_ID
        .lock()
        .map(|id| id.clone())
        .unwrap_or_default();
    if !requested.is_empty() {
        config.app_id = requested;
    }
    if !UI_SHOW_PAUSED.load(Ordering::Relaxed) {
        config.disable_when_paused = true;
    }
    if !UI_SHOW_MENU.load(Ordering::Relaxed) {
        config.disable_in_menu = true;
    }
    config
}

// ── Metadata ───────────────────────────────────────────────────────────────

#[derive(Debug, Default, Clone)]
pub struct VideoInfo {
    pub poster: String,
    pub name: String,
    pub year: String,
    pub thumbnail: String,
    pub epname: String,
}

fn weserv_contain(url: &str) -> String {
    format!(
        "https://images.weserv.nl/?url={}&w=1024&h=1024&fit=contain",
        urlencoding::encode(url)
    )
}

fn metadata_cache() -> &'static Mutex<HashMap<String, VideoInfo>> {
    static CACHE: OnceLock<Mutex<HashMap<String, VideoInfo>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Fetch title metadata from Cinemeta (or Anime-Kitsu for `kitsu:` ids).
/// Results are memoised so the refresh loop does not re-hit the network.
pub fn getvidinfo(type_: &str, id: &str, season: &str, episode: &str) -> Option<VideoInfo> {
    if type_.is_empty() || id.is_empty() || type_ == "unknown" {
        return None;
    }

    let cache_key = format!("{}|{}|{}|{}", type_, id, season, episode);
    if let Ok(cache) = metadata_cache().lock() {
        if let Some(hit) = cache.get(&cache_key) {
            return Some(hit.clone());
        }
    }

    let base_url = if id.starts_with("kitsu") {
        KITSU_ENDPOINT
    } else {
        CINEMETA_ENDPOINT
    };
    let url = format!("{}/{}/{}.json", base_url, type_, id);

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .user_agent("MyStremio-Shell/DiscordRPC")
        .build()
        .ok()?;

    let response = match client.get(&url).send() {
        Ok(response) => response,
        Err(error) => {
            eprintln!("[DiscordRPC] metadata request failed: {error}");
            return None;
        }
    };

    if !response.status().is_success() {
        eprintln!(
            "[DiscordRPC] metadata endpoint returned {} for {}",
            response.status(),
            url
        );
        return None;
    }

    let response_text = response.text().unwrap_or_default();

    let json: Value = serde_json::from_str(&response_text).unwrap_or_default();
    if !json.is_object() {
        return None;
    }

    let mut video_info = VideoInfo {
        poster: String::new(),
        name: "Unknown Title".to_string(),
        year: String::new(),
        thumbnail: String::new(),
        epname: "Unknown Episode Name".to_string(),
    };

    if let Some(meta) = json.get("meta") {
        if let Some(poster) = meta.get("poster").and_then(|p| p.as_str()) {
            video_info.poster = poster.to_string();
        }
        if let Some(name) = meta.get("name").and_then(|n| n.as_str()) {
            video_info.name = name.to_string();
        }
        if let Some(year) = meta.get("year").and_then(|y| y.as_str()) {
            video_info.year = year.to_string();
        }

        if type_ == "series" {
            if let Some(videos) = meta.get("videos").and_then(|v| v.as_array()) {
                let expected_id = if season.is_empty() {
                    format!("{}:{}", id, episode)
                } else {
                    format!("{}:{}:{}", id, season, episode)
                };

                for video in videos {
                    let matches = video
                        .get("id")
                        .and_then(|v| v.as_str())
                        .map(|video_id| video_id == expected_id)
                        .unwrap_or(false);
                    if !matches {
                        continue;
                    }

                    if let Some(thumbnail) = video.get("thumbnail").and_then(|t| t.as_str()) {
                        video_info.thumbnail = thumbnail.to_string();
                    }
                    if let Some(epname) = video.get("name").and_then(|e| e.as_str()) {
                        video_info.epname = epname.to_string();
                    } else if let Some(title) = video.get("title").and_then(|t| t.as_str()) {
                        video_info.epname = title.to_string();
                    } else {
                        video_info.epname = video_info.name.clone();
                    }
                    break;
                }
            }
        }
    }

    // Do not memoise an empty/failed lookup -- retry it on the next route hit.
    let resolved = !video_info.poster.is_empty() || video_info.name != "Unknown Title";
    if resolved {
        if let Ok(mut cache) = metadata_cache().lock() {
            // Keep the cache from growing without bound over a long session.
            if cache.len() > 256 {
                cache.clear();
            }
            cache.insert(cache_key, video_info.clone());
        }
    }

    Some(video_info)
}

/// Split a Stremio video id into `(type, id, season, episode)`.
fn parse_video_id(video_id: &str) -> (String, String, String, String) {
    let parts: Vec<&str> = video_id.split(':').collect();
    let unknown = || {
        (
            "unknown".to_string(),
            String::new(),
            String::new(),
            String::new(),
        )
    };

    if parts.first() == Some(&"kitsu") {
        match parts.len() {
            2 => (
                "movie".to_string(),
                format!("{}:{}", parts[0], parts[1]),
                String::new(),
                String::new(),
            ),
            3 => (
                "series".to_string(),
                format!("{}:{}", parts[0], parts[1]),
                String::new(),
                parts[2].to_string(),
            ),
            _ => unknown(),
        }
    } else {
        match parts.len() {
            1 => (
                "movie".to_string(),
                parts[0].to_string(),
                String::new(),
                String::new(),
            ),
            3 => (
                "series".to_string(),
                parts[0].to_string(),
                parts[1].to_string(),
                parts[2].to_string(),
            ),
            _ => unknown(),
        }
    }
}

fn percent_decode(raw: &str) -> String {
    urlencoding::decode(raw)
        .map(|value| value.into_owned())
        .unwrap_or_else(|_| raw.to_string())
}

/// Extract `/series/<id>/...` or `/movie/<id>/...` from a route.
fn content_segment(cur_url: &str) -> String {
    if cur_url.contains("/series/") {
        cur_url
            .split_once("/series/")
            .map(|(_, part)| format!("/series/{}", part))
            .unwrap_or_default()
    } else if cur_url.contains("/movie/") {
        cur_url
            .split_once("/movie/")
            .map(|(_, part)| format!("/movie/{}", part))
            .unwrap_or_default()
    } else {
        String::new()
    }
}

fn content_id_from_segment(last_segment: &str) -> String {
    let trimmed = last_segment
        .trim_start_matches("/series/")
        .trim_start_matches("/movie/");
    percent_decode(trimmed.split('/').next().unwrap_or(""))
}

/// Build the (external link, stremio deep link, label) button triple.
fn button_targets(
    config: &Config,
    content_id: &str,
    last_segment: &str,
) -> (Option<String>, Option<String>, &'static str) {
    if !config.show_buttons || content_id.is_empty() {
        return (None, None, "");
    }

    let (label, url) = if let Some(id_part) = content_id.strip_prefix("kitsu:") {
        ("Kitsu", format!("https://kitsu.app/anime/{}", id_part))
    } else {
        ("IMDb", format!("https://www.imdb.com/title/{}", content_id))
    };

    let stremio = if config.link_target == "web" {
        format!("https://web.stremio.com/#/detail{}", last_segment)
    } else {
        format!("stremio:///detail{}", last_segment)
    };

    (Some(url), Some(stremio), label)
}

// ── Activity builders ──────────────────────────────────────────────────────

type ActivityResult = Result<(), Box<dyn std::error::Error>>;

#[allow(clippy::too_many_arguments)]
fn build_player_activity(
    drp: &mut DiscordIpcClient,
    config: &Config,
    info: &VideoInfo,
    media_type: &str,
    season: &str,
    episode: &str,
    current_time: f64,
    total_duration: f64,
    is_paused: bool,
    app_start_time: SystemTime,
    cur_url: &str,
) -> ActivityResult {
    let now_unix = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;

    let (start, end) = if is_paused {
        let start_time = if config.disable_when_paused {
            app_start_time.duration_since(UNIX_EPOCH)?.as_secs() as i64
        } else {
            now_unix - current_time as i64
        };
        (start_time, None)
    } else {
        let start = now_unix - current_time as i64;
        let end = if total_duration > 0.0 {
            Some(start + total_duration as i64)
        } else {
            None
        };
        (start, end)
    };

    let mut timestamps = Timestamps::new().start(start);
    if let Some(end) = end {
        timestamps = timestamps.end(end);
    }

    let lobby = lobby_presence(config);
    let lobby_text = lobby.as_ref().and_then(LobbyPresence::others_text);

    let (mut activity_name, mut details, mut state_text) = if media_type == "series" {
        (
            info.name.clone(),
            info.epname.clone(),
            format!("S{}E{}", season, episode),
        )
    } else {
        (info.name.clone(), info.name.clone(), info.year.clone())
    };
    if config.swap_name_and_title {
        std::mem::swap(&mut activity_name, &mut details);
    }
    if let Some(text) = &lobby_text {
        state_text = if state_text.is_empty() {
            text.clone()
        } else {
            format!("{state_text} • {text}")
        };
    }
    if state_text.is_empty() {
        state_text = if is_paused { "Paused" } else { "Watching" }.to_string();
    }

    let large_text = if info.year.is_empty() {
        info.name.clone()
    } else {
        format!("{} ({})", info.name, info.year)
    };
    let poster_url = if info.poster.is_empty() {
        ICON_URL.to_string()
    } else {
        weserv_contain(&info.poster)
    };

    let mut assets = Assets::new()
        .large_image(&poster_url)
        .large_text(&large_text);

    if config.show_small_image {
        let (small_image, small_text) = if is_paused {
            (PAUSED_ICON_URL, "Paused")
        } else {
            (ICON_URL, "Playing")
        };
        assets = assets.small_image(small_image).small_text(small_text);
    }

    let mut activity = Activity::new()
        .activity_type(ActivityType::Watching)
        .name(&activity_name)
        .details(&details)
        .state(&state_text)
        .timestamps(timestamps)
        .assets(assets);

    let last_segment = content_segment(cur_url);
    let content_id = content_id_from_segment(&last_segment);
    let (external_url, stremio_url, button_label) =
        button_targets(config, &content_id, &last_segment);

    // Discord allows at most 2 buttons.
    let mut buttons = Vec::new();
    if let Some(external) = external_url.as_deref() {
        buttons.push(Button::new(button_label, external));
    }
    if let Some(lobby) = &lobby {
        if !lobby.join_secret.is_empty() {
            buttons.push(Button::new("Join Watch Party", lobby.join_secret.as_str()));
        }
    } else if let Some(stremio) = stremio_url.as_deref() {
        buttons.push(Button::new("Open in Stremio", stremio));
    }
    if !buttons.is_empty() {
        activity = activity.buttons(buttons);
    }

    if let Some(lobby) = &lobby {
        activity = activity.party(
            Party::new()
                .id(&lobby.party_id)
                .size([lobby.member_count, lobby.max_size]),
        );
    }

    drp.set_activity(activity)?;
    Ok(())
}

fn build_detail_activity(
    drp: &mut DiscordIpcClient,
    config: &Config,
    info: &VideoInfo,
    media_type: &str,
    cur_url: &str,
    app_start_time: SystemTime,
) -> ActivityResult {
    let large_text = if info.year.is_empty() {
        info.name.clone()
    } else {
        format!("{} ({})", info.name, info.year)
    };
    let poster_url = if info.poster.is_empty() {
        ICON_URL.to_string()
    } else {
        weserv_contain(&info.poster)
    };

    let mut assets = Assets::new()
        .large_image(&poster_url)
        .large_text(&large_text);
    if config.show_small_image {
        assets = assets.small_image(ICON_URL).small_text("MyStremio");
    }

    let state_text = if media_type == "series" {
        "Viewing Series"
    } else {
        "Viewing Movie"
    };

    let start_time = app_start_time.duration_since(UNIX_EPOCH)?.as_secs() as i64;

    let mut activity = Activity::new()
        .activity_type(ActivityType::Watching)
        .name(&info.name)
        .details(&info.name)
        .state(state_text)
        .timestamps(Timestamps::new().start(start_time))
        .assets(assets);

    let last_segment = content_segment(cur_url);
    let content_id = content_id_from_segment(&last_segment);
    let (external_url, stremio_url, button_label) =
        button_targets(config, &content_id, &last_segment);

    let mut buttons = Vec::new();
    if let Some(external) = external_url.as_deref() {
        buttons.push(Button::new(button_label, external));
    }
    if let Some(stremio) = stremio_url.as_deref() {
        buttons.push(Button::new("Open in Stremio", stremio));
    }
    if !buttons.is_empty() {
        activity = activity.buttons(buttons);
    }

    drp.set_activity(activity)?;
    Ok(())
}

fn build_menu_activity(
    drp: &mut DiscordIpcClient,
    cur_url: &str,
    app_start_time: SystemTime,
) -> ActivityResult {
    let start_time = app_start_time.duration_since(UNIX_EPOCH)?.as_secs() as i64;

    let base_url = cur_url.split('?').next().unwrap_or(cur_url);
    let base_url = base_url.trim_end_matches('/');
    let (state, details) = if base_url.ends_with("/settings") {
        ("Settings", "Changing configuration")
    } else if base_url.ends_with("/addons") {
        ("Addons", "Managing addons")
    } else if base_url.ends_with("/library") {
        ("Library", "Browsing library")
    } else if base_url.ends_with("/calendar") {
        ("Calendar", "Viewing Calendar")
    } else if base_url.ends_with("/discover") {
        ("Discover", "Browsing Catalog")
    } else if base_url.ends_with("/search") {
        ("Search", "Searching")
    } else {
        ("Browsing", "In MyStremio Menu")
    };

    let activity = Activity::new()
        .activity_type(ActivityType::Watching)
        .name("MyStremio")
        .details(details)
        .state(state)
        .timestamps(Timestamps::new().start(start_time))
        .assets(Assets::new().large_image(ICON_URL).large_text("MyStremio"));

    drp.set_activity(activity)?;
    Ok(())
}

// ── Background presence loop ───────────────────────────────────────────────

enum Signal {
    Enable,
    Disable,
    Refresh,
}

static SIGNAL_TX: OnceLock<flume::Sender<Signal>> = OnceLock::new();

fn signal(sig: Signal) {
    ensure_started();
    if let Some(tx) = SIGNAL_TX.get() {
        let _ = tx.send(sig);
    }
}

/// Spawn the presence thread once, lazily.
fn ensure_started() {
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        let (tx, rx) = flume::unbounded::<Signal>();
        let _ = SIGNAL_TX.set(tx);
        spawn_discordrpc_loop(SystemTime::now(), rx);
    });
}

/// Call once during shell start-up (optional -- the module also self-starts).
pub fn init() {
    ensure_started();
}

fn spawn_discordrpc_loop(
    app_start_time: SystemTime,
    control_rx: flume::Receiver<Signal>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut retry_count: u32 = 0;

        loop {
            // Stay fully disconnected until the webui asks us to connect.
            loop {
                match control_rx.recv() {
                    Ok(Signal::Enable) => break,
                    Ok(_) => {
                        if ENABLED.load(Ordering::Relaxed) {
                            break;
                        }
                    }
                    Err(_) => return,
                }
            }

            let result = catch_unwind(AssertUnwindSafe(|| {
                let base_config = load_or_create_config();
                let mut config = effective_config(&base_config);
                let mut drp = DiscordIpcClient::new(&config.app_id);

                'connection: loop {
                    match drp.connect() {
                        Ok(_) => {
                            retry_count = 0;
                            println!("[DiscordRPC] connected to Discord IPC");
                        }
                        Err(error) => {
                            eprintln!("[DiscordRPC] connection failed: {error}");
                            match control_rx.recv_timeout(Duration::from_secs(5)) {
                                Ok(Signal::Disable)
                                | Err(flume::RecvTimeoutError::Disconnected) => break 'connection,
                                _ => {}
                            }
                            if !ENABLED.load(Ordering::Relaxed) {
                                break 'connection;
                            }
                            continue;
                        }
                    }

                    let mut last_url = String::new();
                    let mut video_info: Option<VideoInfo> = None;
                    let mut type_ = String::new();
                    let mut season = String::new();
                    let mut episode = String::new();

                    loop {
                        let sleep_time = Duration::from_secs(config.refresh_interval);
                        match control_rx.recv_timeout(sleep_time) {
                            Ok(Signal::Disable) | Err(flume::RecvTimeoutError::Disconnected) => {
                                let _ = drp.clear_activity();
                                break 'connection;
                            }
                            _ => {}
                        }

                        if !ENABLED.load(Ordering::Relaxed) {
                            let _ = drp.clear_activity();
                            break 'connection;
                        }

                        // Pick up ini edits and Settings-page toggles live.
                        config = effective_config(&base_config);

                        let (cur_url, cur_time, is_paused, total_duration) = match (
                            CURRENT_URL.lock(),
                            CURRENT_TIME.lock(),
                            IS_PAUSED.lock(),
                            TOTAL_DURATION.lock(),
                        ) {
                            (Ok(url), Ok(time), Ok(paused), Ok(duration)) => {
                                (url.clone(), *time, *paused, *duration)
                            }
                            _ => {
                                eprintln!("[DiscordRPC] failed to lock state");
                                continue;
                            }
                        };

                        let is_player = cur_url.contains("/player/");
                        let is_detail = cur_url.contains("/detail/");

                        let activity_result = if is_player || is_detail {
                            if cur_url != last_url {
                                type_ = String::new();
                                season = String::new();
                                episode = String::new();
                                video_info = None;

                                if is_player {
                                    // /player/<stream>/<...>/<videoId>
                                    let raw_id = cur_url
                                        .split('/')
                                        .next_back()
                                        .unwrap_or("")
                                        .split('?')
                                        .next()
                                        .unwrap_or("");
                                    let video_id = percent_decode(raw_id);
                                    let (parsed_type, parsed_id, parsed_season, parsed_episode) =
                                        parse_video_id(&video_id);
                                    type_ = parsed_type;
                                    season = parsed_season;
                                    episode = parsed_episode;
                                    video_info = getvidinfo(&type_, &parsed_id, &season, &episode);
                                } else if let Some(detail_part) = cur_url.split("/detail/").nth(1) {
                                    let parts: Vec<&str> = detail_part
                                        .split('?')
                                        .next()
                                        .unwrap_or("")
                                        .split('/')
                                        .collect();
                                    if parts.len() >= 2 {
                                        type_ = parts[0].to_string();
                                        let id = percent_decode(parts[1]);
                                        video_info = getvidinfo(&type_, &id, "", "");
                                    }
                                }
                                last_url = cur_url.clone();
                            }

                            match &video_info {
                                Some(info) => {
                                    if is_player {
                                        if config.disable_when_paused && is_paused {
                                            drp.clear_activity().map_err(|e| {
                                                Box::new(e) as Box<dyn std::error::Error>
                                            })
                                        } else {
                                            build_player_activity(
                                                &mut drp,
                                                &config,
                                                info,
                                                &type_,
                                                &season,
                                                &episode,
                                                cur_time,
                                                total_duration,
                                                is_paused,
                                                app_start_time,
                                                &cur_url,
                                            )
                                        }
                                    } else {
                                        build_detail_activity(
                                            &mut drp,
                                            &config,
                                            info,
                                            &type_,
                                            &cur_url,
                                            app_start_time,
                                        )
                                    }
                                }
                                None => {
                                    // Metadata not resolvable (custom addon id, offline, ...).
                                    // Fall back to a generic presence instead of going dark.
                                    if config.disable_in_menu {
                                        drp.clear_activity()
                                            .map_err(|e| Box::new(e) as Box<dyn std::error::Error>)
                                    } else {
                                        build_menu_activity(&mut drp, &cur_url, app_start_time)
                                    }
                                }
                            }
                        } else if config.disable_in_menu {
                            drp.clear_activity()
                                .map_err(|e| Box::new(e) as Box<dyn std::error::Error>)
                        } else {
                            build_menu_activity(&mut drp, &cur_url, app_start_time)
                        };

                        if let Err(error) = activity_result {
                            eprintln!("[DiscordRPC] activity update failed: {error}");
                            let _ = drp.close();
                            break;
                        }
                    }

                    let _ = drp.close();
                }

                let _ = drp.close();
            }));

            if let Err(error) = result {
                eprintln!("[DiscordRPC] critical error: {:?}", error);
                let delay_secs = 5 * 2u64.pow(retry_count.min(5));
                retry_count = retry_count.saturating_add(1);
                thread::sleep(Duration::from_secs(delay_secs));
            }
        }
    })
}

// ── State feeds ────────────────────────────────────────────────────────────

/// Tap for the mpv event loop -- keeps playback timestamps exact instead of
/// scraping the seek bar. Called from `stremio_player::player`.
pub fn note_mpv_property(name: &str, change: &PropertyData) {
    match (name, change) {
        ("time-pos", PropertyData::Double(value)) => {
            if let Ok(mut guard) = CURRENT_TIME.lock() {
                *guard = *value;
            }
        }
        ("duration", PropertyData::Double(value)) => {
            if let Ok(mut guard) = TOTAL_DURATION.lock() {
                *guard = *value;
            }
        }
        ("pause", PropertyData::Flag(value)) => {
            let changed = IS_PAUSED
                .lock()
                .map(|guard| *guard != *value)
                .unwrap_or(false);
            if let Ok(mut guard) = IS_PAUSED.lock() {
                *guard = *value;
            }
            if changed && ENABLED.load(Ordering::Relaxed) {
                signal(Signal::Refresh);
            }
        }
        _ => {}
    }
}

/// Reset playback state when a file ends / the player closes.
pub fn note_playback_stopped() {
    if let Ok(mut guard) = CURRENT_TIME.lock() {
        *guard = 0.0;
    }
    if let Ok(mut guard) = TOTAL_DURATION.lock() {
        *guard = 0.0;
    }
    if let Ok(mut guard) = IS_PAUSED.lock() {
        *guard = false;
    }
    signal(Signal::Refresh);
}

fn set_enabled(enabled: bool) {
    let was = ENABLED.swap(enabled, Ordering::Relaxed);
    if was == enabled {
        return;
    }
    signal(if enabled {
        Signal::Enable
    } else {
        Signal::Disable
    });
}

/// Entry point used by the webui bridge (`update-discord-presence`).
///
/// The payload only carries routing/config now -- titles, artwork and
/// timestamps are resolved natively.
pub fn update_presence(payload: &Value) -> Result<(), String> {
    ensure_started();

    if let Some(app_id) = payload.get("appId").and_then(Value::as_str) {
        if let Ok(mut guard) = REQUESTED_APP_ID.lock() {
            *guard = app_id.trim().to_string();
        }
    }

    if let Some(show_paused) = payload.get("showPaused").and_then(Value::as_bool) {
        UI_SHOW_PAUSED.store(show_paused, Ordering::Relaxed);
    }
    if let Some(show_menu) = payload.get("showMenu").and_then(Value::as_bool) {
        UI_SHOW_MENU.store(show_menu, Ordering::Relaxed);
    }

    // Route is the authoritative signal for what to display.
    if let Some(route) = payload.get("route").and_then(Value::as_str) {
        if let Ok(mut guard) = CURRENT_URL.lock() {
            if *guard != route {
                *guard = route.to_string();
            }
        }
    }

    // The webui can still supply playback state; native mpv values win when
    // they are available, so only use these as a fallback.
    if let Some(paused) = payload.get("paused").and_then(Value::as_bool) {
        if let Ok(mut guard) = IS_PAUSED.lock() {
            *guard = paused;
        }
    }
    if let Some(current_time) = payload.get("currentTimeSeconds").and_then(Value::as_f64) {
        if let Ok(mut guard) = CURRENT_TIME.lock() {
            if *guard <= 0.0 {
                *guard = current_time;
            }
        }
    }
    if let Some(duration) = payload.get("durationSeconds").and_then(Value::as_f64) {
        if let Ok(mut guard) = TOTAL_DURATION.lock() {
            if *guard <= 0.0 {
                *guard = duration;
            }
        }
    }

    set_enabled(true);
    signal(Signal::Refresh);
    Ok(())
}

/// Entry point used by the webui bridge (`clear-discord-presence`).
pub fn clear_presence() -> Result<(), String> {
    set_enabled(false);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_movie_and_series_ids() {
        assert_eq!(
            parse_video_id("tt0111161"),
            ("movie".into(), "tt0111161".into(), "".into(), "".into())
        );
        assert_eq!(
            parse_video_id("tt0903747:1:3"),
            ("series".into(), "tt0903747".into(), "1".into(), "3".into())
        );
        assert_eq!(
            parse_video_id("kitsu:44081"),
            ("movie".into(), "kitsu:44081".into(), "".into(), "".into())
        );
        assert_eq!(
            parse_video_id("kitsu:44081:5"),
            ("series".into(), "kitsu:44081".into(), "".into(), "5".into())
        );
    }

    #[test]
    fn extracts_detail_route_content() {
        let seg = content_segment("/detail/series/tt0903747/tt0903747%3A1%3A3");
        assert_eq!(seg, "/series/tt0903747/tt0903747%3A1%3A3");
        assert_eq!(content_id_from_segment(&seg), "tt0903747");
    }

    #[test]
    fn builds_imdb_and_deeplink_buttons() {
        let mut cfg = Config::default();
        let seg = "/series/tt0903747/tt0903747:1:3";
        let (ext, deep, label) = button_targets(&cfg, "tt0903747", seg);
        assert_eq!(label, "IMDb");
        assert_eq!(ext.unwrap(), "https://www.imdb.com/title/tt0903747");
        assert_eq!(
            deep.unwrap(),
            "stremio:///detail/series/tt0903747/tt0903747:1:3"
        );

        cfg.link_target = "web".into();
        let (_, deep_web, _) = button_targets(&cfg, "tt0903747", seg);
        assert_eq!(
            deep_web.unwrap(),
            "https://web.stremio.com/#/detail/series/tt0903747/tt0903747:1:3"
        );
    }

    #[test]
    fn builds_kitsu_buttons() {
        let cfg = Config::default();
        let (ext, _, label) = button_targets(&cfg, "kitsu:44081", "/series/kitsu:44081");
        assert_eq!(label, "Kitsu");
        assert_eq!(ext.unwrap(), "https://kitsu.app/anime/44081");
    }

    #[test]
    fn buttons_disabled_by_config() {
        let cfg = Config {
            show_buttons: false,
            ..Config::default()
        };
        let (ext, deep, _) = button_targets(&cfg, "tt0903747", "/series/tt0903747");
        assert!(ext.is_none() && deep.is_none());
    }

    #[test]
    fn poster_is_proxied_and_encoded() {
        let out = weserv_contain("https://images.metahub.space/poster/x/tt1/img.jpg");
        assert!(out.starts_with("https://images.weserv.nl/?url=https%3A%2F%2Fimages.metahub.space"));
        assert!(out.ends_with("&w=1024&h=1024&fit=contain"));
    }

    #[test]
    fn player_video_id_is_last_path_segment() {
        let url = "/player/eyJ4Ijoie/eyJ5Ijoie/tt0903747/series/tt0903747%3A1%3A3";
        let raw = url.split('/').next_back().unwrap();
        let (t, id, s, e) = parse_video_id(&percent_decode(raw));
        assert_eq!(
            (t.as_str(), id.as_str(), s.as_str(), e.as_str()),
            ("series", "tt0903747", "1", "3")
        );
    }

    #[test]
    fn lobby_others_text() {
        let mk = |n| LobbyPresence {
            party_id: "p".into(),
            join_secret: "s".into(),
            member_count: n,
            max_size: 8,
        };
        assert_eq!(mk(1).others_text(), None);
        assert_eq!(mk(2).others_text().unwrap(), "Watching with 1 other");
        assert_eq!(mk(4).others_text().unwrap(), "Watching with 3 others");
    }
}
