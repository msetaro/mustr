//! Comment bodies from forges are Markdown with HTML mixed in: bots wrap long reports in
//! `<details>`, people write `<br>` and `<strong>`. mustr renders Markdown natively, with no
//! HTML engine (`mustr-docs/PLAN.md` §12), so the HTML is turned into the Markdown it means:
//! common formatting tags become Markdown, other tags are dropped while their text is kept, and
//! comments, scripts, and styles are removed with their contents. Nothing is executed or
//! loaded: images become links.

use markdown::parser::PARSE_OPTIONS;
use pulldown_cmark::{Event, Parser, Tag, TagEnd};

/// `body` with its HTML turned into Markdown. Code spans and code blocks are left alone.
pub fn comment_markdown(body: &str) -> String {
    if !body.contains('<') {
        return body.to_string();
    }
    let mut converter = Converter::default();
    let mut output = String::with_capacity(body.len());
    let mut copied_to = 0;
    let mut table_depth = 0usize;
    for (event, range) in Parser::new_ext(body, PARSE_OPTIONS).into_offset_iter() {
        let block = match event {
            Event::Start(Tag::Table(_)) => {
                table_depth += 1;
                continue;
            }
            Event::End(TagEnd::Table) => {
                table_depth = table_depth.saturating_sub(1);
                continue;
            }
            // The whole block at once, since its tags and comments can span lines.
            Event::Start(Tag::HtmlBlock) => true,
            Event::InlineHtml(_) | Event::Html(_) => false,
            _ => continue,
        };
        // The lines of a block that was already converted.
        if range.start < copied_to {
            continue;
        }
        // The text between a `<script>` and its `</script>` goes too.
        if converter.dropping.is_none() {
            output.push_str(&body[copied_to..range.start]);
        }
        converter.in_table = table_depth > 0;
        converter.convert(&body[range.clone()], block, &mut output);
        copied_to = range.end;
    }
    if converter.dropping.is_none() {
        output.push_str(&body[copied_to..]);
    }
    output
}

#[derive(Debug, PartialEq, Eq)]
enum Token<'a> {
    Text(&'a str),
    Tag(HtmlTag),
    /// A comment, doctype, or processing instruction.
    Ignored,
}

#[derive(Debug, PartialEq, Eq)]
struct HtmlTag {
    /// Lowercase.
    name: String,
    closing: bool,
    attributes: Vec<(String, String)>,
}

impl HtmlTag {
    fn attribute(&self, name: &str) -> Option<&str> {
        self.attributes
            .iter()
            .find(|(attribute, _)| attribute == name)
            .map(|(_, value)| value.as_str())
    }
}

/// Splits HTML into text and tags. Anything that doesn't parse as a tag is text.
fn tokenize(html: &str) -> Vec<Token<'_>> {
    let mut tokens = Vec::new();
    let mut text_start = 0;
    let mut index = 0;
    while let Some(offset) = html[index..].find('<') {
        let start = index + offset;
        let Some((token, end)) = parse_markup(html, start) else {
            index = start + 1;
            continue;
        };
        if text_start < start {
            tokens.push(Token::Text(&html[text_start..start]));
        }
        tokens.push(token);
        index = end;
        text_start = end;
    }
    if text_start < html.len() {
        tokens.push(Token::Text(&html[text_start..]));
    }
    tokens
}

/// The markup starting at `start` (a `<`), and where it ends.
fn parse_markup(html: &str, start: usize) -> Option<(Token<'static>, usize)> {
    let rest = &html[start..];
    if let Some(comment) = rest.strip_prefix("<!--") {
        let end = comment
            .find("-->")
            .map_or(html.len(), |end| start + 4 + end + 3);
        return Some((Token::Ignored, end));
    }
    if rest.starts_with("<!") || rest.starts_with("<?") {
        let end = rest.find('>').map_or(html.len(), |end| start + end + 1);
        return Some((Token::Ignored, end));
    }

    let bytes = html.as_bytes();
    let mut index = start + 1;
    let closing = bytes.get(index) == Some(&b'/');
    if closing {
        index += 1;
    }
    let name_start = index;
    if !bytes.get(index).is_some_and(u8::is_ascii_alphabetic) {
        return None;
    }
    while bytes
        .get(index)
        .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'-')
    {
        index += 1;
    }
    let name = html[name_start..index].to_ascii_lowercase();

    let mut attributes = Vec::new();
    loop {
        while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
            index += 1;
        }
        match bytes.get(index)? {
            b'>' => {
                index += 1;
                break;
            }
            b'/' => {
                index += 1;
                continue;
            }
            _ => {}
        }
        let attribute_start = index;
        while bytes
            .get(index)
            .is_some_and(|byte| !byte.is_ascii_whitespace() && !b"=>/".contains(byte))
        {
            index += 1;
        }
        if index == attribute_start {
            // A stray `=`.
            index += 1;
            continue;
        }
        let attribute = html[attribute_start..index].to_ascii_lowercase();
        while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
            index += 1;
        }
        let mut value = String::new();
        if bytes.get(index) == Some(&b'=') {
            index += 1;
            while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
                index += 1;
            }
            match bytes.get(index)? {
                quote @ (b'"' | b'\'') => {
                    let value_start = index + 1;
                    let value_end = value_start + html[value_start..].find(*quote as char)?;
                    value = html[value_start..value_end].to_string();
                    index = value_end + 1;
                }
                _ => {
                    let value_start = index;
                    while bytes
                        .get(index)
                        .is_some_and(|byte| !byte.is_ascii_whitespace() && *byte != b'>')
                    {
                        index += 1;
                    }
                    value = html[value_start..index].to_string();
                }
            }
        }
        attributes.push((attribute, value));
    }
    Some((
        Token::Tag(HtmlTag {
            name,
            closing,
            attributes,
        }),
        index,
    ))
}

/// Tags whose contents are dropped along with them.
const DROPPED_WITH_CONTENTS: &[&str] = &[
    "script", "style", "template", "textarea", "iframe", "object", "noscript", "svg", "math",
];

#[derive(Default)]
struct Converter {
    strong: usize,
    emphasis: usize,
    strikethrough: usize,
    code: usize,
    preformatted: usize,
    /// The URL of each open `<a>`, if it's one mustr links to.
    links: Vec<Option<String>>,
    /// The tag whose contents are being dropped.
    dropping: Option<String>,
    /// In a Markdown table, where a line break would end the table.
    in_table: bool,
}

impl Converter {
    fn convert(&mut self, html: &str, block: bool, output: &mut String) {
        for token in tokenize(html) {
            match token {
                Token::Text(text) => {
                    // Line breaks between block tags mean nothing, and would pile up.
                    let insignificant = block
                        && self.preformatted == 0
                        && text.trim().is_empty()
                        && text.contains('\n')
                        && (output.is_empty() || output.ends_with('\n'));
                    if self.dropping.is_none() && !insignificant {
                        output.push_str(text);
                    }
                }
                Token::Ignored => {}
                Token::Tag(tag) => self.tag(&tag, block, output),
            }
        }
    }

    fn tag(&mut self, tag: &HtmlTag, block: bool, output: &mut String) {
        if let Some(dropping) = &self.dropping {
            if tag.closing && *dropping == tag.name {
                self.dropping = None;
            }
            return;
        }
        if DROPPED_WITH_CONTENTS.contains(&tag.name.as_str()) {
            if !tag.closing {
                self.dropping = Some(tag.name.clone());
            }
            return;
        }
        // Inside `<pre>`, only the text counts.
        if self.preformatted > 0 && tag.name != "pre" {
            if tag.name == "br" {
                output.push('\n');
            }
            return;
        }
        match (tag.name.as_str(), tag.closing) {
            ("b" | "strong" | "summary", false) => {
                if tag.name == "summary" {
                    self.paragraph_break(output);
                }
                self.strong += 1;
                if self.strong == 1 {
                    output.push_str("**");
                }
            }
            ("b" | "strong" | "summary", true) => {
                if self.strong == 1 {
                    output.push_str("**");
                }
                self.strong = self.strong.saturating_sub(1);
                if tag.name == "summary" {
                    self.paragraph_break(output);
                }
            }
            ("i" | "em" | "cite", false) => {
                self.emphasis += 1;
                if self.emphasis == 1 {
                    output.push('*');
                }
            }
            ("i" | "em" | "cite", true) => {
                if self.emphasis == 1 {
                    output.push('*');
                }
                self.emphasis = self.emphasis.saturating_sub(1);
            }
            ("del" | "s" | "strike", false) => {
                self.strikethrough += 1;
                if self.strikethrough == 1 {
                    output.push_str("~~");
                }
            }
            ("del" | "s" | "strike", true) => {
                if self.strikethrough == 1 {
                    output.push_str("~~");
                }
                self.strikethrough = self.strikethrough.saturating_sub(1);
            }
            ("code" | "kbd" | "samp" | "tt", false) => {
                self.code += 1;
                if self.code == 1 {
                    output.push('`');
                }
            }
            ("code" | "kbd" | "samp" | "tt", true) => {
                if self.code == 1 {
                    output.push('`');
                }
                self.code = self.code.saturating_sub(1);
            }
            ("pre", false) => {
                self.paragraph_break(output);
                if !self.in_table {
                    output.push_str("```\n");
                }
                self.preformatted += 1;
            }
            ("pre", true) => {
                if self.preformatted > 0 {
                    self.preformatted -= 1;
                    if !self.in_table {
                        if !output.ends_with('\n') {
                            output.push('\n');
                        }
                        output.push_str("```");
                    }
                    self.paragraph_break(output);
                }
            }
            ("a", false) => {
                // Links can't hold links, so only the outermost one is kept.
                let url = tag
                    .attribute("href")
                    .filter(|_| self.links.iter().all(Option::is_none))
                    .and_then(link_url);
                if url.is_some() {
                    output.push('[');
                }
                self.links.push(url);
            }
            ("a", true) => {
                if let Some(Some(url)) = self.links.pop() {
                    output.push_str(&format!("](<{url}>)"));
                }
            }
            ("img", false) => {
                let alt = tag
                    .attribute("alt")
                    .map(str::trim)
                    .filter(|alt| !alt.is_empty())
                    .unwrap_or("image");
                let alt = escape_link_text(alt);
                match tag.attribute("src").and_then(link_url) {
                    Some(src) if self.links.iter().all(Option::is_none) => {
                        output.push_str(&format!("[{alt}](<{src}>)"));
                    }
                    _ => output.push_str(&alt),
                }
            }
            ("br", _) => {
                if self.in_table {
                    output.push(' ');
                } else {
                    output.push_str("  \n");
                }
            }
            ("hr", _) => {
                self.paragraph_break(output);
                if !self.in_table {
                    output.push_str("---");
                }
                self.paragraph_break(output);
            }
            ("h1" | "h2" | "h3" | "h4" | "h5" | "h6", closing) => {
                self.paragraph_break(output);
                if !closing && !self.in_table {
                    let level = tag.name[1..].parse::<usize>().unwrap_or(1);
                    output.push_str(&"#".repeat(level));
                    output.push(' ');
                }
            }
            ("li", false) => {
                if self.in_table {
                    output.push(' ');
                } else {
                    if !output.is_empty() && !output.ends_with('\n') {
                        output.push('\n');
                    }
                    output.push_str("- ");
                }
            }
            ("li", true) => {
                if !self.in_table && !output.ends_with('\n') {
                    output.push('\n');
                }
            }
            ("td" | "th", true) => output.push_str(" | "),
            (
                "p" | "div" | "details" | "ul" | "ol" | "blockquote" | "table" | "tr" | "section"
                | "article" | "header" | "footer" | "center" | "dl" | "dt" | "dd" | "figure"
                | "figcaption",
                _,
            ) => {
                if block || matches!(tag.name.as_str(), "p" | "div" | "details" | "tr") {
                    self.paragraph_break(output);
                }
            }
            // Everything else (`span`, `sub`, `sup`, `picture`, ...) keeps just its text.
            _ => {}
        }
    }

    /// Ends the current paragraph, unless the output already did.
    fn paragraph_break(&self, output: &mut String) {
        if self.in_table {
            if !output.ends_with(' ') {
                output.push(' ');
            }
            return;
        }
        let trimmed = output.trim_end_matches([' ', '\t']).len();
        output.truncate(trimmed);
        if output.is_empty() || output.ends_with("\n\n") {
            return;
        }
        output.push_str(if output.ends_with('\n') { "\n" } else { "\n\n" });
    }
}

/// URLs mustr links to: web and mail links only.
fn link_url(url: &str) -> Option<String> {
    let url = url.trim();
    let lowercase = url.to_ascii_lowercase();
    let allowed = ["https://", "http://", "mailto:"]
        .iter()
        .any(|scheme| lowercase.starts_with(scheme));
    (allowed && !url.contains(['<', '>', '\n', '\r'])).then(|| url.to_string())
}

fn escape_link_text(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('[', "\\[")
        .replace(']', "\\]")
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    /// The conversion with runs of blank lines collapsed, which Markdown ignores anyway.
    fn convert(body: &str) -> String {
        let mut output = comment_markdown(body);
        while output.contains("\n\n\n") {
            output = output.replace("\n\n\n", "\n\n");
        }
        output.trim().to_string()
    }

    #[test]
    fn plain_markdown_is_untouched() {
        let body = "Should this say **hello, world**?\n\n```html\n<b>not html</b>\n```\n\nUse `<br>` here.";
        assert_eq!(comment_markdown(body), body);
        assert_eq!(comment_markdown("a < b and c > d"), "a < b and c > d");
    }

    #[test]
    fn inline_formatting_becomes_markdown() {
        assert_eq!(
            convert(
                "This is <strong>important</strong>,<br>really <em>quite</em> <b><b>so</b></b>."
            ),
            "This is **important**,  \nreally *quite* **so**."
        );
        assert_eq!(
            convert(
                "Press <kbd>Ctrl</kbd>+<kbd>C</kbd>, see <del>old</del> <span class=\"x\">new</span><sup>1</sup>"
            ),
            "Press `Ctrl`+`C`, see ~~old~~ new1"
        );
    }

    #[test]
    fn details_and_summary_show_expanded() {
        let body = "<details>\n<summary>Walkthrough</summary>\n\n## Changes\n\n- One\n\n</details>\n\nDone.";
        assert_eq!(
            convert(body),
            "**Walkthrough**\n\n## Changes\n\n- One\n\nDone."
        );

        let body = "<details><summary><b>3 files</b> reviewed</summary>\nAll fine.\n</details>";
        assert_eq!(convert(body), "**3 files reviewed**\n\nAll fine.");
    }

    #[test]
    fn comments_scripts_and_styles_are_removed() {
        let body = "<!-- This is an auto-generated comment: summarize by coderabbit.ai -->\n<!--\nsecret\n-->\nHello <script>alert(1)</script>world<style>p{}</style>!";
        assert_eq!(convert(body), "Hello world!");
        assert_eq!(convert("Inline <!-- hidden --> comment"), "Inline  comment");
    }

    #[test]
    fn links_and_images_are_links_to_safe_urls_only() {
        assert_eq!(
            convert("See <a href=\"https://example.com/a b\">the docs</a>."),
            "See [the docs](<https://example.com/a b>)."
        );
        assert_eq!(
            convert("<a href=\"javascript:alert(1)\">click</a>"),
            "click"
        );
        assert_eq!(
            convert("<p align=\"center\"><img src=\"https://x.test/i.png\" alt=\"Logo [v2]\"></p>"),
            "[Logo \\[v2\\]](<https://x.test/i.png>)"
        );
        // A badge: an image inside a link keeps only its alt text.
        assert_eq!(
            convert("<a href='https://ci.test'><img src='https://ci.test/b.svg' alt='CI'/></a> ok"),
            "[CI](<https://ci.test>) ok"
        );
        assert_eq!(convert("<img src=data:image/png;base64,AAAA>"), "image");
    }

    #[test]
    fn tables_and_lists_stay_readable() {
        assert_eq!(
            convert("| a | b |\n|---|---|\n| one<br>two | <b>x</b> |"),
            "| a | b |\n|---|---|\n| one two | **x** |"
        );
        assert_eq!(
            convert("<ul>\n<li>One</li>\n<li>Two</li>\n</ul>"),
            "- One\n- Two"
        );
        assert_eq!(
            convert(
                "<table><tr><th>File</th><th>Change</th></tr><tr><td>a.rs</td><td>+1</td></tr></table>"
            ),
            "File | Change |\n\na.rs | +1 |"
        );
        assert_eq!(
            convert("<pre><code>let x = <b>1</b>;\n</code></pre>"),
            "```\nlet x = 1;\n```"
        );
    }

    #[test]
    fn tokenizer_handles_attributes_and_stray_brackets() {
        assert_eq!(
            tokenize("a <b x=1 y='2' z=\"3\" checked/> <3 </b>"),
            vec![
                Token::Text("a "),
                Token::Tag(HtmlTag {
                    name: "b".to_string(),
                    closing: false,
                    attributes: vec![
                        ("x".to_string(), "1".to_string()),
                        ("y".to_string(), "2".to_string()),
                        ("z".to_string(), "3".to_string()),
                        ("checked".to_string(), String::new()),
                    ],
                }),
                Token::Text(" <3 "),
                Token::Tag(HtmlTag {
                    name: "b".to_string(),
                    closing: true,
                    attributes: Vec::new(),
                }),
            ]
        );
        // An unterminated tag is text.
        assert_eq!(tokenize("<a href=\"x"), vec![Token::Text("<a href=\"x")]);
    }
}
