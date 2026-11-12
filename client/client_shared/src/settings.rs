use crate::{TranscriptTarget, Workspace};
use psa_api::ProbeId;
use serde::{Deserialize, Serialize};

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ConnectionSettings {
    pub api_url: String,
    pub client_key: String,
}
impl Default for ConnectionSettings {
    fn default() -> Self {
        Self {
            api_url: "http://127.0.0.1:8080".into(),
            client_key: String::new(),
        }
    }
}
impl std::fmt::Debug for ConnectionSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectionSettings")
            .field("api_url", &self.api_url)
            .field("client_key", &"[REDACTED]")
            .finish()
    }
}
impl ConnectionSettings {
    pub fn client(&self) -> Result<psa_api::Client, psa_api::ClientError> {
        psa_api::Client::new(&self.api_url, &self.client_key)
    }
    /// API endpoint is the persistence identity; client keys never enter identity or logs.
    pub fn server_identity(&self) -> String {
        self.api_url.trim().trim_end_matches('/').to_owned()
    }
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SidebarMode {
    #[default]
    Browser,
    Details,
    Probes,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SidebarState {
    pub collapsed: bool,
    pub expanded_mode: SidebarMode,
    /// Width in CSS pixels, shared by the browser and details modes.
    pub width: u16,
}
impl Default for SidebarState {
    fn default() -> Self {
        Self {
            collapsed: false,
            expanded_mode: SidebarMode::Browser,
            width: Self::DEFAULT_WIDTH,
        }
    }
}
impl SidebarState {
    pub const DEFAULT_WIDTH: u16 = 286;
    pub const MIN_WIDTH: u16 = 180;
    pub const MAX_WIDTH: u16 = 720;

    /// Clamps the saved width too, so an out-of-range value from an older or
    /// hand-edited settings file still renders usably.
    pub fn width(&self) -> u16 {
        self.width.clamp(Self::MIN_WIDTH, Self::MAX_WIDTH)
    }

    pub fn set_width(&mut self, width: f64) {
        if width.is_finite() {
            self.width = (width.round() as u16).clamp(Self::MIN_WIDTH, Self::MAX_WIDTH);
        }
    }

    pub fn toggle_mode(&mut self, mode: SidebarMode) {
        if !self.collapsed && self.expanded_mode == mode {
            self.collapsed = true;
        } else {
            self.expanded_mode = mode;
            self.collapsed = false;
        }
    }
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReasoningDisplay {
    Hidden,
    #[default]
    Collapsed,
    Shown,
}

/// How the start of each model turn is marked in a transcript: the boundary
/// between what the turn was sent and what it produced.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum TurnDividerDisplay {
    Hidden,
    /// A plain hairline.
    Line,
    /// A hairline carrying the turn's number, usage and context link.
    #[default]
    Labeled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiPreferences {
    pub show_hidden_agents: bool,
    pub transcript_font_size: u16,
    pub conversation_max_width_ch: u16,
    pub auto_follow: bool,
    pub double_click_session_opens_details: bool,
    pub hide_text_recorded_results: bool,
    pub hide_agent_statuses: bool,
    pub turn_dividers: TurnDividerDisplay,
    pub reasoning_display: ReasoningDisplay,
}
impl Default for UiPreferences {
    fn default() -> Self {
        Self {
            show_hidden_agents: false,
            transcript_font_size: 14,
            conversation_max_width_ch: 85,
            auto_follow: true,
            double_click_session_opens_details: true,
            hide_text_recorded_results: true,
            hide_agent_statuses: true,
            turn_dividers: TurnDividerDisplay::default(),
            reasoning_display: ReasoningDisplay::default(),
        }
    }
}
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerWorkspace {
    pub server_url: String,
    pub workspace: Workspace,
    pub sidebar: SidebarState,
    pub selected_probe: Option<ProbeId>,
    pub last_viewed_agents: Vec<TranscriptTarget>,
}
impl ServerWorkspace {
    pub fn record_agent(&mut self, target: TranscriptTarget) {
        self.last_viewed_agents
            .retain(|t| t.session_id != target.session_id);
        self.last_viewed_agents.push(target);
    }
}
pub const LOCAL_PROFILE_ID: &str = "local";

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct ConnectionProfile {
    pub id: String,
    pub name: String,
    pub connection: ConnectionSettings,
    pub workspace: ServerWorkspace,
}
impl Serialize for ConnectionProfile {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;

        let mut profile = serializer.serialize_struct("ConnectionProfile", 4)?;
        profile.serialize_field("id", &self.id)?;
        profile.serialize_field("name", &self.name)?;
        if self.id == LOCAL_PROFILE_ID {
            let connection = ConnectionSettings {
                api_url: self.connection.api_url.clone(),
                client_key: String::new(),
            };
            profile.serialize_field("connection", &connection)?;
        } else {
            profile.serialize_field("connection", &self.connection)?;
        }
        profile.serialize_field("workspace", &self.workspace)?;
        profile.end()
    }
}
impl Default for ConnectionProfile {
    fn default() -> Self {
        Self {
            id: LOCAL_PROFILE_ID.into(),
            name: "Local".into(),
            connection: ConnectionSettings::default(),
            workspace: ServerWorkspace::default(),
        }
    }
}
impl ConnectionProfile {
    pub fn new_remote(name: impl Into<String>, connection: ConnectionSettings) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.into(),
            connection,
            workspace: ServerWorkspace::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DesktopSettings {
    pub profiles: Vec<ConnectionProfile>,
    pub active_profile_id: String,
    pub preferences: UiPreferences,
}
impl Default for DesktopSettings {
    fn default() -> Self {
        Self {
            profiles: vec![ConnectionProfile::default()],
            active_profile_id: LOCAL_PROFILE_ID.into(),
            preferences: UiPreferences::default(),
        }
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct SettingsWire {
    profiles: Option<Vec<ConnectionProfile>>,
    active_profile_id: Option<String>,
    preferences: UiPreferences,
    connection: ConnectionSettings,
    servers: Vec<ServerWorkspace>,
}
impl<'de> Deserialize<'de> for DesktopSettings {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = SettingsWire::deserialize(deserializer)?;
        let mut settings = Self::default();
        settings.preferences = wire.preferences;
        if let Some(profiles) = wire.profiles {
            settings.profiles = profiles;
            if !settings.profiles.iter().any(|p| p.id == LOCAL_PROFILE_ID) {
                settings.profiles.insert(0, ConnectionProfile::default());
            }
            settings.active_profile_id = wire.active_profile_id.unwrap_or_default();
            if !settings
                .profiles
                .iter()
                .any(|p| p.id == settings.active_profile_id)
            {
                settings.active_profile_id = LOCAL_PROFILE_ID.into();
            }
            return Ok(settings);
        }

        let active_url = wire.connection.server_identity();
        let local_url = ConnectionSettings::default().server_identity();
        if active_url == local_url {
            settings.profiles[0].connection = wire.connection.clone();
        } else {
            let profile = ConnectionProfile::new_remote(active_url.clone(), wire.connection);
            settings.active_profile_id = profile.id.clone();
            settings.profiles.push(profile);
        }
        for workspace in wire.servers {
            let url = workspace.server_url.trim().trim_end_matches('/');
            if url == local_url {
                settings.profiles[0].workspace = workspace;
            } else if url == active_url {
                settings.active_profile_mut().workspace = workspace;
            } else {
                let connection = ConnectionSettings {
                    api_url: workspace.server_url.clone(),
                    ..Default::default()
                };
                let mut profile =
                    ConnectionProfile::new_remote(workspace.server_url.clone(), connection);
                profile.workspace = workspace;
                settings.profiles.push(profile);
            }
        }
        Ok(settings)
    }
}
impl DesktopSettings {
    pub fn active_profile(&self) -> &ConnectionProfile {
        self.profiles
            .iter()
            .find(|p| p.id == self.active_profile_id)
            .expect("active profile must exist")
    }
    pub fn active_profile_mut(&mut self) -> &mut ConnectionProfile {
        self.profiles
            .iter_mut()
            .find(|p| p.id == self.active_profile_id)
            .expect("active profile must exist")
    }
    pub fn remove_profile(&mut self, id: &str) -> bool {
        if id == LOCAL_PROFILE_ID {
            return false;
        }
        let before = self.profiles.len();
        self.profiles.retain(|p| p.id != id);
        if self.active_profile_id == id && self.profiles.len() != before {
            self.active_profile_id = LOCAL_PROFILE_ID.into();
        }
        self.profiles.len() != before
    }
    pub fn current_server(&self) -> Option<&ServerWorkspace> {
        Some(&self.active_profile().workspace)
    }
    pub fn ensure_current_server(&mut self) -> &mut ServerWorkspace {
        &mut self.active_profile_mut().workspace
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sidebar_width_defaults_clamps_and_persists() {
        let mut sidebar: SidebarState = serde_json::from_str(r#"{"collapsed":false}"#).unwrap();
        assert_eq!(sidebar.width(), SidebarState::DEFAULT_WIDTH);
        sidebar.set_width(10_000.0);
        assert_eq!(sidebar.width(), SidebarState::MAX_WIDTH);
        sidebar.set_width(f64::NAN);
        assert_eq!(sidebar.width(), SidebarState::MAX_WIDTH);
        sidebar.set_width(321.4);
        let restored: SidebarState =
            serde_json::from_str(&serde_json::to_string(&sidebar).unwrap()).unwrap();
        assert_eq!(restored.width(), 321);
        sidebar.width = 5;
        assert_eq!(sidebar.width(), SidebarState::MIN_WIDTH);
    }
    #[test]
    fn sidebar_mode_buttons_toggle_and_switch() {
        let mut sidebar = SidebarState::default();
        sidebar.toggle_mode(SidebarMode::Browser);
        assert!(sidebar.collapsed);
        sidebar.toggle_mode(SidebarMode::Details);
        assert!(!sidebar.collapsed);
        assert_eq!(sidebar.expanded_mode, SidebarMode::Details);
        sidebar.toggle_mode(SidebarMode::Browser);
        assert!(!sidebar.collapsed);
        assert_eq!(sidebar.expanded_mode, SidebarMode::Browser);
        sidebar.toggle_mode(SidebarMode::Browser);
        assert!(sidebar.collapsed);
        sidebar.toggle_mode(SidebarMode::Browser);
        assert!(!sidebar.collapsed);
        sidebar.toggle_mode(SidebarMode::Probes);
        assert!(!sidebar.collapsed);
        assert_eq!(sidebar.expanded_mode, SidebarMode::Probes);
        sidebar.toggle_mode(SidebarMode::Probes);
        assert!(sidebar.collapsed);
    }

    #[test]
    fn defaults_and_secret_redaction() {
        let s: DesktopSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(s.preferences.transcript_font_size, 14);
        assert_eq!(s.preferences.conversation_max_width_ch, 85);
        let c = ConnectionSettings {
            client_key: "sensitive".into(),
            ..Default::default()
        };
        assert!(!format!("{c:?}").contains("sensitive"));
    }
    #[test]
    fn conversation_width_is_global_and_persists() {
        let mut settings = DesktopSettings::default();
        settings.preferences.conversation_max_width_ch = 110;
        let remote = ConnectionProfile::new_remote("Remote", ConnectionSettings::default());
        settings.active_profile_id = remote.id.clone();
        settings.profiles.push(remote);
        let encoded = serde_json::to_string(&settings).unwrap();
        let mut restored: DesktopSettings = serde_json::from_str(&encoded).unwrap();
        assert_eq!(restored.preferences.conversation_max_width_ch, 110);
        restored.active_profile_id = LOCAL_PROFILE_ID.into();
        assert_eq!(restored.preferences.conversation_max_width_ch, 110);
    }
    #[test]
    fn profile_isolation_and_protection() {
        let mut s = DesktopSettings::default();
        s.ensure_current_server().sidebar.collapsed = true;
        let remote = ConnectionProfile::new_remote("Remote", ConnectionSettings::default());
        let id = remote.id.clone();
        s.profiles.push(remote);
        s.active_profile_id = id.clone();
        assert!(!s.current_server().unwrap().sidebar.collapsed);
        assert!(!s.remove_profile(LOCAL_PROFILE_ID));
        assert!(s.remove_profile(&id));
        assert_eq!(s.active_profile_id, LOCAL_PROFILE_ID);
        assert!(s.current_server().unwrap().sidebar.collapsed);
    }
    #[test]
    fn double_click_details_preference_defaults_and_roundtrips() {
        let mut settings: DesktopSettings = serde_json::from_str("{}").unwrap();
        assert!(settings.preferences.double_click_session_opens_details);
        settings.preferences.double_click_session_opens_details = false;
        let encoded = serde_json::to_string(&settings).unwrap();
        let restored: DesktopSettings = serde_json::from_str(&encoded).unwrap();
        assert!(!restored.preferences.double_click_session_opens_details);
        let legacy: UiPreferences = serde_json::from_str(r#"{"auto_follow":false}"#).unwrap();
        assert_eq!(legacy.conversation_max_width_ch, 85);
        assert!(legacy.double_click_session_opens_details);
    }

    #[test]
    fn transcript_appearance_defaults_and_roundtrips() {
        let mut settings: DesktopSettings =
            serde_json::from_str(r#"{"preferences":{"auto_follow":false}}"#).unwrap();
        assert!(settings.preferences.hide_text_recorded_results);
        assert!(settings.preferences.hide_agent_statuses);
        assert_eq!(
            settings.preferences.reasoning_display,
            ReasoningDisplay::Collapsed
        );
        assert_eq!(
            settings.preferences.turn_dividers,
            TurnDividerDisplay::Labeled
        );

        settings.preferences.hide_text_recorded_results = false;
        settings.preferences.hide_agent_statuses = false;
        settings.preferences.reasoning_display = ReasoningDisplay::Shown;
        settings.preferences.turn_dividers = TurnDividerDisplay::Line;
        let encoded = serde_json::to_string(&settings).unwrap();
        let restored: DesktopSettings = serde_json::from_str(&encoded).unwrap();
        assert!(!restored.preferences.hide_text_recorded_results);
        assert!(!restored.preferences.hide_agent_statuses);
        assert_eq!(
            restored.preferences.reasoning_display,
            ReasoningDisplay::Shown
        );
        assert_eq!(restored.preferences.turn_dividers, TurnDividerDisplay::Line);
    }

    #[test]
    fn migrates_legacy_settings_and_roundtrips() {
        let json = r#"{"connection":{"api_url":"https://remote/","client_key":"secret"},"preferences":{"transcript_font_size":18},"servers":[{"server_url":"https://remote","sidebar":{"collapsed":true}},{"server_url":"https://other","sidebar":{"collapsed":true}}]}"#;
        let s: DesktopSettings = serde_json::from_str(json).unwrap();
        assert_eq!(s.profiles.len(), 3);
        assert_eq!(s.active_profile().connection.client_key, "secret");
        assert!(s.active_profile().workspace.sidebar.collapsed);
        assert_eq!(s.preferences.transcript_font_size, 18);
        assert!(!format!("{s:?}").contains("secret"));
        let saved = serde_json::to_string(&s).unwrap();
        let restored: DesktopSettings = serde_json::from_str(&saved).unwrap();
        assert_eq!(restored.active_profile_id, s.active_profile_id);
        assert!(restored.active_profile().workspace.sidebar.collapsed);
    }
    #[test]
    fn local_key_is_never_serialized_but_remote_key_is() {
        let mut settings = DesktopSettings::default();
        settings.active_profile_mut().connection.client_key = "local-secret".into();
        let mut remote = ConnectionProfile::new_remote("remote", ConnectionSettings::default());
        remote.connection.client_key = "remote-secret".into();
        settings.profiles.push(remote);
        let serialized = serde_json::to_string(&settings).unwrap();
        assert!(!serialized.contains("local-secret"));
        assert!(serialized.contains("remote-secret"));
        assert_eq!(
            settings.active_profile().connection.client_key,
            "local-secret"
        );
        let restored: DesktopSettings = serde_json::from_str(&serialized).unwrap();
        assert!(restored.profiles[0].connection.client_key.is_empty());
        assert_eq!(restored.profiles[1].connection.client_key, "remote-secret");
    }
    #[test]
    fn invalid_active_profile_falls_back_to_local() {
        let s: DesktopSettings =
            serde_json::from_str(r#"{"profiles":[],"active_profile_id":"missing"}"#).unwrap();
        assert_eq!(s.active_profile().id, LOCAL_PROFILE_ID);
    }
}
