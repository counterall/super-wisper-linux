//! Keysym names, shortcut parsing and keysym → keycode lookup.

use anyhow::{anyhow, bail, Result};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConnectionExt, Keycode, Keysym, ModMask};

pub const XK_ESCAPE: Keysym = 0xff1b;
pub const XK_CONTROL_L: Keysym = 0xffe3;
pub const XK_CONTROL_R: Keysym = 0xffe4;
pub const XK_SHIFT_L: Keysym = 0xffe1;
pub const XK_SHIFT_R: Keysym = 0xffe2;
pub const XK_ALT_L: Keysym = 0xffe9;
pub const XK_ALT_R: Keysym = 0xffea;
pub const XK_ISO_LEVEL3_SHIFT: Keysym = 0xfe03; // AltGr on many layouts
pub const XK_SUPER_L: Keysym = 0xffeb;
pub const XK_SUPER_R: Keysym = 0xffec;
pub const XK_V: Keysym = 0x76;

/// Modifiers that are part of a shortcut; Lock (CapsLock) and Mod2
/// (NumLock) are ignored when matching.
pub fn significant_mods() -> ModMask {
    ModMask::CONTROL | ModMask::SHIFT | ModMask::M1 | ModMask::M4
}

/// Variants of a grab that cover CapsLock/NumLock being on.
pub fn lock_variants() -> [ModMask; 4] {
    [
        ModMask::from(0u16),
        ModMask::LOCK,
        ModMask::M2,
        ModMask::LOCK | ModMask::M2,
    ]
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shortcut {
    pub mods: ModMask,
    pub keysym: Keysym,
}

/// Parses `"Ctrl+Alt+space"`-style shortcuts (case-insensitive).
pub fn parse_shortcut(s: &str) -> Result<Shortcut> {
    let mut mods = ModMask::from(0u16);
    let mut keysym = None;
    for part in s.split('+').map(str::trim).filter(|p| !p.is_empty()) {
        match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => mods |= ModMask::CONTROL,
            "shift" => mods |= ModMask::SHIFT,
            "alt" => mods |= ModMask::M1,
            "super" | "win" | "meta" => mods |= ModMask::M4,
            _ => {
                if keysym.is_some() {
                    bail!("shortcut {s:?} has more than one non-modifier key");
                }
                keysym = Some(keysym_from_name(part).ok_or_else(|| anyhow!("unknown key {part:?}"))?);
            }
        }
    }
    let keysym = keysym.ok_or_else(|| anyhow!("shortcut {s:?} has no key"))?;
    Ok(Shortcut { mods, keysym })
}

pub fn keysym_from_name(name: &str) -> Option<Keysym> {
    let lower = name.to_ascii_lowercase();
    if lower.len() == 1 {
        let c = lower.as_bytes()[0];
        if c.is_ascii_alphanumeric() {
            return Some(c as Keysym);
        }
    }
    if let Some(n) = lower.strip_prefix('f').and_then(|n| n.parse::<u32>().ok()) {
        if (1..=24).contains(&n) {
            return Some(0xffbe + n - 1);
        }
    }
    if let Some(n) = lower.strip_prefix("kp_").and_then(|n| n.parse::<u32>().ok()) {
        if n <= 9 {
            return Some(0xffb0 + n);
        }
    }
    Some(match lower.as_str() {
        "space" => 0x20,
        "grave" | "`" => 0x60,
        "minus" | "-" => 0x2d,
        "equal" | "=" => 0x3d,
        "bracketleft" | "[" => 0x5b,
        "bracketright" | "]" => 0x5d,
        "semicolon" | ";" => 0x3b,
        "apostrophe" | "'" => 0x27,
        "comma" | "," => 0x2c,
        "period" | "." => 0x2e,
        "slash" | "/" => 0x2f,
        "backslash" | "\\" => 0x5c,
        "return" | "enter" => 0xff0d,
        "tab" => 0xff09,
        "escape" | "esc" => XK_ESCAPE,
        "backspace" => 0xff08,
        "insert" => 0xff63,
        "delete" => 0xffff,
        "home" => 0xff50,
        "end" => 0xff57,
        "page_up" | "pageup" | "prior" => 0xff55,
        "page_down" | "pagedown" | "next" => 0xff56,
        "left" => 0xff51,
        "up" => 0xff52,
        "right" => 0xff53,
        "down" => 0xff54,
        "pause" => 0xff13,
        "scroll_lock" => 0xff14,
        "print" => 0xff61,
        "menu" => 0xff67,
        _ => return None,
    })
}

/// Snapshot of the server's keycode → keysym table.
pub struct Keymap {
    min_keycode: Keycode,
    per_keycode: usize,
    keysyms: Vec<Keysym>,
}

impl Keymap {
    pub fn load(conn: &impl Connection) -> Result<Self> {
        let setup = conn.setup();
        let (min, max) = (setup.min_keycode, setup.max_keycode);
        let reply = conn.get_keyboard_mapping(min, max - min + 1)?.reply()?;
        Ok(Self {
            min_keycode: min,
            per_keycode: reply.keysyms_per_keycode as usize,
            keysyms: reply.keysyms,
        })
    }

    /// Keycodes producing `keysym` without a level shift (first two columns,
    /// i.e. plain or Shift); letters match either case.
    pub fn keycodes(&self, keysym: Keysym) -> Vec<Keycode> {
        let upper = if (0x61..=0x7a).contains(&keysym) { keysym - 0x20 } else { keysym };
        self.keysyms
            .chunks(self.per_keycode.max(1))
            .enumerate()
            .filter(|(_, syms)| syms.iter().take(2).any(|&s| s == keysym || s == upper))
            .map(|(i, _)| self.min_keycode + i as Keycode)
            .collect()
    }

    pub fn keycode(&self, keysym: Keysym) -> Option<Keycode> {
        self.keycodes(keysym).into_iter().next()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_shortcuts() {
        assert_eq!(
            parse_shortcut("Alt+grave").unwrap(),
            Shortcut { mods: ModMask::M1, keysym: 0x60 }
        );
        assert_eq!(
            parse_shortcut("ctrl + shift + Space").unwrap(),
            Shortcut { mods: ModMask::CONTROL | ModMask::SHIFT, keysym: 0x20 }
        );
        assert_eq!(parse_shortcut("Super+F12").unwrap().keysym, 0xffc9);
        assert_eq!(parse_shortcut("Ctrl+R").unwrap().keysym, 0x72);
        assert!(parse_shortcut("Ctrl+Alt").is_err());
        assert!(parse_shortcut("Ctrl+a+b").is_err());
        assert!(parse_shortcut("Ctrl+nope").is_err());
    }
}
