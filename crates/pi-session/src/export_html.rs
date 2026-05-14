//! Single-file HTML export for sessions. No external CSS/JS so the output
//! works offline and is safe to attach to issues. Tool outputs are wrapped
//! in `<details>` so they fold by default.

use pi_core::Role;

use crate::Session;

pub fn render(session: &Session) -> String {
    let mut out = String::with_capacity(2048);
    out.push_str("<!doctype html>\n<html lang=\"zh-cn\">\n<head>\n<meta charset=\"utf-8\">\n");
    out.push_str(&format!(
        "<title>Pi Session {}</title>\n",
        escape(&session.id)
    ));
    out.push_str("<style>\n");
    out.push_str(STYLE);
    out.push_str("</style>\n</head>\n<body>\n");
    out.push_str(&format!(
        "<header><h1>Pi Session <code>{}</code></h1>",
        escape(&session.id)
    ));
    if let Some(header) = &session.header {
        out.push_str("<dl class=\"meta\">");
        if let Some(cwd) = &header.cwd {
            out.push_str(&format!("<dt>cwd</dt><dd>{}</dd>", escape(cwd)));
        }
        out.push_str(&format!("<dt>created</dt><dd>{}</dd>", header.created_ms));
        out.push_str(&format!("<dt>version</dt><dd>{}</dd>", header.version));
        out.push_str("</dl>");
    }
    out.push_str("</header>\n<main>\n");

    for message in &session.messages {
        let role = message.role.as_str();
        let class = match message.role {
            Role::User => "msg user",
            Role::Assistant => "msg assistant",
            Role::Tool => "msg tool",
            Role::System => "msg system",
        };
        out.push_str(&format!("<section class=\"{class}\">"));
        out.push_str(&format!(
            "<h2><span class=\"role\">{role}</span><span class=\"ts\">{}</span></h2>",
            message.timestamp_ms
        ));
        match message.role {
            Role::Tool => {
                let summary = message
                    .tool_call_id
                    .as_deref()
                    .unwrap_or("tool")
                    .to_string();
                out.push_str(&format!(
                    "<details><summary>{}</summary><pre>{}</pre></details>",
                    escape(&summary),
                    escape(&message.content)
                ));
            }
            _ => {
                if !message.content.is_empty() {
                    out.push_str(&format!("<pre>{}</pre>", escape(&message.content)));
                }
                if !message.tool_calls.is_empty() {
                    out.push_str("<ul class=\"tool-calls\">");
                    for call in &message.tool_calls {
                        out.push_str(&format!(
                            "<li><code>{}</code> ({}) → <pre>{}</pre></li>",
                            escape(&call.name),
                            escape(call.id.as_deref().unwrap_or("")),
                            escape(&call.input)
                        ));
                    }
                    out.push_str("</ul>");
                }
                if !message.attachments.is_empty() {
                    out.push_str("<ul class=\"attachments\">");
                    for attachment in &message.attachments {
                        if let Some(url) = attachment.data_url() {
                            out.push_str(&format!(
                                "<li>{}<br><img alt=\"attachment\" src=\"{}\"></li>",
                                escape(&attachment.mime_type),
                                escape(&url)
                            ));
                        }
                    }
                    out.push_str("</ul>");
                }
            }
        }
        out.push_str("</section>\n");
    }
    out.push_str("</main>\n</body>\n</html>\n");
    out
}

const STYLE: &str = r#"
body { font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Helvetica, Arial, sans-serif; max-width: 920px; margin: 0 auto; padding: 24px; background: #0f1115; color: #e6edf3; }
header h1 { font-size: 18px; margin: 0 0 8px 0; }
dl.meta { display: grid; grid-template-columns: max-content 1fr; gap: 2px 12px; font-size: 12px; color: #8b949e; margin: 0 0 24px 0; }
dl.meta dt { font-weight: bold; }
section.msg { padding: 12px 16px; margin: 12px 0; border-radius: 6px; }
section.msg h2 { margin: 0 0 6px 0; font-size: 12px; font-weight: bold; display: flex; gap: 8px; }
section.msg pre { white-space: pre-wrap; word-break: break-word; margin: 0; font-family: SFMono-Regular, Menlo, Consolas, monospace; font-size: 13px; }
section.user { background: #102a4f; }
section.assistant { background: #0d2f24; }
section.tool { background: #1d1730; }
section.system { background: #1a1d22; color: #8b949e; }
section.msg .role { text-transform: uppercase; letter-spacing: 0.1em; }
section.msg .ts { color: #8b949e; font-weight: normal; }
details summary { cursor: pointer; color: #8b949e; }
ul.tool-calls li { margin: 4px 0; }
ul.attachments img { max-width: 100%; border-radius: 4px; margin-top: 4px; }
"#;

fn escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_core::{Message, Role};

    #[test]
    fn renders_basic_session() {
        let mut session = Session::new("alpha");
        session.push(Message::new(Role::User, "你好"));
        session.push(Message::new(Role::Assistant, "<script>alert(1)</script>"));
        let html = render(&session);
        assert!(html.contains("<title>Pi Session alpha</title>"));
        assert!(html.contains("<h1>Pi Session <code>alpha</code></h1>"));
        assert!(html.contains("&lt;script&gt;"));
        assert!(html.contains("section class=\"msg user\""));
        assert!(html.contains("section class=\"msg assistant\""));
    }

    #[test]
    fn tool_output_is_folded() {
        let mut session = Session::new("t");
        session.push(Message::tool_result(
            Some("call_1".into()),
            "lots of output",
        ));
        let html = render(&session);
        assert!(html.contains("<details>"));
    }
}
