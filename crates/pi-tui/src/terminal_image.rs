//! Inline image rendering for terminal emulators that support it.
//!
//! Three protocols, picked by environment detection:
//!
//! 1. **iTerm2 inline image protocol** (`TERM_PROGRAM=iTerm.app`,
//!    `LC_TERMINAL=iTerm2`, or `TERM_PROGRAM=WezTerm`): wrap base64 bytes
//!    in a special `OSC 1337 ; File=...:<b64> BEL` escape sequence.
//! 2. **Kitty graphics** (`TERM=xterm-kitty`): `\x1b_Ga=T,f=100;<b64>\x1b\\`.
//! 3. **Sixel** (`TERM` contains `sixel` or `xterm-256color` on certain
//!    builds): not implemented inline — we fall back to text placeholder.
//!
//! For terminals that support none of the above, we emit a text marker
//! `[image attached: <path or mime>]`. The TUI transcript renderer pipes
//! the chosen string into a ratatui `Paragraph` so the right protocol's
//! bytes reach the terminal directly.

use std::env;

use pi_core::{Attachment, AttachmentData, AttachmentKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageProtocol {
    Iterm2,
    Kitty,
    Sixel,
    None,
}

pub fn detect_protocol() -> ImageProtocol {
    if env::var("PI_DISABLE_INLINE_IMAGES").ok().as_deref() == Some("1") {
        return ImageProtocol::None;
    }
    if env::var("TERM_PROGRAM")
        .map(|t| matches!(t.as_str(), "iTerm.app" | "WezTerm"))
        .unwrap_or(false)
    {
        return ImageProtocol::Iterm2;
    }
    if env::var("LC_TERMINAL")
        .map(|t| t == "iTerm2")
        .unwrap_or(false)
    {
        return ImageProtocol::Iterm2;
    }
    let term = env::var("TERM").unwrap_or_default();
    if term == "xterm-kitty" {
        return ImageProtocol::Kitty;
    }
    if term.contains("sixel") {
        return ImageProtocol::Sixel;
    }
    ImageProtocol::None
}

/// Render the attachment as the right ANSI escape sequence for the
/// current terminal. Returns `None` when no inline-image protocol is
/// available — callers fall back to a text placeholder.
pub fn render(attachment: &Attachment) -> Option<String> {
    if attachment.kind != AttachmentKind::Image {
        return None;
    }
    let bytes = match &attachment.data {
        AttachmentData::Base64 { data } => data.clone(),
        AttachmentData::Url { .. } => return None,
    };
    match detect_protocol() {
        ImageProtocol::Iterm2 => Some(format!(
            "\x1b]1337;File=inline=1;preserveAspectRatio=1;size={}:{}\x07",
            bytes.len(),
            bytes
        )),
        ImageProtocol::Kitty => Some(format!("\x1b_Ga=T,f=100,t=d;{}\x1b\\", bytes)),
        ImageProtocol::Sixel | ImageProtocol::None => None,
    }
}

/// Text fallback when no inline protocol is supported.
pub fn placeholder(attachment: &Attachment) -> String {
    match attachment.kind {
        AttachmentKind::Image => format!("[image attached: {}]", attachment.mime_type),
        AttachmentKind::File => "[file attached]".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_png() -> Attachment {
        Attachment::image_from_bytes("image/png", b"fake")
    }

    #[test]
    fn placeholder_mentions_mime() {
        let p = placeholder(&make_png());
        assert!(p.contains("image/png"));
    }

    #[test]
    fn url_attachments_have_no_inline_form() {
        let attachment = Attachment::image_url("https://example.com/a.png");
        assert!(render(&attachment).is_none());
    }
}
