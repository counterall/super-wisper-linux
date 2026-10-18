//! Finds desktop shortcuts that collide with our trigger.
//!
//! Window managers (Muffin, Mutter) register their keybindings with XInput2
//! passive grabs, which win over our core grab without the server reporting
//! a conflict. So a taken shortcut fails silently unless we look it up in
//! the desktop's own settings.

use std::process::Command;

use x11rb::protocol::xproto::ModMask;

use crate::keys::{self, Shortcut};

/// Schema prefixes holding keybindings, per desktop.
fn schema_prefixes() -> &'static [&'static str] {
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().to_ascii_lowercase();
    if desktop.contains("cinnamon") {
        &["org.cinnamon.desktop.keybindings", "org.cinnamon.muffin.keybindings"]
    } else if desktop.contains("gnome") {
        &[
            "org.gnome.desktop.wm.keybindings",
            "org.gnome.settings-daemon.plugins.media-keys",
            "org.gnome.shell.keybindings",
            "org.gnome.mutter.keybindings",
        ]
    } else {
        &[]
    }
}

/// Desktop actions bound to the same key combination, as "schema key".
pub fn find(shortcut: &Shortcut) -> Vec<String> {
    let prefixes = schema_prefixes();
    if prefixes.is_empty() {
        return Vec::new();
    }
    let Ok(out) = Command::new("gsettings").arg("list-recursively").output() else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|line| prefixes.iter().any(|p| line.starts_with(p)))
        .filter_map(|line| {
            let mut parts = line.splitn(3, ' ');
            let (schema, key, value) = (parts.next()?, parts.next()?, parts.next()?);
            accelerators(value)
                .filter_map(parse_accelerator)
                .any(|acc| acc == *shortcut)
                .then(|| format!("{schema} {key}"))
        })
        .collect()
}

/// Quoted strings of a gsettings array value: `['<Alt>Above_Tab', 'x']`.
fn accelerators(value: &str) -> impl Iterator<Item = &str> {
    value.split('\'').skip(1).step_by(2)
}

/// Parses GTK accelerators such as `<Primary><Alt>t` or `<Alt>Above_Tab`.
pub fn parse_accelerator(acc: &str) -> Option<Shortcut> {
    let mut mods = ModMask::from(0u16);
    let mut rest = acc;
    while let Some(stripped) = rest.strip_prefix('<') {
        let (name, after) = stripped.split_once('>')?;
        mods |= match name.to_ascii_lowercase().as_str() {
            "alt" | "mod1" => ModMask::M1,
            "shift" => ModMask::SHIFT,
            "control" | "ctrl" | "primary" => ModMask::CONTROL,
            "super" | "mod4" => ModMask::M4,
            _ => return None,
        };
        rest = after;
    }
    // "Above_Tab" is Mutter/Muffin's name for the key above Tab (grave on
    // most layouts).
    let keysym = match rest {
        "Above_Tab" => keys::keysym_from_name("grave")?,
        name => keys::keysym_from_name(name)?,
    };
    Some(Shortcut { mods, keysym })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_gtk_accelerators() {
        assert_eq!(parse_accelerator("<Alt>Above_Tab"), Some(keys::parse_shortcut("Alt+grave").unwrap()));
        assert_eq!(parse_accelerator("<Primary><Alt>t"), Some(keys::parse_shortcut("Ctrl+Alt+t").unwrap()));
        assert_eq!(parse_accelerator("<Super>E"), Some(keys::parse_shortcut("Super+e").unwrap()));
        assert_eq!(parse_accelerator("XF86AudioMute"), None);
        assert_eq!(parse_accelerator("<Hyper>a"), None);
    }

    #[test]
    fn extracts_quoted_values() {
        let v: Vec<_> = accelerators("['<Super>Above_Tab', '<Alt>Above_Tab']").collect();
        assert_eq!(v, ["<Super>Above_Tab", "<Alt>Above_Tab"]);
        assert_eq!(accelerators("@as []").count(), 0);
    }
}
