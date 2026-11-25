//! Modern Windows notifications with action buttons.
//!
//! SyncApp registers itself (per user, no admin) as a notification sender and as the
//! handler for `syncapp:` links. Buttons in a notification open such a link, which starts
//! SyncApp with a fixed, known action; anything else is ignored. If modern notifications
//! aren't available, callers fall back to the classic tray balloon.

use std::path::Path;

use windows::core::HSTRING;
use windows::Data::Xml::Dom::XmlDocument;
use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};

use crate::win;

pub const AUMID: &str = "SyncApp";
const AUMID_KEY: &str = r"Software\Classes\AppUserModelId\SyncApp";
const PROTOCOL_KEY: &str = r"Software\Classes\syncapp";

/// What a click on a notification (or one of its buttons) should do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Open,
    ShowUpdate,
}

impl Action {
    fn uri(self) -> &'static str {
        match self {
            Action::Open => "syncapp:open",
            Action::ShowUpdate => "syncapp:update",
        }
    }

    /// Only these exact links are understood; anything else just opens SyncApp.
    pub fn from_uri(s: &str) -> Option<Action> {
        let s = s.trim().trim_end_matches('/').to_ascii_lowercase();
        if !s.starts_with("syncapp:") {
            return None;
        }
        Some(if s == "syncapp:update" { Action::ShowUpdate } else { Action::Open })
    }
}

/// Register the notification sender identity and the `syncapp:` link handler for this
/// exe. Cheap when already up to date.
pub fn register(exe: &Path) {
    let exe_s = exe.display().to_string();
    let command = format!("\"{exe_s}\" \"%1\"");
    if win::reg_get_str(&format!(r"{PROTOCOL_KEY}\shell\open\command"), "").as_deref() == Some(command.as_str()) {
        return;
    }
    let icon = crate::config::data_dir().join("syncapp.ico");
    let _ = std::fs::create_dir_all(icon.parent().unwrap());
    let _ = std::fs::write(&icon, include_bytes!("../assets/syncapp.ico"));
    let _ = win::reg_set_str(AUMID_KEY, "DisplayName", "SyncApp");
    let _ = win::reg_set_str(AUMID_KEY, "IconUri", &icon.display().to_string());
    let _ = win::reg_set_str(PROTOCOL_KEY, "", "URL:SyncApp");
    let _ = win::reg_set_str(PROTOCOL_KEY, "URL Protocol", "");
    let _ = win::reg_set_str(&format!(r"{PROTOCOL_KEY}\DefaultIcon"), "", &format!("{exe_s},0"));
    let _ = win::reg_set_str(&format!(r"{PROTOCOL_KEY}\shell\open\command"), "", &command);
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

/// Build the notification XML (separate for testing).
pub fn toast_xml(title: &str, body: &str, click: Action, buttons: &[(&str, Action)]) -> String {
    let mut x = format!(
        r#"<toast activationType="protocol" launch="{}"><visual><binding template="ToastGeneric"><text>{}</text><text>{}</text></binding></visual>"#,
        click.uri(),
        xml_escape(title),
        xml_escape(body)
    );
    if !buttons.is_empty() {
        x += "<actions>";
        for (label, action) in buttons {
            x += &format!(
                r#"<action content="{}" activationType="protocol" arguments="{}"/>"#,
                xml_escape(label),
                action.uri()
            );
        }
        x += "</actions>";
    }
    x + "</toast>"
}

/// Show a modern notification. Returns false if Windows refused (caller falls back).
pub fn show(title: &str, body: &str, click: Action, buttons: &[(&str, Action)]) -> bool {
    let xml = toast_xml(title, body, click, buttons);
    let res: windows::core::Result<()> = (|| {
        unsafe {
            let _ = RoInitialize(RO_INIT_MULTITHREADED);
        }
        let doc = XmlDocument::new()?;
        doc.LoadXml(&HSTRING::from(xml.as_str()))?;
        let toast = ToastNotification::CreateToastNotification(&doc)?;
        ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(AUMID))?.Show(&toast)
    })();
    res.is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_and_xml() {
        assert_eq!(Action::from_uri("syncapp:update"), Some(Action::ShowUpdate));
        assert_eq!(Action::from_uri("SyncApp:Open/"), Some(Action::Open));
        // Unknown actions only ever open the app; other schemes are ignored.
        assert_eq!(Action::from_uri("syncapp:install?url=evil"), Some(Action::Open));
        assert_eq!(Action::from_uri("https://example.com"), None);
        let x = toast_xml("A <b> & \"c\"", "body", Action::Open, &[("Update now", Action::ShowUpdate)]);
        assert!(x.contains("A &lt;b&gt; &amp; &quot;c&quot;"));
        assert!(x.contains(r#"arguments="syncapp:update""#));
        assert!(x.contains(r#"launch="syncapp:open""#));
    }
}
